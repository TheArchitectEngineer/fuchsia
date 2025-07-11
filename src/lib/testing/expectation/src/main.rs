// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use expectations_matcher::Outcome;
use fuchsia_component::client;
use fuchsia_fs::file::read_in_namespace_to_string;
use futures::{StreamExt as _, TryStreamExt as _};
use itertools::Itertools as _;

fn outcome_from_test_status(status: fidl_fuchsia_test::Status) -> Outcome {
    match status {
        fidl_fuchsia_test::Status::Passed => Outcome::Pass,
        fidl_fuchsia_test::Status::Failed => Outcome::Fail,
        fidl_fuchsia_test::Status::Skipped => Outcome::Skip,
    }
}

#[derive(Debug)]
enum ExpectationError {
    Mismatch { got: Outcome, want: Outcome },
    NoExpectationFound,
}

struct CaseStart {
    invocation: fidl_fuchsia_test::Invocation,
    std_handles: fidl_fuchsia_test::StdHandles,
}

#[derive(Debug, Clone)]
struct CaseEnd {
    result: fidl_fuchsia_test::Result_,
}

#[derive(Debug)]
struct ExpectationsComparer {
    expectations: ser::Expectations,
}

impl ExpectationsComparer {
    fn expected_outcome(&self, invocation: &fidl_fuchsia_test::Invocation) -> Option<Outcome> {
        let name = invocation
            .name
            .as_ref()
            .unwrap_or_else(|| panic!("invocation {invocation:?} did not have name"));
        expectations_matcher::expected_outcome(name, &self.expectations)
    }

    fn check_against_expectation(
        &self,
        invocation: &fidl_fuchsia_test::Invocation,
        status: fidl_fuchsia_test::Status,
    ) -> Result<fidl_fuchsia_test::Status, ExpectationError> {
        let got_outcome = outcome_from_test_status(status);
        let want_outcome = self.expected_outcome(invocation);
        match (got_outcome, want_outcome) {
            (Outcome::Skip, None | Some(Outcome::Fail | Outcome::Pass | Outcome::Skip)) => {
                Ok(fidl_fuchsia_test::Status::Skipped)
            }
            (Outcome::Pass | Outcome::Fail, None) => Err(ExpectationError::NoExpectationFound),
            (got_outcome, Some(want_outcome)) if got_outcome == want_outcome => {
                Ok(fidl_fuchsia_test::Status::Passed)
            }
            (got_outcome, Some(want_outcome)) => {
                Err(ExpectationError::Mismatch { got: got_outcome, want: want_outcome })
            }
        }
    }

    async fn handle_case(
        &self,
        run_listener_proxy: &fidl_fuchsia_test::RunListenerProxy,
        CaseStart { invocation, std_handles }: CaseStart,
        end_stream: impl futures::TryStream<Ok = CaseEnd, Error = anyhow::Error>,
    ) -> Result<Option<(fidl_fuchsia_test::Invocation, ExpectationError)>, anyhow::Error> {
        let (case_listener_proxy, case_listener) = fidl::endpoints::create_proxy();
        run_listener_proxy
            .on_test_case_started(&invocation, std_handles, case_listener)
            .context("error calling run_listener_proxy.on_test_case_started(...)")?;

        let name = invocation.name.as_ref().expect("fuchsia.test/Invocation had no name");
        let case_listener_proxy = &case_listener_proxy;
        let result = match &end_stream
            .try_collect::<Vec<_>>()
            .await
            .context("error getting case results")?[..]
        {
            [] => return Err(anyhow::anyhow!("Received no result for case {}", name)),
            [CaseEnd { result }] => result.clone(),
            results => {
                return Err(anyhow::anyhow!(
                    "Received multiple results for case {}: {:?}",
                    name,
                    results
                ))
            }
        };
        let fidl_fuchsia_test::Result_ { status, .. } = result;
        let original_status = status.expect("fuchsia.test/Result had no status");
        let (status, expectation_error) =
            match self.check_against_expectation(&invocation, original_status) {
                Ok(status) => (status, None),
                Err(err) => {
                    match &err {
                        ExpectationError::Mismatch { got, want } => {
                            log::error!(
                                "Failing test case {}: got {:?}, expected {:?}",
                                name,
                                got,
                                want,
                            );
                        }
                        ExpectationError::NoExpectationFound => {
                            log::error!("No expectation matches {}", name);
                        }
                    };
                    (fidl_fuchsia_test::Status::Failed, Some(err))
                }
            };

        if matches!(
            (original_status, status),
            (fidl_fuchsia_test::Status::Failed, fidl_fuchsia_test::Status::Passed)
        ) {
            log::info!("{name} failure is expected, so it will be reported to the test runner as having passed.")
        } else if matches!(
            (original_status, status),
            (fidl_fuchsia_test::Status::Passed, fidl_fuchsia_test::Status::Passed)
        ) {
            log::info!("{name} success is expected.")
        }

        case_listener_proxy
            .finished(&fidl_fuchsia_test::Result_ { status: Some(status), ..Default::default() })
            .context("case listener proxy fidl error")?;

        Ok(expectation_error.map(|err| (invocation, err)))
    }

