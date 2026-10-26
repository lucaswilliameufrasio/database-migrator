//! Versioned protocol for delegating a complete compiled migration plan.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::async_runtime::run_blocking;
use crate::config::LoadedConfig;
use crate::engine::{self, RunnerAction};
use crate::error::{DbmError, Result};
use crate::registry::MigrationRegistry;

pub const RUNNER_PROTOCOL_VERSION: u32 = 2;
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
const HANDSHAKE_TIMEOUT_SECONDS: u64 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunnerOperation {
    Validate,
    Plan,
    Status,
    History,
    Doctor,
    Up,
    Down { steps: usize },
}

#[derive(Debug)]
pub(crate) enum RunnerOutput {
    Plan(Value),
    ReadOnly(Value),
    Execution(Value),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WireAction {
    Validate,
    Plan,
    Status,
    History,
    Doctor,
    Up,
    Down,
}

impl RunnerOperation {
    fn action(self) -> WireAction {
        match self {
            Self::Validate => WireAction::Validate,
            Self::Plan => WireAction::Plan,
            Self::Status => WireAction::Status,
            Self::History => WireAction::History,
            Self::Doctor => WireAction::Doctor,
            Self::Up => WireAction::Up,
            Self::Down { .. } => WireAction::Down,
        }
    }

    fn steps(self) -> Option<usize> {
        match self {
            Self::Down { steps } => Some(steps),
            Self::Validate
            | Self::Plan
            | Self::Status
            | Self::History
            | Self::Doctor
            | Self::Up => None,
        }
    }

    fn engine_action(self) -> RunnerAction {
        match self {
            Self::Validate => RunnerAction::Validate,
            Self::Plan => RunnerAction::Plan,
            Self::Status => RunnerAction::Status,
            Self::History => RunnerAction::History,
            Self::Doctor => RunnerAction::Doctor,
            Self::Up => RunnerAction::Up,
            Self::Down { steps } => RunnerAction::Down { steps },
        }
    }

    fn is_read_only(self) -> bool {
        matches!(
            self,
            Self::Validate | Self::Plan | Self::Status | Self::History | Self::Doctor
        )
    }

