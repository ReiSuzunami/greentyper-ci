//! Product-owned local process Tool adapter.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use std::time::Instant;

use greentyper_core::agent_team::{
    Capability, CapabilitySnapshot, CommandOutcome, ResourceBudget, TaskScope, TaskSpec,
    TeamCommand,
};
use greentyper_core::runtime::{RuntimeError, RuntimeKernel};
use greentyper_core::tool_runtime::{
    ApprovalDecision, AuthorizedToolCall, ToolArguments, ToolCallOutcome, ToolCallStatus,
    ToolEffectExecutor, ToolExecution, ToolIntent, ToolRequestOutcome, ToolResources,
    ToolRuntimeError,
};
use serde::Deserialize;

pub(crate) const LOCAL_ECHO_TOOL: &str = "local.echo";
const LOCAL_ECHO_PROCESS: &str = "greentyper.local.echo.v1";
const CHILD_COMMAND: &str = "__local-process-child";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const SMOKE_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const READ_BUFFER_BYTES: usize = 8 * 1024;
const EXECUTION_FAILED: &str = "local process execution failed";
const EXECUTION_AMBIGUOUS: &str = "local process outcome is ambiguous";
const ENVIRONMENT_PROBE: &str = "GREENTYPER_LOCAL_PROCESS_SECRET";

pub(crate) enum LocalProcessSmokeOutcome {
    Succeeded(Vec<u8>),
    SucceededWithoutOutput,
    Failed,
    ReconciliationRequired,
    ReconciliationRequiredExisting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalProcessSmokeScenario {
    Echo,
    Timeout,
    OutputLimit,
    OutputFlood,
    NonzeroExit,
    Environment,
    DescendantDenied,
    NetworkDenied,
    FilesystemReadDenied,
    FilesystemWriteDenied,
    ProcessMismatch,
    InvalidArguments,
    DescendantTimeout,
    BlockedStdin,
    SpawnFailure,
    WorkingDirectory,
}

impl LocalProcessSmokeScenario {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "echo" => Some(Self::Echo),
            "timeout" => Some(Self::Timeout),
            "output-limit" => Some(Self::OutputLimit),
            "output-flood" => Some(Self::OutputFlood),
            "nonzero-exit" => Some(Self::NonzeroExit),
            "environment" => Some(Self::Environment),
            "descendant-denied" => Some(Self::DescendantDenied),
            "network-denied" => Some(Self::NetworkDenied),
            "filesystem-read-denied" => Some(Self::FilesystemReadDenied),
            "filesystem-write-denied" => Some(Self::FilesystemWriteDenied),
            "process-mismatch" => Some(Self::ProcessMismatch),
            "invalid-arguments" => Some(Self::InvalidArguments),
            "descendant-timeout" => Some(Self::DescendantTimeout),
            "blocked-stdin" => Some(Self::BlockedStdin),
            "spawn-failure" => Some(Self::SpawnFailure),
            "working-directory" => Some(Self::WorkingDirectory),
            _ => None,
        }
    }

    fn identity(self) -> &'static str {
        match self {
            Self::Echo => "product-local-echo-v1",
            Self::Timeout => "product-local-echo-timeout-v1",
            Self::OutputLimit => "product-local-echo-output-limit-v1",
            Self::OutputFlood => "product-local-echo-output-flood-v1",
            Self::NonzeroExit => "product-local-echo-nonzero-exit-v1",
            Self::Environment => "product-local-echo-environment-v1",
            Self::DescendantDenied => "product-local-echo-descendant-denied-v1",
            Self::NetworkDenied => "product-local-echo-network-denied-v1",
            Self::FilesystemReadDenied => "product-local-echo-filesystem-read-denied-v1",
            Self::FilesystemWriteDenied => "product-local-echo-filesystem-write-denied-v1",
            Self::ProcessMismatch => "product-local-echo-process-mismatch-v1",
            Self::InvalidArguments => "product-local-echo-invalid-arguments-v1",
            Self::DescendantTimeout => "product-local-echo-descendant-timeout-v1",
            Self::BlockedStdin => "product-local-echo-blocked-stdin-v1",
            Self::SpawnFailure => "product-local-echo-spawn-failure-v1",
            Self::WorkingDirectory => "product-local-echo-working-directory-v1",
        }
    }

    fn requests_network(self) -> bool {
        matches!(self, Self::NetworkDenied)
    }

    fn requests_filesystem_read(self) -> bool {
        matches!(self, Self::FilesystemReadDenied)
    }

    fn requests_filesystem_write(self) -> bool {
        matches!(self, Self::FilesystemWriteDenied)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalProcessChildMode {
    Echo,
    Hang,
    Overflow,
    Flood,
    Fail,
    Environment,
    Descendant,
    DescendantHang,
    ShortHang,
    BlockedStdin,
    WorkingDirectory,
}

impl LocalProcessChildMode {
    pub(crate) fn parse(value: Option<&str>) -> Option<Self> {
        match value {
            Some("echo") => Some(Self::Echo),
            Some("hang") => Some(Self::Hang),
            Some("overflow") => Some(Self::Overflow),
            Some("flood") => Some(Self::Flood),
            Some("fail") => Some(Self::Fail),
            Some("environment") => Some(Self::Environment),
            Some("descendant") => Some(Self::Descendant),
            Some("descendant-hang") => Some(Self::DescendantHang),
            Some("short-hang") => Some(Self::ShortHang),
            Some("blocked-stdin") => Some(Self::BlockedStdin),
            Some("working-directory") => Some(Self::WorkingDirectory),
            _ => None,
        }
    }

    fn argument(self) -> &'static str {
        match self {
            Self::Echo => "echo",
            Self::Hang => "hang",
            Self::Overflow => "overflow",
            Self::Flood => "flood",
            Self::Fail => "fail",
            Self::Environment => "environment",
            Self::Descendant => "descendant",
            Self::DescendantHang => "descendant-hang",
            Self::ShortHang => "short-hang",
            Self::BlockedStdin => "blocked-stdin",
            Self::WorkingDirectory => "working-directory",
        }
    }
}

