// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::doctor_ledger::*;
use crate::ledger_view::*;
use anyhow::{anyhow, Context, Result};
use async_lock::Mutex;
use async_trait::async_trait;
use doctor_utils::{DaemonManager, DefaultDaemonManager, DoctorCheck, DoctorRecorder, Recorder};
use errors::{ffx_bail, ffx_error};
use ffx_build_version::VersionInfo;
use ffx_command::{ExternalSubToolSuite, FfxCommandLine, ToolSuite};
use ffx_config::{print_config, EnvironmentContext};
use ffx_daemon::DaemonConfig;
use ffx_doctor_args::DoctorCommand;
use ffx_ssh::{SshKeyErrorKind, SshKeyFiles};
use ffx_target::get_target_specifier;
use ffx_target_show::ShowTool;
use ffx_target_show_args::TargetShow;
use ffx_writer::{SimpleWriter, VerifiedMachineWriter};
use fho::{FfxMain, FfxTool, FhoEnvironment};
use fidl::endpoints::create_proxy;
use fidl::prelude::*;
use fidl_fuchsia_developer_ffx::{
    TargetCollectionMarker, TargetCollectionProxy, TargetCollectionReaderMarker,
    TargetCollectionReaderRequest, TargetInfo, TargetMarker, TargetQuery, TargetState,
};
use fidl_fuchsia_developer_remotecontrol::RemoteControlMarker;
use fuchsia_lockfile::{LockfileCreateError, LockfileCreateErrorKind};
use futures::TryStreamExt;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::io::{stdout, BufWriter, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use termion::{color, style};
use timeout::timeout;

mod doctor_ledger;
mod gcheck;
mod ledger_view;
mod single_target_diagnostics;

const DOCTOR_OUTPUT_FILENAME: &str = "doctor_output.txt";
const PLATFORM_INFO_FILENAME: &str = "platform.json";
const USER_CONFIG_FILENAME: &str = "user_config.txt";
const RECORD_CONFIG_SETTING: &str = "doctor.record_config";

#[derive(Debug, PartialEq)]
enum StepType {
    DoctorSummaryInitNormal(),
    DoctorSummaryInitVerbose(),
    GeneratingRecord,
    Output(String),
    RecordGenerated(PathBuf),
}

#[derive(Debug)]
enum StepResult {
    Success,
}

impl std::fmt::Display for StepResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StepResult::Success => "success".to_string(),
        };

        write!(f, "{}", s)
    }
}

impl std::fmt::Display for StepType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StepType::DoctorSummaryInitNormal() => {
                let msg = "Doctor summary (to see all details, run ffx doctor -v):".to_string();
                format!("\n{}{}{}\n", style::Bold, msg, style::Reset)
            }
            StepType::DoctorSummaryInitVerbose() => {
                let msg = "Doctor summary:".to_string();
                format!("\n{}{}{}\n", style::Bold, msg, style::Reset)
            }
            StepType::GeneratingRecord => "Generating record...".to_string(),
            StepType::Output(data_str) => {
                format!("{}", data_str)
            }
            StepType::RecordGenerated(path) => {
                format!("Record generated at: {}\n", path.to_string_lossy().into_owned())
            }
        };

        write!(f, "{}", s)
    }
}

#[async_trait]
trait DoctorStepHandler {
    async fn step(&mut self, step: StepType) -> Result<()>;
    async fn output_step(&mut self, step: StepType) -> Result<()>;
    async fn record(&mut self, step: StepType) -> Result<()>;
    async fn result(&mut self, result: StepResult) -> Result<()>;
}

struct DefaultDoctorStepHandler {
    recorder: Arc<Mutex<dyn Recorder + Send>>,
    writer: Box<dyn Write + Send + Sync>,
}

#[async_trait]
// The StepHandler interface exists to provide a clean separation between the
// imperative implementation logic and the output of each step. It has the added
// benefit of clearly indicating what each block of `doctor` is attempting to do.
impl DoctorStepHandler for DefaultDoctorStepHandler {
    // This is a logical step which will have a result. Right now the only difference
    // between it and an output_step is the addition of a newline after the step content.
    async fn step(&mut self, step: StepType) -> Result<()> {
        write!(&mut self.writer, "{}", step)?;
        self.writer.flush()?;
        let mut r = self.recorder.lock().await;
        r.add_content(DOCTOR_OUTPUT_FILENAME, format!("{}", step));
        Ok(())
    }

    // This is step which exists merely to provide output (such as an introduction or
    // result summary).
    async fn output_step(&mut self, step: StepType) -> Result<()> {
        writeln!(&mut self.writer, "{}", step)?;
        let mut r = self.recorder.lock().await;
        r.add_content(DOCTOR_OUTPUT_FILENAME, format!("{}\n", step));
        Ok(())
    }

    async fn record(&mut self, step: StepType) -> Result<()> {
        let mut r = self.recorder.lock().await;
        r.add_content(DOCTOR_OUTPUT_FILENAME, format!("{}", step));
        Ok(())
    }

    // This represents the result of a `step`.
    async fn result(&mut self, result: StepResult) -> Result<()> {
        writeln!(&mut self.writer, "{}", result)?;
        let mut r = self.recorder.lock().await;
        r.add_content(DOCTOR_OUTPUT_FILENAME, format!("{}\n", result));
        Ok(())
    }
}

impl DefaultDoctorStepHandler {
    fn new(
        recorder: Arc<Mutex<dyn Recorder + Send>>,
        writer: Box<dyn Write + Send + Sync>,
    ) -> Self {
        Self { recorder, writer }
    }
}

async fn get_config_permission<W: Write>(
    context: &EnvironmentContext,
    mut writer: W,
) -> Result<bool> {
    match context.get(RECORD_CONFIG_SETTING) {
        Ok(true) => {
            writeln!(
                &mut writer,
                "Config recording is enabled - config data will be recorded. You can change this \
                     with `ffx config set doctor.record_config false"
            )?;
            return Ok(true);
        }
        Ok(false) => {
            writeln!(
                &mut writer,
                "Config recording is disabled - config data will not be recorded. You can change \
                     this with `ffx config set doctor.record_config true"
            )?;
            return Ok(false);
        }
        _ => (),
    }

    let permission: bool;
    loop {
        let mut input = String::new();
        writeln!(&mut writer, "Do you want to include your config data `ffx config get`? [y/n]")?;
        // TODO(https://fxbug.dev/42161660) Use a generic read type instead of stdin
        std::io::stdin().read_line(&mut input)?;
        permission = match input.to_lowercase().trim() {
            "yes" | "y" => true,
            "no" | "n" => false,
            _ => continue,
        };
        break;
    }

    writeln!(
        &mut writer,
        "You can permanently enable or disable including config data in doctor records with:"
    )?;
    writeln!(&mut writer, "`ffx config set {} [true|false]`", RECORD_CONFIG_SETTING)?;
    fuchsia_async::Timer::new(Duration::from_millis(1000)).await;

    Ok(permission)
}

struct DoctorRecorderParameters {
    record: bool,
    user_config_enabled: bool,
    log_root: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    recorder: Arc<Mutex<dyn Recorder>>,
}

pub struct ShowToolWrapper {
    env: FhoEnvironment,
    inner: Option<ShowTool>,
}

impl ShowToolWrapper {
    async fn allocate(&mut self, target_spec: Option<String>) -> fho::Result<()> {
        let context = self
            .env
            .ffx_command()
            .global
            .load_context(self.env.environment_context().exe_kind())?;
        let mut new_ffx = self.env.ffx_command().clone();
        new_ffx.global.target = target_spec;
        let fho_env = FhoEnvironment::new(&context, &new_ffx);
        self.inner.replace(ShowTool::from_env(fho_env, TargetShow::default()).await?);
        Ok(())
    }

    /// This requires that `allocate` is run first. This is really only to ensure that there are
    /// two steps in the process for running an invocation of `ffx target show`.
    async fn run(&mut self) -> fho::Result<(String, String)> {
        let tool = self.inner.take().unwrap();
        let buffers = ffx_writer::TestBuffers::default();
        match tool.main(VerifiedMachineWriter::new_test(None, &buffers)).await {
            Ok(_) => Ok(buffers.into_strings()),
            Err(e) => Err(fho::user_error!("{}\n\tstderr: {}", e, buffers.into_stderr_str())),
        }
    }
}

#[async_trait(?Send)]
impl fho::TryFromEnv for ShowToolWrapper {
    async fn try_from_env(env: &FhoEnvironment) -> fho::Result<Self> {
        Ok(Self { env: env.clone(), inner: None })
    }
}

#[derive(FfxTool)]
pub struct DoctorTool {
    #[command]
    cmd: DoctorCommand,
    show_tool: ShowToolWrapper,
    context: EnvironmentContext,
}

fho::embedded_plugin!(DoctorTool);

#[async_trait(?Send)]
impl FfxMain for DoctorTool {
    type Writer = SimpleWriter;

    async fn main(self, _writer: Self::Writer) -> fho::Result<()> {
        // TODO(b/373720502): This is passing a `Some(self.show_tool)` to make it simpler not to
        // have to update existing tests that take in a dozen arguments. The proper approach for
        // this is to refactor `ffx doctor` to make testing things like this less cumbersome.
        // TODO(b/373723080): Add actual tests for the usage of `ffx target show` within `ffx
        // doctor`.
        doctor_cmd_impl(self.context, self.cmd, Some(self.show_tool), stdout()).await?;
        Ok(())
    }
}

pub async fn doctor_cmd_impl<W: Write + Send + Sync + 'static>(
    context: EnvironmentContext,

    mut cmd: DoctorCommand,
    show_tool: Option<ShowToolWrapper>,
    mut writer: W,
) -> Result<()> {
    let gchecker = gcheck::DefaultGChecker;
    let node = overnet_core::Router::new(None)
        .with_context(|| ffx_error!("Could not initialize Overnet"))?;
    let ascendd_path = context.get_ascendd_path().await?;
    let daemon_manager = DefaultDaemonManager::new(node, ascendd_path);
    let delay = Duration::from_millis(cmd.retry_delay);
    let target_spec = ffx_target::get_target_specifier(&context).await?;
    let target_str = target_spec.unwrap_or_else(String::default);
    let version_info: VersionInfo = context.build_info();
    let mut log_root = None;
    let mut output_dir = None;
    let mut record = cmd.record;
    // Force-enable verbose mode if `record` is enabled.
    if record {
        cmd.verbose = true;
    }
    match context.get("log.enabled") {
        Ok(enabled) => {
            let enabled: bool = enabled;
            if !enabled && cmd.record {
                writeln!(&mut writer,
                    "{}WARNING:{} --record was provided but ffx logs are not enabled. This means your record will only include doctor output.",
                    color::Fg(color::Red), style::Reset
                )?;
                writeln!(&mut writer,
                    "ffx doctor will proceed, but if you want to enable logs, you can do so by running:"
                )?;
                writeln!(&mut writer, "  ffx config set log.enabled true")?;
                writeln!(&mut writer, "You will then need to restart the ffx daemon:")?;
                writeln!(&mut writer, "  ffx doctor --force-restart\n\n")?;
                fuchsia_async::Timer::new(Duration::from_millis(10000)).await;
            }

            log_root = Some(context.get("log.dir")?);
            let final_output_dir =
                cmd.output_dir.map(|s| PathBuf::from(s)).unwrap_or(std::env::current_dir()?);

            if !final_output_dir.is_dir() {
                ffx_bail!(
                    "cannot record: output directory does not exist or is unreadable: {:?}",
                    output_dir
                );
            }

            output_dir = Some(final_output_dir);
        }
        Err(e) => {
            writeln!(
                &mut writer,
                "{}WARNING:{} getting log status from ffx config failed. The error was: {:?}",
                color::Fg(color::Red),
                style::Reset,
                e
            )?;
            if cmd.record {
                writeln!(
                    &mut writer,
                    "Record mode requires configuration and will be turned off for this run."
                )?;
            }
            writeln!(
                &mut writer,
                "If this issue persists, please file a bug here: {}",
                errors::BUG_REPORT_URL
            )?;
            fuchsia_async::Timer::new(Duration::from_millis(10000)).await;

            record = false;
        }
    };

    let user_config_enabled = if !record || cmd.no_config {
        false
    } else {
        match get_config_permission(&context, &mut writer).await {
            Ok(b) => b,
            Err(e) => {
                writeln!(&mut writer, "Failed to get permission to record config data: {}", e)?;
                writeln!(&mut writer, "Config data will not be recorded")?;
                false
            }
        }
    };

    if cmd.repair_keys {
        let keys = SshKeyFiles::load(Some(&context)).await?;
        let message = keys.check_keys(true)?;
        writeln!(&mut writer, "{message}")?;
    }

    let recorder = Arc::new(Mutex::new(DoctorRecorder::new()));
    let mut handler = DefaultDoctorStepHandler::new(recorder.clone(), Box::new(writer));
    let target_spec =
        get_target_specifier(&context).await.map_err(|e| format!("{:?}", e).replace("\n", ""));

    // create ledger
    let ledger_mode = match cmd.verbose {
        true => LedgerViewMode::Verbose,
        false => LedgerViewMode::Normal,
    };
    let mut ledger = DoctorLedger::<std::io::Stdout>::new(
        stdout(),
        Box::new(VisualLedgerView::new()),
        ledger_mode,
    );

    doctor(
        &mut handler,
        &mut ledger,
        &daemon_manager,
        &target_str,
        cmd.retry_count,
        delay,
        cmd.restart_daemon,
        version_info,
        target_spec,
        &context,
        DoctorRecorderParameters {
            record,
            user_config_enabled,
            log_root,
            output_dir,
            recorder: recorder.clone(),
        },
        gchecker,
        show_tool,
        true,
    )
    .await?;

    Ok(())
}