    fn report_command(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::Plan => "plan",
            Self::Status => "status",
            Self::History => "history",
            Self::Doctor => "doctor",
            Self::Up => "up",
            Self::Down { .. } => "down",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HelloRequest {
    kind: String,
    protocol_version: u32,
    engine_version: String,
    metadata_schema_version: u32,
    runner_fingerprint: String,
    config_path: String,
    namespace: String,
    action: WireAction,
    down_steps: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExecuteRequest {
    kind: String,
    protocol_version: u32,
    runner_fingerprint: String,
    namespace: String,
    action: WireAction,
    down_steps: Option<usize>,
    manifest_fingerprint: String,
    plan_hash: String,
    target_fingerprint: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadyResponse {
    kind: String,
    protocol_version: u32,
    engine_version: String,
    metadata_schema_version: u32,
    runner_fingerprint: String,
    namespace: String,
    capabilities: Vec<String>,
    manifest_fingerprint: String,
    plan_hash: String,
    target_fingerprint: String,
    report: Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CompleteResponse {
    kind: String,
    protocol_version: u32,
    runner_fingerprint: String,
    namespace: String,
    manifest_fingerprint: String,
    plan_hash: String,
    target_fingerprint: String,
    report: Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ErrorResponse {
    kind: String,
    protocol_version: u32,
    stage: String,
    code: i32,
    message: String,
}

/// Serves one CLI request for a compiled registry. A runner executable should
/// call this explicitly from its own `main`; importing the library is inert.
pub async fn serve(registry: MigrationRegistry) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    serve_io(&registry, &mut input, &mut output).await
}

async fn serve_io<R, W>(registry: &MigrationRegistry, input: &mut R, output: &mut W) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    let hello_frame = match read_frame(input) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Err(protocol_error("runner received no handshake frame")),
        Err(error) => {
            return Err(protocol_error(&format!(
                "could not read handshake: {error}"
            )));
        }
    };
    let hello = match parse_hello(&hello_frame) {
        Ok(request) => request,
        Err(error) => {
            write_error(output, "handshake", &error)?;
            return Ok(());
        }
    };
    let runner_fingerprint = match current_runner_fingerprint() {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            write_error(output, "handshake", &error)?;
            return Ok(());
        }
    };
    if let Err(error) = validate_hello(&hello, &runner_fingerprint) {
        write_error(output, "handshake", &error)?;
        return Ok(());
    }

    let config_path = PathBuf::from(&hello.config_path);
    let config = match crate::config::load_with_namespace(&config_path, Some(&hello.namespace)) {
        Ok(config) => config,
        Err(error) => {
            write_error(output, "handshake", &error)?;
            return Ok(());
        }
    };
    let operation = operation_from_wire(hello.action, hello.down_steps)?;
    let preflight =
        match engine::runner_preflight(&config, registry, operation.engine_action()).await {
            Ok(preflight) => preflight,
            Err(error) => {
                write_error(output, "preflight", &error)?;
                return Ok(());
            }
        };
    let ready = ReadyResponse {
        kind: "ready".to_owned(),
        protocol_version: RUNNER_PROTOCOL_VERSION,
        engine_version: ENGINE_VERSION.to_owned(),
        metadata_schema_version: crate::storage::METADATA_FORMAT_VERSION as u32,
        runner_fingerprint: runner_fingerprint.clone(),
        namespace: preflight.report.namespace.clone(),
        capabilities: runner_capabilities(),
        manifest_fingerprint: preflight.manifest_fingerprint.clone(),
        plan_hash: preflight.plan_hash.clone(),
        target_fingerprint: preflight.target_fingerprint.clone(),
        report: serde_json::to_value(&preflight.report)?,
    };
    write_frame(output, &ready)?;

    if matches!(operation, RunnerOperation::Plan) {
        return Ok(());
    }
    if operation.is_read_only() {
        let report: Result<Value> = match operation {
            RunnerOperation::Validate => engine::validate_with_registry_async(&config, registry)
                .await
                .and_then(|report| serde_json::to_value(report).map_err(DbmError::from)),
            RunnerOperation::Status => engine::status_with_registry_async(&config, registry)
                .await
                .and_then(|report| serde_json::to_value(report).map_err(DbmError::from)),
            RunnerOperation::History => engine::history_async(&config)
                .await
                .and_then(|report| serde_json::to_value(report).map_err(DbmError::from)),
            RunnerOperation::Doctor => engine::doctor_with_registry_async(&config, registry)
                .await
                .and_then(|report| serde_json::to_value(report).map_err(DbmError::from)),
            RunnerOperation::Plan | RunnerOperation::Up | RunnerOperation::Down { .. } => {
                return Err(protocol_error("runner read-only operation is inconsistent"));
            }
        };
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                write_error(output, "read_only", &error)?;
                return Ok(());
            }
        };
        let latest =
            match engine::runner_preflight(&config, registry, operation.engine_action()).await {
                Ok(preflight) => preflight,
                Err(error) => {
                    write_error(output, "read_only", &error)?;
                    return Ok(());
                }
            };
        if !same_preflight(&preflight, &latest) {
            let error = DbmError::Safety(
                "sources, target, or database state changed while preparing the read-only runner response"
                    .to_owned(),
            );
            write_error(output, "read_only", &error)?;
            return Ok(());
        }
        write_frame(
            output,
            &complete_response(report, &preflight, &runner_fingerprint),
        )?;
        return Ok(());
    }

    let execute_frame = match read_frame(input) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Ok(()),
        Err(error) => {
            write_error(
                output,
                "execute_validation",
                &protocol_error(&format!("could not read execution frame: {error}")),
            )?;
            return Ok(());
        }
    };
    let execute = match parse_execute(&execute_frame) {
        Ok(request) => request,
        Err(error) => {
            write_error(output, "execute_validation", &error)?;
            return Ok(());
        }
    };
    if let Err(error) = validate_execute(&execute, operation, &preflight, &runner_fingerprint) {
        write_error(output, "execute_validation", &error)?;
        return Ok(());
    }
    match read_frame(input) {
        Ok(None) => {}
        Ok(Some(_)) => {
            write_error(
                output,
                "execute_validation",
                &protocol_error("runner accepts exactly one execute frame"),
            )?;
            return Ok(());
        }
        Err(error) => {
            write_error(
                output,
                "execute_validation",
                &protocol_error(&format!("could not validate request termination: {error}")),
            )?;
            return Ok(());
        }
    }

    let latest = match engine::runner_preflight(&config, registry, operation.engine_action()).await
    {
        Ok(preflight) => preflight,
        Err(error) => {
            write_error(output, "execute_validation", &error)?;
            return Ok(());
        }
    };
    if !same_preflight(&preflight, &latest) {
        let error = DbmError::Safety(
            "sources, target, or database state changed after runner handshake; rerun plan"
                .to_owned(),
        );
        write_error(output, "execute_validation", &error)?;
        return Ok(());
    }