pub(crate) struct LocalProcessExecutor {
    executable: PathBuf,
    timeout: Duration,
    child_mode: LocalProcessChildMode,
}

impl LocalProcessExecutor {
    fn current() -> Result<Self, LocalProcessError> {
        let executable = std::env::current_exe().map_err(LocalProcessError::Io)?;
        let executable = executable.canonicalize().map_err(LocalProcessError::Io)?;
        if !executable.is_absolute() || !executable.is_file() {
            return Err(LocalProcessError::InvalidExecutable);
        }
        Ok(Self {
            executable,
            timeout: DEFAULT_TIMEOUT,
            child_mode: LocalProcessChildMode::Echo,
        })
    }

    fn for_smoke(scenario: LocalProcessSmokeScenario) -> Result<Self, LocalProcessError> {
        let mut executor = Self::current()?;
        if matches!(scenario, LocalProcessSmokeScenario::Timeout) {
            executor.timeout = SMOKE_TIMEOUT;
            executor.child_mode = LocalProcessChildMode::Hang;
        } else if matches!(scenario, LocalProcessSmokeScenario::OutputLimit) {
            executor.child_mode = LocalProcessChildMode::Overflow;
        } else if matches!(scenario, LocalProcessSmokeScenario::OutputFlood) {
            executor.child_mode = LocalProcessChildMode::Flood;
        } else if matches!(scenario, LocalProcessSmokeScenario::NonzeroExit) {
            executor.child_mode = LocalProcessChildMode::Fail;
        } else if matches!(scenario, LocalProcessSmokeScenario::Environment) {
            executor.child_mode = LocalProcessChildMode::Environment;
        } else if matches!(scenario, LocalProcessSmokeScenario::DescendantDenied) {
            executor.child_mode = LocalProcessChildMode::Descendant;
        } else if matches!(scenario, LocalProcessSmokeScenario::DescendantTimeout) {
            executor.timeout = SMOKE_TIMEOUT;
            executor.child_mode = LocalProcessChildMode::DescendantHang;
        } else if matches!(scenario, LocalProcessSmokeScenario::BlockedStdin) {
            executor.timeout = SMOKE_TIMEOUT;
            executor.child_mode = LocalProcessChildMode::BlockedStdin;
        } else if matches!(scenario, LocalProcessSmokeScenario::SpawnFailure) {
            executor.executable = executor.executable.with_file_name(format!(
                "greentyper-missing-local-process-{}",
                std::process::id()
            ));
        } else if matches!(scenario, LocalProcessSmokeScenario::WorkingDirectory) {
            executor.child_mode = LocalProcessChildMode::WorkingDirectory;
        }
        Ok(executor)
    }

