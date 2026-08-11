use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::agent_team::{
    AgentSession, Capability, CapabilitySnapshot, CommandOutcome, ResourceBudget, TaskScope,
    TaskSpec, TeamCommand,
};
use greentyper_core::config::ConfigLayers;
use greentyper_core::provider::responses::{ResponsesSseDecoder, normalize_responses_events};
use greentyper_core::provider::{
    ProviderError, ProviderEvent, ProviderRequest, ProviderRuntime, ProviderToolOutput,
    ProviderUnavailableStage,
};
use greentyper_core::runtime::{ProviderTurnOutcome, RecoveryStatus, RuntimeError, RuntimeKernel};
use greentyper_core::tool_runtime::{
    ApprovalDecision, AuthorizedToolCall, ToolCallStatus, ToolEffectExecutor, ToolExecution,
    ToolResources,
};

const INITIAL_RESPONSE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/responses/v1/text-and-function-call.sse"
));
const CONTINUATION_RESPONSE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/responses/v1/tool-continuation.sse"
));

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str, ledger: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-provider-tool-{name}-{ledger}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn decode(stream: &[u8]) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut decoder = ResponsesSseDecoder::new(512 * 1024)
        .map_err(|error| ProviderError::unavailable(error.to_string()))?;
    for chunk in stream.chunks(13) {
        decoder
            .push(chunk)
            .map_err(|error| ProviderError::unavailable(error.to_string()))?;
    }
    let events = decoder
        .finish()
        .map_err(|error| ProviderError::unavailable(error.to_string()))?;
    normalize_responses_events(&events)
}

struct FixtureResponsesProvider;

impl ProviderRuntime for FixtureResponsesProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        decode(INITIAL_RESPONSE)
    }

    fn continue_after_tool(
        &mut self,
        _request: &ProviderRequest,
        output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        assert_eq!(output.call_id(), "call_fixture_1");
        assert_eq!(output.output(), "28 C");
        decode(CONTINUATION_RESPONSE)
    }
}

struct PanicAfterToolProvider;

impl ProviderRuntime for PanicAfterToolProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        decode(INITIAL_RESPONSE)
    }

    fn continue_after_tool(
        &mut self,
        _request: &ProviderRequest,
        _output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        panic!("injected process death after durable Tool success")
    }
}

#[derive(Default)]
struct UnavailableContinuationProvider {
    continuations: usize,
}

impl ProviderRuntime for UnavailableContinuationProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        decode(INITIAL_RESPONSE)
    }

    fn continue_after_tool(
        &mut self,
        _request: &ProviderRequest,
        _output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.continuations += 1;
        Err(ProviderError::unavailable_during(
            ProviderUnavailableStage::BeforeResponse,
            "injected continuation outage",
        ))
    }
}

#[derive(Default)]
struct CountingInitialProvider {
    calls: usize,
}

impl ProviderRuntime for CountingInitialProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.calls += 1;
        decode(INITIAL_RESPONSE)
    }
}

struct WeatherExecutor;

impl ToolEffectExecutor for WeatherExecutor {
    fn execute(&mut self, call: &AuthorizedToolCall<'_>) -> ToolExecution {
        assert_eq!(call.tool(), "weather");
        assert_eq!(
            call.arguments().canonical_json(),
            r#"{"city":"香港","unit":"c"}"#
        );
        ToolExecution::Succeeded {
            output: b"28 C".to_vec(),
        }
    }
}

struct AmbiguousWeatherExecutor;

impl ToolEffectExecutor for AmbiguousWeatherExecutor {
    fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
        ToolExecution::Ambiguous {
            reason: "synthetic uncertain process result".into(),
        }
    }
}

struct BinaryWeatherExecutor;

impl ToolEffectExecutor for BinaryWeatherExecutor {
    fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
        ToolExecution::Succeeded {
            output: vec![0xff, 0xfe],
        }
    }
}

struct MalformedArgumentsProvider;

impl ProviderRuntime for MalformedArgumentsProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        Ok(vec![
            ProviderEvent::FunctionCall(greentyper_core::provider::ProviderToolCall::new(
                "call-malformed",
                "weather",
                "not-json",
            )?),
            ProviderEvent::Completed(Default::default()),
        ])
    }
}

#[derive(Default)]
struct CountingContinuationProvider {
    runs: usize,
    continuations: usize,
}