    let execution = match operation {
        RunnerOperation::Plan => return Ok(()),
        RunnerOperation::Validate
        | RunnerOperation::Status
        | RunnerOperation::History
        | RunnerOperation::Doctor => {
            return Err(protocol_error("runner mutation operation is inconsistent"));
        }
        RunnerOperation::Up => engine::up_with_registry_async(&config, registry).await,
        RunnerOperation::Down { steps } => {
            engine::down_with_registry_async(&config, registry, steps).await
        }
    };
    let execution = match execution {
        Ok(report) => report,
        Err(error) => {
            write_error(output, "execution", &error)?;
            return Ok(());
        }
    };
    let complete = complete_response(
        serde_json::to_value(execution)?,
        &preflight,
        &runner_fingerprint,
    );
    write_frame(output, &complete)?;
    Ok(())
}

/// Runs the configured compiled runner after a read-only handshake succeeds.
pub(crate) async fn invoke(
    config_path: &Path,
    config: &LoadedConfig,
    operation: RunnerOperation,
) -> Result<RunnerOutput> {
    let config_path = config_path.to_path_buf();
    let config = config.clone();
    run_blocking(move || invoke_blocking(&config_path, &config, operation)).await
}

fn invoke_blocking(
    config_path: &Path,
    config: &LoadedConfig,
    operation: RunnerOperation,
) -> Result<RunnerOutput> {
    let (canonical_config, project_root, runner_path) = resolve_runner_paths(config_path, config)?;
    let runner_fingerprint = binary_fingerprint(&runner_path)?;
    let timeout = Duration::from_secs(config.config.execution.runner_timeout_seconds);
    let handshake_timeout = timeout.min(Duration::from_secs(HANDSHAKE_TIMEOUT_SECONDS));
    let mut child = spawn_runner(&runner_path, &project_root, config)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DbmError::Execution("compiled runner stdout was not piped".to_owned()))?;
    let _reader = thread::Builder::new()
        .name("dbm-runner-protocol-reader".to_owned())
        .spawn(move || read_runner_frames(stdout, sender))
        .map_err(|_| DbmError::Execution("could not start runner protocol reader".to_owned()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| DbmError::Execution("compiled runner stdin was not piped".to_owned()))?;

    let hello = HelloRequest {
        kind: "hello".to_owned(),
        protocol_version: RUNNER_PROTOCOL_VERSION,
        engine_version: ENGINE_VERSION.to_owned(),
        metadata_schema_version: crate::storage::METADATA_FORMAT_VERSION as u32,
        runner_fingerprint: runner_fingerprint.clone(),
        config_path: canonical_config.display().to_string(),
        namespace: config.config.namespace.clone(),
        action: operation.action(),
        down_steps: operation.steps(),
    };
    if let Err(error) = write_frame(&mut stdin, &hello) {
        stop_child(&mut child);
        return Err(protocol_error(&format!(
            "could not send runner handshake: {error}"
        )));
    }
    let ready_frame = match receive_frame(&receiver, handshake_timeout) {
        Ok(frame) => frame,
        Err(error) => {
            stop_child(&mut child);
            return Err(error);
        }
    };
    let ready = match parse_ready(&ready_frame) {
        Ok(ready) => ready,
        Err(error) => {
            stop_child(&mut child);
            return Err(error);
        }
    };
    if let Err(error) = validate_ready(&ready, &runner_fingerprint, &config.config.namespace) {
        stop_child(&mut child);
        return Err(error);
    }

    if matches!(operation, RunnerOperation::Plan) {
        drop(stdin);
        let status = wait_child(&mut child, timeout)?;
        ensure_successful_exit(status, false)?;
        ensure_no_extra_frame(&receiver)?;
        return Ok(RunnerOutput::Plan(ready.report));
    }
    if operation.is_read_only() {
        drop(stdin);
        let complete_frame = match receive_frame(&receiver, timeout) {
            Ok(frame) => frame,
            Err(error) => {
                stop_child(&mut child);
                return Err(error);
            }
        };
        let complete = match parse_complete(&complete_frame) {
            Ok(complete) => complete,
            Err(error) => {
                stop_child(&mut child);
                return Err(error);
            }
        };
        if let Err(error) = validate_complete(&complete, &ready, operation) {
            stop_child(&mut child);
            return Err(error);
        }
        let status = match wait_child(&mut child, timeout) {
            Ok(status) => status,
            Err(error) => {
                stop_child(&mut child);
                return Err(error);
            }
        };
        ensure_successful_exit(status, false)?;
        ensure_no_extra_frame(&receiver)?;
        return Ok(RunnerOutput::ReadOnly(complete.report));
    }

    let execute = ExecuteRequest {
        kind: "execute".to_owned(),
        protocol_version: RUNNER_PROTOCOL_VERSION,
        runner_fingerprint: ready.runner_fingerprint.clone(),
        namespace: config.config.namespace.clone(),
        action: operation.action(),
        down_steps: operation.steps(),
        manifest_fingerprint: ready.manifest_fingerprint.clone(),
        plan_hash: ready.plan_hash.clone(),
        target_fingerprint: ready.target_fingerprint.clone(),
    };
    if let Err(error) = write_frame(&mut stdin, &execute) {
        stop_child(&mut child);
        return Err(protocol_error(&format!(
            "could not send runner execution request: {error}"
        )));
    }
    drop(stdin);
    let complete_frame = match receive_frame(&receiver, timeout) {
        Ok(frame) => frame,
        Err(_) => {
            stop_child(&mut child);
            return Err(DbmError::ReconciliationRequired(
                "compiled runner stopped responding after execution was authorized; inspect status and doctor before retrying"
                    .to_owned(),
            ));
        }
    };
    let complete = match parse_complete(&complete_frame) {
        Ok(complete) => complete,
        Err(error) => {
            stop_child(&mut child);
            return Err(DbmError::ReconciliationRequired(format!(
                "compiled runner returned an invalid execution result; inspect status and doctor before retrying: {error}"
            )));
        }
    };
    if let Err(error) = validate_complete(&complete, &ready, operation) {
        stop_child(&mut child);
        return Err(DbmError::ReconciliationRequired(format!(
            "compiled runner result did not match its handshake; inspect status and doctor before retrying: {error}"
        )));
    }
    let status = wait_child(&mut child, timeout).map_err(|_| {
        DbmError::ReconciliationRequired(
            "compiled runner returned a result but did not exit; inspect status and doctor before retrying"
                .to_owned(),
        )
    })?;
    ensure_successful_exit(status, true)?;
    ensure_no_extra_frame(&receiver).map_err(|error| {
        DbmError::ReconciliationRequired(format!(
            "compiled runner wrote unexpected protocol data after execution; inspect status and doctor before retrying: {error}"
        ))
    })?;
    Ok(RunnerOutput::Execution(complete.report))
}

