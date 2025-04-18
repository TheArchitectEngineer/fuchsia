// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{format_err, Context as _, Error};
use fidl_fidl_test_compatibility::{
    EchoEchoArraysWithErrorResult, EchoEchoMinimalWithErrorResult, EchoEchoStructWithErrorResult,
    EchoEchoTableWithErrorResult, EchoEchoUnionPayloadWithErrorRequest,
    EchoEchoUnionPayloadWithErrorRequestUnknown, EchoEchoVectorsWithErrorResult,
    EchoEchoXunionsWithErrorResult, EchoEvent, EchoMarker, EchoProxy, EchoRequest,
    EchoRequestStream, RequestUnion, RequestUnionUnknown, RespondWith, ResponseTable,
    ResponseUnion,
};
use fidl_fidl_test_imported::{
    ComposedEchoUnionResponseWithErrorComposedResponse, SimpleStruct, WantResponse,
};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt, TryStreamExt};
use std::thread;

fn connect_to_echo() -> Result<EchoProxy, Error> {
    let echo = connect_to_protocol::<EchoMarker>()?;
    Ok(echo)
}

async fn echo_server(stream: EchoRequestStream) -> Result<(), Error> {
    let handler = move |request| {
        Box::pin(async move {
            match request {
                EchoRequest::EchoMinimal { forward_to_server, responder } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        echo.echo_minimal("")
                            .await
                            .context("Error calling echo_minimal on proxy")?;
                    }
                    responder.send().context("Error responding")?;
                }
                EchoRequest::EchoMinimalWithError {
                    forward_to_server,
                    result_variant,
                    responder,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let result = echo
                            .echo_minimal_with_error("", result_variant)
                            .await
                            .context("Error calling echo_minimal_with_error on proxy")?;
                        responder.send(result).context("Error responding")?;
                    } else {
                        let result = if let RespondWith::Err = result_variant {
                            EchoEchoMinimalWithErrorResult::Err(0)
                        } else {
                            EchoEchoMinimalWithErrorResult::Ok(())
                        };
                        responder.send(result).context("Error responding")?;
                    }
                }
                EchoRequest::EchoMinimalNoRetVal { forward_to_server, control_handle } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        echo.echo_minimal_no_ret_val("")
                            .context("Error sending echo_minimal_no_ret_val to proxy")?;
                        let mut event_stream = echo.take_event_stream();
                        match event_stream
                            .try_next()
                            .await
                            .context("Error getting event response from proxy")?
                            .ok_or_else(|| format_err!("Proxy sent no events"))?
                        {
                            EchoEvent::EchoMinimalEvent {} => (),
                            _ => panic!("Unexpected event type"),
                        };
                    }
                    control_handle
                        .send_echo_minimal_event()
                        .context("Error responding with event")?;
                }
                EchoRequest::EchoStruct { mut value, forward_to_server, responder } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        value = echo
                            .echo_struct(value, "")
                            .await
                            .context("Error calling echo_struct on proxy")?;
                    }
                    responder.send(value).context("Error responding")?;
                }
                EchoRequest::EchoStructWithError {
                    value,
                    result_err,
                    forward_to_server,
                    result_variant,
                    responder,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let result = echo
                            .echo_struct_with_error(value, result_err, "", result_variant)
                            .await
                            .context("Error calling echo_struct_with_error on proxy")?;
                        responder.send(result).context("Error responding")?;
                    } else {
                        let result = if let RespondWith::Err = result_variant {
                            EchoEchoStructWithErrorResult::Err(result_err)
                        } else {
                            EchoEchoStructWithErrorResult::Ok(value)
                        };
                        responder.send(result).context("Error responding")?;
                    }
                }
                EchoRequest::EchoStructNoRetVal {
                    mut value,
                    forward_to_server,
                    control_handle,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        echo.echo_struct_no_ret_val(value, "")
                            .context("Error sending echo_struct_no_ret_val to proxy")?;
                        let mut event_stream = echo.take_event_stream();
                        if let EchoEvent::EchoEvent { value: response_val } = event_stream
                            .try_next()
                            .await
                            .context("Error getting event response from proxy")?
                            .ok_or_else(|| format_err!("Proxy sent no events"))?
                        {
                            value = response_val;
                        } else {
                            panic!("Unexpected event type");
                        }
                    }
                    control_handle.send_echo_event(value).context("Error responding with event")?;
                }
                EchoRequest::EchoArrays { mut value, forward_to_server, responder } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        value = echo
                            .echo_arrays(value, "")
                            .await
                            .context("Error calling echo_arrays on proxy")?;
                    }
                    responder.send(value).context("Error responding")?;
                }
                EchoRequest::EchoArraysWithError {
                    value,
                    result_err,
                    forward_to_server,
                    result_variant,
                    responder,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let result = echo
                            .echo_arrays_with_error(value, result_err, "", result_variant)
                            .await
                            .context("Error calling echo_struct_with_error on proxy")?;
                        responder.send(result).context("Error responding")?;
                    } else {
                        let result = if let RespondWith::Err = result_variant {
                            EchoEchoArraysWithErrorResult::Err(result_err)
                        } else {
                            EchoEchoArraysWithErrorResult::Ok(value)
                        };
                        responder.send(result).context("Error responding")?;
                    }
                }
                EchoRequest::EchoVectors { mut value, forward_to_server, responder } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        value = echo
                            .echo_vectors(value, "")
                            .await
                            .context("Error calling echo_vectors on proxy")?;
                    }
                    responder.send(value).context("Error responding")?;
                }
                EchoRequest::EchoVectorsWithError {
                    value,
                    result_err,
                    forward_to_server,
                    result_variant,
                    responder,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let result = echo
                            .echo_vectors_with_error(value, result_err, "", result_variant)
                            .await
                            .context("Error calling echo_struct_with_error on proxy")?;
                        responder.send(result).context("Error responding")?;
                    } else {
                        let result = if let RespondWith::Err = result_variant {
                            EchoEchoVectorsWithErrorResult::Err(result_err)
                        } else {
                            EchoEchoVectorsWithErrorResult::Ok(value)
                        };
                        responder.send(result).context("Error responding")?;
                    }
                }
                EchoRequest::EchoTable { mut value, forward_to_server, responder } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        value = echo
                            .echo_table(value, "")
                            .await
                            .context("Error calling echo_table on proxy")?;
                    }
                    responder.send(value).context("Error responding")?;
                }
                EchoRequest::EchoTableWithError {
                    value,
                    result_err,
                    forward_to_server,
                    result_variant,
                    responder,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let result = echo
                            .echo_table_with_error(value, result_err, "", result_variant)
                            .await
                            .context("Error calling echo_struct_with_error on proxy")?;
                        responder.send(result).context("Error responding")?;
                    } else {
                        let result = if let RespondWith::Err = result_variant {
                            EchoEchoTableWithErrorResult::Err(result_err)
                        } else {
                            EchoEchoTableWithErrorResult::Ok(value)
                        };
                        responder.send(result).context("Error responding")?;
                    }
                }
                EchoRequest::EchoXunions { mut value, forward_to_server, responder } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        value = echo
                            .echo_xunions(value, "")
                            .await
                            .context("Error calling echo_xunions on proxy")?;
                    }
                    responder.send(value).context("Error responding")?;
                }
                EchoRequest::EchoXunionsWithError {
                    value,
                    result_err,
                    forward_to_server,
                    result_variant,
                    responder,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let result = echo
                            .echo_xunions_with_error(value, result_err, "", result_variant)
                            .await
                            .context("Error calling echo_struct_with_error on proxy")?;
                        responder.send(result).context("Error responding")?;
                    } else {
                        let result = if let RespondWith::Err = result_variant {
                            EchoEchoXunionsWithErrorResult::Err(result_err)
                        } else {
                            EchoEchoXunionsWithErrorResult::Ok(value)
                        };
                        responder.send(result).context("Error responding")?;
                    }
                }

                EchoRequest::EchoNamedStruct { mut value, forward_to_server, responder } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        value = echo
                            .echo_named_struct(&value, "")
                            .await
                            .context("Error calling echo_named_struct on proxy")?;
                    }
                    responder.send(&value).context("Error responding")?;
                }
                EchoRequest::EchoNamedStructWithError {
                    value,
                    result_err,
                    forward_to_server,
                    result_variant,
                    responder,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let result = echo
                            .echo_named_struct_with_error(&value, result_err, "", result_variant)
                            .await
                            .context("Error calling echo_named_struct_with_error on proxy")?;
                        responder
                            .send(result.as_ref().map_err(|e| *e))
                            .context("Error responding")?;
                    } else {
                        let result = if let WantResponse::Err = result_variant {
                            Err(result_err)
                        } else {
                            Ok(&value)
                        };
                        responder.send(result).context("Error responding")?;
                    }
                }
                EchoRequest::EchoNamedStructNoRetVal {
                    mut value,
                    forward_to_server,
                    control_handle,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        echo.echo_named_struct_no_ret_val(&value, "")
                            .context("Error sending echo_named_struct_no_ret_val to proxy")?;
                        let mut event_stream = echo.take_event_stream();
                        if let EchoEvent::OnEchoNamedEvent { value: response_val } = event_stream
                            .try_next()
                            .await
                            .context("Error getting event response from proxy")?
                            .ok_or_else(|| format_err!("Proxy sent no events"))?
                        {
                            value = response_val;
                        } else {
                            panic!("Unexpected event type");
                        }
                    }
                    control_handle
                        .send_on_echo_named_event(&value)
                        .context("Error responding with event")?;
                }

                EchoRequest::EchoTablePayload { mut payload, responder } => {
                    let forward_to_server = payload.forward_to_server.take();
                    match forward_to_server {
                        Some(_forward_to_server) => {
                            let echo = connect_to_echo().context("Error connecting to proxy")?;
                            let resp = echo
                                .echo_table_payload(&payload)
                                .await
                                .context("Error calling echo_table_payload on proxy")?;
                            responder.send(&resp).context("Error responding")?;
                        }
                        None => {
                            let mut resp = ResponseTable::default();
                            resp.value = payload.value;
                            responder.send(&resp).context("Error responding")?;
                        }
                    }
                }
                EchoRequest::EchoTablePayloadWithError { mut payload, responder } => {
                    let forward_to_server = payload.forward_to_server.take();
                    match forward_to_server {
                        Some(_forward_to_server) => {
                            let echo = connect_to_echo().context("Error connecting to proxy")?;
                            let res = echo
                                .echo_table_payload_with_error(&payload)
                                .await
                                .context("Error calling echo_table_payload_with_error on proxy")?;
                            responder
                                .send(res.as_ref().map_err(|e| *e))
                                .context("Error responding")?;
                        }
                        None => {
                            let table;
                            let result = match payload.result_variant.unwrap() {
                                RespondWith::Success => {
                                    table = ResponseTable {
                                        value: payload.value,
                                        ..Default::default()
                                    };
                                    Ok(&table)
                                }
                                RespondWith::Err => Err(payload.result_err.unwrap()),
                            };
                            responder.send(result).context("Error responding")?;
                        }
                    }
                }
                EchoRequest::EchoTablePayloadNoRetVal { mut payload, control_handle } => {
                    let mut resp = ResponseTable::default();
                    let forward_to_server = payload.forward_to_server.take();
                    match forward_to_server {
                        Some(_forward_to_server) => {
                            let echo = connect_to_echo().context("Error connecting to proxy")?;
                            echo.echo_table_payload_no_ret_val(&payload)
                                .context("Error sending echo_table_payload_no_ret_val to proxy")?;
                            let mut event_stream = echo.take_event_stream();
                            if let EchoEvent::OnEchoTablePayloadEvent { payload: response } =
                                event_stream
                                    .try_next()
                                    .await
                                    .context("Error getting event response from proxy")?
                                    .ok_or_else(|| format_err!("Proxy sent no events"))?
                            {
                                resp = response
                            } else {
                                panic!("Unexpected event type");
                            }
                        }
                        None => {
                            resp.value = payload.value;
                        }
                    }
                    control_handle
                        .send_on_echo_table_payload_event(&resp)
                        .context("Error responding with event")?;
                }
                EchoRequest::EchoTableRequestComposed { mut payload, responder } => {
                    let forward_to_server = payload.forward_to_server.take();
                    match forward_to_server {
                        Some(_forward_to_server) => {
                            let echo = connect_to_echo().context("Error connecting to proxy")?;
                            let resp = echo
                                .echo_table_request_composed(&payload)
                                .await
                                .context("Error calling echo_table_payload on proxy")?;
                            responder.send(&resp).context("Error responding")?;
                        }
                        None => {
                            responder
                                .send(&SimpleStruct { f1: true, f2: payload.value.unwrap() })
                                .context("Error responding")?;
                        }
                    }
                }

                EchoRequest::EchoUnionPayload { mut payload, responder } => {
                    let forward_to_server = match payload {
                        RequestUnion::Unsigned(ref mut unsigned) => {
                            std::mem::take(&mut unsigned.forward_to_server)
                        }
                        RequestUnion::Signed(ref mut signed) => {
                            std::mem::take(&mut signed.forward_to_server)
                        }
                        RequestUnionUnknown!() => String::new(),
                    };
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let resp = echo
                            .echo_union_payload(&payload)
                            .await
                            .context("Error calling echo_union_payload on proxy")?;
                        responder.send(&resp).context("Error responding")?
                    } else {
                        let resp = match payload {
                            RequestUnion::Unsigned(unsigned) => {
                                ResponseUnion::Unsigned(unsigned.value.clone())
                            }
                            RequestUnion::Signed(signed) => {
                                ResponseUnion::Signed(signed.value.clone())
                            }
                            RequestUnionUnknown!() => {
                                return Err(format_err!("Unknown union variant"))
                            }
                        };
                        responder.send(&resp).context("Error responding")?;
                    }
                }
                EchoRequest::EchoUnionPayloadWithError { mut payload, responder } => {
                    let forward_to_server = match payload {
                        EchoEchoUnionPayloadWithErrorRequest::Unsigned(ref mut unsigned) => {
                            std::mem::take(&mut unsigned.forward_to_server)
                        }
                        EchoEchoUnionPayloadWithErrorRequest::Signed(ref mut signed) => {
                            std::mem::take(&mut signed.forward_to_server)
                        }
                        EchoEchoUnionPayloadWithErrorRequestUnknown!() => String::new(),
                    };
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let res = echo
                            .echo_union_payload_with_error(&payload)
                            .await
                            .context("Error calling echo_union_payload_with_error on proxy")?;
                        responder.send(res.as_ref().map_err(|e| *e)).context("Error responding")?;
                    } else {
                        let unsigned;
                        let signed;
                        let result = match payload {
                            EchoEchoUnionPayloadWithErrorRequest::Unsigned(errorable) => {
                                match errorable.result_variant {
                                    RespondWith::Success => {
                                        unsigned = ResponseUnion::Unsigned(errorable.value);
                                        Ok(&unsigned)
                                    }
                                    RespondWith::Err => Err(errorable.result_err),
                                }
                            }
                            EchoEchoUnionPayloadWithErrorRequest::Signed(errorable) => {
                                match errorable.result_variant {
                                    RespondWith::Success => {
                                        signed = ResponseUnion::Signed(errorable.value);
                                        Ok(&signed)
                                    }
                                    RespondWith::Err => Err(errorable.result_err),
                                }
                            }
                            EchoEchoUnionPayloadWithErrorRequestUnknown!() => {
                                return Err(format_err!("Unknown union variant"))
                            }
                        };
                        responder.send(result).context("Error responding")?
                    }
                }
                EchoRequest::EchoUnionPayloadNoRetVal { mut payload, control_handle } => {
                    let forward_to_server = match payload {
                        RequestUnion::Unsigned(ref mut unsigned) => {
                            std::mem::take(&mut unsigned.forward_to_server)
                        }
                        RequestUnion::Signed(ref mut signed) => {
                            std::mem::take(&mut signed.forward_to_server)
                        }
                        RequestUnionUnknown!() => String::new(),
                    };
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        echo.echo_union_payload_no_ret_val(&payload)
                            .context("Error sending echo_union_payload_no_ret_val to proxy")?;
                        let mut event_stream = echo.take_event_stream();
                        if let EchoEvent::OnEchoUnionPayloadEvent { payload: resp } = event_stream
                            .try_next()
                            .await
                            .context("Error getting event response from proxy")?
                            .ok_or_else(|| format_err!("Proxy sent no events"))?
                        {
                            control_handle
                                .send_on_echo_union_payload_event(&resp)
                                .context("Error responding with event")?;
                        } else {
                            panic!("Unexpected event type");
                        }
                    } else {
                        let resp = match payload {
                            RequestUnion::Unsigned(unsigned) => {
                                ResponseUnion::Unsigned(unsigned.value.clone())
                            }
                            RequestUnion::Signed(signed) => {
                                ResponseUnion::Signed(signed.value.clone())
                            }
                            RequestUnionUnknown!() => {
                                return Err(format_err!("Unknown union variant"))
                            }
                        };
                        control_handle
                            .send_on_echo_union_payload_event(&resp)
                            .context("Error responding with event")?;
                    }
                }
                EchoRequest::EchoUnionResponseWithErrorComposed {
                    value,
                    want_absolute_value,
                    forward_to_server,
                    result_err,
                    result_variant,
                    responder,
                } => {
                    if !forward_to_server.is_empty() {
                        let echo = connect_to_echo().context("Error connecting to proxy")?;
                        let res = echo
                            .echo_union_response_with_error_composed(
                                value,
                                want_absolute_value,
                                "",
                                result_err,
                                result_variant,
                            )
                            .await
                            .context(
                                "Error calling echo_union_response_with_error_composed on proxy",
                            )?;
                        responder.send(res.as_ref().map_err(|e| *e)).context("Error responding")?;
                    } else {
                        let unsigned;
                        let signed;
                        let resp = match result_variant {
                            WantResponse::Err => Err(result_err),
                            WantResponse::Success => {
                                if want_absolute_value {
                                    unsigned =  ComposedEchoUnionResponseWithErrorComposedResponse::Unsigned(
                                        value.abs() as u64,
                                    );
                                    Ok(&unsigned)
                                } else {
                                    signed =
                                        ComposedEchoUnionResponseWithErrorComposedResponse::Signed(
                                            value,
                                        );
                                    Ok(&signed)
                                }
                            }
                        };
                        responder.send(resp).context("Error responding")?
                    }
                }
            }
            Ok(())
        })
    };

    let handle_requests_fut = stream
        .err_into() // change error type from fidl::Error to anyhow::Error
        .try_for_each_concurrent(None /* max concurrent requests per connection */, handler);

    handle_requests_fut.await
}

fn main() -> Result<(), Error> {
    let argv: Vec<String> = std::env::args().collect();
    println!("argv={:?}", argv);

    const STACK_SIZE: usize = 1024 * 1024;

    // Create a child thread with a larger stack size to accommodate large structures being built.
    let thread_handle = thread::Builder::new().stack_size(STACK_SIZE).spawn(run_test)?;

    thread_handle.join().expect("Failed to join test thread")
}

fn run_test() -> Result<(), Error> {
    let mut executor = fasync::LocalExecutor::new();
    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(|stream| stream);
    fs.take_and_serve_directory_handle().context("Error serving directory handle")?;

    let serve_fut =
        fs.for_each_concurrent(None /* max concurrent connections */, |stream| async {
            if let Err(e) = echo_server(stream).await {
                eprintln!("Closing echo server {:?}", e);
            }
        });

    executor.run_singlethreaded(serve_fut);
    Ok(())
}