fn get_kernel_name() -> Result<String> {
    Ok(String::from_utf8(Command::new("uname").output()?.stdout)?)
}

async fn list_targets(query: Option<&str>, tc: &TargetCollectionProxy) -> Result<Vec<TargetInfo>> {
    let (reader, server) = fidl::endpoints::create_endpoints::<TargetCollectionReaderMarker>();

    tc.list_targets(
        &TargetQuery { string_matcher: query.map(|s| s.to_owned()), ..Default::default() },
        reader,
    )?;
    let mut res = Vec::new();
    let mut stream = server.into_stream();
    while let Ok(Some(TargetCollectionReaderRequest::Next { entry, responder })) =
        stream.try_next().await
    {
        responder.send()?;
        if entry.len() > 0 {
            res.extend(entry);
        } else {
            break;
        }
    }
    Ok(res)
}

fn get_platform_info() -> Result<String> {
    let kernel_name = match get_kernel_name() {
        Ok(s) => s,
        Err(e) => format!("Could not get kernel name: {}", e),
    };

    let platform_info = json!({
        "kernel_name": kernel_name.replace("\n",""),
    });

    Ok(serde_json::to_string_pretty(&platform_info)?)
}

fn get_api_level(api_level: Option<u64>) -> String {
    match api_level {
        Some(api) => format!("{}", api),
        None => "UNKNOWN".to_string(),
    }
}

fn get_abi_revision(revision: Option<u64>) -> String {
    match revision {
        Some(abi) => format!("{:#X}", abi),
        None => "UNKNOWN".to_string(),
    }
}

fn get_user_config(ctx: &EnvironmentContext) -> Result<String> {
    let mut writer = BufWriter::new(Vec::new());
    print_config(ctx, &mut writer)?;
    let config_str = String::from_utf8(writer.into_inner()?)?;
    Ok(config_str)
}

async fn doctor<W: Write>(
    step_handler: &mut impl DoctorStepHandler,
    ledger: &mut DoctorLedger<W>,
    daemon_manager: &impl DaemonManager,
    target_str: &str,
    _retry_count: usize,
    retry_delay: Duration,
    restart_daemon: bool,
    version_info: VersionInfo,
    target_spec: Result<Option<String>, String>,
    env_context: &EnvironmentContext,
    record_params: DoctorRecorderParameters,
    gchecker: impl gcheck::GChecker,
    show_tool: Option<ShowToolWrapper>,
    run_additional_diagnostics: bool,
) -> Result<()> {
    if restart_daemon {
        doctor_daemon_restart(daemon_manager, retry_delay, ledger).await?;
    }

    doctor_summary(
        step_handler,
        daemon_manager,
        target_str,
        retry_delay,
        version_info,
        target_spec,
        env_context,
        show_tool,
        run_additional_diagnostics,
        gchecker,
        ledger,
    )
    .await?;

    if record_params.record {
        let mut record_view = RecordLedgerView::new();
        let data = ledger.write_all(&mut record_view)?;
        step_handler.record(StepType::Output(data)).await?;
        doctor_record(env_context, step_handler, record_params).await?;
    }

    Ok(())
}

async fn doctor_record(
    ctx: &EnvironmentContext,
    step_handler: &mut impl DoctorStepHandler,
    record_params: DoctorRecorderParameters,
) -> Result<()> {
    let log_root =
        record_params.log_root.context("log_root not present despite record set to true")?;
    let output_dir =
        record_params.output_dir.context("output_dir not present despite record set to true")?;

    let log_files: Vec<PathBuf> = collect_log_files(log_root.clone())?;

    step_handler.step(StepType::GeneratingRecord).await?;

    let platform_info = match get_platform_info() {
        Ok(s) => s,
        Err(e) => format!("Could not serialize platform info: {}", e),
    };

    let final_path = {
        let mut r = record_params.recorder.lock().await;
        r.add_sources(log_files);
        r.add_content(PLATFORM_INFO_FILENAME, platform_info);

        if record_params.user_config_enabled {
            let config_str = match get_user_config(ctx) {
                Ok(s) => s,
                Err(e) => format!("Could not get config data output: {}", e),
            };
            r.add_content(USER_CONFIG_FILENAME, config_str);
        }

        match r.generate(output_dir.clone()) {
            Ok(p) => p,
            Err(e) => {
                let path = &output_dir.to_str().unwrap_or("path undefined");
                let advice = "You can change the output directory for the generated zip file \
                                  using `--output-dir`.";
                let default_err_msg =
                    Err(anyhow!("{}\nCould not write to: {}\n{}", e, &path, advice));

                match e.downcast_ref::<zip::result::ZipError>() {
                    Some(zip::result::ZipError::Io(io_error)) => match io_error.raw_os_error() {
                        Some(27) => Err(anyhow!(
                            "{}\nMake sure you can write files larger than 1MB to: {}\n{}",
                            e,
                            &path,
                            advice
                        ))?,
                        _ => default_err_msg?,
                    },
                    _ => default_err_msg?,
                }
            }
        }
    };

    step_handler.result(StepResult::Success).await?;
    step_handler.output_step(StepType::RecordGenerated(final_path.canonicalize()?)).await?;
    Ok(())
}

fn collect_log_files(root_dir: PathBuf) -> Result<Vec<PathBuf>> {
    let now = std::time::SystemTime::now();
    // Get all log files that have been modified recently.
    const NINETY_DAYS_SECS: u64 = 60 * 60 * 24 * 90;
    const MAX_AGE: Duration = Duration::from_secs(NINETY_DAYS_SECS);

    let list = root_dir
        .read_dir()?
        .filter_map(|entry| {
            if let Ok(d) = entry {
                Some(d.path())
            } else {
                log::info!("Skipping read dir was an error: {entry:?}");
                None
            }
        })
        .filter_map(|p| {
            if p.is_dir() {
                log::info!("Skipping dir {:?}", p);
                None
            } else {
                Some(p)
            }
        })
        .filter(|p| {
            if p.extension().unwrap_or_default() == "log" {
                true
            } else {
                log::info!("Skipping non .log extension {:?}", p);
                false
            }
        })
        .filter_map(|p| match fs::metadata(p.clone()) {
            Ok(mdata) => Some((p, mdata)),
            Err(e) => {
                log::error!("could not read metadata for {:?}: {e}", p);
                None
            }
        })
        .filter_map(|(p, mdata)| match mdata.modified() {
            Ok(mdate) => Some((p, mdate)),
            Err(e) => {
                log::error!("could not read modified time for {:?}: {e}", p);
                None
            }
        })
        .filter_map(|(p, mdate)| match now.duration_since(mdate) {
            Ok(age) => {
                if age < MAX_AGE {
                    Some(p)
                } else {
                    log::info!("Skipping {p:?} too  old {}", age.as_secs());
                    None
                }
            }
            Err(e) => {
                log::error!("could not determine duration {p:?}: {e}");
                None
            }
        })
        .collect();
    Ok(list)
}

async fn get_daemon_pid<W: Write>(
    daemon_manager: &impl DaemonManager,
    ledger: &mut DoctorLedger<W>,
) -> Option<Vec<usize>> {
    match daemon_manager.get_pid().await {
        Ok(vec) => return Some(vec),
        Err(e) => {
            let node = ledger
                .add_node(&format!("Error getting daemon pid: {}", e), LedgerMode::Automatic)
                .ok()?;
            ledger.set_outcome(node, LedgerOutcome::SoftWarning).ok()?;
            return None;
        }
    }
}

// Return the elements of `a` that are not in `b`.
// Note: this function preserves order for simpler testing.
fn difference(a: &Vec<usize>, b: &Vec<usize>) -> Vec<usize> {
    let sb: HashSet<usize> = b.iter().cloned().collect();
    a.iter().filter(|&e| !sb.contains(e)).cloned().collect()
}

// Update the current, the added, and the dropped daemon pids.
// Display if there are any errors while fetching the pids.
// Note: we display pid fetching error only one time. has_error is set once there is an error.
async fn calc_daemon_pid_diff<W: Write>(
    has_error: &mut bool,
    daemon_manager: &impl DaemonManager,
    ledger: &mut DoctorLedger<W>,
    current_pids: &mut Vec<usize>,
    added_pids: &mut Vec<usize>,
    dropped_pids: &mut Vec<usize>,
) {
    // Setup
    added_pids.clear();
    dropped_pids.clear();

    if *has_error {
        current_pids.clear();
        return ();
    }

    // Get pid vector
    let new_pids = match get_daemon_pid(daemon_manager, ledger).await {
        Some(v) => v,
        None => {
            current_pids.clear();
            *has_error = true;
            return ();
        }
    };

    // Update
    added_pids.extend(difference(&new_pids, current_pids));
    dropped_pids.extend(difference(current_pids, &new_pids));
    current_pids.clear();
    current_pids.extend(new_pids);
}

fn format_vec(a: &Vec<usize>) -> String {
    format!(
        "[{}]",
        a.iter()
            .enumerate()
            .map(|(i, v)| match i {
                0 => format!("{}", v),
                _ => format!(", {}", v),
            })
            .collect::<String>(),
    )
}