fn resolve_runner_paths(
    config_path: &Path,
    config: &LoadedConfig,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let current_dir = std::env::current_dir()
        .map_err(|error| DbmError::io("current working directory", error))?;
    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        current_dir.join(config_path)
    };
    let canonical_config = fs::canonicalize(&config_path)
        .map_err(|error| DbmError::io(config_path.display(), error))?;
    let project_root = canonical_config
        .parent()
        .ok_or_else(|| DbmError::Configuration("config file has no parent directory".to_owned()))?
        .to_path_buf();
    let runner_relative = config.config.execution.runner.as_deref().ok_or_else(|| {
        DbmError::Configuration(
            "compiled migrations require execution.runner in the project config".to_owned(),
        )
    })?;
    let runner_path = project_root.join(runner_relative);
    let canonical_runner = fs::canonicalize(&runner_path)
        .map_err(|error| DbmError::io(runner_path.display(), error))?;
    if !canonical_runner.starts_with(&project_root) || !canonical_runner.is_file() {
        return Err(DbmError::Safety(
            "configured runner must resolve to a regular file inside the project".to_owned(),
        ));
    }
    Ok((canonical_config, project_root, canonical_runner))
}

fn spawn_runner(runner: &Path, project_root: &Path, config: &LoadedConfig) -> Result<Child> {
    let mut command = Command::new(runner);
    command
        .current_dir(project_root)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut allowed_environment = BTreeSet::new();
    if let Some(name) = config.config.target.url_env.as_ref() {
        allowed_environment.insert(name.clone());
    }
    if let Some(name) = config.config.target.tls_ca_cert_env.as_ref() {
        allowed_environment.insert(name.clone());
    }
    for name in &config.config.execution.runner_env {
        allowed_environment.insert(name.clone());
    }
    for name in allowed_environment {
        if let Some(value) = std::env::var_os(&name) {
            command.env(name, value);
        }
    }

    command.spawn().map_err(|_| {
        DbmError::Configuration(
            "could not start the configured compiled migration runner".to_owned(),
        )
    })
}

fn binary_fingerprint(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|error| DbmError::io(path.display(), error))?;
    Ok(crate::identity::raw_checksum(&bytes))
}

fn current_runner_fingerprint() -> Result<String> {
    let executable = std::env::current_exe()
        .map_err(|error| DbmError::io("compiled migration runner executable", error))?;
    binary_fingerprint(&executable)
}

