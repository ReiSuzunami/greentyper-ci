use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use greentyper_core::runtime::RuntimeKernel;
use greentyper_core::tool_runtime::ToolCallStatus;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
const FAST_ABORT_LIMIT: Duration = Duration::from_millis(1_500);

struct TempRun {
    root: PathBuf,
}

impl TempRun {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-local-process-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create local-process test directory");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn ledger(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.ledger"))
    }
}

impl Drop for TempRun {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove local-process test directory");
    }
}

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_greentyper"))
}

#[test]
fn approved_local_echo_runs_in_a_child_and_replays_its_digest() {
    let run = TempRun::new("echo");
    let output = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("echo")
        .arg("--message")
        .arg("hello from child")
        .output()
        .expect("run local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"hello from child\n");

    let (kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
        run.ledger("runtime"),
        run.ledger("team"),
        run.ledger("tool"),
        1,
    )
    .expect("reopen local-process Ledgers");
    assert_eq!(recovery.into_sessions().len(), 1);
    let snapshot = kernel.tool_snapshot().expect("Tool snapshot");
    assert_eq!(snapshot.calls.len(), 1);
    assert_eq!(snapshot.calls[0].status, ToolCallStatus::Succeeded);
    assert!(snapshot.calls[0].result_digest.is_some());
    drop(kernel);
}

#[test]
fn timed_out_child_requires_reconciliation_and_is_not_started_again() {
    let run = TempRun::new("timeout");
    let first = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("timeout")
        .arg("--message")
        .arg("never returned")
        .output()
        .expect("run timed-out local-process smoke");
    assert!(first.status.success(), "{first:?}");
    assert_eq!(first.stdout, b"reconciliation-required\n");

    let started = Instant::now();
    let second = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("timeout")
        .arg("--message")
        .arg("never returned")
        .output()
        .expect("reopen timed-out local-process smoke");
    assert!(second.status.success(), "{second:?}");
    assert_eq!(second.stdout, b"reconciliation-required-existing\n");
    assert!(started.elapsed() < FAST_ABORT_LIMIT);

    let (kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
        run.ledger("runtime"),
        run.ledger("team"),
        run.ledger("tool"),
        1,
    )
    .expect("reopen timed-out local-process Ledgers");
    assert_eq!(recovery.into_sessions().len(), 1);
    let snapshot = kernel.tool_snapshot().expect("Tool snapshot");
    assert_eq!(snapshot.calls.len(), 1);
    assert_eq!(
        snapshot.calls[0].status,
        ToolCallStatus::ReconciliationRequired
    );
    assert!(snapshot.calls[0].result_digest.is_none());
    drop(kernel);
}

#[test]
fn oversized_child_output_requires_reconciliation_and_is_not_replayed() {
    let run = TempRun::new("output-limit");
    let first = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("output-limit")
        .arg("--message")
        .arg("ignored")
        .output()
        .expect("run oversized-output local-process smoke");
    assert!(first.status.success(), "{first:?}");
    assert_eq!(first.stdout, b"reconciliation-required\n");

    let second = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("output-limit")
        .arg("--message")
        .arg("ignored")
        .output()
        .expect("reopen oversized-output local-process smoke");
    assert!(second.status.success(), "{second:?}");
    assert_eq!(second.stdout, b"reconciliation-required-existing\n");

    let (kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
        run.ledger("runtime"),
        run.ledger("team"),
        run.ledger("tool"),
        1,
    )
    .expect("reopen oversized-output local-process Ledgers");
    assert_eq!(recovery.into_sessions().len(), 1);
    let snapshot = kernel.tool_snapshot().expect("Tool snapshot");
    assert_eq!(snapshot.calls.len(), 1);
    assert_eq!(
        snapshot.calls[0].status,
        ToolCallStatus::ReconciliationRequired
    );
    assert!(snapshot.calls[0].result_digest.is_none());
    drop(kernel);
}

#[test]
fn output_flood_is_stopped_as_soon_as_the_combined_limit_is_crossed() {
    let run = TempRun::new("output-flood");
    let started = Instant::now();
    let output = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("output-flood")
        .arg("--message")
        .arg("ignored")
        .output()
        .expect("run output-flood local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"reconciliation-required\n");
    assert!(started.elapsed() < FAST_ABORT_LIMIT);
}

#[cfg(unix)]
#[test]
fn timeout_kills_the_entire_unix_process_group() {
    let run = TempRun::new("unix-process-group");
    let started = Instant::now();
    let output = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("descendant-timeout")
        .arg("--message")
        .arg("ignored")
        .output()
        .expect("run Unix process-group local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"reconciliation-required\n");
    assert!(started.elapsed() < FAST_ABORT_LIMIT);
}

#[cfg(unix)]
#[test]
fn blocked_stdin_write_cannot_bypass_the_process_deadline() {
    let run = TempRun::new("blocked-stdin");
    let started = Instant::now();
    let output = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("blocked-stdin")
        .arg("--message")
        .arg("ignored")
        .output()
        .expect("run blocked-stdin local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"reconciliation-required\n");
    assert!(started.elapsed() < FAST_ABORT_LIMIT);
}

#[test]
fn nonzero_child_exit_is_a_durable_failure_and_is_not_replayed() {
    let run = TempRun::new("nonzero-exit");
    for expected in [b"failed\n".as_slice(), b"failed\n".as_slice()] {
        let output = binary()
            .arg("__local-process-smoke")
            .arg("--run-dir")
            .arg(run.path())
            .arg("--scenario")
            .arg("nonzero-exit")
            .arg("--message")
            .arg("ignored")
            .output()
            .expect("run failed local-process smoke");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected);
    }

    let (kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
        run.ledger("runtime"),
        run.ledger("team"),
        run.ledger("tool"),
        1,
    )
    .expect("reopen failed local-process Ledgers");
    assert_eq!(recovery.into_sessions().len(), 1);
    let snapshot = kernel.tool_snapshot().expect("Tool snapshot");
    assert_eq!(snapshot.calls.len(), 1);
    assert_eq!(snapshot.calls[0].status, ToolCallStatus::Failed);
    assert!(snapshot.calls[0].result_digest.is_none());
    assert_eq!(
        snapshot.calls[0].reason.as_deref(),
        Some("Tool execution failed")
    );
    drop(kernel);
}

#[test]
fn spawn_failure_is_known_failed_instead_of_ambiguous() {
    let run = TempRun::new("spawn-failure");
    let output = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("spawn-failure")
        .arg("--message")
        .arg("must not execute")
        .output()
        .expect("run spawn-failure local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"failed\n");

    let (kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
        run.ledger("runtime"),
        run.ledger("team"),
        run.ledger("tool"),
        1,
    )
    .expect("reopen spawn-failure local-process Ledgers");
    assert_eq!(recovery.into_sessions().len(), 1);
    let snapshot = kernel.tool_snapshot().expect("Tool snapshot");
    assert_eq!(snapshot.calls.len(), 1);
    assert_eq!(snapshot.calls[0].status, ToolCallStatus::Failed);
    drop(kernel);
}

#[test]
fn local_child_does_not_inherit_the_parent_environment() {
    let run = TempRun::new("environment");
    let output = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("environment")
        .arg("--message")
        .arg("ignored")
        .env(
            "GREENTYPER_LOCAL_PROCESS_SECRET",
            "confidential-parent-value",
        )
        .output()
        .expect("run environment-isolation local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"unset\n");

    let ledger = fs::read(run.ledger("tool")).expect("read Tool Ledger");
    assert!(
        !ledger
            .windows(b"confidential-parent-value".len())
            .any(|window| window == b"confidential-parent-value")
    );
}

#[test]
fn local_child_does_not_inherit_the_parent_working_directory() {
    let run = TempRun::new("working-directory");
    let output = binary()
        .current_dir(run.path())
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("working-directory")
        .arg("--message")
        .arg(run.path())
        .output()
        .expect("run working-directory local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"detached\n");
}

#[test]
fn local_echo_fails_closed_when_network_authority_is_requested() {
    let run = TempRun::new("network-denied");
    let output = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("network-denied")
        .arg("--message")
        .arg("ignored")
        .output()
        .expect("run network-denied local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"failed\n");

    let (kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
        run.ledger("runtime"),
        run.ledger("team"),
        run.ledger("tool"),
        1,
    )
    .expect("reopen network-denied local-process Ledgers");
    assert_eq!(recovery.into_sessions().len(), 1);
    let snapshot = kernel.tool_snapshot().expect("Tool snapshot");
    assert_eq!(snapshot.calls.len(), 1);
    assert_eq!(snapshot.calls[0].status, ToolCallStatus::Failed);
    assert_eq!(snapshot.calls[0].resource_binding.network_target_count(), 1);
    drop(kernel);

    let ledger = fs::read(run.ledger("tool")).expect("read Tool Ledger");
    assert!(
        !ledger
            .windows(b"https://private.invalid".len())
            .any(|window| window == b"https://private.invalid")
    );
}

#[test]
fn local_echo_rejects_unsupported_authority_and_argument_shapes() {
    for scenario in [
        "filesystem-read-denied",
        "filesystem-write-denied",
        "process-mismatch",
        "invalid-arguments",
    ] {
        let run = TempRun::new(scenario);
        let output = binary()
            .arg("__local-process-smoke")
            .arg("--run-dir")
            .arg(run.path())
            .arg("--scenario")
            .arg(scenario)
            .arg("--message")
            .arg("must not execute")
            .output()
            .expect("run rejected local-process smoke");
        assert!(output.status.success(), "{scenario}: {output:?}");
        assert_eq!(output.stdout, b"failed\n", "{scenario}");

        let (kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
            run.ledger("runtime"),
            run.ledger("team"),
            run.ledger("tool"),
            1,
        )
        .expect("reopen rejected local-process Ledgers");
        assert_eq!(recovery.into_sessions().len(), 1);
        let snapshot = kernel.tool_snapshot().expect("Tool snapshot");
        assert_eq!(snapshot.calls.len(), 1);
        assert_eq!(snapshot.calls[0].status, ToolCallStatus::Failed);
        assert!(snapshot.calls[0].result_digest.is_none());
        drop(kernel);
    }
}

#[cfg(windows)]
#[test]
fn windows_job_assigns_before_execution_and_denies_descendants() {
    let run = TempRun::new("windows-descendant");
    let output = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("descendant-denied")
        .arg("--message")
        .arg("ignored")
        .output()
        .expect("run Windows Job local-process smoke");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"descendant-denied\n");
}