    fn execute_echo(&self, message: &[u8]) -> ToolExecution {
        match self.run_echo(message) {
            Ok(ProcessOutcome::Succeeded(output)) => ToolExecution::Succeeded { output },
            Ok(ProcessOutcome::Failed) | Err(ProcessRunError::NotStarted) => {
                ToolExecution::Failed {
                    reason: EXECUTION_FAILED.into(),
                }
            }
            Ok(ProcessOutcome::Ambiguous) | Err(ProcessRunError::Ambiguous) => {
                ToolExecution::Ambiguous {
                    reason: EXECUTION_AMBIGUOUS.into(),
                }
            }
        }
    }

    fn run_echo(&self, message: &[u8]) -> Result<ProcessOutcome, ProcessRunError> {
        let mut container = ProcessContainer::new().map_err(|_| ProcessRunError::NotStarted)?;
        let mut command = Command::new(&self.executable);
        let working_directory = self
            .executable
            .parent()
            .ok_or(ProcessRunError::NotStarted)?;
        command
            .arg(CHILD_COMMAND)
            .arg(self.child_mode.argument())
            .env_clear()
            .current_dir(working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_command(&mut command);
        let mut child = command.spawn().map_err(|_| ProcessRunError::NotStarted)?;
        if container.activate(&mut child).is_err() {
            terminate_uncontained(&mut child);
            return Err(ProcessRunError::Ambiguous);
        }

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                terminate_and_wait(&container, &mut child);
                return Err(ProcessRunError::Ambiguous);
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                terminate_and_wait(&container, &mut child);
                return Err(ProcessRunError::Ambiguous);
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                terminate_and_wait(&container, &mut child);
                return Err(ProcessRunError::Ambiguous);
            }
        };
        let output_bytes = Arc::new(AtomicUsize::new(0));
        let output_limit_exceeded = Arc::new(AtomicBool::new(false));
        let stdout_reader = spawn_reader(
            stdout,
            true,
            Arc::clone(&output_bytes),
            Arc::clone(&output_limit_exceeded),
        );
        let stderr_reader = spawn_reader(
            stderr,
            false,
            Arc::clone(&output_bytes),
            Arc::clone(&output_limit_exceeded),
        );

        let input = if matches!(self.child_mode, LocalProcessChildMode::BlockedStdin) {
            vec![b'x'; MAX_INPUT_BYTES * 16]
        } else {
            message.to_vec()
        };
        let stdin_writer = spawn_writer(stdin, input);

        let status = match wait_for_exit(&mut child, self.timeout, &output_limit_exceeded) {
            Ok(Some(status)) => status,
            Ok(None) => {
                terminate_and_wait(&container, &mut child);
                let _ = join_writer(stdin_writer);
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Ok(ProcessOutcome::Ambiguous);
            }
            Err(_) => {
                terminate_and_wait(&container, &mut child);
                let _ = join_writer(stdin_writer);
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Ok(ProcessOutcome::Ambiguous);
            }
        };
        // The production echo child cannot spawn or detach descendants. A future
        // caller-selected process adapter must add a bounded post-exit pipe policy.
        let write_result = join_writer(stdin_writer);
        let stdout = join_reader(stdout_reader)?;
        let stderr = join_reader(stderr_reader)?;
        if write_result.is_err() {
            return Ok(ProcessOutcome::Ambiguous);
        }
        Ok(classify_process(
            status,
            stdout,
            stderr,
            output_limit_exceeded.load(Ordering::Relaxed),
        ))
    }
}