    async fn handle_suite_run_request(
        &self,
        suite_proxy: &fidl_fuchsia_test::SuiteProxy,
        tests: Vec<fidl_fuchsia_test::Invocation>,
        options: fidl_fuchsia_test::RunOptions,
        listener: fidl::endpoints::ClientEnd<fidl_fuchsia_test::RunListenerMarker>,
    ) -> Result<Vec<(fidl_fuchsia_test::Invocation, ExpectationError)>, anyhow::Error> {
        let tests_and_expects = tests.into_iter().map(|invocation| {
            let outcome = self.expected_outcome(&invocation);
            (invocation, outcome)
        });
        let (skipped, not_skipped): (Vec<_>, Vec<_>) = tests_and_expects
            .partition(|(_invocation, outcome)| matches!(outcome, Some(Outcome::Skip)));

        let listener_proxy = listener.into_proxy();
        for (invocation, _) in skipped {
            let (case_listener_proxy, case_listener_server_end) = fidl::endpoints::create_proxy();
            let name = invocation.name.as_ref().expect("fuchsia.test/Invocation had no name");
            log::info!("{name} skip is expected.");
            listener_proxy
                .on_test_case_started(
                    &invocation,
                    fidl_fuchsia_test::StdHandles::default(),
                    case_listener_server_end,
                )
                .context("error while telling run listener that a skipped test case had started")?;
            case_listener_proxy
                .finished(&fidl_fuchsia_test::Result_ {
                    status: Some(fidl_fuchsia_test::Status::Skipped),
                    ..Default::default()
                })
                .context(
                    "error while telling run listener that a skipped test case had finished",
                )?;
        }

        let failures = futures::lock::Mutex::new(Vec::new());

        if !not_skipped.is_empty() {
            let case_stream = {
                let (listener, listener_request_stream) = fidl::endpoints::create_request_stream();
                suite_proxy
                    .run(
                        &not_skipped
                            .into_iter()
                            .map(|(invocation, _outcome)| invocation)
                            .collect::<Vec<_>>(),
                        &options,
                        listener,
                    )
                    .context("error calling original test component's fuchsia.test/Suite#Run")?;
                listener_request_stream
                    .try_take_while(|request| {
                        futures::future::ok(!matches!(
                            request,
                            fidl_fuchsia_test::RunListenerRequest::OnFinished { control_handle: _ }
                        ))
                    })
                    .map_err(anyhow::Error::new)
                    .and_then(|request| match request {
                        fidl_fuchsia_test::RunListenerRequest::OnFinished { control_handle: _ } => {
                            unreachable!()
                        }
                        fidl_fuchsia_test::RunListenerRequest::OnTestCaseStarted {
                            invocation,
                            std_handles,
                            listener,
                            control_handle: _,
                        } => {
                            async move {
                                Ok((
                                    CaseStart { invocation, std_handles },
                                    listener
                                        .into_stream()
                                        .map_ok(
                                            |fidl_fuchsia_test::CaseListenerRequest::Finished {
                                                 result,
                                                 control_handle: _,
                                             }| {
                                                CaseEnd { result }
                                            },
                                        )
                                        .map_err(anyhow::Error::new),
                                ))
                            }
                        }
                    })
            };

            {
                let listener_proxy = &listener_proxy;
                let failures = &failures;
                case_stream
                    .try_for_each_concurrent(None, |(start, end_stream)| async move {
                        if let Some(result) =
                            self.handle_case(listener_proxy, start, end_stream).await?
                        {
                            failures.lock().await.push(result);
                        }
                        Ok(())
                    })
                    .await
                    .context("error handling test case stream")?;
            }
        }

        listener_proxy.on_finished().context("error calling listener_proxy.on_finished()")?;

        Ok(failures.into_inner())
    }

