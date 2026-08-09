use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use greentyper_core::runtime::{RecoveryStatus, RuntimeKernel};

static NEXT_LEDGER: AtomicU64 = AtomicU64::new(1);
const FAST_ABORT_LIMIT: Duration = Duration::from_millis(1_500);
const PRIVATE_ERROR_MARKER: &[u8] = b"provider-private-error-marker";

struct TempLedger {
    path: PathBuf,
}

impl TempLedger {
    fn new(name: &str) -> Self {
        Self {
            path: temp_ledger_path(name),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempLedger {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn temp_ledger_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-provider-http-{name}-{}-{nonce}-{}.ledger",
        std::process::id(),
        NEXT_LEDGER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_greentyper"))
}

#[test]
fn loopback_responses_http_stream_finishes_through_the_runtime() {
    let ledger = TempLedger::new("success");
    let output = binary()
        .arg("__provider-http-smoke")
        .arg("--ledger")
        .arg(ledger.path())
        .arg("--scenario")
        .arg("success")
        .arg("--input")
        .arg("hello over HTTP")
        .output()
        .expect("run Provider HTTP smoke");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, "fixture network 中\n".as_bytes());

    let snapshot =
        RuntimeKernel::inspect(ledger.path()).expect("inspect Provider HTTP Runtime Ledger");
    assert_eq!(snapshot.status, RecoveryStatus::Ready);
    assert_eq!(
        snapshot.items.last().expect("assistant item").text(),
        "fixture network 中"
    );
}

#[test]
fn http_error_body_never_enters_diagnostics_or_the_runtime_ledger() {
    let ledger = TempLedger::new("http-error");
    let output = binary()
        .arg("__provider-http-smoke")
        .arg("--ledger")
        .arg(ledger.path())
        .arg("--scenario")
        .arg("http-error")
        .arg("--input")
        .arg("private request marker")
        .output()
        .expect("run Provider HTTP error smoke");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"provider-unavailable\n");
    assert!(
        !output
            .stderr
            .windows(PRIVATE_ERROR_MARKER.len())
            .any(|window| { window == PRIVATE_ERROR_MARKER })
    );
    let ledger_bytes = std::fs::read(ledger.path()).expect("read Provider HTTP Runtime Ledger");
    assert!(
        !ledger_bytes
            .windows(PRIVATE_ERROR_MARKER.len())
            .any(|window| window == PRIVATE_ERROR_MARKER)
    );
}

#[test]
fn stalled_responses_stream_times_out_without_hanging_the_runtime() {
    let ledger = TempLedger::new("timeout");
    let started = Instant::now();
    let output = binary()
        .arg("__provider-http-smoke")
        .arg("--ledger")
        .arg(ledger.path())
        .arg("--scenario")
        .arg("timeout")
        .arg("--input")
        .arg("timeout request")
        .output()
        .expect("run Provider HTTP timeout smoke");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"provider-unavailable\n");
    assert!(started.elapsed() < FAST_ABORT_LIMIT);
}