fn read_runner_frames<R: Read>(mut stdout: R, sender: SyncSender<ReaderMessage>) {
    let mut reader = io::BufReader::new(&mut stdout);
    loop {
        match read_frame(&mut reader) {
            Ok(Some(frame)) => {
                if sender.send(ReaderMessage::Frame(frame)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(ReaderMessage::Eof);
                return;
            }
            Err(_) => {
                let _ = sender.send(ReaderMessage::InvalidFrame);
                return;
            }
        }
    }
}

enum ReaderMessage {
    Frame(Vec<u8>),
    Eof,
    InvalidFrame,
}

fn receive_frame(receiver: &Receiver<ReaderMessage>, timeout: Duration) -> Result<Vec<u8>> {
    match receiver.recv_timeout(timeout) {
        Ok(ReaderMessage::Frame(frame)) => Ok(frame),
        Ok(ReaderMessage::Eof) => Err(protocol_error("compiled runner closed stdout")),
        Ok(ReaderMessage::InvalidFrame) => Err(protocol_error(
            "compiled runner emitted an oversized or malformed frame",
        )),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(protocol_error(
            "compiled runner did not complete the protocol step before timeout",
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(protocol_error(
            "compiled runner protocol reader stopped unexpectedly",
        )),
    }
}

fn ensure_no_extra_frame(receiver: &Receiver<ReaderMessage>) -> Result<()> {
    match receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(ReaderMessage::Eof) => Ok(()),
        Ok(ReaderMessage::Frame(_)) => Err(protocol_error(
            "compiled runner emitted more than one response for the request",
        )),
        Ok(ReaderMessage::InvalidFrame) => Err(protocol_error(
            "compiled runner emitted an oversized or malformed frame",
        )),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(protocol_error(
            "compiled runner did not close its protocol output",
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
    }
}

fn wait_child(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|_| {
            DbmError::Execution("could not inspect compiled runner process".to_owned())
        })? {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            stop_child(child);
            return Err(protocol_error(
                "compiled runner exceeded its configured timeout",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn ensure_successful_exit(status: ExitStatus, execution_started: bool) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    if execution_started {
        return Err(DbmError::ReconciliationRequired(
            "compiled runner exited unsuccessfully after execution; inspect status and doctor before retrying"
                .to_owned(),
        ));
    }
    Err(protocol_error("compiled runner exited unsuccessfully"))
}

fn parse_hello(frame: &[u8]) -> Result<HelloRequest> {
    let request: HelloRequest = serde_json::from_slice(frame)
        .map_err(|_| protocol_error("runner handshake JSON is invalid"))?;
    if request.kind != "hello" {
        return Err(protocol_error("expected a hello request"));
    }
    Ok(request)
}

fn parse_execute(frame: &[u8]) -> Result<ExecuteRequest> {
    let request: ExecuteRequest = serde_json::from_slice(frame)
        .map_err(|_| protocol_error("runner execution JSON is invalid"))?;
    if request.kind != "execute" {
        return Err(protocol_error("expected an execute request"));
    }
    Ok(request)
}

fn parse_ready(frame: &[u8]) -> Result<ReadyResponse> {
    let kind = response_kind(frame)?;
    if kind == "error" {
        return Err(parse_remote_error(frame, false));
    }
    let response: ReadyResponse = serde_json::from_slice(frame)
        .map_err(|_| protocol_error("runner handshake response is invalid"))?;
    if response.kind != "ready" {
        return Err(protocol_error("expected a ready response"));
    }
    Ok(response)
}

fn parse_complete(frame: &[u8]) -> Result<CompleteResponse> {
    let kind = response_kind(frame)?;
    if kind == "error" {
        return Err(parse_remote_error(frame, true));
    }
    let response: CompleteResponse = serde_json::from_slice(frame)
        .map_err(|_| protocol_error("runner completion response is invalid"))?;
    if response.kind != "complete" {
        return Err(protocol_error("expected a complete response"));
    }
    Ok(response)
}

fn response_kind(frame: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(frame)
        .map_err(|_| protocol_error("runner response is not valid JSON"))?;
    value
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| protocol_error("runner response is missing its kind"))
}

fn parse_remote_error(frame: &[u8], execution_started: bool) -> DbmError {
    let response: ErrorResponse = match serde_json::from_slice(frame) {
        Ok(response) => response,
        Err(_) => return protocol_error("runner error response is invalid"),
    };
    if response.kind != "error" || response.protocol_version != RUNNER_PROTOCOL_VERSION {
        return protocol_error("runner error response version is incompatible");
    }
    if execution_started && response.stage == "execution" {
        return DbmError::ReconciliationRequired(format!(
            "compiled runner execution failed (code {}); inspect status and doctor before retrying: {}",
            response.code, response.message
        ));
    }
    DbmError::Execution(format!(
        "compiled runner rejected the request (code {}): {}",
        response.code, response.message
    ))
}

fn validate_hello(request: &HelloRequest, runner_fingerprint: &str) -> Result<()> {
    if request.protocol_version != RUNNER_PROTOCOL_VERSION {
        return Err(DbmError::Unsupported(format!(
            "runner protocol version {} is incompatible with required version {}",
            request.protocol_version, RUNNER_PROTOCOL_VERSION
        )));
    }
    if request.engine_version != ENGINE_VERSION {
        return Err(DbmError::Unsupported(
            "runner engine version does not match the requested engine version".to_owned(),
        ));
    }
    if request.metadata_schema_version != crate::storage::METADATA_FORMAT_VERSION as u32 {
        return Err(DbmError::Unsupported(
            "runner metadata schema version is incompatible".to_owned(),
        ));
    }
    if request.runner_fingerprint != runner_fingerprint {
        return Err(DbmError::Safety(
            "compiled runner binary changed between discovery and process startup".to_owned(),
        ));
    }
    if request.config_path.is_empty() {
        return Err(protocol_error("runner handshake has no config path"));
    }
    crate::identity::validate_namespace(&request.namespace)?;
    validate_action_shape(request.action, request.down_steps)
}

fn validate_execute(
    request: &ExecuteRequest,
    operation: RunnerOperation,
    preflight: &engine::RunnerPreflight,
    runner_fingerprint: &str,
) -> Result<()> {
    if request.protocol_version != RUNNER_PROTOCOL_VERSION {
        return Err(DbmError::Unsupported(
            "runner execution protocol version is incompatible".to_owned(),
        ));
    }
    validate_action_shape(request.action, request.down_steps)?;
    if request.runner_fingerprint != runner_fingerprint {
        return Err(DbmError::Safety(
            "runner execution request fingerprint does not match the running binary".to_owned(),
        ));
    }
    if request.action != operation.action() || request.down_steps != operation.steps() {
        return Err(DbmError::Safety(
            "runner execution request does not match the negotiated operation".to_owned(),
        ));
    }
    if request.namespace != preflight.report.namespace {
        return Err(DbmError::Safety(
            "runner execution namespace does not match the read-only handshake".to_owned(),
        ));
    }
    if request.manifest_fingerprint != preflight.manifest_fingerprint
        || request.plan_hash != preflight.plan_hash
        || request.target_fingerprint != preflight.target_fingerprint
    {
        return Err(DbmError::Safety(
            "runner execution request fingerprints do not match the read-only handshake".to_owned(),
        ));
    }
    Ok(())
}

fn validate_ready(
    response: &ReadyResponse,
    expected_runner_fingerprint: &str,
    expected_namespace: &str,
) -> Result<()> {
    if response.protocol_version != RUNNER_PROTOCOL_VERSION {
        return Err(DbmError::Unsupported(
            "compiled runner protocol version is incompatible".to_owned(),
        ));
    }
    if response.engine_version != ENGINE_VERSION {
        return Err(DbmError::Unsupported(
            "compiled runner engine version is incompatible".to_owned(),
        ));
    }
    if response.metadata_schema_version != crate::storage::METADATA_FORMAT_VERSION as u32 {
        return Err(DbmError::Unsupported(
            "compiled runner metadata schema version is incompatible".to_owned(),
        ));
    }
    if response.runner_fingerprint.is_empty()
        || response.runner_fingerprint != expected_runner_fingerprint
    {
        return Err(protocol_error(
            "compiled runner binary fingerprint does not match the configured executable",
        ));
    }
    if response.namespace != expected_namespace
        || response.report.get("namespace").and_then(Value::as_str) != Some(expected_namespace)
    {
        return Err(DbmError::Safety(
            "compiled runner selected a namespace different from the CLI request".to_owned(),
        ));
    }
    for required in runner_capabilities() {
        if !response.capabilities.contains(&required) {
            return Err(DbmError::Unsupported(format!(
                "compiled runner lacks required capability '{required}'"
            )));
        }
    }
    if response.manifest_fingerprint.is_empty()
        || response.plan_hash.is_empty()
        || response.target_fingerprint.is_empty()
    {
        return Err(protocol_error(
            "compiled runner handshake omitted a fingerprint",
        ));
    }
    let report_hash = response
        .report
        .get("plan_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("compiled runner plan is missing its plan hash"))?;
    if report_hash != response.plan_hash {
        return Err(protocol_error(
            "compiled runner plan hash does not match its handshake",
        ));
    }
    Ok(())
}

fn validate_complete(
    response: &CompleteResponse,
    ready: &ReadyResponse,
    operation: RunnerOperation,
) -> Result<()> {
    let report_command = response.report.get("command").and_then(Value::as_str);
    let report_namespace = response.report.get("namespace").and_then(Value::as_str);
    let report_namespace_matches = match report_namespace {
        Some(namespace) => namespace == ready.namespace,
        None => operation == RunnerOperation::Validate,
    };
    if response.protocol_version != RUNNER_PROTOCOL_VERSION
        || response.runner_fingerprint != ready.runner_fingerprint
        || response.namespace != ready.namespace
        || !report_namespace_matches
        || report_command != Some(operation.report_command())
        || response.manifest_fingerprint != ready.manifest_fingerprint
        || response.plan_hash != ready.plan_hash
        || response.target_fingerprint != ready.target_fingerprint
    {
        return Err(protocol_error(
            "compiled runner completion fingerprints do not match the handshake",
        ));
    }
    Ok(())
}

fn same_preflight(left: &engine::RunnerPreflight, right: &engine::RunnerPreflight) -> bool {
    left.manifest_fingerprint == right.manifest_fingerprint
        && left.plan_hash == right.plan_hash
        && left.target_fingerprint == right.target_fingerprint
}

fn validate_action_shape(action: WireAction, down_steps: Option<usize>) -> Result<()> {
    match (action, down_steps) {
        (WireAction::Down, Some(steps)) if steps > 0 => Ok(()),
        (WireAction::Down, _) => Err(protocol_error("down action requires positive down_steps")),
        (
            WireAction::Validate
            | WireAction::Plan
            | WireAction::Status
            | WireAction::History
            | WireAction::Doctor
            | WireAction::Up,
            None,
        ) => Ok(()),
        (
            WireAction::Validate
            | WireAction::Plan
            | WireAction::Status
            | WireAction::History
            | WireAction::Doctor
            | WireAction::Up,
            Some(_),
        ) => Err(protocol_error("only down action accepts down_steps")),
    }
}

fn operation_from_wire(action: WireAction, down_steps: Option<usize>) -> Result<RunnerOperation> {
    validate_action_shape(action, down_steps)?;
    match (action, down_steps) {
        (WireAction::Validate, None) => Ok(RunnerOperation::Validate),
        (WireAction::Plan, None) => Ok(RunnerOperation::Plan),
        (WireAction::Status, None) => Ok(RunnerOperation::Status),
        (WireAction::History, None) => Ok(RunnerOperation::History),
        (WireAction::Doctor, None) => Ok(RunnerOperation::Doctor),
        (WireAction::Up, None) => Ok(RunnerOperation::Up),
        (WireAction::Down, Some(steps)) => Ok(RunnerOperation::Down { steps }),
        _ => Err(protocol_error(
            "runner action and step count are inconsistent",
        )),
    }
}

fn complete_response(
    report: Value,
    preflight: &engine::RunnerPreflight,
    runner_fingerprint: &str,
) -> CompleteResponse {
    CompleteResponse {
        kind: "complete".to_owned(),
        protocol_version: RUNNER_PROTOCOL_VERSION,
        runner_fingerprint: runner_fingerprint.to_owned(),
        namespace: preflight.report.namespace.clone(),
        manifest_fingerprint: preflight.manifest_fingerprint.clone(),
        plan_hash: preflight.plan_hash.clone(),
        target_fingerprint: preflight.target_fingerprint.clone(),
        report,
    }
}

fn runner_capabilities() -> Vec<String> {
    vec![
        "filesystem_sql".to_owned(),
        "embedded_sql".to_owned(),
        "schema_dsl".to_owned(),
        "rust_plan".to_owned(),
        "mixed_plan".to_owned(),
    ]
}

fn write_error<W: Write>(writer: &mut W, stage: &str, error: &DbmError) -> Result<()> {
    let message = bounded_message(&error.to_string(), 8 * 1024);
    let response = ErrorResponse {
        kind: "error".to_owned(),
        protocol_version: RUNNER_PROTOCOL_VERSION,
        stage: stage.to_owned(),
        code: error.exit_code(),
        message,
    };
    write_frame(writer, &response)
}

fn bounded_message(message: &str, max_bytes: usize) -> String {
    let mut bounded = String::new();
    for character in message.chars() {
        if bounded.len() + character.len_utf8() > max_bytes {
            bounded.push('…');
            break;
        }
        bounded.push(character);
    }
    bounded
}

fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    let frame = serde_json::to_vec(value)?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(protocol_error(
            "runner protocol frame exceeds the size limit",
        ));
    }
    writer
        .write_all(&frame)
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|_| protocol_error("could not write runner protocol frame"))
}

fn read_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unterminated runner frame",
            ));
        }
        let has_newline = available.contains(&b'\n');
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        let content_count = if has_newline { count - 1 } else { count };
        if frame.len() + content_count > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runner frame exceeds size limit",
            ));
        }
        frame.extend_from_slice(&available[..content_count]);
        reader.consume(count);
        if has_newline {
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }
    }
}