impl ProviderRuntime for CountingContinuationProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.runs += 1;
        decode(INITIAL_RESPONSE)
    }

    fn continue_after_tool(
        &mut self,
        _request: &ProviderRequest,
        _output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.continuations += 1;
        decode(CONTINUATION_RESPONSE)
    }
}

fn admit_root(kernel: &mut RuntimeKernel) -> AgentSession {
    let operation = kernel
        .dispatch_team(TeamCommand::AdmitRoot {
            task: TaskSpec::new(
                "answer weather with one approved tool",
                TaskScope::from_labels(["provider", "tool"]),
            ),
            budget: ResourceBudget::new(1_000, 4),
            capabilities: CapabilitySnapshot::from_capabilities([
                Capability::Tool("weather".into()),
                Capability::Process,
            ]),
        })
        .expect("admit root");
    kernel
        .acknowledge_team_operation(operation.operation)
        .expect("acknowledge root admission");
    match operation.commit.outcome {
        CommandOutcome::RootAdmitted { session, .. } => session,
        other => panic!("unexpected root outcome: {other:?}"),
    }
}

#[test]
fn fixture_responses_turn_runs_one_approved_tool_and_finishes_canonically() {
    let runtime_path = temp_path("happy", "runtime");
    let team_path = temp_path("happy", "team");
    let tool_path = temp_path("happy", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Provider Tool Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel);
    let mut provider = FixtureResponsesProvider;

    let outcome = kernel
        .execute_provider_turn(
            root,
            &ConfigLayers::default(),
            "What is the weather in Hong Kong?",
            &mut provider,
            |call| {
                assert_eq!(call.tool(), "weather");
                Ok(ToolResources::default().with_process("weather"))
            },
        )
        .expect("prepare Provider Tool call");
    let approval = match outcome {
        ProviderTurnOutcome::ApprovalRequired(approval) => approval,
        other => panic!("unexpected Provider Turn outcome: {other:?}"),
    };
    assert!(approval.identity().starts_with("provider-turn-1-"));
    assert_eq!(approval.tool(), "weather");
    assert_eq!(
        approval.arguments().canonical_json(),
        r#"{"city":"香港","unit":"c"}"#
    );
    assert_eq!(approval.resources().process(), Some("weather"));
    let _arguments_hash = approval.arguments_hash();

    let output = kernel
        .resolve_provider_tool_call(
            approval,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut WeatherExecutor,
            &mut provider,
        )
        .expect("continue Provider Turn after approved Tool effect");
    assert_eq!(output.text(), "Hello 中\nWeather: 28 C");
    assert_eq!(output.usage_records().len(), 2);
    assert_eq!(output.usage_records()[0].input_tokens(), Some(11));
    assert_eq!(output.usage_records()[0].cached_input_tokens(), Some(3));
    assert_eq!(output.usage_records()[1].input_tokens(), Some(13));
    assert_eq!(output.usage_records()[1].reasoning_output_tokens(), Some(1));
    kernel
        .acknowledge(output.delivery())
        .expect("acknowledge canonical output");

    let tool_snapshot = kernel.tool_snapshot().expect("Tool snapshot");
    assert_eq!(tool_snapshot.calls.len(), 1);
    assert_eq!(tool_snapshot.calls[0].status, ToolCallStatus::Succeeded);
    drop(kernel);

    let (recovered, rebound) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("recover Provider Tool Kernel");
    assert_eq!(rebound.into_sessions().len(), 1);
    let snapshot = recovered.snapshot();
    assert_eq!(snapshot.items.len(), 2);
    assert_eq!(snapshot.items[1].text(), "Hello 中\nWeather: 28 C");
    assert_eq!(
        recovered
            .tool_snapshot()
            .expect("replayed Tool")
            .calls
            .len(),
        1
    );
    drop(recovered);

    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn recovered_provider_turn_never_repeats_a_succeeded_tool_without_its_raw_result() {
    let runtime_path = temp_path("lost-result", "runtime");
    let team_path = temp_path("lost-result", "team");
    let tool_path = temp_path("lost-result", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Provider Tool Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel);
    let mut provider = PanicAfterToolProvider;
    let approval = match kernel
        .execute_provider_turn(
            root,
            &ConfigLayers::default(),
            "What is the weather in Hong Kong?",
            &mut provider,
            |_| Ok(ToolResources::default().with_process("weather")),
        )
        .expect("prepare Provider Tool call")
    {
        ProviderTurnOutcome::ApprovalRequired(approval) => approval,
        other => panic!("unexpected Provider Turn outcome: {other:?}"),
    };
    let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = kernel.resolve_provider_tool_call(
            approval,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut WeatherExecutor,
            &mut provider,
        );
    }));
    assert!(crash.is_err());
    assert_eq!(
        kernel.tool_snapshot().expect("Tool snapshot").calls[0].status,
        ToolCallStatus::Succeeded
    );
    drop(kernel);

    let (mut recovered, rebound) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("recover Provider Tool Kernel");
    let fresh_root = rebound
        .into_sessions()
        .into_iter()
        .find(|session| session.agent() == root.agent())
        .expect("rebound root");
    let mut provider = FixtureResponsesProvider;
    assert!(matches!(
        recovered.resume_provider_turn(fresh_root, &mut provider, |_| Ok(
            ToolResources::default().with_process("weather")
        ),),
        Err(RuntimeError::ProviderToolResultUnavailable(_))
    ));
    assert!(matches!(
        recovered.snapshot().status,
        RecoveryStatus::Blocked { .. }
    ));
    let calls = &recovered.tool_snapshot().expect("replayed Tool").calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, ToolCallStatus::Succeeded);
    drop(recovered);

    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn provider_continuation_failure_after_tool_success_is_not_retryable() {
    let runtime_path = temp_path("continuation-unavailable", "runtime");
    let team_path = temp_path("continuation-unavailable", "team");
    let tool_path = temp_path("continuation-unavailable", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Provider Tool Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel);
    let mut provider = UnavailableContinuationProvider::default();
    let approval = match kernel
        .execute_provider_turn(
            root,
            &ConfigLayers::default(),
            "What is the weather in Hong Kong?",
            &mut provider,
            |_| Ok(ToolResources::default().with_process("weather")),
        )
        .expect("prepare Provider Tool call")
    {
        ProviderTurnOutcome::ApprovalRequired(approval) => approval,
        other => panic!("unexpected Provider Turn outcome: {other:?}"),
    };
    assert!(matches!(
        kernel.resolve_provider_tool_call(
            approval,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut WeatherExecutor,
            &mut provider,
        ),
        Err(RuntimeError::Provider(ProviderError::Unavailable { .. }))
    ));
    assert_eq!(provider.continuations, 1);
    let RecoveryStatus::Blocked {
        turn,
        retryable: false,
        ..
    } = kernel.snapshot().status
    else {
        panic!("Provider continuation failure was incorrectly retryable")
    };
    assert_eq!(
        kernel.tool_snapshot().expect("Tool snapshot").calls[0].status,
        ToolCallStatus::Succeeded
    );
    let root_agent = root.agent();
    drop(kernel);
    let runtime_before = fs::read(&runtime_path).expect("read Runtime Ledger before rejection");
    let team_before = fs::read(&team_path).expect("read Team Ledger before rejection");
    let tool_before = fs::read(&tool_path).expect("read Tool Ledger before rejection");

    let (mut recovered, rebound) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("reopen Runtime, Team, and Tool Ledgers");
    let fresh_root = rebound
        .into_sessions()
        .into_iter()
        .find(|session| session.agent() == root_agent)
        .expect("rebound root Agent Session");
    assert!(matches!(
        recovered.request_blocked_provider_turn_retry(fresh_root, turn),
        Err(RuntimeError::TurnRetryNotAllowed(actual)) if actual == turn
    ));
    drop(recovered);
    assert_eq!(
        fs::read(&runtime_path).expect("read Runtime Ledger after rejection"),
        runtime_before
    );
    assert_eq!(
        fs::read(&team_path).expect("read Team Ledger after rejection"),
        team_before
    );
    assert_eq!(
        fs::read(&tool_path).expect("read Tool Ledger after rejection"),
        tool_before
    );

    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn stale_session_cannot_admit_or_invoke_a_provider_turn() {
    let runtime_path = temp_path("stale", "runtime");
    let team_path = temp_path("stale", "team");
    let tool_path = temp_path("stale", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Provider Tool Kernel");
    assert!(recovery.into_sessions().is_empty());
    let stale_root = admit_root(&mut kernel);
    drop(kernel);

    let (mut recovered, rebound) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("recover Provider Tool Kernel");
    assert_eq!(rebound.into_sessions().len(), 1);
    let before = recovered.snapshot();
    let mut provider = CountingInitialProvider::default();
    assert!(matches!(
        recovered.execute_provider_turn(
            stale_root,
            &ConfigLayers::default(),
            "must not run",
            &mut provider,
            |_| Ok(ToolResources::default().with_process("weather")),
        ),
        Err(RuntimeError::Team(_))
    ));
    assert_eq!(provider.calls, 0);
    assert_eq!(recovered.snapshot(), before);
    assert!(
        recovered
            .tool_snapshot()
            .expect("Tool snapshot")
            .calls
            .is_empty()
    );
    drop(recovered);

    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn ambiguous_tool_effect_blocks_provider_continuation_and_resume() {
    let runtime_path = temp_path("ambiguous", "runtime");
    let team_path = temp_path("ambiguous", "team");
    let tool_path = temp_path("ambiguous", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Provider Tool Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel);
    let mut provider = CountingContinuationProvider::default();
    let approval = match kernel
        .execute_provider_turn(
            root,
            &ConfigLayers::default(),
            "What is the weather in Hong Kong?",
            &mut provider,
            |_| Ok(ToolResources::default().with_process("weather")),
        )
        .expect("prepare Provider Tool call")
    {
        ProviderTurnOutcome::ApprovalRequired(approval) => approval,
        other => panic!("unexpected Provider Turn outcome: {other:?}"),
    };
    assert!(matches!(
        kernel.resolve_provider_tool_call(
            approval,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut AmbiguousWeatherExecutor,
            &mut provider,
        ),
        Err(RuntimeError::ToolReconciliationRequired(_))
    ));
    assert_eq!(provider.runs, 1);
    assert_eq!(provider.continuations, 0);
    assert!(matches!(
        kernel.snapshot().status,
        RecoveryStatus::ResumeRequired { .. }
    ));
    assert_eq!(
        kernel.tool_snapshot().expect("Tool snapshot").calls[0].status,
        ToolCallStatus::ReconciliationRequired
    );
    assert!(matches!(
        kernel.resume_provider_turn(root, &mut provider, |_| Ok(
            ToolResources::default().with_process("weather")
        )),
        Err(RuntimeError::ToolReconciliationRequired(_))
    ));
    assert_eq!(provider.runs, 1);
    assert_eq!(provider.continuations, 0);
    drop(kernel);

    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn non_utf8_tool_output_is_never_sent_to_provider_continuation() {
    let runtime_path = temp_path("binary", "runtime");
    let team_path = temp_path("binary", "team");
    let tool_path = temp_path("binary", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Provider Tool Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel);
    let mut provider = CountingContinuationProvider::default();
    let approval = match kernel
        .execute_provider_turn(
            root,
            &ConfigLayers::default(),
            "What is the weather in Hong Kong?",
            &mut provider,
            |_| Ok(ToolResources::default().with_process("weather")),
        )
        .expect("prepare Provider Tool call")
    {
        ProviderTurnOutcome::ApprovalRequired(approval) => approval,
        other => panic!("unexpected Provider Turn outcome: {other:?}"),
    };
    assert!(matches!(
        kernel.resolve_provider_tool_call(
            approval,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut BinaryWeatherExecutor,
            &mut provider,
        ),
        Err(RuntimeError::InvalidProviderOutput(
            "Provider Tool output is not UTF-8"
        ))
    ));
    assert_eq!(provider.continuations, 0);
    assert!(matches!(
        kernel.snapshot().status,
        RecoveryStatus::Blocked { .. }
    ));
    assert_eq!(
        kernel.tool_snapshot().expect("Tool snapshot").calls[0].status,
        ToolCallStatus::Succeeded
    );
    drop(kernel);

    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn malformed_provider_arguments_are_rejected_before_resource_mapping() {
    let runtime_path = temp_path("malformed-arguments", "runtime");
    let team_path = temp_path("malformed-arguments", "team");
    let tool_path = temp_path("malformed-arguments", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Provider Tool Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel);
    let mut mappings = 0;
    assert!(matches!(
        kernel.execute_provider_turn(
            root,
            &ConfigLayers::default(),
            "invalid Tool arguments",
            &mut MalformedArgumentsProvider,
            |_| {
                mappings += 1;
                Ok(ToolResources::default().with_process("weather"))
            },
        ),
        Err(RuntimeError::Tool(_))
    ));
    assert_eq!(mappings, 0);
    assert!(matches!(
        kernel.snapshot().status,
        RecoveryStatus::Blocked { .. }
    ));
    drop(kernel);

    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}