impl ToolEffectExecutor for LocalProcessExecutor {
    fn execute(&mut self, call: &AuthorizedToolCall<'_>) -> ToolExecution {
        let Ok(arguments) = validate_echo_call(call) else {
            return ToolExecution::Failed {
                reason: EXECUTION_FAILED.into(),
            };
        };
        self.execute_echo(arguments.message.as_bytes())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EchoArguments {
    message: String,
}

fn validate_echo_call(call: &AuthorizedToolCall<'_>) -> Result<EchoArguments, ()> {
    if call.tool() != LOCAL_ECHO_TOOL
        || call.resources().process() != Some(LOCAL_ECHO_PROCESS)
        || call.resources().filesystem_reads().next().is_some()
        || call.resources().filesystem_writes().next().is_some()
        || call.resources().network_targets().next().is_some()
    {
        return Err(());
    }
    let arguments: EchoArguments =
        serde_json::from_str(call.arguments().canonical_json()).map_err(|_| ())?;
    (arguments.message.len() <= MAX_INPUT_BYTES)
        .then_some(arguments)
        .ok_or(())
}

enum ProcessOutcome {
    Succeeded(Vec<u8>),
    Failed,
    Ambiguous,
}

enum ProcessRunError {
    NotStarted,
    Ambiguous,
}

impl From<io::Error> for ProcessRunError {
    fn from(_source: io::Error) -> Self {
        Self::Ambiguous
    }
}

struct ReaderOutcome {
    bytes: Vec<u8>,
}

fn spawn_reader(
    reader: impl Read + Send + 'static,
    collect: bool,
    total_bytes: Arc<AtomicUsize>,
    limit_exceeded: Arc<AtomicBool>,
) -> JoinHandle<io::Result<ReaderOutcome>> {
    thread::spawn(move || read_bounded(reader, collect, &total_bytes, &limit_exceeded))
}

fn spawn_writer(
    mut writer: impl Write + Send + 'static,
    input: Vec<u8>,
) -> JoinHandle<io::Result<()>> {
    thread::spawn(move || writer.write_all(&input))
}

fn join_writer(writer: JoinHandle<io::Result<()>>) -> io::Result<()> {
    writer
        .join()
        .map_err(|_| io::Error::other("local process input writer panicked"))?
}

fn read_bounded(
    mut reader: impl Read,
    collect: bool,
    total_bytes: &AtomicUsize,
    limit_exceeded: &AtomicBool,
) -> io::Result<ReaderOutcome> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; READ_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let previous =
            match total_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(read))
            }) {
                Ok(previous) => previous,
                Err(_) => unreachable!("the output byte counter always updates"),
            };
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(previous);
        if collect && remaining > 0 {
            bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            limit_exceeded.store(true, Ordering::Relaxed);
        }
    }
    Ok(ReaderOutcome { bytes })
}

fn join_reader(reader: JoinHandle<io::Result<ReaderOutcome>>) -> io::Result<ReaderOutcome> {
    reader
        .join()
        .map_err(|_| io::Error::other("local process output reader panicked"))?
}

fn classify_process(
    status: ExitStatus,
    stdout: ReaderOutcome,
    _stderr: ReaderOutcome,
    output_limit_exceeded: bool,
) -> ProcessOutcome {
    if output_limit_exceeded {
        return ProcessOutcome::Ambiguous;
    }
    if status.success() {
        ProcessOutcome::Succeeded(stdout.bytes)
    } else {
        ProcessOutcome::Failed
    }
}

#[cfg(unix)]
fn configure_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(all(not(unix), not(windows)))]
fn configure_command(_command: &mut Command) {}

#[cfg(windows)]
fn configure_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

fn wait_for_exit(
    child: &mut Child,
    timeout: Duration,
    output_limit_exceeded: &AtomicBool,
) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::other("local process timeout overflow"))?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if output_limit_exceeded.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10).min(deadline.duration_since(now)));
    }
}

#[cfg(unix)]
struct ProcessContainer {
    process_group: Option<i32>,
}

#[cfg(unix)]
impl ProcessContainer {
    fn new() -> io::Result<Self> {
        Ok(Self {
            process_group: None,
        })
    }

    fn activate(&mut self, child: &mut Child) -> io::Result<()> {
        self.process_group = Some(
            i32::try_from(child.id())
                .map_err(|_| io::Error::other("local process id exceeds the Unix pid range"))?,
        );
        Ok(())
    }

    fn terminate(&self, child: &mut Child) -> io::Result<()> {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        unix_process::terminate_group(
            self.process_group
                .ok_or_else(|| io::Error::other("local process group is unavailable"))?,
        )
    }
}

#[cfg(all(not(unix), not(windows)))]
struct ProcessContainer;

#[cfg(all(not(unix), not(windows)))]
impl ProcessContainer {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    fn activate(&mut self, _child: &mut Child) -> io::Result<()> {
        Ok(())
    }

    fn terminate(&self, child: &mut Child) -> io::Result<()> {
        child.kill()
    }
}

#[cfg(windows)]
type ProcessContainer = windows_job::Job;

