use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use greentyper_core::runtime::RuntimeKernel;
use greentyper_core::tool_runtime::ToolCallStatus;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
const FAST_ABORT_LIMIT: Duration = Duration::from_millis(1_500);
const CRASH_MARKER_TIMEOUT: Duration = Duration::from_secs(5);

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

fn product_binary(run: &TempRun) -> Command {
    let mut command = binary();
    command
        .env("HOME", run.path())
        .env("APPDATA", run.path())
        .env("XDG_CONFIG_HOME", run.path());
    command
}

fn sidecar(path: &Path, kind: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_owned();
    sidecar.push(".");
    sidecar.push(kind);
    PathBuf::from(sidecar)
}

fn crash_after_effect_prepared(run: &TempRun) -> PathBuf {
    let ledger = run.ledger("runtime");
    let marker = run.path().join("effect-prepared");
    let mut child = binary()
        .arg("__local-process-smoke")
        .arg("--run-dir")
        .arg(run.path())
        .arg("--scenario")
        .arg("prepared-crash")
        .arg("--message")
        .arg("must not complete")
        .spawn()
        .expect("start prepared-effect crash child");
    let deadline = Instant::now() + CRASH_MARKER_TIMEOUT;
    loop {
        if marker.exists() {
            break;
        }
        if let Some(status) = child.try_wait().expect("poll crash child") {
            panic!("crash child exited before EffectPrepared marker: {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for EffectPrepared marker");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("kill prepared-effect child");
    let status = child.wait().expect("reap prepared-effect child");
    assert!(!status.success(), "forced crash must not report success");
    ledger
}

#[test]
fn tool_status_is_read_only_and_fails_closed_on_incomplete_sidecars() {
    let run = TempRun::new("tool-status-read-only");
    let ledger = run.ledger("missing-runtime");

    let empty = product_binary(&run)
        .args(["tool", "status", "--ledger"])
        .arg(&ledger)
        .output()
        .expect("inspect missing Product Tool state");
    assert!(empty.status.success(), "{empty:?}");
    let empty_json: serde_json::Value =
        serde_json::from_slice(&empty.stdout).expect("empty Tool status JSON");
    assert_eq!(empty_json["calls"].as_array().map(Vec::len), Some(0));
    assert!(!ledger.exists());
    assert!(!sidecar(&ledger, "team").exists());
    assert!(!sidecar(&ledger, "tool").exists());

    let unavailable = product_binary(&run)
        .args(["tool", "reconcile", "--ledger"])
        .arg(&ledger)
        .args(["--call", "1", "--failed"])
        .output()
        .expect("reject missing Product Tool state reconciliation");
    assert!(!unavailable.status.success(), "{unavailable:?}");
    assert!(unavailable.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&unavailable.stderr).contains("Product Tool state is unavailable"),
        "{unavailable:?}"
    );
    assert!(!ledger.exists());
    assert!(!sidecar(&ledger, "team").exists());
    assert!(!sidecar(&ledger, "tool").exists());

    fs::write(sidecar(&ledger, "team"), b"incomplete").expect("write incomplete Team sidecar");
    let incomplete = product_binary(&run)
        .args(["tool", "status", "--ledger"])
        .arg(&ledger)
        .output()
        .expect("reject incomplete Product Tool state");
    assert!(!incomplete.status.success(), "{incomplete:?}");
    assert!(incomplete.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&incomplete.stderr)
            .contains("Product driver sidecar state is incomplete"),
        "{incomplete:?}"
    );
    assert!(!ledger.exists());
    assert!(!sidecar(&ledger, "tool").exists());
}

#[test]
fn prepared_effect_process_death_requires_explicit_product_reconciliation() {
    let run = TempRun::new("prepared-crash-reconcile");
    let ledger = crash_after_effect_prepared(&run);

    let status = product_binary(&run)
        .args(["tool", "status", "--ledger"])
        .arg(&ledger)
        .output()
        .expect("inspect prepared Tool effect");
    assert!(status.status.success(), "{status:?}");
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("Tool status JSON");
    assert_eq!(status_json["calls"].as_array().map(Vec::len), Some(1));
    assert_eq!(status_json["calls"][0]["call"], 1);
    assert_eq!(status_json["calls"][0]["status"], "reconciliation_required");
    assert_eq!(
        status_json["calls"][0]["result_sha256"],
        serde_json::Value::Null
    );

    let blocked = product_binary(&run)
        .args(["headless", "--ledger"])
        .arg(&ledger)
        .args(["--input", "must remain blocked"])
        .output()
        .expect("run blocked Product Turn");
    assert!(!blocked.status.success(), "{blocked:?}");
    assert!(blocked.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("Tool call 1 requires reconciliation"),
        "{blocked:?}"
    );

    let reconciled = product_binary(&run)
        .args(["tool", "reconcile", "--ledger"])
        .arg(&ledger)
        .args(["--call", "1", "--failed"])
        .output()
        .expect("reconcile prepared Tool effect");
    assert!(reconciled.status.success(), "{reconciled:?}");
    let reconciled_json: serde_json::Value =
        serde_json::from_slice(&reconciled.stdout).expect("Tool reconciliation JSON");
    assert_eq!(reconciled_json["call"], 1);
    assert_eq!(reconciled_json["status"], "failed");

    let resumed = product_binary(&run)
        .args(["headless", "--ledger"])
        .arg(&ledger)
        .args(["--input", "after reconciliation"])
        .output()
        .expect("run Product Turn after reconciliation");
    assert!(resumed.status.success(), "{resumed:?}");
    assert_eq!(resumed.stdout, b"simulated: after reconciliation\n");
    assert!(sidecar(&ledger, "team").exists());
    assert!(sidecar(&ledger, "tool").exists());
}

#[test]
fn prepared_effect_can_be_reconciled_as_observed_success_without_reexecution() {
    let run = TempRun::new("prepared-crash-succeeded");
    let ledger = crash_after_effect_prepared(&run);
    let digest = "ab".repeat(32);

    let reconciled = product_binary(&run)
        .args(["tool", "reconcile", "--ledger"])
        .arg(&ledger)
        .args(["--call", "1", "--succeeded-digest", &digest])
        .output()
        .expect("reconcile observed Tool success");
    assert!(reconciled.status.success(), "{reconciled:?}");
    let reconciled_json: serde_json::Value =
        serde_json::from_slice(&reconciled.stdout).expect("Tool reconciliation JSON");
    assert_eq!(reconciled_json["call"], 1);
    assert_eq!(reconciled_json["status"], "succeeded");
    assert_eq!(reconciled_json["result_sha256"], digest);

    let conflicting_repeat = product_binary(&run)
        .args(["tool", "reconcile", "--ledger"])
        .arg(&ledger)
        .args(["--call", "1", "--failed"])
        .output()
        .expect("repeat reconciliation after terminal success");
    assert!(
        conflicting_repeat.status.success(),
        "{conflicting_repeat:?}"
    );
    let repeat_json: serde_json::Value =
        serde_json::from_slice(&conflicting_repeat.stdout).expect("repeat reconciliation JSON");
    assert_eq!(repeat_json["status"], "succeeded");
    assert_eq!(repeat_json["result_sha256"], digest);

    let status = product_binary(&run)
        .args(["tool", "status", "--ledger"])
        .arg(&ledger)
        .output()
        .expect("inspect reconciled Tool success");
    assert!(status.status.success(), "{status:?}");
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("Tool status JSON");
    assert_eq!(status_json["calls"][0]["status"], "succeeded");
    assert_eq!(status_json["calls"][0]["result_sha256"], digest);
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
