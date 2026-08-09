use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::config::ConfigLayers;
use greentyper_core::provider::{
    DeterministicProvider, ProviderError, ProviderEvent, ProviderRequest, ProviderRuntime,
};
use greentyper_core::runtime::RuntimeKernel;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-headless-{name}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_greentyper"))
}

#[test]
fn status_of_an_unused_path_is_ready_and_read_only() {
    let path = temp_path("missing-status");
    let status = binary()
        .args(["status", "--ledger"])
        .arg(&path)
        .output()
        .expect("run status command");
    assert!(status.status.success(), "{status:?}");
    assert_eq!(status.stdout, b"ready\n");
    assert!(!path.exists());
}

#[test]
fn headless_command_outputs_then_durably_acknowledges() {
    let path = temp_path("happy");
    let output = binary()
        .args(["headless", "--ledger"])
        .arg(&path)
        .args(["--input", "hello"])
        .output()
        .expect("run headless command");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"simulated: hello\n");

    let status = binary()
        .args(["status", "--ledger"])
        .arg(&path)
        .output()
        .expect("run status command");
    assert!(status.status.success(), "{status:?}");
    assert_eq!(status.stdout, b"ready\n");
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn headless_refuses_to_repeat_prepared_unacknowledged_output() {
    let path = temp_path("reconcile");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let prepared = runtime
        .execute(&ConfigLayers::default(), "print once", &mut provider)
        .expect("prepare output");
    let delivery = prepared.delivery();
    drop(runtime);

    let blocked = binary()
        .args(["headless", "--ledger"])
        .arg(&path)
        .args(["--input", "must not run"])
        .output()
        .expect("run blocked headless command");
    assert!(!blocked.status.success());
    assert!(blocked.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("reconciliation-required"),
        "{blocked:?}"
    );

    let reconciled = binary()
        .args(["reconcile", "--ledger"])
        .arg(&path)
        .args(["--delivery", &delivery.get().to_string()])
        .output()
        .expect("run reconcile command");
    assert!(reconciled.status.success(), "{reconciled:?}");
    assert_eq!(reconciled.stdout, b"reconciled\n");
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn resume_command_continues_a_durably_admitted_turn() {
    let path = temp_path("resume");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut provider = PanicProvider;
        let _ = runtime.execute(&ConfigLayers::default(), "continue", &mut provider);
    }));
    assert!(result.is_err());
    drop(runtime);

    let resumed = binary()
        .args(["resume", "--ledger"])
        .arg(&path)
        .output()
        .expect("run resume command");
    assert!(resumed.status.success(), "{resumed:?}");
    assert_eq!(resumed.stdout, b"simulated: continue\n");
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

struct PanicProvider;

impl ProviderRuntime for PanicProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        panic!("injected crash after admission")
    }
}