async fn daemon_restart<W: Write>(
    daemon_manager: &impl DaemonManager,
    retry_delay: Duration,
    ledger: &mut DoctorLedger<W>,
) -> Result<()> {
    let mut main_node = ledger.add_node("Killing Daemon", LedgerMode::Automatic)?;

    let mut error_pid = false;
    let mut cur_pids = Vec::<usize>::new();
    let mut add_pids = Vec::<usize>::new();
    let mut sub_pids = Vec::<usize>::new();
    let error = &mut error_pid;
    let cpid = &mut cur_pids;
    let apid = &mut add_pids;
    let spid = &mut sub_pids;

    calc_daemon_pid_diff(error, daemon_manager, ledger, cpid, apid, spid).await;

    // Kill the daemon if it is running.
    let daemon_killed = if daemon_manager.is_daemon_running().await {
        let node = ledger.add_node("Killing running daemons.", LedgerMode::Automatic)?;
        daemon_manager.kill_all().await?;
        ledger.set_outcome(node, LedgerOutcome::Success)?;
        true
    } else {
        if daemon_manager.kill_all().await? {
            let node = ledger.add_node("Killing zombie daemons.", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            true
        } else {
            let node = ledger.add_node("No running daemons found.", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            false
        }
    };

    // Display killed daemon PIDs.
    calc_daemon_pid_diff(error, daemon_manager, ledger, cpid, apid, spid).await;
    if daemon_killed && !*error {
        {
            let node = ledger.add_node(
                &format!("Killed daemon PID: {}", format_vec(spid)),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }

        if cpid.len() > 0 {
            let node = ledger.add_node(
                &format!("Daemon are still running, PID: {}", format_vec(cpid)),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Warning)?;
        }
    }

    ledger.close(main_node)?;

    if daemon_killed {
        // HACK: Wait a few seconds before spawning a new daemon. Attempting
        // to spawn one too quickly after killing one will lead to timeouts
        // when attempting to communicate with the spawned daemon.
        // Temporary fix for https://fxbug.dev/42145822. Remove when that bug is resolved.
        fuchsia_async::Timer::new(Duration::from_millis(5000)).await;
    };

    main_node = ledger.add_node("Starting Daemon", LedgerMode::Automatic)?;

    // Spawn daemon.
    match timeout(retry_delay, daemon_manager.spawn()).await {
        Ok(Ok(_)) => {
            let node = ledger.add_node("Daemon spawned", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }
        Ok(Err(e)) => {
            let node =
                ledger.add_node(&format!("Error spawning daemon: {}", e), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Err(_) => {
            let node = ledger.add_node("Timeout spawning daemon", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    }

    calc_daemon_pid_diff(error, daemon_manager, ledger, cpid, apid, spid).await;
    if !*error {
        let node =
            ledger.add_node(&format!("Daemon PID: {}", format_vec(apid)), LedgerMode::Automatic)?;
        ledger.set_outcome(node, LedgerOutcome::Success)?;
    }

    // Check daemon connection.
    let daemon_proxy = match timeout(retry_delay, daemon_manager.find_and_connect()).await {
        Ok(Ok(val)) => {
            let node = ledger.add_node("Connected to daemon", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            val
        }
        Ok(Err(e)) => {
            let node = ledger.add_node(
                &format!("Error connecting to daemon: {}. Run `ffx doctor --restart-daemon`", e),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Err(_) => {
            let node = ledger.add_node(
                "Timeout while connecting to daemon. Run `ffx doctor --restart-daemon`",
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    };

    match timeout(retry_delay, daemon_proxy.get_version_info()).await {
        Ok(Ok(v)) => {
            let daemon_version = v.build_version.clone().unwrap_or_else(|| "UNKNOWN".to_string());
            let node = ledger
                .add_node(&format!("Daemon version: {}", daemon_version), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            let node = ledger.add_node(
                &format!("abi-revision: {}", get_abi_revision(v.abi_revision)),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            let node = ledger.add_node(
                &format!("api-level: {}", get_api_level(v.api_level)),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }
        Ok(Err(e)) => {
            let node = ledger
                .add_node(&format!("Error getting daemon version: {}", e), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Err(_) => {
            let node =
                ledger.add_node("Timeout while getting daemon version", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    }

    ledger.close(main_node)?;
    return Ok(());
}

async fn doctor_daemon_restart<W: Write>(
    daemon_manager: &impl DaemonManager,
    spawn_delay: Duration,
    ledger: &mut DoctorLedger<W>,
) -> Result<()> {
    match daemon_restart(daemon_manager, spawn_delay, ledger).await {
        Err(err) => {
            let node = ledger.add_node(&format!("Error: {}", err), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
        }
        _ => (),
    };
    return Ok(());
}

fn make_ssh_fix_suggestion(ssh_log: &String) -> Option<&'static str> {
    if ssh_log.contains("Connection refused") {
        Some("SSH connection was refused. You may need to (re-)establish a tunnel connection.")
    } else if ssh_log.contains("Permission denied") {
        Some("SSH connection could not authenticate. You may need to re-provision (pave or flash) your target to ensure SSH keys are appropriately setup.")
    } else {
        None
    }
}

async fn doctor_summary<W: Write>(
    step_handler: &mut impl DoctorStepHandler,
    daemon_manager: &impl DaemonManager,
    target_str: &str,
    retry_delay: Duration,
    version_info: VersionInfo,
    target_spec: Result<Option<String>, String>,
    env_context: &EnvironmentContext,
    mut show_tool: Option<ShowToolWrapper>,
    run_additional_diagnostics: bool,
    gchecker: impl gcheck::GChecker,
    ledger: &mut DoctorLedger<W>,
) -> Result<()> {
    match ledger.get_ledger_mode() {
        LedgerViewMode::Normal => {
            step_handler.output_step(StepType::DoctorSummaryInitNormal()).await?
        }
        LedgerViewMode::Verbose => {
            step_handler.output_step(StepType::DoctorSummaryInitVerbose()).await?
        }
    }

    let mut main_node = ledger.add_node("FFX doctor", LedgerMode::Automatic)?;
    let frontend_version =
        version_info.build_version.clone().unwrap_or_else(|| "UNKNOWN".to_string());
    let version_node =
        ledger.add_node(&format!("Frontend version: {}", frontend_version), LedgerMode::Verbose)?;
    ledger.set_outcome(version_node, LedgerOutcome::Success)?;

    let abi_revision_node = ledger.add_node(
        &format!("abi-revision: {}", get_abi_revision(version_info.abi_revision)),
        LedgerMode::Verbose,
    )?;
    ledger.set_outcome(abi_revision_node, LedgerOutcome::Success)?;

    let api_level_node = ledger.add_node(
        &format!("api-level: {}", get_api_level(version_info.api_level)),
        LedgerMode::Verbose,
    )?;
    ledger.set_outcome(api_level_node, LedgerOutcome::Success)?;

    let ffx_path = match std::env::current_exe() {
        Ok(path) => format!("{}", path.display()),
        _ => "not found".to_string(),
    };
    let ffx_path_node =
        ledger.add_node(&format!("Path to ffx: {}", ffx_path), LedgerMode::Verbose)?;
    ledger.set_outcome(ffx_path_node, LedgerOutcome::Info)?;

    ledger.close(main_node)?;

    main_node = ledger.add_node("FFX Environment Context", LedgerMode::Normal)?;

    let environment_kind_node = ledger.add_node(
        &format!("Kind of Environment: {kind}", kind = env_context.env_kind()),
        LedgerMode::Normal,
    )?;
    ledger.set_outcome(environment_kind_node, LedgerOutcome::Success)?;

    let (outcome, description) = match env_context.env_file_path() {
        Ok(env_file) => (
            LedgerOutcome::Success,
            format!("Environment File Location: {env_file}", env_file = env_file.display()),
        ),
        Err(e) => {
            (LedgerOutcome::Failure, format!("Error find or loading the environment file: {e:?}"))
        }
    };
    let env_file_node = ledger.add_node(&description, LedgerMode::Verbose)?;
    ledger.set_outcome(env_file_node, outcome)?;

    let build_dir_node = if let Some(build_dir) = env_context.build_dir() {
        ledger.add_node(
            &format!(
                "Environment-default build directory: {build_dir}",
                build_dir = build_dir.display()
            ),
            LedgerMode::Normal,
        )?
    } else {
        ledger.add_node("No build directory discovered in the environment.", LedgerMode::Verbose)?
    };
    ledger.set_outcome(build_dir_node, LedgerOutcome::Success)?;

    let lock_node = ledger.add_node("Config Lock Files", LedgerMode::Automatic)?;

    for (file, locked) in ffx_config::environment::Environment::check_locks(env_context).await? {
        let (outcome, description) = match locked {
            Ok(lockfile) => (
                LedgerOutcome::Success,
                format!("{path} locked by {lock}", path=file.display(), lock=lockfile.display()),
            ),
            Err(err) => {
                match *err {
                    LockfileCreateError {
                        kind: LockfileCreateErrorKind::TimedOut,
                        lock_path,
                        owner,
                        ..
                    } => {
                        let mut msg = format!(
                            "Lockfile `{lockfile}` was owned by another process that didn't release it in our timeout.",
                            lockfile=lock_path.display(),
                        );

                        if let Some(owner) = owner {
                            msg = format!("{msg} Check that it's running? Pid {pid}", pid=owner.pid);
                        }

                        (LedgerOutcome::Failure, msg)
                    }
                    LockfileCreateError {
                        kind: LockfileCreateErrorKind::Io(error),
                        lock_path,
                        ..
                    } => {
                        (
                            LedgerOutcome::Failure,
                            format!(
                                "Could not open lockfile `{lockfile}` due to error: {error:?}. Check permissions on the directory.",
                                lockfile=lock_path.display(),
                            ),
                        )
                    }
                }
            }
        };
        let node = ledger.add_node(&description, LedgerMode::Automatic)?;
        ledger.set_outcome(node, outcome)?;
    }

    ledger.close(lock_node)?;

    // Check SSH Keys
    let ssh_node: usize;
    match SshKeyFiles::load(None).await {
        Ok(ssh_files) => {
            let ( description, outcome) = match ssh_files.check_keys(false) {
                Ok(_) => (format!("The public & private Fuchsia keys are consistent"), LedgerOutcome::Success),
                Err(e)  => {
                    match e.kind {
                        SshKeyErrorKind::BadKeyType => (format!("SSH keys type not supported: {}", e.message), LedgerOutcome::Warning),
                        SshKeyErrorKind::BadConfiguration => (format!("SSH keys configuration problem: {e}"), LedgerOutcome::Failure),
                        SshKeyErrorKind::IOError | SshKeyErrorKind::FileNotFound => (format!("{}. Check configuration or run `ffx doctor --repair-keys`", e.message), LedgerOutcome::Failure),
                        SshKeyErrorKind::KeyMismatch => (format!("{}. Check configuration or run `ffx doctor --repair-keys`", e.message), LedgerOutcome::Failure),
                        _ => (format!("SSH keys problem: {e}. Check configuration or run `ffx doctor --repair-keys`"), LedgerOutcome::Failure)
                    }
                }
            };
            ssh_node = ledger.add_node(&description, LedgerMode::Automatic)?;
            ledger.set_outcome(ssh_node, outcome)?;
        }
        Err(e) => {
            ssh_node = ledger
                .add_node(&format!("Could not get SSH key paths {e}"), LedgerMode::Automatic)?;
            ledger.set_outcome(ssh_node, LedgerOutcome::Failure)?;
        }
    };
    ledger.close(ssh_node)?;

    ledger.close(main_node)?;

    main_node = ledger.add_node("Checking daemon", LedgerMode::Automatic)?;

    if daemon_manager.is_daemon_running().await {
        let pid_vec = get_daemon_pid(daemon_manager, ledger).await.unwrap_or_default();
        let node = ledger
            .add_node(&format!("Daemon found: {}", format_vec(&pid_vec)), LedgerMode::Automatic)?;
        ledger.set_outcome(node, LedgerOutcome::Success)?;
    } else {
        let node = ledger.add_node(
            "No running daemons found. Run `ffx doctor --restart-daemon`",
            LedgerMode::Automatic,
        )?;
        ledger.set_outcome(node, LedgerOutcome::Failure)?;
        ledger.close(main_node)?;
        return Ok(());
    }

    let daemon_proxy = match timeout(retry_delay, daemon_manager.find_and_connect()).await {
        Ok(Ok(val)) => {
            let node = ledger.add_node("Connecting to daemon", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            val
        }
        Ok(Err(e)) => {
            let node = ledger.add_node(
                &format!("Error connecting to daemon: {}. Run `ffx doctor --restart-daemon`", e),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Err(_) => {
            let node = ledger.add_node(
                "Timeout while connecting to daemon. Run `ffx doctor --restart-daemon`",
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    };

    match timeout(retry_delay, daemon_proxy.get_version_info()).await {
        Ok(Ok(v)) => {
            let daemon_version = v.build_version.clone().unwrap_or_else(|| "UNKNOWN".to_string());
            let node = ledger
                .add_node(&format!("Daemon version: {}", daemon_version), LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            let path = std::env::current_exe().map(|x| x.to_string_lossy().to_string()).ok();
            let have_path = path.is_some();
            if let (Some(path), Some(exec_path)) = (path, v.exec_path.clone()) {
                if path != exec_path {
                    let node = ledger.add_node(
                        &format!("Daemon ran from {} but this command is {}. Run `ffx doctor --restart-daemon`", exec_path, path),
                        LedgerMode::Automatic,
                    )?;
                    ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
                }

                let node = ledger.add_node(&format!("path: {}", exec_path), LedgerMode::Verbose)?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
            } else if !have_path {
                let node = ledger.add_node(
                    "Could not get current command path to compare with daemon",
                    LedgerMode::Automatic,
                )?;
                ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
            } else {
                let node = ledger.add_node("Daemon is too old to report its executable path. Run `ffx doctor --restart-daemon`", LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
            }

            let node = ledger.add_node(
                &format!("abi-revision: {}", get_abi_revision(v.abi_revision)),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            let node = ledger.add_node(
                &format!("api-level: {}", get_api_level(v.api_level)),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;

            if v.api_level != version_info.api_level {
                let node = ledger.add_node("Daemon and frontend are at different API levels. Run `ffx doctor --restart-daemon`", LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
            }
        }
        Ok(Err(e)) => {
            let node = ledger
                .add_node(&format!("Error getting daemon version: {}", e), LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            // Continue, not a critical error.
        }
        Err(_) => {
            let node =
                ledger.add_node("Timeout while getting daemon version", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            // Continue, not a critical error.
        }
    }

    match target_spec {
        Ok(t) => {
            let default_target_display = {
                if t.is_none() || t.as_ref().unwrap().is_empty() {
                    "(none)".to_string()
                } else {
                    t.unwrap()
                }
            };
            let node = ledger.add_node(
                &format!("Default target: {}", default_target_display),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
        }
        Err(e) => {
            let node =
                ledger.add_node(&format!("config read failed: {:?}", e), LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
        }
    }

    ledger.close(main_node)?;

    /*
    Look up external sub tool suite to check for existence of gdoctor fo Googler-specific
    network configuration checks.
    */
    match ExternalSubToolSuite::from_env(&env_context) {
        Ok(sub_tool_suite) => {
            let command_line_args =
                vec!["ffx".to_string(), "gdoctor".to_string(), "all-safe".to_string()];
            let mut command = FfxCommandLine::new(None, &command_line_args).unwrap();
            let mut ffx_args = ["--machine".to_string(), "json".to_string()].to_vec();
            command.ffx_args.append(&mut ffx_args);
            let workspace_command = sub_tool_suite.find_workspace_tool(&command);
            match workspace_command {
                // If the command exists in the workspace call it and show the results
                Some(wcmd) => {
                    main_node = ledger.add_node("Google Network Checks", LedgerMode::Automatic)?;
                    let (_exit_status, stdout, _stderr) = wcmd.run_and_capture().await?;
                    for line in stdout.trim().lines().filter(|l| !l.trim().is_empty()) {
                        match serde_json::from_str::<DoctorCheck>(&line) {
                            Ok(data) => {
                                let node = ledger.add_node(
                                    &format!("{}: {}", data.name, data.message),
                                    LedgerMode::Automatic,
                                )?;
                                ledger.set_outcome(
                                    node,
                                    match data.passed {
                                        true => LedgerOutcome::Success,
                                        false => LedgerOutcome::Failure,
                                    },
                                )?;
                            }
                            Err(e) => {
                                eprintln!(
                            "Warning: Failed to parse gdoctor output line as DoctorCheck: {}",
                            e
                        );
                            }
                        }
                    }
                    ledger.close(main_node)?;
                }
                None => {
                    if gchecker.is_gcorp_machine() {
                        let network_check_node =
                            ledger.add_node("Google Network Checks", LedgerMode::Automatic)?;
                        let node = ledger.add_node(
                                &format!(
                                    "Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`"
                                ),
                                LedgerMode::Automatic,
                            )?;
                        ledger.set_outcome(node, LedgerOutcome::Failure)?;
                        ledger.close(network_check_node)?;
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("Warning: could not find subtool suite: {}", e);
        }
    }

    main_node = ledger.add_node("Searching for targets", LedgerMode::Automatic)?;
    let (tc_proxy, tc_server) = fidl::endpoints::create_proxy::<TargetCollectionMarker>();
    match timeout(
        retry_delay,
        daemon_proxy
            .connect_to_protocol(TargetCollectionMarker::PROTOCOL_NAME, tc_server.into_channel()),
    )
    .await
    {
        Ok(Err(e)) => {
            let node = ledger.add_node(
                &format!("Error connecting to target service: {}", e),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Ok(_) => {}
        Err(_) => {
            let node = ledger
                .add_node("Timeout while connecting to target service", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    }

    let targets = match timeout(retry_delay, list_targets(Some(target_str), &tc_proxy)).await {
        Ok(Ok(list)) => {
            if list.len() > 0 {
                let node = ledger
                    .add_node(&format!("{} targets found", list.len()), LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
                list
            } else {
                let node = ledger.add_node("No targets found!", LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                ledger.close(main_node)?;
                return Ok(());
            }
        }
        Ok(Err(e)) => {
            let node =
                ledger.add_node(&format!("Error getting targets: {}", e), LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
        Err(_) => {
            let node =
                ledger.add_node("Timeout while getting target list", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(main_node)?;
            return Ok(());
        }
    };

    ledger.close(main_node)?;
    let mut verify_inode = LedgerNode::new("Verifying Targets".to_string(), LedgerMode::Normal);
    verify_inode.set_fold_function(OutcomeFoldFunction::FailureToSuccess, LedgerOutcome::Failure);
    main_node = ledger.add(verify_inode)?;

    for target in targets.iter() {
        let target_name = target.nodename.clone().unwrap_or_else(|| "UNKNOWN".to_string());

        // Note: this match statement intentionally does not have a fallback case in order to
        // ensure that behavior is considered when we add a new state.
        match target.target_state {
            None => {}
            Some(TargetState::Unknown) => {}
            Some(TargetState::Disconnected) => {}
            Some(TargetState::Product) => {}
            Some(TargetState::Fastboot) => {
                let node = ledger.add_node(
                    &format!(
                        "Target found in fastboot mode: {}",
                        target.serial_number.as_deref().unwrap_or("UNKNOWN serial number")
                    ),
                    LedgerMode::Automatic,
                )?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
                continue;
            }
            Some(TargetState::Zedboot) => {
                let node = ledger.add_node(
                    &format!("Skipping target in zedboot: {}", target_name),
                    LedgerMode::Automatic,
                )?;
                ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
                continue;
            }
        }
        let target_node =
            ledger.add_node(&format!("Target: {}", target_name), LedgerMode::Normal)?;

        let (compatibility_state, compatibility_message) = match &target.compatibility {
            Some(info) => (info.state.into(), info.message.clone()),
            None => (
                compat_info::CompatibilityState::Absent,
                "Compatibility information is not available".to_string(),
            ),
        };

        let state_node = ledger.add_node(
            &format!("Compatibility state: {compatibility_state}"),
            LedgerMode::Verbose,
        )?;
        let message_node = ledger.add_node(&compatibility_message, LedgerMode::Verbose)?;

        let outcome = match compatibility_state {
            compat_info::CompatibilityState::Supported => LedgerOutcome::Success,
            compat_info::CompatibilityState::Error => LedgerOutcome::Failure,
            compat_info::CompatibilityState::Absent => LedgerOutcome::SoftWarning,
            compat_info::CompatibilityState::Unsupported => LedgerOutcome::Warning,
            compat_info::CompatibilityState::Unknown => LedgerOutcome::SoftWarning,
        };
        ledger.set_outcome(state_node, outcome)?;
        ledger.set_outcome(message_node, outcome)?;

        //TODO(https://fxbug.dev/42167543): Offer a fix when we cannot connect to a device via RCS.
        let (target_proxy, target_server) = fidl::endpoints::create_proxy::<TargetMarker>();
        match timeout(
            retry_delay,
            tc_proxy.open_target(
                &TargetQuery { string_matcher: target.nodename.clone(), ..Default::default() },
                target_server,
            ),
        )
        .await
        {
            Ok(Ok(_)) => {
                let node = ledger.add_node("Opened target handle", LedgerMode::Verbose)?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
            }
            Ok(Err(e)) => {
                let node = ledger.add_node(
                    &format!("Error while opening target handle: {}", e),
                    LedgerMode::Verbose,
                )?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                ledger.close(target_node)?;
                continue;
            }
            Err(_) => {
                let node =
                    ledger.add_node("Timeout while opening target handle", LedgerMode::Verbose)?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                ledger.close(target_node)?;
                continue;
            }
        }

        let (remote_proxy, remote_server_end) = create_proxy::<RemoteControlMarker>();

        match timeout(retry_delay, target_proxy.open_remote_control(remote_server_end)).await {
            Ok(Ok(res)) => {
                let node = ledger.add_node("Connecting to RCS", LedgerMode::Verbose)?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
                match res {
                    Ok(_) => {}
                    Err(_) => {
                        let logs = target_proxy.get_ssh_logs().await?;
                        let node = ledger.add_node(
                            &format!("Error while connecting to RCS: could not establish SSH connection to the target: {}", logs),
                            LedgerMode::Verbose,
                        )?;
                        ledger.set_outcome(node, LedgerOutcome::Failure)?;
                        if let Some(suggestion) = make_ssh_fix_suggestion(&logs) {
                            let node = ledger.add_node(suggestion, LedgerMode::Automatic)?;
                            ledger.set_outcome(node, LedgerOutcome::Info)?;
                        }
                        ledger.close(target_node)?;
                        continue;
                    }
                };
            }
            Ok(Err(e)) => {
                let node = ledger.add_node(
                    &format!("Error while connecting to RCS: {}", e),
                    LedgerMode::Verbose,
                )?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                ledger.close(target_node)?;
                continue;
            }
            Err(_) => {
                let node =
                    ledger.add_node("Timeout while connecting to RCS", LedgerMode::Verbose)?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                ledger.close(target_node)?;
                continue;
            }
        }

        match timeout(retry_delay, remote_proxy.identify_host()).await {
            Ok(Ok(_)) => {
                let node = ledger.add(LedgerNode::new(
                    "Communicating with RCS".to_string(),
                    LedgerMode::Verbose,
                ))?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
            }
            Ok(Err(e)) => {
                let node = ledger.add_node(
                    &format!("Error while communicating with RCS: {}", e),
                    LedgerMode::Verbose,
                )?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                ledger.close(target_node)?;
                continue;
            }
            Err(_) => {
                let node =
                    ledger.add_node("Timeout while communicating with RCS", LedgerMode::Verbose)?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                ledger.close(target_node)?;
                continue;
            }
        }

        if let Some(ref mut show_tool) = show_tool.as_mut() {
            let node = ledger
                .add_node("Running `ffx target show` against device", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Info)?;
            match show_tool.allocate(target.nodename.clone()).await {
                Ok(_) => {
                    let node = ledger.add(LedgerNode::new(
                        "Allocating proxies for `target show`".to_string(),
                        LedgerMode::Verbose,
                    ))?;
                    ledger.set_outcome(node, LedgerOutcome::Success)?;
                    match show_tool.run().await {
                        Ok((stdout, stderr)) => {
                            let node = ledger.add(LedgerNode::new(
                                "Executing `ffx target show`".to_string(),
                                LedgerMode::Verbose,
                            ))?;
                            ledger.set_outcome(node, LedgerOutcome::Success)?;
                            let node = ledger.add(LedgerNode::new(
                                format!("stdout:\n\t{}", stdout.replace("\n", "\n\t"),),
                                LedgerMode::Verbose,
                            ))?;
                            ledger.set_outcome(node, LedgerOutcome::Info)?;
                            if !stderr.is_empty() {
                                let node = ledger.add(LedgerNode::new(
                                    format!("stderr:\n\t{}", stderr.replace("\n", "\n\t")),
                                    LedgerMode::Verbose,
                                ))?;
                                ledger.set_outcome(node, LedgerOutcome::Info)?;
                            }
                        }
                        Err(e) => {
                            let node = ledger.add_node(
                                &format!("Error executing `target show`: {:?}", e),
                                LedgerMode::Verbose,
                            )?;
                            ledger.set_outcome(node, LedgerOutcome::Failure)?;
                        }
                    }
                }
                Err(e) => {
                    let node = ledger.add_node(
                        &format!("Error while setting up `target show`: {:?}", e),
                        LedgerMode::Verbose,
                    )?;
                    ledger.set_outcome(node, LedgerOutcome::Failure)?;
                }
            };
        }

        // TODO(b/423023263): This function is missing test coverage. There should either be
        // something mockable here, and the underlying crate should also be tested.
        if run_additional_diagnostics {
            let node = ledger.add_node(
                &format!("Running additional diagnostics against {target_name}"),
                LedgerMode::Verbose,
            )?;

            crate::single_target_diagnostics::run_single_target_diagnostics(
                env_context,
                target.clone(),
                ledger,
                retry_delay,
            )
            .await?;
            ledger.close(node)?;
        }

        ledger.close(target_node)?;
    }

    ledger.close(main_node)?;

    match ledger.calc_outcome(main_node) {
        LedgerOutcome::Failure => {
            let msg = match ledger.get_ledger_mode() {
                LedgerViewMode::Normal => String::from(
                    "Doctor found issues in one or more categories; \
                    run 'ffx doctor -v' for more details.",
                ),
                _ => String::from("Doctor found issues in one or more categories."),
            };
            main_node = ledger.add_node(&msg, LedgerMode::Automatic)?;
            ledger.set_outcome(main_node, LedgerOutcome::Failure)?;
        }
        _ => {
            main_node = ledger.add_node("No issues found", LedgerMode::Automatic)?;
            ledger.set_outcome(main_node, LedgerOutcome::Success)?;
        }
    }

    Ok(())
}

///////////////////////////////////////////////////////////////////////////////////////////////////
// Tests
///////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test {
    use super::*;
    use async_trait::async_trait;
    use ffx_config::{ConfigLevel, TestEnv};
    use ffx_doctor_test_utils::MockWriter;
    use fidl::endpoints::{ProtocolMarker, Request, RequestStream, ServerEnd};
    use fidl::Channel;
    use fidl_fuchsia_developer_ffx::{
        DaemonProxy, DaemonRequest, OpenTargetError, RemoteControlState, TargetCollectionRequest,
        TargetCollectionRequestStream, TargetConnectionError, TargetRequest,
    };
    use fidl_fuchsia_developer_remotecontrol::{IdentifyHostResponse, RemoteControlRequest};
    use fidl_test_util::spawn_local_stream_handler;
    use fuchsia_async as fasync;
    use futures::channel::oneshot::{self, Receiver};
    use futures::future::Shared;
    use futures::{Future, FutureExt, TryFutureExt};
    use std::cell::Cell;
    use std::fmt;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    const NODENAME: &str = "fake-nodename";
    const UNRESPONSIVE_NODENAME: &str = "fake-nodename-unresponsive";
    const SSH_ERR_NODENAME: &str = "fake-nodename-ssh-error";
    const FASTBOOT_NODENAME: &str = "fastboot-nodename-unresponsive";
    const NON_EXISTENT_NODENAME: &str = "extra-fake-nodename";
    const SERIAL_NUMBER: &str = "123123123";
    const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(2000);
    const DAEMON_VERSION: &str = "daemon-build-string";
    const FRONTEND_VERSION: &str = "fake version";
    const INDENT_STR: &str = "    ";
    const FAKE_ABI_REVISION: u64 = 17063755220075245312;
    const ABI_REVISION_STR: &str = "0xECCEA2F70ACD6F00";
    const FAKE_API_LEVEL: u64 = 7;
    const ANOTHER_FAKE_API_LEVEL: u64 = 8;

    struct FakeGChecker;

    impl gcheck::GChecker for FakeGChecker {
        fn is_gcorp_machine(&self) -> bool {
            true
        }
    }

    #[derive(PartialEq)]
    struct TestStep {
        step_type: StepType,
        output_only: bool,
    }

    impl std::fmt::Debug for TestStep {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let output_str = if self.output_only { " (output)" } else { "" };

            write!(f, "{:?}{}", self.step_type, output_str)
        }
    }

    struct TestStepEntry {
        step: Option<TestStep>,
        result: Option<StepResult>,
    }

    impl TestStepEntry {
        fn step(step_type: StepType) -> Self {
            Self { step: Some(TestStep { step_type, output_only: false }), result: None }
        }

        fn output_step(step_type: StepType) -> Self {
            Self { step: Some(TestStep { step_type, output_only: true }), result: None }
        }

        fn result(result: StepResult) -> Self {
            Self { result: Some(result), step: None }
        }
    }

    impl std::fmt::Debug for TestStepEntry {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if self.step.is_some() {
                write!(f, "{:?}", self.step.as_ref().unwrap())
            } else if self.result.is_some() {
                write!(f, "{:?}", self.result.as_ref().unwrap())
            } else {
                panic!("attempted to debug TestStepEntry with empty step and result")
            }
        }
    }

    impl PartialEq for TestStepEntry {
        fn eq(&self, other: &Self) -> bool {
            if self.step != other.step {
                return false;
            }

            match (self.result.as_ref(), other.result.as_ref()) {
                (Some(r), Some(r2)) => match (r, r2) {
                    (StepResult::Success, StepResult::Success) => true,
                },
                (None, None) => true,
                _ => false,
            }
        }
    }

    struct FakeLedgerView {
        tree: LedgerViewNode,
        omit_error_reason: bool,
    }

    impl FakeLedgerView {
        pub fn new() -> Self {
            FakeLedgerView { tree: LedgerViewNode::default(), omit_error_reason: true }
        }
        pub fn new_with_error_reason() -> Self {
            FakeLedgerView { tree: LedgerViewNode::default(), omit_error_reason: false }
        }
        fn gen_output(&self, parent_node: &LedgerViewNode, indent_level: usize) -> String {
            let mut data = parent_node.data.clone();
            // Remove error details to make the tests more stable
            if self.omit_error_reason && data.starts_with("Error") {
                let v: Vec<_> = data.split(":").collect();
                if v.len() > 1 {
                    data = format!("{}: <reason omitted>", v.first().unwrap().to_string());
                }
            }

            let mut output_str = format!(
                "{}[{}] {}\n",
                INDENT_STR.repeat(indent_level),
                parent_node.outcome.format(false),
                data
            );

            for child_node in &parent_node.children {
                let child_str = self.gen_output(child_node, indent_level + 1);
                output_str = format!("{}{}", output_str, child_str);
            }

            return output_str;
        }
    }

    impl fmt::Display for FakeLedgerView {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.gen_output(&self.tree, 0))
        }
    }

    impl LedgerView for FakeLedgerView {
        fn set(&mut self, new_tree: LedgerViewNode) {
            self.tree = new_tree;
        }
    }

    struct FakeStepHandler {
        steps: Arc<Mutex<Vec<TestStepEntry>>>,
    }
    impl FakeStepHandler {
        fn new() -> Self {
            Self { steps: Arc::new(Mutex::new(Vec::new())) }
        }

        async fn assert_matches_steps(&self, expected_steps: Vec<TestStepEntry>) {
            let steps = self.steps.lock().await;
            if *steps != expected_steps {
                println!("got: {:#?}\nexpected: {:#?}", steps, expected_steps);

                for (step, expected) in steps.iter().zip(expected_steps) {
                    if *step != expected {
                        println!("different step: got: {:?}, expected: {:?}", step, expected)
                    }
                }
                panic!("steps didn't match. differences are listed above.");
            }
        }
    }

    #[async_trait]
    impl DoctorStepHandler for FakeStepHandler {
        async fn step(&mut self, step: StepType) -> Result<()> {
            let mut v = self.steps.lock().await;
            v.push(TestStepEntry::step(step));
            Ok(())
        }

        async fn output_step(&mut self, step: StepType) -> Result<()> {
            let mut v = self.steps.lock().await;
            v.push(TestStepEntry::output_step(step));
            Ok(())
        }

        async fn record(&mut self, step: StepType) -> Result<()> {
            let mut v = self.steps.lock().await;
            v.push(TestStepEntry::output_step(step));
            Ok(())
        }

        async fn result(&mut self, result: StepResult) -> Result<()> {
            let mut v = self.steps.lock().await;
            v.push(TestStepEntry::result(result));
            Ok(())
        }
    }

    struct FakeRecorder {
        expected_sources: Vec<PathBuf>,
        expected_output_dir: PathBuf,
        generate_called: Cell<bool>,
    }

    impl FakeRecorder {
        fn new(expected_sources: Vec<PathBuf>, expected_output_dir: PathBuf) -> Self {
            return Self {
                expected_sources,
                expected_output_dir,
                generate_called: Cell::new(false),
            };
        }

        fn assert_generate_called(&self) {
            assert!(self.generate_called.get())
        }

        fn result_path() -> PathBuf {
            PathBuf::from("/tmp").canonicalize().unwrap()
        }
    }

    impl Recorder for FakeRecorder {
        fn add_sources(&mut self, sources: Vec<PathBuf>) {
            let source_set: HashSet<_> = sources.iter().collect();
            let expected_set: HashSet<_> = self.expected_sources.iter().collect();
            assert_eq!(source_set, expected_set);
        }

        fn add_content(&mut self, _filename: &str, _content: String) {
            // Do nothing, we don't verify output in tests.
        }

        fn generate(&self, output_dir: PathBuf) -> Result<PathBuf> {
            assert_eq!(output_dir, self.expected_output_dir);
            self.generate_called.set(true);
            Ok(Self::result_path())
        }
    }
    struct DisabledRecorder {}

    impl DisabledRecorder {
        fn new() -> Self {
            return Self {};
        }
    }

    impl Recorder for DisabledRecorder {
        fn add_sources(&mut self, _sources: Vec<PathBuf>) {
            panic!("add_sources should not be called.")
        }

        fn add_content(&mut self, _filename: &str, _content: String) {
            // Do nothing, we don't verify output in tests.
        }

        fn generate(&self, _output_dir: PathBuf) -> Result<PathBuf> {
            panic!("generate should not be called.")
        }
    }

    struct FakeStateManager {
        kill_results: Vec<Result<bool>>,
        daemons_running_results: Vec<bool>,
        spawn_results: Vec<Result<()>>,
        find_and_connect_results: Vec<Result<DaemonProxy>>,
        get_pid_results: Vec<Result<Vec<usize>>>,
    }

    struct FakeDaemonManager {
        state_manager: Arc<Mutex<FakeStateManager>>,
    }

    impl FakeDaemonManager {
        fn new(
            daemons_running_results: Vec<bool>,
            kill_results: Vec<Result<bool>>,
            spawn_results: Vec<Result<()>>,
            find_and_connect_results: Vec<Result<DaemonProxy>>,
            get_pid_results: Vec<Result<Vec<usize>>>,
        ) -> Self {
            return FakeDaemonManager {
                state_manager: Arc::new(Mutex::new(FakeStateManager {
                    kill_results,
                    daemons_running_results,
                    spawn_results,
                    find_and_connect_results,
                    get_pid_results,
                })),
            };
        }

        async fn assert_no_leftover_calls(&self) {
            let state = self.state_manager.lock().await;
            assert!(
                state.kill_results.is_empty(),
                "too few calls to kill_all. remaining entries: {:?}",
                state.kill_results
            );
            assert!(
                state.daemons_running_results.is_empty(),
                "too few calls to is_daemon_running. remaining entries: {:?}",
                state.daemons_running_results
            );
            assert!(
                state.spawn_results.is_empty(),
                "too few calls to spawn. remaining entries: {:?}",
                state.spawn_results
            );
            assert!(
                state.find_and_connect_results.is_empty(),
                "too few calls to find_and_connect. remaining entries: {:?}",
                state.find_and_connect_results
            );
        }
    }

    #[async_trait]
    impl DaemonManager for FakeDaemonManager {
        async fn kill_all(&self) -> Result<bool> {
            let mut state = self.state_manager.lock().await;
            assert!(!state.kill_results.is_empty(), "too many calls to kill_all");
            state.kill_results.remove(0)
        }

        async fn get_pid(&self) -> Result<Vec<usize>> {
            let mut state = self.state_manager.lock().await;
            assert!(!state.get_pid_results.is_empty(), "too many calls to spawn");
            state.get_pid_results.remove(0)
        }

        async fn is_daemon_running(&self) -> bool {
            let mut state = self.state_manager.lock().await;
            assert!(
                !state.daemons_running_results.is_empty(),
                "too many calls to is_daemon_running"
            );
            state.daemons_running_results.remove(0)
        }

        async fn spawn(&self) -> Result<()> {
            let mut state = self.state_manager.lock().await;
            assert!(!state.spawn_results.is_empty(), "too many calls to spawn");
            state.spawn_results.remove(0)
        }

        async fn find_and_connect(&self) -> Result<DaemonProxy> {
            let mut state = self.state_manager.lock().await;
            assert!(
                !state.find_and_connect_results.is_empty(),
                "too many calls to find_and_connect"
            );
            state.find_and_connect_results.remove(0)
        }
    }

    fn serve_stream<T, F, Fut>(stream: T::RequestStream, mut f: F)
    where
        T: ProtocolMarker,
        F: FnMut(Request<T>) -> Fut + 'static + std::marker::Send,
        Fut: Future<Output = ()> + 'static + std::marker::Send,
    {
        fasync::Task::local(
            stream
                .try_for_each(move |r| f(r).map(Ok))
                .unwrap_or_else(|e| panic!("failed to handle request: {:?}", e)),
        )
        .detach();
    }

    // Spawns a target collection, accepting closures for handling listing and opening target
    // handles.
    fn spawn_target_collection<F, F2>(
        server_channel: Channel,
        list_closure: F,
        open_targets_closure: F2,
    ) where
        F: Fn(TargetQuery) -> Vec<TargetInfo> + Clone + 'static,
        F2: Fn(TargetQuery, ServerEnd<TargetMarker>) -> Result<(), OpenTargetError>
            + Clone
            + 'static,
    {
        let channel = fidl::AsyncChannel::from_channel(server_channel);
        let mut stream = TargetCollectionRequestStream::from_channel(channel);
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    TargetCollectionRequest::ListTargets { query, reader, .. } => {
                        let reader = reader.into_proxy();
                        let list_closure = list_closure.clone();
                        let results = (list_closure)(query);
                        if !results.is_empty() {
                            reader.next(&results).await.unwrap();
                            reader.next(&[]).await.unwrap();
                        } else {
                            reader.next(&[]).await.unwrap();
                        }
                    }
                    TargetCollectionRequest::OpenTarget { query, responder, target_handle } => {
                        let res = (open_targets_closure)(query, target_handle);
                        responder.send(res).unwrap();
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }

    fn spawn_target_handler<F>(target_handle: ServerEnd<TargetMarker>, handler: F)
    where
        F: Fn(TargetRequest) -> () + 'static,
    {
        fuchsia_async::Task::local(async move {
            let mut stream = target_handle.into_stream();
            while let Ok(Some(req)) = stream.try_next().await {
                (handler)(req)
            }
        })
        .detach();
    }

    fn setup_responsive_daemon_server() -> DaemonProxy {
        spawn_local_stream_handler(move |req| async move {
            match req {
                DaemonRequest::GetVersionInfo { responder } => {
                    responder.send(&daemon_version_info()).unwrap();
                }
                DaemonRequest::ConnectToProtocol { responder, name: _, server_end } => {
                    spawn_target_collection(
                        server_end,
                        |_| vec![],
                        |_query, target_handle| {
                            spawn_target_handler(target_handle, |req| match req {
                                TargetRequest::OpenRemoteControl {
                                    responder,
                                    remote_control: _,
                                } => {
                                    responder.send(Ok(())).unwrap();
                                }
                                r => panic!("unexpected request: {:?}", r),
                            });
                            Ok(())
                        },
                    );
                    responder.send(Ok(())).unwrap();
                }
                _ => {
                    assert!(false, "got unexpected request: {:?}", req);
                }
            }
        })
    }

    fn serve_responsive_rcs(server_end: ServerEnd<RemoteControlMarker>) {
        serve_stream::<RemoteControlMarker, _, _>(
            server_end.into_stream(),
            move |req| async move {
                match req {
                    RemoteControlRequest::IdentifyHost { responder } => responder
                        .send(Ok(&IdentifyHostResponse {
                            addresses: Some(vec![]),
                            nodename: Some(NODENAME.to_string()),
                            ..Default::default()
                        }))
                        .unwrap(),
                    _ => panic!("Unexpected request: {:?}", req),
                }
            },
        );
    }
    fn serve_unresponsive_rcs(
        server_end: ServerEnd<RemoteControlMarker>,
        waiter: Shared<Receiver<()>>,
    ) {
        serve_stream::<RemoteControlMarker, _, _>(server_end.into_stream(), move |req| {
            let waiter = waiter.clone();
            async move {
                match req {
                    RemoteControlRequest::IdentifyHost { responder: _ } => {
                        waiter.await.unwrap();
                    }
                    _ => panic!("Unexpected request: {:?}", req),
                }
            }
        });
    }

    fn setup_responsive_daemon_server_with_fastboot_target() -> DaemonProxy {
        spawn_local_stream_handler(move |req| async move {
            match req {
                DaemonRequest::GetVersionInfo { responder } => {
                    responder.send(&daemon_version_info()).unwrap();
                }
                DaemonRequest::ConnectToProtocol { name: _, server_end, responder } => {
                    spawn_target_collection(
                        server_end,
                        |_| {
                            vec![TargetInfo {
                                nodename: Some(FASTBOOT_NODENAME.to_string()),
                                serial_number: Some(SERIAL_NUMBER.to_string()),
                                addresses: Some(vec![]),
                                age_ms: Some(0),
                                rcs_state: Some(RemoteControlState::Unknown),
                                target_state: Some(TargetState::Fastboot),
                                ..Default::default()
                            }]
                        },
                        |_query, target_handle| {
                            spawn_target_handler(target_handle, |req| match req {
                                TargetRequest::OpenRemoteControl { responder, remote_control } => {
                                    serve_responsive_rcs(remote_control);
                                    responder.send(Ok(())).unwrap();
                                }
                                r => panic!("unexpected request: {:?}", r),
                            });
                            Ok(())
                        },
                    );
                    responder.send(Ok(())).unwrap();
                }
                req => {
                    assert!(false, "got unexpected request: {:?}", req);
                }
            }
        })
    }

    fn setup_responsive_daemon_server_with_targets(
        has_nodename: bool,
        ssh_error: Option<&'static str>,
        waiter: Shared<Receiver<()>>,
    ) -> DaemonProxy {
        spawn_local_stream_handler(move |req| {
            let waiter = waiter.clone();
            async move {
                let nodename = if has_nodename { Some(NODENAME.to_string()) } else { None };
                match req {
                    DaemonRequest::GetVersionInfo { responder } => {
                        responder.send(&daemon_version_info()).unwrap();
                    }
                    DaemonRequest::ConnectToProtocol { name: _, server_end, responder } => {
                        let nodename = nodename.clone();
                        let waiter = waiter.clone();
                        spawn_target_collection(
                            server_end,
                            move |query| {
                                let query = query.string_matcher.as_deref().unwrap_or("");
                                if !query.is_empty()
                                    && query != NODENAME
                                    && query != UNRESPONSIVE_NODENAME
                                    && query != SSH_ERR_NODENAME
                                {
                                    vec![]
                                } else if query == NODENAME {
                                    vec![TargetInfo {
                                        nodename: nodename.clone(),
                                        addresses: Some(vec![]),
                                        age_ms: Some(0),
                                        rcs_state: Some(RemoteControlState::Unknown),
                                        target_state: Some(TargetState::Unknown),
                                        ..Default::default()
                                    }]
                                } else if ssh_error.is_some() {
                                    vec![TargetInfo {
                                        nodename: Some(SSH_ERR_NODENAME.to_string()),
                                        addresses: Some(vec![]),
                                        age_ms: Some(0),
                                        rcs_state: Some(RemoteControlState::Unknown),
                                        target_state: Some(TargetState::Unknown),
                                        ..Default::default()
                                    }]
                                } else {
                                    vec![
                                        TargetInfo {
                                            nodename: nodename.clone(),
                                            addresses: Some(vec![]),
                                            age_ms: Some(0),
                                            rcs_state: Some(RemoteControlState::Unknown),
                                            target_state: Some(TargetState::Unknown),
                                            ..Default::default()
                                        },
                                        TargetInfo {
                                            nodename: Some(UNRESPONSIVE_NODENAME.to_string()),
                                            addresses: Some(vec![]),
                                            age_ms: Some(0),
                                            rcs_state: Some(RemoteControlState::Unknown),
                                            target_state: Some(TargetState::Unknown),
                                            ..Default::default()
                                        },
                                    ]
                                }
                            },
                            move |query, target_handle| {
                                let waiter = waiter.clone();
                                spawn_target_handler(target_handle, move |req| match req {
                                    TargetRequest::OpenRemoteControl {
                                        responder,
                                        remote_control,
                                    } => {
                                        let target =
                                            query.string_matcher.as_deref().unwrap_or(NODENAME);
                                        if target == UNRESPONSIVE_NODENAME || !has_nodename {
                                            serve_unresponsive_rcs(remote_control, waiter.clone());
                                        } else if target == NODENAME || target == SSH_ERR_NODENAME {
                                            serve_responsive_rcs(remote_control);
                                        } else {
                                            panic!("got unexpected target string: '{}'", target);
                                        }
                                        if target == SSH_ERR_NODENAME {
                                            responder
                                                .send(Err(TargetConnectionError::UnknownError))
                                                .unwrap();
                                        } else {
                                            responder.send(Ok(())).unwrap();
                                        }
                                    }
                                    TargetRequest::GetSshLogs { responder } => {
                                        // This shouldn't even be requested if there is no ssh error
                                        responder.send(ssh_error.unwrap()).unwrap();
                                    }
                                    r => panic!("unexpected request: {:?}", r),
                                });
                                Ok(())
                            },
                        );
                        responder.send(Ok(())).unwrap();
                    }
                    _ => {
                        assert!(false, "got unexpected request: {:?}", req);
                    }
                }
            }
        })
    }

    fn setup_daemon_server_list_fails() -> DaemonProxy {
        spawn_local_stream_handler(move |req| async move {
            match req {
                DaemonRequest::GetVersionInfo { responder } => {
                    responder.send(&daemon_version_info()).unwrap();
                }
                DaemonRequest::ConnectToProtocol { name: _, server_end: _, responder } => {
                    // Do nothing with the server_end.
                    responder.send(Ok(())).unwrap();
                }
                _ => {
                    assert!(false, "got unexpected request: {:?}", req);
                }
            }
        })
    }

    fn setup_daemon_server_echo_hangs(waiter: Shared<Receiver<()>>) -> DaemonProxy {
        spawn_local_stream_handler(move |req| {
            let waiter = waiter.clone();
            async move {
                match req {
                    DaemonRequest::GetVersionInfo { responder: _ } => {
                        waiter.await.unwrap();
                    }
                    _ => {
                        assert!(false, "got unexpected request: {:?}", req);
                    }
                }
            }
        })
    }

    fn ffx_path() -> String {
        format!("{}", std::env::current_exe().unwrap().display())
    }

    fn frontend_version_info(use_default_api_level: bool) -> VersionInfo {
        VersionInfo {
            commit_hash: None,
            commit_timestamp: None,
            build_version: Some(FRONTEND_VERSION.to_string()),
            abi_revision: Some(FAKE_ABI_REVISION),
            api_level: if use_default_api_level {
                Some(FAKE_API_LEVEL)
            } else {
                Some(ANOTHER_FAKE_API_LEVEL)
            },
            ..Default::default()
        }
    }

    fn daemon_version_info() -> fidl_fuchsia_developer_ffx::VersionInfo {
        fidl_fuchsia_developer_ffx::VersionInfo {
            commit_hash: None,
            commit_timestamp: None,
            build_version: Some(DAEMON_VERSION.to_string()),
            abi_revision: Some(FAKE_ABI_REVISION),
            api_level: Some(FAKE_API_LEVEL),
            exec_path: Some(ffx_path()),
            ..Default::default()
        }
    }

    fn record_params_no_record() -> DoctorRecorderParameters {
        DoctorRecorderParameters {
            record: false,
            user_config_enabled: false,
            log_root: None,
            output_dir: None,
            recorder: Arc::new(Mutex::new(DisabledRecorder::new())),
        }
    }

    fn record_params_with_temp(
        root: PathBuf,
    ) -> (Arc<Mutex<FakeRecorder>>, DoctorRecorderParameters) {
        let mut fe_log = root.clone();
        fe_log.push("ffx.log");
        let mut daemon_log = root.clone();
        daemon_log.push("ffx.daemon.log");
        fs::write(&fe_log, "ffx.log contents").expect("writing test ffx.log");
        fs::write(&daemon_log, "ffx.daemon.log contents").expect("writing test ffx.daemon.log");
        let recorder =
            Arc::new(Mutex::new(FakeRecorder::new(vec![fe_log, daemon_log], root.clone())));
        (
            recorder.clone(),
            DoctorRecorderParameters {
                record: true,
                user_config_enabled: false,
                log_root: Some(root.clone()),
                output_dir: Some(root.clone()),
                recorder: recorder.clone(),
            },
        )
    }

    async fn setup_ssh_keys(test_env: &TestEnv) -> Result<()> {
        let pub_key = test_env.isolate_root.path().join("test_authorized_keys");
        let priv_key = test_env.isolate_root.path().join("test_ed25519_key");
        // Set the paths to use for the SSH keys
        test_env
            .context
            .query("ssh.pub")
            .level(Some(ConfigLevel::User))
            .set(json!([&pub_key]))
            .await?;
        test_env
            .context
            .query("ssh.priv")
            .level(Some(ConfigLevel::User))
            .set(json!([&priv_key]))
            .await?;
        let keys = SshKeyFiles::load(Some(&test_env.context)).await?;
        keys.create_keys_if_needed(false)?;
        Ok(())
    }

    #[fuchsia::test]
    async fn test_single_try_no_daemon_running_no_targets_with_default_target() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![false],
            vec![Ok(false)],
            vec![Ok(())],
            vec![Ok(setup_responsive_daemon_server())],
            vec![],
        );

        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(Some(NODENAME.to_string())),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✗] Checking daemon\
                   \n    [✗] No running daemons found. Run `ffx doctor --restart-daemon`\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_no_targets() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✓] Checking daemon\
                   \n    [✓] Daemon found: [1]\
                   \n    [✓] Connecting to daemon\
                   \n    [✓] Daemon version: {DAEMON_VERSION}\
                   \n    [✓] path: {ffx_path}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [✓] Default target: (none)\
                   \n[✗] Google Network Checks\
                   \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                   \n[✗] Searching for targets\
                   \n    [✗] No targets found!\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_connection_error() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Err(anyhow!("Some error message"))],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new_with_error_reason());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(Some("".to_string())),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✗] Checking daemon\
                   \n    [✓] Daemon found: [1]\
                   \n    [✗] Error connecting to daemon: Some error message. Run `ffx doctor --restart-daemon`\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_no_targets_default_target_empty() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(Some("".to_string())),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✓] Checking daemon\
                   \n    [✓] Daemon found: [1]\
                   \n    [✓] Connecting to daemon\
                   \n    [✓] Daemon version: {DAEMON_VERSION}\
                   \n    [✓] path: {ffx_path}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [✓] Default target: (none)\
                   \n[✗] Google Network Checks\
                   \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                   \n[✗] Searching for targets\
                   \n    [✗] No targets found!\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_two_tries_daemon_running_list_fails() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![true, false],
            vec![Ok(true), Ok(false)],
            vec![Ok(())],
            vec![Ok(setup_daemon_server_list_fails()), Ok(setup_daemon_server_list_fails())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            2,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✗] Searching for targets\
            \n    [✗] No targets found!\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_two_tries_no_daemon_running_echo_timeout() {
        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![false, true],
            vec![Ok(false), Ok(true)],
            vec![Ok(()), Ok(())],
            vec![
                Ok(setup_daemon_server_echo_hangs(rx.shared())),
                Ok(setup_responsive_daemon_server()),
            ],
            vec![Ok(vec![]), Ok(vec![]), Ok(vec![1]), Ok(vec![2]), Ok(vec![]), Ok(vec![3])],
        );

        // restart daemon
        {
            let ledger_view = Box::new(FakeLedgerView::new());
            let mut ledger = DoctorLedger::<MockWriter>::new(
                MockWriter::new(),
                ledger_view,
                LedgerViewMode::Verbose,
            );

            doctor_daemon_restart(&fake, DEFAULT_RETRY_DELAY, &mut ledger).await.unwrap();

            assert_eq!(
                ledger.writer.get_data(),
                "\
                    \n[✓] Killing Daemon\
                    \n    [✓] No running daemons found.\
                    \n[✗] Starting Daemon\
                    \n    [✓] Daemon spawned\
                    \n    [✓] Daemon PID: [1]\
                    \n    [✓] Connected to daemon\
                    \n    [✗] Timeout while getting daemon version\
                    \n"
            );
        }

        // restart daemon
        {
            let ledger_view = Box::new(FakeLedgerView::new());
            let mut ledger = DoctorLedger::<MockWriter>::new(
                MockWriter::new(),
                ledger_view,
                LedgerViewMode::Verbose,
            );

            doctor_daemon_restart(&fake, DEFAULT_RETRY_DELAY, &mut ledger).await.unwrap();

            assert_eq!(
                ledger.writer.get_data(),
                format!(
                    "\
                    \n[✓] Killing Daemon\
                    \n    [✓] Killing running daemons.\
                    \n    [✓] Killed daemon PID: [2]\
                    \n[✓] Starting Daemon\
                    \n    [✓] Daemon spawned\
                    \n    [✓] Daemon PID: [3]\
                    \n    [✓] Connected to daemon\
                    \n    [✓] Daemon version: {DAEMON_VERSION}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n",
                )
            );
        }

        tx.send(()).unwrap();
    }

    struct RcsTestArgs {
        ledger_mode: LedgerViewMode,
        ssh_error: Option<&'static str>,
        with_reason: bool,
    }

    impl Default for RcsTestArgs {
        fn default() -> Self {
            RcsTestArgs { ledger_mode: LedgerViewMode::Normal, ssh_error: None, with_reason: false }
        }
    }

    impl RcsTestArgs {
        fn verbose(mut self) -> Self {
            self.ledger_mode = LedgerViewMode::Verbose;
            self
        }

        fn with_ssh_error(mut self, e: &'static str) -> Self {
            self.ssh_error = Some(e);
            self
        }

        fn with_reason(mut self) -> Self {
            self.with_reason = true;
            self
        }
    }

    async fn test_finds_target_connects_to_rcs_setup(
        test_env: &TestEnv,
        modes: RcsTestArgs,
    ) -> DoctorLedger<MockWriter> {
        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_targets(
                true,
                modes.ssh_error,
                rx.shared(),
            ))],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(if modes.with_reason {
            FakeLedgerView::new_with_error_reason()
        } else {
            FakeLedgerView::new()
        });
        let mut ledger =
            DoctorLedger::<MockWriter>::new(MockWriter::new(), ledger_view, modes.ledger_mode);

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();
        tx.send(()).unwrap();

        return ledger;
    }

    #[fuchsia::test]
    async fn test_finds_target_connects_to_rcs_with_ssh_error_verbose() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let ledger = test_finds_target_connects_to_rcs_setup(
            &test_env,
            RcsTestArgs::default().verbose().with_ssh_error("some ssh error"),
        )
        .await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 1 targets found\
            \n[✗] Verifying Targets\
            \n    [✗] Target: {SSH_ERR_NODENAME}\
            \n        [!] Compatibility state: absent\
            \n            [!] Compatibility information is not available\
            \n        [✓] Opened target handle\
            \n        [✓] Connecting to RCS\
            \n        [✗] Error while connecting to RCS: <reason omitted>\
            \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_ssh_connection_refused_recommends_tunnel() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");

        let ledger = test_finds_target_connects_to_rcs_setup(
            &test_env,
            RcsTestArgs::default().with_ssh_error("Connection refused").with_reason(),
        )
        .await;
        let output = ledger.writer.get_data();
        assert!(output.contains(
            "[i] SSH connection was refused. You may need to (re-)establish a tunnel connection.\n"
        ));
    }

    #[fuchsia::test]
    async fn test_ssh_permission_denied_recommends_repave() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");

        let ledger = test_finds_target_connects_to_rcs_setup(
            &test_env,
            RcsTestArgs::default()
                .with_ssh_error("Permission denied (publickey,keyboard-interactive)")
                .with_reason(),
        )
        .await;
        let output = ledger.writer.get_data();
        assert!(output.contains(
            "[i] SSH connection could not authenticate. You may need to re-provision (pave or flash) your target to ensure SSH keys are appropriately setup.\n"
        ));
    }

    #[fuchsia::test]
    async fn test_finds_target_connects_to_rcs_verbose() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let ledger =
            test_finds_target_connects_to_rcs_setup(&test_env, RcsTestArgs::default().verbose())
                .await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 2 targets found\
            \n[✓] Verifying Targets\
            \n    [✓] Target: {NODENAME}\
            \n        [!] Compatibility state: absent\
            \n            [!] Compatibility information is not available\
            \n        [✓] Opened target handle\
            \n        [✓] Connecting to RCS\
            \n        [✓] Communicating with RCS\
            \n    [✗] Target: {UNRESPONSIVE_NODENAME}\
            \n        [!] Compatibility state: absent\
            \n            [!] Compatibility information is not available\
            \n        [✓] Opened target handle\
            \n        [✓] Connecting to RCS\
            \n        [✗] Timeout while communicating with RCS\
            \n[✓] No issues found\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_finds_target_connects_to_rcs_normal() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let ledger =
            test_finds_target_connects_to_rcs_setup(&test_env, RcsTestArgs::default()).await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                \n[✓] FFX Environment Context\
                \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                \n    [✓] Config Lock Files\
                \n        [✓] {user_file} locked by {user_file}.lock\
                \n        [✓] {global_file} locked by {global_file}.lock\
                \n    [✓] The public & private Fuchsia keys are consistent\
                \n[✓] Checking daemon\
                \n    [✓] Daemon found: [1]\
                \n    [✓] Connecting to daemon\
                \n[✗] Google Network Checks\
                \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                \n[✓] Searching for targets\
                \n    [✓] 2 targets found\
                \n[✓] Verifying Targets\
                \n    [✓] Target: {NODENAME}\
                \n    [✗] Target: {UNRESPONSIVE_NODENAME}\
                \n[✓] No issues found\n",
                isolated_root=test_env.isolate_root.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_finds_target_with_filter() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_targets(true, None, rx.shared()))],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            &NODENAME,
            2,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();
        tx.send(()).unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 1 targets found\
            \n[✓] Verifying Targets\
            \n    [✓] Target: {NODENAME}\
            \n        [!] Compatibility state: absent\
            \n            [!] Compatibility information is not available\
            \n        [✓] Opened target handle\
            \n        [✓] Connecting to RCS\
            \n        [✓] Communicating with RCS\
            \n[✓] No issues found\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_invalid_filter_finds_no_targets() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_targets(true, None, rx.shared()))],
            vec![Ok(vec![1])],
        );

        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            &NON_EXISTENT_NODENAME,
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();
        tx.send(()).unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✗] Searching for targets\
            \n    [✗] No targets found!\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_restart_daemon() {
        let fake = FakeDaemonManager::new(
            vec![false],
            vec![Ok(true), Ok(false)],
            vec![Ok(())],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1, 2, 3]), Ok(vec![]), Ok(vec![4])],
        );
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor_daemon_restart(&fake, DEFAULT_RETRY_DELAY, &mut ledger).await.unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] Killing Daemon\
            \n    [✓] Killing zombie daemons.\
            \n    [✓] Killed daemon PID: [1, 2, 3]\
            \n[✓] Starting Daemon\
            \n    [✓] Daemon spawned\
            \n    [✓] Daemon PID: [4]\
            \n    [✓] Connected to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n"
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_restart_daemon_pid_error() {
        let fake = FakeDaemonManager::new(
            vec![false],
            vec![Ok(true), Ok(false)],
            vec![Ok(())],
            vec![Ok(setup_responsive_daemon_server())],
            vec![
                Err(anyhow!("some error msg")),
                Err(anyhow!("some error msg")),
                Err(anyhow!("some error msg")),
            ],
        );
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor_daemon_restart(&fake, DEFAULT_RETRY_DELAY, &mut ledger).await.unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] Killing Daemon\
            \n    [!] Error getting daemon pid: <reason omitted>\
            \n    [✓] Killing zombie daemons.\
            \n[✓] Starting Daemon\
            \n    [✓] Daemon spawned\
            \n    [✓] Connected to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n"
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_no_targets_record_enabled() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (fake_recorder, params) = record_params_with_temp(root);

        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            params,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        let r = fake_recorder.lock().await;
        handler
            .assert_matches_steps(vec![
                TestStepEntry::output_step(StepType::DoctorSummaryInitVerbose()),
                TestStepEntry::output_step(StepType::Output(format!(
                    "\
                    [✓] FFX doctor\
                    \n    [✓] Frontend version: {FRONTEND_VERSION}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n    [i] Path to ffx: {ffx_path}\n\
                    \n[✓] FFX Environment Context\
                    \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                    \n    [✓] Environment File Location: {env_file}\
                    \n    [✓] No build directory discovered in the environment.\
                    \n    [✓] Config Lock Files\
                    \n        [✓] {user_file} locked by {user_file}.lock\
                    \n        [✓] {global_file} locked by {global_file}.lock\
                    \n    [✓] The public & private Fuchsia keys are consistent\n\
                    \n[✓] Checking daemon\
                    \n    [✓] Daemon found: [1]\
                    \n    [✓] Connecting to daemon\
                    \n    [✓] Daemon version: {DAEMON_VERSION}\
                    \n    [✓] path: {ffx_path}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n    [✓] Default target: (none)\n\
                    \n[✗] Google Network Checks\
                    \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                    \n\n[✗] Searching for targets\
                    \n    [✗] No targets found!\n\n",
                    ffx_path=ffx_path(),
                    isolated_root=test_env.isolate_root.path().display(),
                    env_file=test_env.env_file.path().display(),
                    user_file=test_env.user_file.path().display(),
                    global_file=test_env.global_file.path().display(),
                ))),
                TestStepEntry::step(StepType::GeneratingRecord),
                TestStepEntry::result(StepResult::Success),
                TestStepEntry::output_step(StepType::RecordGenerated(FakeRecorder::result_path())),
            ])
            .await;
        r.assert_generate_called();
    }

    async fn missing_field_test(
        fake_recorder: Arc<Mutex<FakeRecorder>>,
        params: DoctorRecorderParameters,
    ) {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();

        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        assert!(doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            params,
            FakeGChecker,
            None,
            false,
        )
        .await
        .is_err());

        let _ = fake_recorder.lock().await;
        handler
            .assert_matches_steps(vec![
                TestStepEntry::output_step(StepType::DoctorSummaryInitVerbose()),
                TestStepEntry::output_step(StepType::Output(format!(
                    "\
                    [✓] FFX doctor\
                    \n    [✓] Frontend version: {FRONTEND_VERSION}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n    [i] Path to ffx: {ffx_path}\n\
                    \n[✓] FFX Environment Context\
                    \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                    \n    [✓] Environment File Location: {env_file}\
                    \n    [✓] No build directory discovered in the environment.\
                    \n    [✓] Config Lock Files\
                    \n        [✓] {user_file} locked by {user_file}.lock\
                    \n        [✓] {global_file} locked by {global_file}.lock\
                    \n    [✓] The public & private Fuchsia keys are consistent\n\
                    \n[✓] Checking daemon\
                    \n    [✓] Daemon found: [1]\
                    \n    [✓] Connecting to daemon\
                    \n    [✓] Daemon version: {DAEMON_VERSION}\
                    \n    [✓] path: {ffx_path}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n    [✓] Default target: (none)\n\
                    \n[✗] Google Network Checks\
                    \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                    \n\n[✗] Searching for targets\
                    \n    [✗] No targets found!\n\n",
                    ffx_path=ffx_path(),
                    isolated_root=test_env.isolate_root.path().display(),
                    env_file=test_env.env_file.path().display(),
                    user_file=test_env.user_file.path().display(),
                    global_file=test_env.global_file.path().display(),
                ))),
                // Error will occur here.
            ])
            .await;
        fake.assert_no_leftover_calls().await;
    }

    #[fuchsia::test]
    async fn test_record_mode_missing_log_root_fails() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (fake_recorder, mut params) = record_params_with_temp(root);
        params.log_root = None;
        missing_field_test(fake_recorder, params).await;
    }

    #[fuchsia::test]
    async fn test_record_mode_missing_output_dir_fails() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (fake_recorder, mut params) = record_params_with_temp(root);
        params.output_dir = None;
        missing_field_test(fake_recorder, params).await;
    }

    async fn test_finds_target_with_missing_nodename_setup(
        test_env: &TestEnv,
        mode: LedgerViewMode,
    ) -> DoctorLedger<MockWriter> {
        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_targets(false, None, rx.shared()))],
            vec![Ok(vec![1])],
        );

        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(MockWriter::new(), ledger_view, mode);

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();
        tx.send(()).unwrap();

        return ledger;
    }

    #[fuchsia::test]
    async fn test_finds_target_with_missing_nodename_verbose() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let ledger =
            test_finds_target_with_missing_nodename_setup(&test_env, LedgerViewMode::Verbose).await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                \n[✓] FFX doctor\
                \n    [✓] Frontend version: {FRONTEND_VERSION}\
                \n    [✓] abi-revision: {ABI_REVISION_STR}\
                \n    [✓] api-level: {FAKE_API_LEVEL}\
                \n    [i] Path to ffx: {ffx_path}\
                \n[✓] FFX Environment Context\
                \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                \n    [✓] Environment File Location: {env_file}\
                \n    [✓] No build directory discovered in the environment.\
                \n    [✓] Config Lock Files\
                \n        [✓] {user_file} locked by {user_file}.lock\
                \n        [✓] {global_file} locked by {global_file}.lock\
                \n    [✓] The public & private Fuchsia keys are consistent\
                \n[✓] Checking daemon\
                \n    [✓] Daemon found: [1]\
                \n    [✓] Connecting to daemon\
                \n    [✓] Daemon version: {DAEMON_VERSION}\
                \n    [✓] path: {ffx_path}\
                \n    [✓] abi-revision: {ABI_REVISION_STR}\
                \n    [✓] api-level: {FAKE_API_LEVEL}\
                \n    [✓] Default target: (none)\
                \n[✗] Google Network Checks\
                \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                \n[✓] Searching for targets\
                \n    [✓] 2 targets found\
                \n[✗] Verifying Targets\
                \n    [✗] Target: UNKNOWN\
                \n        [!] Compatibility state: absent\
                \n            [!] Compatibility information is not available\
                \n        [✓] Opened target handle\
                \n        [✓] Connecting to RCS\
                \n        [✗] Timeout while communicating with RCS\
                \n    [✗] Target: {UNRESPONSIVE_NODENAME}\
                \n        [!] Compatibility state: absent\
                \n            [!] Compatibility information is not available\
                \n        [✓] Opened target handle\
                \n        [✓] Connecting to RCS\
                \n        [✗] Timeout while communicating with RCS\
                \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_finds_target_with_missing_nodename_normal() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let ledger =
            test_finds_target_with_missing_nodename_setup(&test_env, LedgerViewMode::Normal).await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                \n[✓] FFX Environment Context\
                \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                \n    [✓] Config Lock Files\
                \n        [✓] {user_file} locked by {user_file}.lock\
                \n        [✓] {global_file} locked by {global_file}.lock\
                \n    [✓] The public & private Fuchsia keys are consistent\
                \n[✓] Checking daemon\
                \n    [✓] Daemon found: [1]\
                \n    [✓] Connecting to daemon\
                \n[✗] Google Network Checks\
                \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                \n[✓] Searching for targets\
                \n    [✓] 2 targets found\
                \n[✗] Verifying Targets\
                \n    [✗] Target: UNKNOWN\
                \n    [✗] Target: {UNRESPONSIVE_NODENAME}\
                \n[✗] Doctor found issues in one or more categories; \
                run 'ffx doctor -v' for more details.\n",
                isolated_root=test_env.isolate_root.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_fastboot_target() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_fastboot_target())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 1 targets found\
            \n[✓] Verifying Targets\
            \n    [✓] Target found in fastboot mode: {SERIAL_NUMBER}\
            \n[✓] No issues found\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_different_api_level() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        setup_ssh_keys(&test_env).await.expect("setting up ssh test keys");

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(false),
            Ok(Some("".to_string())),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {ANOTHER_FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✓] Checking daemon\
                   \n    [✓] Daemon found: [1]\
                   \n    [✓] Connecting to daemon\
                   \n    [✓] Daemon version: {DAEMON_VERSION}\
                   \n    [✓] path: {ffx_path}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [!] Daemon and frontend are at different API levels. Run `ffx doctor --restart-daemon`\
                   \n    [✓] Default target: (none)\
                   \n[✗] Google Network Checks\
                   \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                   \n[✗] Searching for targets\
                   \n    [✗] No targets found!\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_missing_ssh_keys() {
        let test_env = ffx_config::test_init().await.expect("Setting up test environment");
        let pub_key = test_env.isolate_root.path().join("test_authorized_keys");
        let priv_key = test_env.isolate_root.path().join("test_ed25519_key");
        // Set the paths to use for the SSH keys
        test_env
            .context
            .query("ssh.pub")
            .level(Some(ConfigLevel::User))
            .set(json!([&pub_key]))
            .await
            .unwrap();
        test_env
            .context
            .query("ssh.priv")
            .level(Some(ConfigLevel::User))
            .set(json!([&priv_key]))
            .await
            .unwrap();

        // Do not generate the keys - so they are missing.

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_fastboot_target())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✗] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✗] Private key {priv_key} does not exist. Check configuration or run `ffx doctor --repair-keys`\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 1 targets found\
            \n[✓] Verifying Targets\
            \n    [✓] Target found in fastboot mode: {SERIAL_NUMBER}\
            \n[✓] No issues found\n",
                ffx_path=ffx_path(),
                isolated_root=test_env.isolate_root.path().display(),
                env_file=test_env.env_file.path().display(),
                user_file=test_env.user_file.path().display(),
                global_file=test_env.global_file.path().display(),
                priv_key = priv_key.display()
            )
        );
    }

    #[test]
    fn test_collect_log_files() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let mut expected = vec![root.join("f1.log"), root.join("f2.log")];
        for p in &expected {
            fs::write(p, "something").expect("written testdata");
        }
        // write out other files
        fs::write(root.join("no_extension"), "something").expect("written testdata");
        fs::write(root.join("notlog.txt"), "something").expect("written testdata");
        fs::write(root.join("save.log.save"), "something").expect("written testdata");

        let subdir = root.join("subdir");
        fs::create_dir_all(&subdir).expect("subdir created");
        fs::write(subdir.join("sublog.log"), "something").expect("written testdata");

        let mut actual = collect_log_files(root.clone()).expect("collecting");
        // Sort the lists to make comparison easy.
        expected.sort();
        actual.sort();
        assert_eq!(expected, actual);
    }

    #[fuchsia::test]
    async fn test_doctor_summary_with_gdoctor_subtool() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let subtool_search_dir_path = temp_dir.path();
        let mock_gdoctor_path = temp_dir.path().join("ffx-gdoctor");
        // Scope to ensure File handle is dropped (and file closed) before setting permissions
        {
            let mut mock_gdoctor_script =
                fs::File::create(&mock_gdoctor_path).expect("Failed to create mock script");
            // example data in DoctorCheck format
            write!(
            mock_gdoctor_script,
            "#!/bin/sh\n\
             echo '{{\"name\": \"Corp DHCP\", \"message\": \"Successfully connected\", \"passed\": true}}'\n\
             echo '{{\"name\": \"GPN\", \"message\": \"GPN not detected\", \"passed\": false}}'\n"
        )
        .expect("Failed to write to mock script");
        }

        let mut perms = fs::metadata(&mock_gdoctor_path)
            .expect("Failed to get metadata for mock script")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock_gdoctor_path, perms)
            .expect("Failed to set permissions on mock script");
        let metadata_path = temp_dir.path().join("ffx-gdoctor.json");
        let metadata_content = serde_json::json!({
            "name": "gdoctor",
            "description": "Mock gdoctor for testing",
            "requires_fho": 0,
            "fho_details": {
                "version": 0
            }
        });
        fs::write(&metadata_path, metadata_content.to_string()).expect("Failed to write metadata");

        // Configure ffx to use our temporary search path
        let testenv = ffx_config::test_env()
            .env_var(EnvironmentContext::FFX_BIN_ENV, "host-tools/ffx")
            .runtime_config("ffx.subtool-search-paths", json!([subtool_search_dir_path]))
            .build()
            .await
            .expect("Setting up test environment");

        let fake_daemon = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![123])],
        );
        let mut handler = FakeStepHandler::new();
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            Box::new(FakeLedgerView::new()),
            LedgerViewMode::Verbose,
        );

        doctor(
            &mut handler,
            &mut ledger,
            &fake_daemon,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &testenv.context,
            record_params_no_record(),
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        let output = ledger.writer.get_data();
        assert!(
            output.contains("[✗] Google Network Checks"),
            "Main 'Google Network Checks' node missing or has wrong outcome. Output:\n{}",
            output
        );
        assert!(
            output.contains("[✓] Corp DHCP: Successfully connected"),
            "'Corp DHCP' check missing or has wrong outcome. Output:\n{}",
            output
        );
        assert!(
            output.contains("[✗] GPN: GPN not detected"),
            "'GPN' check missing or has wrong outcome. Output:\n{}",
            output
        );
    }
}
