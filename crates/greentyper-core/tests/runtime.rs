use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::config::{ConfigDocument, ConfigLayers, ConfigPaths, ConfigRuntime};
use greentyper_core::context::{
    ContextPressure, ContextPressureAccuracy, ContextPressureInput, ContextPressurePolicy,
    ContextReductionPolicy,
};
use greentyper_core::ledger::{EventData, FileLedger, LedgerHead};
use greentyper_core::provider::{
    DeterministicProvider, ProviderError, ProviderEvent, ProviderProfileSnapshot, ProviderRequest,
    ProviderRuntime, ProviderUnavailableStage, UsageAccuracy, UsageRecord,
};
use greentyper_core::runtime::{
    AcknowledgeOutcome, CancelTurnOutcome, RecoveryStatus, RuntimeError, RuntimeKernel,
};
use greentyper_core::usage::{UsageAttemptOutcome, UsageCostProvenance, UsageTimestamp};

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
fn context_checkpoint_publishes_at_a_safe_barrier_and_replays() {
    let path = temp_path("context-checkpoint-replay");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let output = runtime
        .execute(&ConfigLayers::default(), "checkpoint me", &mut provider)
        .expect("prepare output");
    runtime
        .acknowledge(output.delivery())
        .expect("complete Turn");

    let source_head = runtime.snapshot().head;
    let draft = runtime
        .prepare_context_checkpoint(ContextReductionPolicy::new(64, 1).expect("policy"))
        .expect("prepare checkpoint");
    assert_eq!(draft.source().head(), source_head);
    assert_eq!(draft.view().artifacts().len(), 1);
    assert_eq!(draft.view().recent_items().len(), 1);

    let receipt = runtime
        .publish_context_checkpoint(draft)
        .expect("publish checkpoint");
    assert_eq!(receipt.event_count, 1);
    let published = runtime
        .context_checkpoint()
        .cloned()
        .expect("published checkpoint");
    assert_eq!(published.source().head(), source_head);
    assert_eq!(published.view().artifacts().len(), 1);
    assert_eq!(published.view().recent_items().len(), 1);
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("reopen Runtime");
    let replayed = recovered
        .context_checkpoint()
        .cloned()
        .expect("replayed checkpoint");
    assert_eq!(replayed, published);
    assert_eq!(replayed.source().head(), source_head);
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn context_checkpoint_rejects_stale_source_without_mutating_the_ledger() {
    let path = temp_path("context-checkpoint-stale");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let first = runtime
        .execute(&ConfigLayers::default(), "first", &mut provider)
        .expect("prepare first output");
    runtime
        .acknowledge(first.delivery())
        .expect("complete first Turn");
    let stale = runtime
        .prepare_context_checkpoint(ContextReductionPolicy::new(64, 1).expect("policy"))
        .expect("prepare stale checkpoint");

    let second = runtime
        .execute(&ConfigLayers::default(), "second", &mut provider)
        .expect("prepare second output");
    runtime
        .acknowledge(second.delivery())
        .expect("complete second Turn");
    let before = runtime.snapshot();
    let bytes_before = fs::read(&path).expect("read Runtime ledger");

    assert!(matches!(
        runtime.publish_context_checkpoint(stale),
        Err(RuntimeError::StaleContextCheckpoint { expected, actual })
            if expected != actual && actual == before.head
    ));
    assert_eq!(runtime.snapshot(), before);
    assert_eq!(
        fs::read(&path).expect("read unchanged Runtime ledger"),
        bytes_before
    );
    drop(runtime);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn context_checkpoint_requires_a_safe_barrier_without_mutating_state() {
    let path = temp_path("context-checkpoint-barrier");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let _output = runtime
        .execute(
            &ConfigLayers::default(),
            "awaiting acknowledgement",
            &mut provider,
        )
        .expect("prepare output");
    let before = runtime.snapshot();

    assert!(matches!(
        runtime.prepare_context_checkpoint(ContextReductionPolicy::new(64, 1).expect("policy")),
        Err(RuntimeError::Busy(
            RecoveryStatus::ReconciliationRequired { .. }
        ))
    ));
    assert_eq!(runtime.snapshot(), before);
    drop(runtime);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn hard_context_pressure_stops_admission_before_ledger_or_provider_effects() {
    let path = temp_path("context-pressure-hard");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let initial = runtime.snapshot();
    let pressure = ContextPressure::project(
        ContextPressureInput::known(1_000, 800, 100, ContextPressureAccuracy::Exact),
        ContextPressurePolicy::default(),
    )
    .expect("hard Context Pressure");
    let mut provider = CountingProvider::default();

    assert!(matches!(
        runtime.execute_with_context_pressure(
            &ConfigLayers::default(),
            pressure,
            "must not be admitted",
            &mut provider,
        ),
        Err(RuntimeError::ContextAdmissionBlocked { pressure: actual }) if actual == pressure
    ));
    assert_eq!(provider.calls, 0);
    assert_eq!(runtime.snapshot(), initial);
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("reopen unchanged Runtime");
    assert_eq!(recovered.snapshot(), initial);
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn soft_and_unknown_context_pressure_preserve_existing_admission() {
    let projections = [
        ContextPressure::project(
            ContextPressureInput::known(1_000, 550, 100, ContextPressureAccuracy::Exact),
            ContextPressurePolicy::default(),
        )
        .expect("soft Context Pressure"),
        ContextPressure::project(
            ContextPressureInput::new(
                None,
                Some(550),
                Some(100),
                Some(ContextPressureAccuracy::Estimated),
            ),
            ContextPressurePolicy::default(),
        )
        .expect("unknown Context Pressure"),
    ];

    for (index, pressure) in projections.into_iter().enumerate() {
        let path = temp_path(&format!("context-pressure-admit-{index}"));
        let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
        let mut provider = CountingProvider::default();
        let output = runtime
            .execute_with_context_pressure(
                &ConfigLayers::default(),
                pressure,
                "admitted",
                &mut provider,
            )
            .expect("soft and unknown pressure preserve admission");
        assert_eq!(provider.calls, 1);
        runtime
            .acknowledge(output.delivery())
            .expect("acknowledge output");
        drop(runtime);
        fs::remove_file(path).expect("cleanup Runtime ledger");
    }
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
    let usage = recovered.usage_snapshot(UsageTimestamp::now().unwrap());
    assert_eq!(usage.attempts().len(), 2);
    assert_eq!(
        usage.attempts()[0].outcome(),
        UsageAttemptOutcome::Interrupted
    );
    assert_eq!(
        usage.attempts()[1].outcome(),
        UsageAttemptOutcome::Succeeded
    );
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn output_token_limit_survives_admission_crash_and_resume() {
    let path = temp_path("output-token-resume");
    let mut layers = ConfigLayers::default();
    layers.cli.max_output_tokens = Some(4_096);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
        let mut provider = PanicProvider;
        let _ = runtime.execute(&layers, "resume bounded", &mut provider);
    }));
    assert!(result.is_err());

    layers.cli.max_output_tokens = Some(8_192);
    let mut recovered = RuntimeKernel::open(&path).expect("recover Runtime");
    let mut provider = OutputTokenProvider { seen: None };
    let output = recovered
        .resume(&mut provider)
        .expect("resume with frozen output-token limit");
    assert_eq!(provider.seen, Some(4_096));
    recovered
        .acknowledge(output.delivery())
        .expect("acknowledge resumed output");
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn provider_profile_snapshot_survives_recovery_and_rejects_mismatched_resume() {
    const CREDENTIAL_MATERIAL: &str = "credential-material-must-never-enter-ledger";
    let path = temp_path("provider-profile-recovery");
    let (layers, snapshot) = provider_profile_fixture("/responses");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
        let mut provider = SnapshotPanicProvider {
            snapshot: snapshot.clone(),
        };
        let _ = runtime.execute(&layers, "resume frozen profile", &mut provider);
    }));
    assert!(result.is_err());

    let bytes = fs::read(&path).expect("read Runtime Ledger");
    assert!(
        bytes
            .windows("edge-credential".len())
            .any(|window| { window == "edge-credential".as_bytes() })
    );
    assert!(
        !bytes
            .windows(CREDENTIAL_MATERIAL.len())
            .any(|window| { window == CREDENTIAL_MATERIAL.as_bytes() })
    );

    let (_, mismatched) = provider_profile_fixture("/different-responses");
    let mut runtime = RuntimeKernel::open(&path).expect("recover frozen Provider profile");
    assert_eq!(
        runtime
            .pending_provider_epoch()
            .and_then(|epoch| epoch.profile_snapshot()),
        Some(&snapshot)
    );
    let mut wrong = SnapshotProvider::new(mismatched);
    assert!(matches!(
        runtime.resume(&mut wrong),
        Err(RuntimeError::Provider(ProviderError::InvalidConfiguration(
            _
        )))
    ));
    assert_eq!(wrong.calls, 0);
    assert!(matches!(
        runtime.snapshot().status,
        RecoveryStatus::ResumeRequired { .. }
    ));

    let mut matching = SnapshotProvider::new(snapshot.clone());
    let output = runtime
        .resume(&mut matching)
        .expect("resume matching frozen Provider profile");
    assert_eq!(matching.calls, 1);
    assert_eq!(matching.seen.as_ref(), Some(&snapshot));
    runtime
        .acknowledge(output.delivery())
        .expect("acknowledge frozen Provider output");
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("replay frozen Provider profile");
    assert_eq!(recovered.snapshot().status, RecoveryStatus::Ready);
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn non_simulator_without_a_profile_snapshot_is_rejected_before_admission() {
    let path = temp_path("provider-profile-required");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let layers = ConfigLayers {
        cli: greentyper_core::config::ConfigLayer {
            provider_profile: Some("edge".to_owned()),
            provider_model: Some("fixture-model".to_owned()),
            ..greentyper_core::config::ConfigLayer::default()
        },
        ..ConfigLayers::default()
    };
    let mut provider = NoSnapshotProvider { calls: 0 };
    assert!(matches!(
        runtime.execute(&layers, "must not run", &mut provider),
        Err(RuntimeError::Provider(ProviderError::InvalidConfiguration(
            _
        )))
    ));
    assert_eq!(provider.calls, 0);
    assert_eq!(runtime.snapshot().status, RecoveryStatus::Ready);
    assert!(runtime.snapshot().items.is_empty());
    drop(runtime);

    let inspected = RuntimeKernel::inspect(&path).expect("inspect empty Runtime Ledger");
    assert_eq!(inspected.status, RecoveryStatus::Ready);
    assert!(inspected.items.is_empty());
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
    let RecoveryStatus::Blocked {
        turn,
        retryable: false,
        ..
    } = runtime.snapshot().status
    else {
        panic!("malformed Provider output was incorrectly retryable")
    };
    let blocked_head = runtime.snapshot().head;
    assert!(matches!(
        runtime.request_blocked_turn_retry(turn),
        Err(RuntimeError::TurnRetryNotAllowed(actual)) if actual == turn
    ));
    assert_eq!(runtime.snapshot().head, blocked_head);
    drop(runtime);

    let mut recovered = RuntimeKernel::open(&path).expect("recover Runtime");
    let RecoveryStatus::Blocked { turn, .. } = recovered.snapshot().status else {
        panic!("malformed Provider output did not replay as blocked")
    };
    assert!(matches!(
        recovered
            .cancel_blocked_turn(turn)
            .expect("cancel malformed Provider Turn"),
        CancelTurnOutcome::Durable(_)
    ));
    assert_eq!(recovered.snapshot().status, RecoveryStatus::Ready);
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
    let RecoveryStatus::Blocked {
        reason, retryable, ..
    } = runtime.snapshot().status
    else {
        panic!("Provider error did not block the Turn");
    };
    assert_eq!(reason, "Provider became unavailable before a response");
    assert!(retryable);
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
            if reason == "Provider became unavailable before a response"
    ));
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn retryable_provider_failure_is_durably_rearmed_and_records_a_new_attempt() {
    let path = temp_path("provider-retry");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut unavailable = UnavailableAtStageProvider {
        stage: ProviderUnavailableStage::BeforeFirstEvent,
    };
    assert!(matches!(
        runtime.execute(&ConfigLayers::default(), "retry input", &mut unavailable),
        Err(RuntimeError::Provider(ProviderError::Unavailable { .. }))
    ));
    let RecoveryStatus::Blocked {
        turn,
        retryable: true,
        ..
    } = runtime.snapshot().status
    else {
        panic!("early Provider failure was not marked retryable")
    };
    let frozen_provider = runtime
        .pending_provider_epoch()
        .expect("pending Provider Epoch")
        .clone();
    let blocked_head = runtime.snapshot().head;
    runtime
        .request_blocked_turn_retry(turn)
        .expect("durably request retry");
    assert_ne!(runtime.snapshot().head, blocked_head);
    assert_eq!(
        runtime.snapshot().status,
        RecoveryStatus::ResumeRequired { turn }
    );
    let retry_head = runtime.snapshot().head;
    assert!(matches!(
        runtime.request_blocked_turn_retry(turn),
        Err(RuntimeError::TurnRetryNotAllowed(actual)) if actual == turn
    ));
    assert_eq!(runtime.snapshot().head, retry_head);
    drop(runtime);

    let mut recovered = RuntimeKernel::open(&path).expect("recover requested retry");
    assert_eq!(recovered.pending_provider_epoch(), Some(&frozen_provider));
    assert_eq!(
        recovered.snapshot().status,
        RecoveryStatus::ResumeRequired { turn }
    );
    let mut provider = DeterministicProvider::default();
    let output = recovered.resume(&mut provider).expect("run explicit retry");
    assert_eq!(output.text(), "simulated: retry input");
    let usage = recovered.usage_snapshot(UsageTimestamp::now().unwrap());
    assert_eq!(usage.attempts().len(), 2);
    assert_eq!(usage.attempts()[0].outcome(), UsageAttemptOutcome::Failed);
    assert_eq!(
        usage.attempts()[1].outcome(),
        UsageAttemptOutcome::Succeeded
    );
    recovered
        .acknowledge(output.delivery())
        .expect("acknowledge retried output");
    assert_eq!(recovered.snapshot().status, RecoveryStatus::Ready);
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn partial_provider_failure_is_not_retryable_and_does_not_mutate_on_rejection() {
    let path = temp_path("provider-retry-partial");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut unavailable = UnavailableAtStageProvider {
        stage: ProviderUnavailableStage::AfterFirstEvent,
    };
    assert!(matches!(
        runtime.execute(&ConfigLayers::default(), "partial input", &mut unavailable),
        Err(RuntimeError::Provider(ProviderError::Unavailable { .. }))
    ));
    let RecoveryStatus::Blocked {
        turn,
        retryable: false,
        ..
    } = runtime.snapshot().status
    else {
        panic!("partial Provider failure was incorrectly retryable")
    };
    let blocked_head = runtime.snapshot().head;
    assert!(matches!(
        runtime.request_blocked_turn_retry(turn),
        Err(RuntimeError::TurnRetryNotAllowed(actual)) if actual == turn
    ));
    assert_eq!(runtime.snapshot().head, blocked_head);
    assert_eq!(
        runtime
            .usage_snapshot(UsageTimestamp::now().unwrap())
            .attempts()
            .len(),
        1
    );
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("recover partial Provider failure");
    assert!(matches!(
        recovered.snapshot().status,
        RecoveryStatus::Blocked {
            retryable: false,
            ..
        }
    ));
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn a_retry_that_fails_early_requires_another_explicit_retry_request() {
    let path = temp_path("provider-retry-again");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut first_failure = UnavailableAtStageProvider {
        stage: ProviderUnavailableStage::BeforeResponse,
    };
    assert!(matches!(
        runtime.execute(&ConfigLayers::default(), "retry twice", &mut first_failure,),
        Err(RuntimeError::Provider(ProviderError::Unavailable { .. }))
    ));
    let RecoveryStatus::Blocked {
        turn,
        retryable: true,
        ..
    } = runtime.snapshot().status
    else {
        panic!("first early failure was not retryable")
    };
    runtime
        .request_blocked_turn_retry(turn)
        .expect("request first retry");
    let mut second_failure = UnavailableAtStageProvider {
        stage: ProviderUnavailableStage::BeforeFirstEvent,
    };
    assert!(matches!(
        runtime.resume(&mut second_failure),
        Err(RuntimeError::Provider(ProviderError::Unavailable { .. }))
    ));
    let RecoveryStatus::Blocked {
        turn: blocked_turn,
        retryable: true,
        ..
    } = runtime.snapshot().status
    else {
        panic!("second early failure was not blocked for explicit retry")
    };
    assert_eq!(blocked_turn, turn);
    let usage = runtime.usage_snapshot(UsageTimestamp::now().unwrap());
    assert_eq!(usage.attempts().len(), 2);
    assert!(
        usage
            .attempts()
            .iter()
            .all(|attempt| attempt.outcome() == UsageAttemptOutcome::Failed)
    );
    let blocked_head = runtime.snapshot().head;
    runtime
        .request_blocked_turn_retry(turn)
        .expect("request second retry explicitly");
    assert_ne!(runtime.snapshot().head, blocked_head);
    assert_eq!(
        runtime.snapshot().status,
        RecoveryStatus::ResumeRequired { turn }
    );
    assert_eq!(
        runtime
            .usage_snapshot(UsageTimestamp::now().unwrap())
            .attempts()
            .len(),
        2
    );
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("recover second retry request");
    assert_eq!(
        recovered.snapshot().status,
        RecoveryStatus::ResumeRequired { turn }
    );
    drop(recovered);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn provider_blocked_turn_cancels_once_and_allows_a_new_turn_without_replay() {
    let path = temp_path("provider-cancel");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    assert!(matches!(
        runtime.execute(
            &ConfigLayers::default(),
            "blocked input",
            &mut UnavailableProvider
        ),
        Err(RuntimeError::Provider(ProviderError::Unavailable { .. }))
    ));
    let RecoveryStatus::Blocked { turn, .. } = runtime.snapshot().status else {
        panic!("Provider failure did not block the Turn")
    };
    let before_cancel = runtime.snapshot().head;
    assert!(matches!(
        runtime
            .cancel_blocked_turn(turn)
            .expect("cancel blocked Provider Turn"),
        CancelTurnOutcome::Durable(_)
    ));
    let cancelled_head = runtime.snapshot().head;
    assert_ne!(cancelled_head, before_cancel);
    assert_eq!(runtime.snapshot().status, RecoveryStatus::Ready);
    assert_eq!(
        runtime
            .usage_snapshot(UsageTimestamp::now().unwrap())
            .attempts()
            .len(),
        1
    );
    assert_eq!(
        runtime
            .cancel_blocked_turn(turn)
            .expect("repeat cancellation is idempotent"),
        CancelTurnOutcome::AlreadyCancelled
    );
    assert_eq!(runtime.snapshot().head, cancelled_head);
    drop(runtime);

    let mut recovered = RuntimeKernel::open(&path).expect("recover cancelled Runtime");
    assert_eq!(recovered.snapshot().status, RecoveryStatus::Ready);
    assert_eq!(
        recovered
            .cancel_blocked_turn(turn)
            .expect("replayed cancellation is idempotent"),
        CancelTurnOutcome::AlreadyCancelled
    );
    let mut provider = CountingProvider::default();
    let output = recovered
        .execute(&ConfigLayers::default(), "next input", &mut provider)
        .expect("execute next Turn");
    assert_eq!(provider.calls, 1);
    recovered
        .acknowledge(output.delivery())
        .expect("acknowledge next Turn");
    assert_eq!(recovered.snapshot().status, RecoveryStatus::Ready);
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
                schema: 13,
                kind: 1,
                payload: 1_u64.to_le_bytes().to_vec(),
            }],
        )
        .expect("append unsupported Runtime Event");
    drop(ledger);
    assert!(matches!(
        RuntimeKernel::open(&path),
        Err(RuntimeError::UnsupportedRuntimeEventSchema {
            supported: 12,
            actual: 13
        })
    ));
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn schema_one_runtime_turn_replays_and_can_continue_with_current_schema() {
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
    let legacy_usage = runtime.usage_snapshot(UsageTimestamp::now().expect("usage time"));
    assert_eq!(legacy_usage.attempts().len(), 1);
    let legacy_attempt = &legacy_usage.attempts()[0];
    assert_eq!(legacy_attempt.started_at(), None);
    assert_eq!(legacy_attempt.outcome(), UsageAttemptOutcome::Succeeded);
    assert_eq!(
        legacy_attempt.cost_provenance(),
        UsageCostProvenance::Unknown
    );
    let legacy_record = legacy_attempt.usage().expect("legacy Usage Record");
    assert_eq!(legacy_record.accuracy(), UsageAccuracy::Estimated);
    assert_eq!(legacy_record.input_tokens(), Some(2));
    assert_eq!(legacy_record.output_tokens(), Some(3));
    assert_eq!(legacy_record.total_tokens(), Some(5));
    let mut provider = DeterministicProvider::default();
    let output = runtime
        .execute(&layers, "current input", &mut provider)
        .expect("write current-schema Turn");
    runtime
        .acknowledge(output.delivery())
        .expect("acknowledge current-schema Turn");
    drop(runtime);

    let recovered = RuntimeKernel::open(&path).expect("replay mixed schema Turns");
    assert_eq!(recovered.snapshot().items.len(), 4);
    assert_eq!(recovered.snapshot().items[1].text(), legacy_text);
    assert_eq!(
        recovered.snapshot().items[3].text(),
        "simulated: current input"
    );
    let mixed_usage = recovered.usage_snapshot(UsageTimestamp::now().expect("usage time"));
    assert_eq!(mixed_usage.attempts().len(), 2);
    assert_eq!(
        mixed_usage.attempts()[0].usage().unwrap().accuracy(),
        UsageAccuracy::Estimated
    );
    assert_eq!(
        mixed_usage.attempts()[1].usage().unwrap().accuracy(),
        UsageAccuracy::Estimated
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

struct OutputTokenProvider {
    seen: Option<u32>,
}

impl ProviderRuntime for OutputTokenProvider {
    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.seen = request
            .config
            .resolved()
            .max_output_tokens()
            .map(|value| *value.value());
        DeterministicProvider::default().run(request)
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

struct UnavailableAtStageProvider {
    stage: ProviderUnavailableStage,
}

impl ProviderRuntime for UnavailableAtStageProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        Err(ProviderError::unavailable_during(
            self.stage,
            "private Provider diagnostic",
        ))
    }
}

fn provider_profile_fixture(route: &str) -> (ConfigLayers, ProviderProfileSnapshot) {
    let root = temp_path("provider-profile-config");
    let config = ConfigDocument::parse(&format!(
        r#"
schema_version = 1

[provider]
profile = "edge"
model = "fixture-model"

[providers.edge]
template = "openai-compatible"
credential = "edge-credential"
base_url = "https://gateway.example.com/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "{route}"

[providers.edge.pricing]
source = "unknown"
"#
    ))
    .expect("parse Provider profile fixture");
    let runtime = ConfigRuntime::open(
        ConfigPaths::new(
            root.with_extension("user.toml"),
            root.with_extension("project.toml"),
        ),
        config,
    )
    .expect("resolve Provider profile fixture");
    let layers = runtime
        .config_layers()
        .expect("resolved Config layers")
        .clone();
    let snapshot = runtime
        .selected_provider_profile()
        .expect("resolve selected Provider profile")
        .expect("custom Provider profile");
    (layers, snapshot)
}

struct SnapshotPanicProvider {
    snapshot: ProviderProfileSnapshot,
}

struct NoSnapshotProvider {
    calls: usize,
}

impl ProviderRuntime for NoSnapshotProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.calls += 1;
        Ok(vec![
            ProviderEvent::TextDelta("must not execute".to_owned()),
            ProviderEvent::Completed(UsageRecord::default()),
        ])
    }
}

impl ProviderRuntime for SnapshotPanicProvider {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.snapshot)
    }

    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        panic!("injected crash after frozen Provider admission")
    }
}

struct SnapshotProvider {
    snapshot: ProviderProfileSnapshot,
    seen: Option<ProviderProfileSnapshot>,
    calls: usize,
}

impl SnapshotProvider {
    fn new(snapshot: ProviderProfileSnapshot) -> Self {
        Self {
            snapshot,
            seen: None,
            calls: 0,
        }
    }
}

impl ProviderRuntime for SnapshotProvider {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.snapshot)
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.calls += 1;
        self.seen = request.provider.profile_snapshot().cloned();
        Ok(vec![
            ProviderEvent::TextDelta("frozen profile output".to_owned()),
            ProviderEvent::Completed(UsageRecord::default()),
        ])
    }
}
