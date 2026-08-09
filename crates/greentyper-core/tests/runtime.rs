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
fn provider_error_details_never_enter_the_runtime_ledger() {
    const SECRET: &str = "https://provider.test/?token=private-token";
    let path = temp_path("provider-error-redaction");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    assert!(matches!(
        runtime.execute(&ConfigLayers::default(), "input", &mut UnavailableProvider,),
        Err(RuntimeError::Provider(ProviderError::Unavailable { .. }))
    ));
    let RecoveryStatus::Blocked { reason, .. } = runtime.snapshot().status else {
        panic!("Provider error did not block the Turn");
    };
    assert_eq!(reason, "Provider became unavailable");
    drop(runtime);

    let bytes = fs::read(&path).expect("read Runtime Ledger");
    assert!(
        !bytes
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes())
    );
    let recovered = RuntimeKernel::open(&path).expect("replay Runtime Ledger");
    assert!(matches!(
        recovered.snapshot().status,
        RecoveryStatus::Blocked { ref reason, .. }
            if reason == "Provider became unavailable"
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
                schema: 3,
                kind: 1,
                payload: 1_u64.to_le_bytes().to_vec(),
            }],
        )
        .expect("append unsupported Runtime Event");
    drop(ledger);
    assert!(matches!(
        RuntimeKernel::open(&path),
        Err(RuntimeError::UnsupportedRuntimeEventSchema {
            supported: 2,
            actual: 3
        })
    ));
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn schema_one_runtime_turn_replays_and_can_continue_with_schema_two() {
    let path = temp_path("schema-one-replay");
    let layers = ConfigLayers::default();
    let config = greentyper_core::config::ConfigEpoch::freeze(
        greentyper_core::model::ConfigEpochId::new(1).expect("Config Epoch id"),
        &layers,
    )
    .expect("freeze Config");
    let profile = config.resolved().provider_profile().value();
    let model = config.resolved().provider_model().value();
    let mut config_payload = Vec::new();
    push_u64(&mut config_payload, config.id().get());
    push_u64(&mut config_payload, config.fingerprint());
    push_string(&mut config_payload, profile);
    config_payload.push(1);
    push_string(&mut config_payload, model);
    config_payload.push(1);
    push_u32(
        &mut config_payload,
        *config.resolved().max_output_bytes().value(),
    );
    config_payload.push(1);

    let legacy_text = "legacy output";
    let legacy = vec![
        EventData {
            schema: 1,
            kind: 1,
            payload: 1_u64.to_le_bytes().to_vec(),
        },
        EventData {
            schema: 1,
            kind: 2,
            payload: config_payload,
        },
        EventData {
            schema: 1,
            kind: 3,
            payload: encoded(|payload| {
                push_u64(payload, 1);
                push_string(payload, profile);
                push_string(payload, model);
            }),
        },
        EventData {
            schema: 1,
            kind: 4,
            payload: encoded(|payload| {
                for value in [1, 1, 1, 1, 1] {
                    push_u64(payload, value);
                }
                push_string(payload, "legacy input");
            }),
        },
        EventData {
            schema: 1,
            kind: 5,
            payload: encoded(|payload| {
                push_u64(payload, 1);
                push_u64(payload, 2);
            }),
        },
        EventData {
            schema: 1,
            kind: 6,
            payload: encoded(|payload| {
                push_u64(payload, 1);
                push_u64(payload, 2);
                push_string(payload, legacy_text);
            }),
        },
        EventData {
            schema: 1,
            kind: 7,
            payload: encoded(|payload| {
                push_u64(payload, 1);
                push_u64(payload, 2);
                push_u64(payload, 1);
                push_string(payload, legacy_text);
                push_u32(payload, 2);
                push_u32(payload, 3);
            }),
        },
        EventData {
            schema: 1,
            kind: 8,
            payload: encoded(|payload| {
                push_u64(payload, 1);
                push_u64(payload, 1);
            }),
        },
        EventData {
            schema: 1,
            kind: 9,
            payload: 1_u64.to_le_bytes().to_vec(),
        },
    ];
    let (mut ledger, _) = FileLedger::open(&path).expect("open legacy Ledger");
    ledger
        .append(LedgerHead::default(), &legacy)
        .expect("append schema one Turn");
    drop(ledger);

    let mut runtime = RuntimeKernel::open(&path).expect("replay schema one Turn");
    assert_eq!(runtime.snapshot().items[1].text(), legacy_text);
    let mut provider = DeterministicProvider::default();
    let output = runtime
        .execute(&layers, "current input", &mut provider)
        .expect("write schema two Turn");
    runtime
        .acknowledge(output.delivery())
        .expect("acknowledge schema two Turn");
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("replay mixed schema Turns");
    assert_eq!(recovered.snapshot().items.len(), 4);
    assert_eq!(recovered.snapshot().items[1].text(), legacy_text);
    assert_eq!(
        recovered.snapshot().items[3].text(),
        "simulated: current input"
    );
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime Ledger");
}

fn encoded(write: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut payload = Vec::new();
    write(&mut payload);
    payload
}

fn push_u32(payload: &mut Vec<u8>, value: u32) {
    payload.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(payload: &mut Vec<u8>, value: u64) {
    payload.extend_from_slice(&value.to_le_bytes());
}

fn push_string(payload: &mut Vec<u8>, value: &str) {
    push_u32(
        payload,
        u32::try_from(value.len()).expect("fixture string length"),
    );
    payload.extend_from_slice(value.as_bytes());
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

struct UnavailableProvider;

impl ProviderRuntime for UnavailableProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        Err(ProviderError::unavailable(
            "https://provider.test/?token=private-token",
        ))
    }
}