    async fn handle_suite_request_stream(
        &self,
        suite_request_stream: fidl_fuchsia_test::SuiteRequestStream,
    ) -> Result<(), anyhow::Error> {
        let suite_proxy = &client::connect_to_protocol::<fidl_fuchsia_test::SuiteMarker>()
            .context("error connecting to original test component's fuchsia.test/Suite")?;

        // `fx test`, via `ffx test`, connects to the `fuchsia.test/Suite` protocol only once, but
        // it makes multiple invocations to `fuchsia.test/Suite#Run`. Therefore, in order to print
        // all of the mismatched expectations at the end of the `fx test` invocation, we need to
        // collect them across the entire `fuchsia.test/Suite` request stream and emit them once the
        // `fuchsia.test/Suite` handle has been closed.
        let failures = suite_request_stream
            .map_err(anyhow::Error::new)
            .and_then(|request| async move {
                match request {
                    fidl_fuchsia_test::SuiteRequest::GetTests { iterator, control_handle: _ } => {
                        suite_proxy.get_tests(iterator).context("error enumerating test cases")?;
                        Ok(Vec::new())
                    }
                    fidl_fuchsia_test::SuiteRequest::Run {
                        tests,
                        options,
                        listener,
                        control_handle: _,
                    } => self
                        .handle_suite_run_request(suite_proxy, tests, options, listener)
                        .await
                        .context("error handling Suite run request"),
                }
            })
            .try_collect::<Vec<_>>()
            .await
            .context("error handling suite request stream")?
            .into_iter()
            .flatten();

        let (mismatch, missing): (Vec<_>, Vec<_>) =
            failures.partition_map(|(invocation, error)| match error {
                ExpectationError::Mismatch { got, want } => {
                    itertools::Either::Left((invocation, got, want))
                }
                ExpectationError::NoExpectationFound => itertools::Either::Right(invocation),
            });

        if !missing.is_empty() {
            log::error!("Observed {} test results with no matching expectation", missing.len());
            for invocation in missing {
                let name = invocation.name.unwrap();
                log::error!("{name} -- no expectation found");
            }
        }

        if !mismatch.is_empty() {
            log::error!("Observed {} test results that did not match expectations", mismatch.len());
            for (invocation, got, want) in mismatch {
                let name = invocation.name.unwrap();
                log::error!("{name} -- got {got:?}, expected {want:?}");
            }
        }
        Ok(())
    }
}

const EXPECTATIONS_SPECIFIC_PATH: &str = "/expectations/expectations.json5";
const EXPECTATIONS_PKG_PATH: &str = "/pkg/expectations.json5";

#[fuchsia::main]
async fn main() {
    let mut fs = fuchsia_component::server::ServiceFs::new_local();
    let _: &mut fuchsia_component::server::ServiceFsDir<'_, _> =
        fs.dir("svc").add_fidl_service(|s: fidl_fuchsia_test::SuiteRequestStream| s);
    let _: &mut fuchsia_component::server::ServiceFs<_> =
        fs.take_and_serve_directory_handle().expect("failed to serve ServiceFs directory");

    let expectations = if let Ok(expectations) =
        read_in_namespace_to_string(EXPECTATIONS_SPECIFIC_PATH).await
    {
        expectations
    } else {
        read_in_namespace_to_string(EXPECTATIONS_PKG_PATH).await.unwrap_or_else(|err| {
                panic!("failed to read expectations file at either {EXPECTATIONS_SPECIFIC_PATH} (for component-specific expectations) \
                        or {EXPECTATIONS_PKG_PATH} (for test-package-wide expectations): {err}")
            })
    };

    let comparer = ExpectationsComparer {
        expectations: serde_json5::from_str(&expectations).expect("failed to parse expectations"),
    };

    fs.then(|s| comparer.handle_suite_request_stream(s))
        .for_each_concurrent(None, |result| {
            let () = result.expect("error handling fuchsia.test/Suite request stream");
            futures::future::ready(())
        })
        .await
}

#[cfg(test)]
mod test {
    #[test]
    fn a_passing_test() {
        println!("this is a passing test")
    }

    #[fuchsia::test]
    async fn a_passing_test_with_err_logs() {
        log::error!("this is an error");
        println!("this is a passing test");
    }

    #[test]
    fn a_failing_test() {
        panic!("this is a failing test")
    }

    #[fuchsia::test]
    async fn a_failing_test_with_err_logs() {
        log::error!("this is an error");
        panic!("this is a failing test")
    }

    #[test]
    fn a_skipped_test() {
        unreachable!("this is a skipped test")
    }
}
