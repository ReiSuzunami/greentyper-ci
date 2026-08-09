use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::config::ConfigLayers;
use greentyper_core::ledger::{EventData, FileLedger, LedgerHead};
use greentyper_core::provider::{
    DeterministicProvider, ProviderError, ProviderEvent, ProviderRequest, ProviderRuntime,
    UsageRecord,
};
use greentyper_core::runtime::{AcknowledgeOutcome, RecoveryStatus, RuntimeError, RuntimeKernel};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-runtime-{name}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn prepared_output_is_acknowledged_once_and_replays_ready() {
    let path = temp_path("happy");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let output = runtime
        .execute(&ConfigLayers::default(), "hello", &mut provider)
        .expect("prepare output");
    assert_eq!(output.text(), "simulated: hello");
    assert!(matches!(
        runtime.snapshot().status,
        RecoveryStatus::ReconciliationRequired { .. }
    ));
    assert!(matches!(
        runtime
            .acknowledge(output.delivery())
            .expect("acknowledge output"),
        AcknowledgeOutcome::Durable(_)
    ));
    let acknowledged_head = runtime.snapshot().head;
    assert_eq!(
        runtime
            .acknowledge(output.delivery())
            .expect("duplicate acknowledgement is idempotent"),
        AcknowledgeOutcome::AlreadyAcknowledged
    );
    assert_eq!(runtime.snapshot().head, acknowledged_head);
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("recover Runtime");
    let snapshot = recovered.snapshot();
    assert_eq!(snapshot.status, RecoveryStatus::Ready);
    assert_eq!(snapshot.items.len(), 2);
    assert_eq!(snapshot.items[0].text(), "hello");
    assert_eq!(snapshot.items[1].text(), "simulated: hello");
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn prepared_but_unacknowledged_output_is_never_reemitted_or_rerun() {
    let path = temp_path("reconcile");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let output = runtime
        .execute(&ConfigLayers::default(), "once", &mut provider)
        .expect("prepare output");
    let delivery = output.delivery();
    drop(runtime);

    let mut recovered = RuntimeKernel::open(&path).expect("recover Runtime");
    assert!(matches!(
        recovered.snapshot().status,
        RecoveryStatus::ReconciliationRequired {
            delivery: actual,
            ..
        } if actual == delivery
    ));
    let mut counting = CountingProvider::default();
    assert!(matches!(
        recovered.execute(&ConfigLayers::default(), "again", &mut counting),
        Err(RuntimeError::Busy(
            RecoveryStatus::ReconciliationRequired { .. }
        ))
    ));
    assert_eq!(counting.calls, 0);
    recovered
        .acknowledge(delivery)
        .expect("explicit reconciliation acknowledgement");
    assert_eq!(recovered.snapshot().status, RecoveryStatus::Ready);
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn crash_after_admission_requires_explicit_resume() {
    let path = temp_path("resume");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut provider = PanicProvider;
        let _ = runtime.execute(&ConfigLayers::default(), "resume me", &mut provider);
    }));
    assert!(result.is_err());
    drop(runtime);

    let mut recovered = RuntimeKernel::open(&path).expect("recover Runtime");
    assert!(matches!(
        recovered.snapshot().status,
        RecoveryStatus::ResumeRequired { .. }
    ));
    let mut provider = DeterministicProvider::default();
    let output = recovered
        .resume(&mut provider)
        .expect("resume provider Turn");
    assert_eq!(output.text(), "simulated: resume me");
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn malformed_provider_output_is_durably_blocked() {
    let path = temp_path("blocked");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut provider = CompletedOnlyProvider;
    assert!(matches!(
        runtime.execute(&ConfigLayers::default(), "input", &mut provider),
        Err(RuntimeError::InvalidProviderOutput(_))
    ));
    assert!(matches!(
        runtime.snapshot().status,
        RecoveryStatus::Blocked { .. }
    ));
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("recover Runtime");
    assert!(matches!(
        recovered.snapshot().status,
        RecoveryStatus::Blocked { .. }
    ));
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn unsupported_runtime_event_schema_fails_closed() {
    let path = temp_path("unsupported-event");
    let (mut ledger, _) = FileLedger::open(&path).expect("open Ledger");
    ledger
        .append(
            LedgerHead::default(),
            &[EventData {
                schema: 2,
                kind: 1,
                payload: 1_u64.to_le_bytes().to_vec(),
            }],
        )
        .expect("append unsupported Runtime Event");
    drop(ledger);
    assert!(matches!(
        RuntimeKernel::open(&path),
        Err(RuntimeError::UnsupportedRuntimeEventSchema {
            supported: 1,
            actual: 2
        })
    ));
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn invalid_runtime_transition_in_a_valid_frame_fails_closed() {
    let path = temp_path("invalid-transition");
    let (mut ledger, _) = FileLedger::open(&path).expect("open Ledger");
    ledger
        .append(
            LedgerHead::default(),
            &[EventData {
                schema: 1,
                kind: 9,
                payload: 1_u64.to_le_bytes().to_vec(),
            }],
        )
        .expect("append invalid transition");
    drop(ledger);
    assert!(matches!(
        RuntimeKernel::open(&path),
        Err(RuntimeError::CorruptState(_))
    ));
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[derive(Default)]
struct CountingProvider {
    calls: usize,
}

impl ProviderRuntime for CountingProvider {
    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.calls += 1;
        DeterministicProvider::default().run(request)
    }
}

struct PanicProvider;

impl ProviderRuntime for PanicProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        panic!("injected crash after durable admission")
    }
}

struct CompletedOnlyProvider;

impl ProviderRuntime for CompletedOnlyProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        Ok(vec![ProviderEvent::Completed(UsageRecord::default())])
    }
}