fn terminate_uncontained(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_and_wait(container: &ProcessContainer, child: &mut Child) {
    let _ = container.terminate(child);
    let _ = child.wait();
}

#[cfg(unix)]
mod unix_process {
    #![allow(unsafe_code)]

    use std::io;

    pub(super) fn terminate_group(process_group: i32) -> io::Result<()> {
        // SAFETY: a negative pid targets only the dedicated child process group.
        if unsafe { libc::kill(-process_group, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let source = io::Error::last_os_error();
        if source.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(source)
        }
    }
}

pub(crate) fn run_local_process_child(mode: LocalProcessChildMode) -> io::Result<()> {
    if matches!(mode, LocalProcessChildMode::Hang) {
        thread::sleep(Duration::from_secs(60));
        return Ok(());
    }
    if matches!(mode, LocalProcessChildMode::Overflow) {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        let chunk = [b'x'; READ_BUFFER_BYTES];
        let mut remaining = MAX_OUTPUT_BYTES + 1;
        while remaining > 0 {
            let bytes = remaining.min(chunk.len());
            stdout.write_all(&chunk[..bytes])?;
            remaining -= bytes;
        }
        return stdout.flush();
    }
    if matches!(mode, LocalProcessChildMode::Flood) {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        let chunk = [b'x'; READ_BUFFER_BYTES];
        let mut remaining = MAX_OUTPUT_BYTES + 1;
        while remaining > 0 {
            let bytes = remaining.min(chunk.len());
            stdout.write_all(&chunk[..bytes])?;
            remaining -= bytes;
        }
        stdout.flush()?;
        thread::sleep(Duration::from_secs(60));
        return Ok(());
    }
    if matches!(mode, LocalProcessChildMode::Fail) {
        return Err(io::Error::other("local process fixture failed"));
    }
    if matches!(mode, LocalProcessChildMode::Environment) {
        let status = if std::env::var_os(ENVIRONMENT_PROBE).is_some() {
            b"set".as_slice()
        } else {
            b"unset".as_slice()
        };
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(status)?;
        return stdout.flush();
    }
    if matches!(mode, LocalProcessChildMode::Descendant) {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg(CHILD_COMMAND)
            .arg(LocalProcessChildMode::Hang.argument())
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let denied = match command.spawn() {
            Err(_) => true,
            Ok(mut descendant) => {
                let deadline = Instant::now() + Duration::from_secs(2);
                let denied = loop {
                    if descendant.try_wait()?.is_some() {
                        break true;
                    }
                    if Instant::now() >= deadline {
                        break false;
                    }
                    thread::sleep(Duration::from_millis(10));
                };
                if !denied {
                    terminate_uncontained(&mut descendant);
                }
                denied
            }
        };
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(if denied {
            b"descendant-denied"
        } else {
            b"descendant-allowed"
        })?;
        return stdout.flush();
    }
    if matches!(mode, LocalProcessChildMode::DescendantHang) {
        let _descendant = Command::new(std::env::current_exe()?)
            .arg(CHILD_COMMAND)
            .arg(LocalProcessChildMode::ShortHang.argument())
            .env_clear()
            .stdin(Stdio::null())
            .spawn()?;
        thread::sleep(Duration::from_secs(60));
        return Ok(());
    }
    if matches!(mode, LocalProcessChildMode::ShortHang) {
        thread::sleep(Duration::from_secs(2));
        return Ok(());
    }
    if matches!(mode, LocalProcessChildMode::BlockedStdin) {
        thread::sleep(Duration::from_secs(2));
        return Ok(());
    }
    if matches!(mode, LocalProcessChildMode::WorkingDirectory) {
        let mut input = Vec::new();
        io::stdin()
            .take((MAX_INPUT_BYTES + 1) as u64)
            .read_to_end(&mut input)?;
        if input.len() > MAX_INPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "local process input exceeds its byte limit",
            ));
        }
        let expected = PathBuf::from(
            String::from_utf8(input)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid test path"))?,
        )
        .canonicalize()?;
        let status = if std::env::current_dir()?.canonicalize()? == expected {
            b"inherited".as_slice()
        } else {
            b"detached".as_slice()
        };
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout.write_all(status)?;
        return stdout.flush();
    }
    let mut input = Vec::new();
    io::stdin()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > MAX_INPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local process input exceeds its byte limit",
        ));
    }
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(&input)?;
    stdout.flush()
}