fn protocol_error(message: &str) -> DbmError {
    DbmError::Driver {
        driver: "migration runner protocol",
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use crate::engine;

    use super::{
        ExecuteRequest, MAX_FRAME_BYTES, RUNNER_PROTOCOL_VERSION, RunnerOperation, WireAction,
        operation_from_wire, read_frame, validate_action_shape, validate_execute,
    };

    #[test]
    fn protocol_framing_reads_one_bounded_json_line() {
        let input = Cursor::new(b"{\"kind\":\"hello\"}\nsecond\n".to_vec());
        let mut reader = BufReader::new(input);
        assert_eq!(
            read_frame(&mut reader).expect("read first frame"),
            Some(b"{\"kind\":\"hello\"}".to_vec())
        );
        assert_eq!(
            read_frame(&mut reader).expect("read second frame"),
            Some(b"second".to_vec())
        );
        assert_eq!(read_frame(&mut reader).expect("read EOF"), None);
    }

    #[test]
    fn protocol_framing_rejects_oversized_and_unterminated_frames() {
        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut oversized = BufReader::new(Cursor::new(oversized));
        assert!(read_frame(&mut oversized).is_err());

        let mut unterminated = BufReader::new(Cursor::new(b"partial".to_vec()));
        assert!(read_frame(&mut unterminated).is_err());
    }

    #[test]
    fn protocol_rejects_inconsistent_operation_and_step_count() {
        assert!(validate_action_shape(WireAction::Up, Some(1)).is_err());
        assert!(validate_action_shape(WireAction::Down, None).is_err());
        assert!(matches!(
            operation_from_wire(WireAction::Down, Some(3)).expect("valid down request"),
            RunnerOperation::Down { steps: 3 }
        ));
    }

    #[test]
    fn execute_handshake_rejects_every_fingerprint_divergence() {
        let preflight = engine::RunnerPreflight {
            manifest_fingerprint: "sha256:manifest".to_owned(),
            plan_hash: "sha256:plan".to_owned(),
            target_fingerprint: "sha256:target".to_owned(),
            report: engine::PlanReport {
                schema_version: "1.0",
                command: "plan",
                target: "sqlite:test.db".to_owned(),
                namespace: "default".to_owned(),
                status: "ready",
                plan_hash: "sha256:plan".to_owned(),
                read_only: true,
                operations: Vec::new(),
                warnings: Vec::new(),
                errors: Vec::new(),
            },
        };
        let baseline = ExecuteRequest {
            kind: "execute".to_owned(),
            protocol_version: RUNNER_PROTOCOL_VERSION,
            runner_fingerprint: "sha256:runner".to_owned(),
            namespace: "default".to_owned(),
            action: WireAction::Up,
            down_steps: None,
            manifest_fingerprint: preflight.manifest_fingerprint.clone(),
            plan_hash: preflight.plan_hash.clone(),
            target_fingerprint: preflight.target_fingerprint.clone(),
        };
        assert!(
            validate_execute(&baseline, RunnerOperation::Up, &preflight, "sha256:runner").is_ok()
        );

        for fingerprint in [
            "runner_fingerprint",
            "manifest_fingerprint",
            "plan_hash",
            "target_fingerprint",
        ] {
            let mut request = ExecuteRequest {
                kind: baseline.kind.clone(),
                protocol_version: baseline.protocol_version,
                runner_fingerprint: baseline.runner_fingerprint.clone(),
                namespace: baseline.namespace.clone(),
                action: baseline.action,
                down_steps: baseline.down_steps,
                manifest_fingerprint: baseline.manifest_fingerprint.clone(),
                plan_hash: baseline.plan_hash.clone(),
                target_fingerprint: baseline.target_fingerprint.clone(),
            };
            match fingerprint {
                "runner_fingerprint" => {
                    request.runner_fingerprint = "sha256:wrong-runner".to_owned();
                }
                "manifest_fingerprint" => {
                    request.manifest_fingerprint = "sha256:wrong-manifest".to_owned();
                }
                "plan_hash" => {
                    request.plan_hash = "sha256:wrong-plan".to_owned();
                }
                "target_fingerprint" => {
                    request.target_fingerprint = "sha256:wrong-target".to_owned();
                }
                _ => unreachable!("fingerprint test list is exhaustive"),
            }
            assert!(
                validate_execute(&request, RunnerOperation::Up, &preflight, "sha256:runner")
                    .is_err(),
                "runner must reject divergent {fingerprint}"
            );
        }

        let mut wrong_namespace = baseline;
        wrong_namespace.namespace = "tenant_b".to_owned();
        assert!(
            validate_execute(
                &wrong_namespace,
                RunnerOperation::Up,
                &preflight,
                "sha256:runner"
            )
            .is_err()
        );
    }
}