pub(crate) fn run_smoke(
    run_dir: &Path,
    scenario: LocalProcessSmokeScenario,
    message: &str,
) -> Result<LocalProcessSmokeOutcome, LocalProcessError> {
    fs::create_dir_all(run_dir).map_err(LocalProcessError::Io)?;
    let runtime_path = run_dir.join("runtime.ledger");
    let team_path = run_dir.join("team.ledger");
    let tool_path = run_dir.join("tool.ledger");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(runtime_path, team_path, tool_path, 1)
            .map_err(LocalProcessError::Runtime)?;
    let mut sessions = recovery.into_sessions();
    let session = if let Some(session) = sessions.pop() {
        if !sessions.is_empty() {
            return Err(LocalProcessError::UnexpectedRecovery);
        }
        session
    } else {
        let operation = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "exercise the bounded local process adapter",
                    TaskScope::from_labels(["local-process-smoke"]),
                ),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::from_capabilities(
                    [
                        Some(Capability::Tool(LOCAL_ECHO_TOOL.into())),
                        Some(Capability::Process),
                        scenario.requests_network().then_some(Capability::Network),
                        scenario
                            .requests_filesystem_read()
                            .then_some(Capability::WorkspaceRead),
                        scenario
                            .requests_filesystem_write()
                            .then_some(Capability::WorkspaceWrite),
                    ]
                    .into_iter()
                    .flatten(),
                ),
            })
            .map_err(LocalProcessError::Runtime)?;
        kernel
            .acknowledge_team_operation(operation.operation)
            .map_err(LocalProcessError::Runtime)?;
        match operation.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            _ => return Err(LocalProcessError::UnexpectedRecovery),
        }
    };

    let identity = scenario.identity();
    if let Some(record) = kernel.tool_snapshot().and_then(|snapshot| {
        snapshot
            .calls
            .into_iter()
            .find(|record| record.identity == identity)
    }) {
        return existing_smoke_outcome(record.status);
    }

    let argument_value = if matches!(scenario, LocalProcessSmokeScenario::InvalidArguments) {
        serde_json::json!({ "message": message, "unexpected": true })
    } else {
        serde_json::json!({ "message": message })
    };
    let arguments = ToolArguments::parse(
        &serde_json::to_string(&argument_value).map_err(LocalProcessError::Json)?,
    )
    .map_err(LocalProcessError::Tool)?;
    let process = if matches!(scenario, LocalProcessSmokeScenario::ProcessMismatch) {
        "greentyper.local.unknown.v1"
    } else {
        LOCAL_ECHO_PROCESS
    };
    let mut resources = ToolResources::default().with_process(process);
    if scenario.requests_network() {
        resources = resources.with_network_target("https://private.invalid");
    }
    if scenario.requests_filesystem_read() {
        resources = resources.with_filesystem_read("workspace/private-read");
    }
    if scenario.requests_filesystem_write() {
        resources = resources.with_filesystem_write("workspace/private-write");
    }
    let intent = ToolIntent::new(identity, LOCAL_ECHO_TOOL, arguments, resources)
        .map_err(LocalProcessError::Tool)?;
    let request = match kernel
        .request_tool_call(session, intent)
        .map_err(LocalProcessError::Runtime)?
    {
        ToolRequestOutcome::ApprovalRequired(request) => request,
        ToolRequestOutcome::Existing(record) => {
            return existing_smoke_outcome(record.status);
        }
    };
    let mut executor = LocalProcessExecutor::for_smoke(scenario)?;
    let outcome = kernel
        .resolve_tool_call(
            request,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut executor,
        )
        .map_err(LocalProcessError::Runtime)?;
    Ok(match outcome {
        ToolCallOutcome::Succeeded { output, .. } => LocalProcessSmokeOutcome::Succeeded(output),
        ToolCallOutcome::Failed(_) | ToolCallOutcome::Denied(_) => LocalProcessSmokeOutcome::Failed,
        ToolCallOutcome::ReconciliationRequired(_) => {
            LocalProcessSmokeOutcome::ReconciliationRequired
        }
    })
}

fn existing_smoke_outcome(
    status: ToolCallStatus,
) -> Result<LocalProcessSmokeOutcome, LocalProcessError> {
    match status {
        ToolCallStatus::Succeeded => Ok(LocalProcessSmokeOutcome::SucceededWithoutOutput),
        ToolCallStatus::Failed | ToolCallStatus::Denied => Ok(LocalProcessSmokeOutcome::Failed),
        ToolCallStatus::ReconciliationRequired => {
            Ok(LocalProcessSmokeOutcome::ReconciliationRequiredExisting)
        }
        ToolCallStatus::AwaitingApproval => Err(LocalProcessError::UnexpectedRecovery),
    }
}

#[derive(Debug)]
pub(crate) enum LocalProcessError {
    Io(io::Error),
    Json(serde_json::Error),
    Runtime(RuntimeError),
    Tool(ToolRuntimeError),
    InvalidExecutable,
    UnexpectedRecovery,
}

impl fmt::Display for LocalProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("local process I/O failed"),
            Self::Json(_) => formatter.write_str("local process arguments could not be encoded"),
            Self::Runtime(source) => write!(formatter, "{source}"),
            Self::Tool(source) => write!(formatter, "{source}"),
            Self::InvalidExecutable => formatter.write_str("local process executable is invalid"),
            Self::UnexpectedRecovery => {
                formatter.write_str("local process recovery state is invalid")
            }
        }
    }
}

impl Error for LocalProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Json(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Tool(source) => Some(source),
            Self::InvalidExecutable | Self::UnexpectedRecovery => None,
        }
    }
}

#[cfg(windows)]
mod windows_job {
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::ptr::{NonNull, null};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    const PROCESS_MEMORY_LIMIT_BYTES: usize = 128 * 1024 * 1024;

    pub(super) struct Job {
        handle: OwnedHandle,
    }

    impl Job {
        pub(super) fn new() -> io::Result<Self> {
            // SAFETY: null attributes and name request a private, non-inheritable Job handle.
            let handle = unsafe { CreateJobObjectW(null(), null()) };
            let handle = OwnedHandle::from_nullable(handle)?;
            let job = Self { handle };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.BasicLimitInformation.ActiveProcessLimit = 1;
            limits.ProcessMemoryLimit = PROCESS_MEMORY_LIMIT_BYTES;
            // SAFETY: pointer and length describe a live limits value for this owned Job handle.
            let configured = unsafe {
                SetInformationJobObject(
                    job.raw(),
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        pub(super) fn activate(&mut self, child: &mut Child) -> io::Result<()> {
            let process = child.as_raw_handle() as HANDLE;
            // SAFETY: process is borrowed from a live Child and self owns a live Job handle.
            if unsafe { AssignProcessToJobObject(self.raw(), process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            resume_main_thread(child.id())
        }

        pub(super) fn terminate(&self, _child: &mut Child) -> io::Result<()> {
            // SAFETY: self owns a live Job handle; exit code is an internal failure marker.
            if unsafe { TerminateJobObject(self.raw(), 1) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        fn raw(&self) -> HANDLE {
            self.handle.raw()
        }
    }

    struct OwnedHandle(NonNull<c_void>);

    impl OwnedHandle {
        fn from_nullable(handle: HANDLE) -> io::Result<Self> {
            NonNull::new(handle)
                .map(Self)
                .ok_or_else(io::Error::last_os_error)
        }

        fn from_snapshot(handle: HANDLE) -> io::Result<Self> {
            if handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Self::from_nullable(handle)
            }
        }

        fn raw(&self) -> HANDLE {
            self.0.as_ptr()
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle is uniquely owned and closed exactly once here.
            let _ = unsafe { CloseHandle(self.raw()) };
        }
    }

    fn resume_main_thread(process_id: u32) -> io::Result<()> {
        // SAFETY: flags request a read-only snapshot; the process id is ignored for thread scans.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        let snapshot = OwnedHandle::from_snapshot(snapshot)?;
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        // SAFETY: entry points to writable storage with the required dwSize initialized.
        if unsafe { Thread32First(snapshot.raw(), &raw mut entry) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let thread_id = loop {
            if entry.th32OwnerProcessID == process_id {
                break entry.th32ThreadID;
            }
            // SAFETY: entry remains valid writable storage for the live snapshot.
            if unsafe { Thread32Next(snapshot.raw(), &raw mut entry) } == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "suspended local process thread was not found",
                ));
            }
        };
        // SAFETY: the thread id came from the snapshot; the handle is non-inheritable.
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
        let thread = OwnedHandle::from_nullable(thread)?;
        // SAFETY: the handle grants suspend/resume access to the suspended primary thread.
        let previous_suspend_count = unsafe { ResumeThread(thread.raw()) };
        if previous_suspend_count == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        if previous_suspend_count != 1 {
            return Err(io::Error::other(
                "local process did not have exactly one suspended primary thread",
            ));
        }
        Ok(())
    }
}
