use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::agent_team::{
    AgentSession, Capability, CapabilitySnapshot, CommandOutcome, DurableTeamError,
    MessageRecipient, ResourceBudget, TaskScope, TaskSpec, TeamCommand, TeamError,
    TeamOperationAcknowledgeOutcome,
};
use greentyper_core::context::ContextReductionPolicy;
use greentyper_core::runtime::{RuntimeError, RuntimeKernel};
use greentyper_core::tool_runtime::{
    ApprovalDecision, AuthorizedToolCall, ToolArguments, ToolCallOutcome, ToolCallStatus,
    ToolEffectExecutor, ToolExecution, ToolIntent, ToolReconciliationDecision, ToolRequestOutcome,
    ToolResources, ToolRuntimeError,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str, ledger: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-tool-runtime-{name}-{ledger}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn root_spec() -> TaskSpec {
    TaskSpec::new(
        "exercise one approved tool",
        TaskScope::from_labels(["repo", "src", "tests"]),
    )
}

fn root_capabilities(extra: impl IntoIterator<Item = Capability>) -> CapabilitySnapshot {
    CapabilitySnapshot::from_capabilities(
        [Capability::Tool("local.echo".into())]
            .into_iter()
            .chain(extra),
    )
}

fn root_session(commit: greentyper_core::agent_team::TeamCommit) -> AgentSession {
    match commit.outcome {
        CommandOutcome::RootAdmitted { session, .. } => session,
        other => panic!("unexpected root admission outcome: {other:?}"),
    }
}

fn admit_root(kernel: &mut RuntimeKernel, capabilities: CapabilitySnapshot) -> AgentSession {
    let operation = kernel
        .dispatch_team(TeamCommand::AdmitRoot {
            task: root_spec(),
            budget: ResourceBudget::new(1_000, 8),
            capabilities,
        })
        .expect("admit root");
    assert!(matches!(
        kernel
            .acknowledge_team_operation(operation.operation)
            .expect("acknowledge root admission"),
        TeamOperationAcknowledgeOutcome::Durable(_)
    ));
    root_session(operation.commit)
}

#[test]
fn context_checkpoint_rejects_an_unresolved_tool_approval_without_writes() {
    let runtime_path = temp_path("context-barrier", "runtime");
    let team_path = temp_path("context-barrier", "team");
    let tool_path = temp_path("context-barrier", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Tool Runtime Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel, root_capabilities([Capability::Process]));
    let request = kernel
        .request_tool_call(
            root,
            intent(
                "context-barrier-call",
                "hello",
                ToolResources::default().with_process("local.echo"),
            ),
        )
        .expect("request Tool call");
    assert!(matches!(request, ToolRequestOutcome::ApprovalRequired(_)));
    drop(kernel);
    let runtime_before = fs::read(&runtime_path).expect("read Runtime Ledger");
    let team_before = fs::read(&team_path).expect("read Team Ledger");
    let tool_before = fs::read(&tool_path).expect("read Tool Ledger");
    let (kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("reopen Tool Runtime Kernel");
    assert_eq!(recovery.into_sessions().len(), 1);

    assert!(matches!(
        kernel.prepare_context_checkpoint(ContextReductionPolicy::default()),
        Err(RuntimeError::ContextCheckpointNotAtSafeBarrier)
    ));
    drop(kernel);
    assert_eq!(
        fs::read(&runtime_path).expect("reread Runtime Ledger"),
        runtime_before
    );
    assert_eq!(
        fs::read(&team_path).expect("reread Team Ledger"),
        team_before
    );
    assert_eq!(
        fs::read(&tool_path).expect("reread Tool Ledger"),
        tool_before
    );

    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

fn intent(identity: &str, message: &str, resources: ToolResources) -> ToolIntent {
    ToolIntent::new(
        identity,
        "local.echo",
        ToolArguments::parse(&format!(r#"{{"message":"{message}"}}"#))
            .expect("canonical arguments"),
        resources,
    )
    .expect("valid Tool intent")
}

struct CountingExecutor {
    calls: usize,
    next: ToolExecution,
}

impl CountingExecutor {
    fn succeeding(output: &[u8]) -> Self {
        Self {
            calls: 0,
            next: ToolExecution::Succeeded {
                output: output.to_vec(),
            },
        }
    }

    fn ambiguous(reason: &str) -> Self {
        Self {
            calls: 0,
            next: ToolExecution::Ambiguous {
                reason: reason.into(),
            },
        }
    }
}

impl ToolEffectExecutor for CountingExecutor {
    fn execute(&mut self, call: &AuthorizedToolCall<'_>) -> ToolExecution {
        self.calls += 1;
        assert_eq!(call.tool(), "local.echo");
        assert_eq!(call.arguments().canonical_json(), r#"{"message":"hello"}"#);
        std::mem::replace(
            &mut self.next,
            ToolExecution::Failed {
                reason: "executor reused".into(),
            },
        )
    }
}

#[test]
fn kernel_persists_tool_identity_and_never_repeats_a_succeeded_effect() {
    let runtime_path = temp_path("success", "runtime");
    let team_path = temp_path("success", "team");
    let tool_path = temp_path("success", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Tool Runtime Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel, root_capabilities([Capability::Process]));
    let call_intent = intent(
        "provider-call-1",
        "hello",
        ToolResources::default().with_process("local.echo"),
    );
    let request = match kernel
        .request_tool_call(root, call_intent.clone())
        .expect("request Tool call")
    {
        ToolRequestOutcome::ApprovalRequired(request) => request,
        other => panic!("unexpected request outcome: {other:?}"),
    };
    let mut executor = CountingExecutor::succeeding(b"hello\n");
    let outcome = kernel
        .resolve_tool_call(
            request,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut executor,
        )
        .expect("execute approved Tool call");
    let succeeded = match outcome {
        ToolCallOutcome::Succeeded { record, output } => {
            assert_eq!(output, b"hello\n");
            record
        }
        other => panic!("unexpected Tool outcome: {other:?}"),
    };
    assert_eq!(executor.calls, 1);
    assert_eq!(succeeded.status, ToolCallStatus::Succeeded);
    assert!(succeeded.result_digest.is_some());

    assert!(matches!(
        kernel
            .request_tool_call(root, call_intent.clone())
            .expect("repeat identical Tool identity"),
        ToolRequestOutcome::Existing(record) if record == succeeded
    ));
    assert_eq!(executor.calls, 1);
    assert!(matches!(
        kernel.request_tool_call(
            root,
            intent(
                "provider-call-1",
                "changed",
                ToolResources::default().with_process("local.echo"),
            ),
        ),
        Err(RuntimeError::Tool(
            ToolRuntimeError::IdentityConflict { .. }
        ))
    ));
    let before_restart = kernel.tool_snapshot().expect("Tool snapshot");
    drop(kernel);

    let (mut recovered, rebound) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("reopen Tool Runtime Kernel");
    assert_eq!(
        recovered.tool_snapshot().expect("replayed Tool state"),
        before_restart
    );
    let fresh_root = rebound
        .into_sessions()
        .into_iter()
        .find(|session| session.agent() == root.agent())
        .expect("rebound root");
    assert!(matches!(
        recovered
            .request_tool_call(fresh_root, call_intent)
            .expect("replay identical identity"),
        ToolRequestOutcome::Existing(record) if record == succeeded
    ));
    assert!(matches!(
        recovered.request_tool_call(root, intent(
            "provider-call-2",
            "hello",
            ToolResources::default().with_process("local.echo"),
        )),
        Err(RuntimeError::Team(DurableTeamError::Team(
            TeamError::InvalidAgentSession { agent }
        ))) if agent == root.agent()
    ));
    drop(recovered);
    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn ambiguous_tool_effect_blocks_progress_until_explicit_reconciliation() {
    let runtime_path = temp_path("ambiguous", "runtime");
    let team_path = temp_path("ambiguous", "team");
    let tool_path = temp_path("ambiguous", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Tool Runtime Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel, root_capabilities([Capability::Process]));
    let request = match kernel
        .request_tool_call(
            root,
            intent(
                "provider-call-ambiguous",
                "hello",
                ToolResources::default().with_process("local.echo"),
            ),
        )
        .expect("request Tool call")
    {
        ToolRequestOutcome::ApprovalRequired(request) => request,
        other => panic!("unexpected request outcome: {other:?}"),
    };
    let mut executor = CountingExecutor::ambiguous("process result unknown");
    let record = match kernel
        .resolve_tool_call(
            request,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut executor,
        )
        .expect("record ambiguous Tool effect")
    {
        ToolCallOutcome::ReconciliationRequired(record) => record,
        other => panic!("unexpected ambiguous outcome: {other:?}"),
    };
    assert_eq!(executor.calls, 1);
    assert_eq!(record.status, ToolCallStatus::ReconciliationRequired);
    assert!(matches!(
        kernel.request_tool_call(
            root,
            intent(
                "provider-call-blocked",
                "hello",
                ToolResources::default().with_process("local.echo"),
            ),
        ),
        Err(RuntimeError::ToolReconciliationRequired(call)) if call == record.call
    ));
    assert!(matches!(
        kernel.dispatch_team(TeamCommand::SendMessage {
            from: root,
            recipient: MessageRecipient::Team,
            body: "must reconcile the effect first".into(),
        }),
        Err(RuntimeError::ToolReconciliationRequired(call)) if call == record.call
    ));
    drop(kernel);

    let (mut recovered, rebound) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("recover ambiguous Tool effect");
    let fresh_root = rebound
        .into_sessions()
        .into_iter()
        .find(|session| session.agent() == root.agent())
        .expect("rebound root");
    assert_eq!(
        recovered
            .tool_snapshot()
            .expect("recovered Tool snapshot")
            .calls[0]
            .status,
        ToolCallStatus::ReconciliationRequired
    );
    let reconciled = recovered
        .reconcile_tool_call(
            fresh_root,
            record.call,
            ToolReconciliationDecision::ObservedSucceeded {
                result_digest: [7; 32],
            },
        )
        .expect("explicitly reconcile Tool effect");
    assert_eq!(reconciled.status, ToolCallStatus::Succeeded);
    assert_eq!(reconciled.result_digest, Some([7; 32]));
    assert!(matches!(
        recovered
            .request_tool_call(
                fresh_root,
                intent(
                    "provider-call-after-reconcile",
                    "hello",
                    ToolResources::default().with_process("local.echo"),
                ),
            )
            .expect("new call after reconciliation"),
        ToolRequestOutcome::ApprovalRequired(_)
    ));
    drop(recovered);
    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn tool_authority_axes_and_approval_expiry_fail_closed() {
    let runtime_path = temp_path("authority", "runtime");
    let team_path = temp_path("authority", "team");
    let tool_path = temp_path("authority", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Tool Runtime Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel, root_capabilities([Capability::Process]));

    assert!(matches!(
        kernel.request_tool_call(
            root,
            intent(
                "network-denied",
                "hello",
                ToolResources::default().with_network_target("api.example.com:443"),
            ),
        ),
        Err(RuntimeError::Tool(ToolRuntimeError::CapabilityDenied {
            capability: Capability::Network,
        }))
    ));
    assert!(matches!(
        kernel.request_tool_call(
            root,
            intent(
                "filesystem-denied",
                "hello",
                ToolResources::default().with_filesystem_read("workspace:src/lib.rs"),
            ),
        ),
        Err(RuntimeError::Tool(ToolRuntimeError::CapabilityDenied {
            capability: Capability::WorkspaceRead,
        }))
    ));

    let request = match kernel
        .request_tool_call(
            root,
            intent(
                "expired-grant",
                "hello",
                ToolResources::default().with_process("local.echo"),
            ),
        )
        .expect("request process Tool")
    {
        ToolRequestOutcome::ApprovalRequired(request) => request,
        other => panic!("unexpected request outcome: {other:?}"),
    };
    let mut executor = CountingExecutor::succeeding(b"not executed");
    assert!(matches!(
        kernel.resolve_tool_call(
            request,
            ApprovalDecision::Grant {
                expires_at_unix_ms: 1,
            },
            &mut executor,
        ),
        Err(RuntimeError::Tool(ToolRuntimeError::ApprovalExpired))
    ));
    assert_eq!(executor.calls, 0);
    assert_eq!(
        kernel
            .tool_snapshot()
            .expect("Tool snapshot")
            .calls
            .last()
            .expect("pending Tool request")
            .status,
        ToolCallStatus::AwaitingApproval
    );
    drop(kernel);
    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn approval_request_is_bound_to_the_current_agent_authority() {
    let runtime_path = temp_path("approval-authority", "runtime");
    let team_path = temp_path("approval-authority", "team");
    let tool_path = temp_path("approval-authority", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Tool Runtime Kernel");
    assert!(recovery.into_sessions().is_empty());
    let root = admit_root(&mut kernel, root_capabilities([Capability::Process]));
    let call_intent = intent(
        "approval-authority",
        "hello",
        ToolResources::default().with_process("local.echo"),
    );
    let stale_request = match kernel
        .request_tool_call(root, call_intent.clone())
        .expect("request Tool call")
    {
        ToolRequestOutcome::ApprovalRequired(request) => request,
        other => panic!("unexpected request outcome: {other:?}"),
    };
    drop(kernel);

    let (mut recovered, rebound) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("reopen Tool Runtime Kernel");
    let fresh_root = rebound
        .into_sessions()
        .into_iter()
        .find(|session| session.agent() == root.agent())
        .expect("rebound root");
    let mut executor = CountingExecutor::succeeding(b"not invoked");
    assert!(matches!(
        recovered.resolve_tool_call(
            stale_request,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut executor,
        ),
        Err(RuntimeError::Team(DurableTeamError::Team(
            TeamError::InvalidAgentSession { agent }
        ))) if agent == root.agent()
    ));
    assert_eq!(executor.calls, 0);

    let fresh_request = match recovered
        .request_tool_call(fresh_root, call_intent)
        .expect("reissue current approval request")
    {
        ToolRequestOutcome::ApprovalRequired(request) => request,
        other => panic!("unexpected request outcome: {other:?}"),
    };
    let outcome = recovered
        .resolve_tool_call(
            fresh_request,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut executor,
        )
        .expect("execute with current authority");
    assert!(matches!(outcome, ToolCallOutcome::Succeeded { .. }));
    assert_eq!(executor.calls, 1);
    drop(recovered);
    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn pending_team_acknowledgement_blocks_tool_admission() {
    let runtime_path = temp_path("team-gate", "runtime");
    let team_path = temp_path("team-gate", "team");
    let tool_path = temp_path("team-gate", "tool");
    let (mut kernel, recovery) =
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
            .expect("open Tool Runtime Kernel");
    assert!(recovery.into_sessions().is_empty());
    let admission = kernel
        .dispatch_team(TeamCommand::AdmitRoot {
            task: root_spec(),
            budget: ResourceBudget::new(1_000, 8),
            capabilities: root_capabilities([Capability::Process]),
        })
        .expect("admit root");
    let root = root_session(admission.commit);
    assert!(matches!(
        kernel.request_tool_call(
            root,
            intent(
                "blocked-before-team-ack",
                "hello",
                ToolResources::default().with_process("local.echo"),
            ),
        ),
        Err(RuntimeError::TeamOperationReconciliationRequired(operation))
            if operation == admission.operation
    ));
    assert!(
        kernel
            .tool_snapshot()
            .expect("Tool snapshot")
            .calls
            .is_empty()
    );
    assert!(matches!(
        kernel
            .acknowledge_team_operation(admission.operation)
            .expect("acknowledge Team operation"),
        TeamOperationAcknowledgeOutcome::Durable(_)
    ));
    assert!(matches!(
        kernel
            .request_tool_call(
                root,
                intent(
                    "admitted-after-team-ack",
                    "hello",
                    ToolResources::default().with_process("local.echo"),
                ),
            )
            .expect("request after Team acknowledgement"),
        ToolRequestOutcome::ApprovalRequired(_)
    ));
    drop(kernel);
    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
    fs::remove_file(tool_path).expect("cleanup Tool Ledger");
}

#[test]
fn runtime_team_and_tool_ledgers_must_use_distinct_paths() {
    let shared_path = temp_path("paths", "shared");
    assert!(matches!(
        RuntimeKernel::open_with_team_and_tools(
            &shared_path,
            &shared_path,
            temp_path("paths", "tool"),
            1
        ),
        Err(RuntimeError::InvalidTeamConfiguration(_))
    ));
    assert!(matches!(
        RuntimeKernel::open_with_team_and_tools(
            temp_path("paths", "runtime"),
            &shared_path,
            &shared_path,
            1,
        ),
        Err(RuntimeError::InvalidToolConfiguration(_))
    ));
    assert!(matches!(
        RuntimeKernel::open_with_team_and_tools(
            &shared_path,
            temp_path("paths", "team"),
            &shared_path,
            1,
        ),
        Err(RuntimeError::InvalidToolConfiguration(_))
    ));
    assert!(!shared_path.exists());
}

#[test]
fn ledger_path_aliases_are_rejected_before_any_file_is_created() {
    let directory = temp_path("path-aliases", "directory");
    let nested = directory.join("nested");
    fs::create_dir_all(&nested).expect("create alias fixture directory");
    let runtime_path = directory.join("shared.ledger");
    let runtime_alias = nested.join("..").join("shared.ledger");
    let team_path = directory.join("team.ledger");
    let team_alias = nested.join("..").join("team.ledger");

    assert!(matches!(
        RuntimeKernel::open_with_team_and_tools(
            &runtime_path,
            &runtime_alias,
            directory.join("tool.ledger"),
            1,
        ),
        Err(RuntimeError::InvalidTeamConfiguration(_))
    ));
    assert!(matches!(
        RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &team_alias, 1,),
        Err(RuntimeError::InvalidToolConfiguration(_))
    ));
    assert!(!runtime_path.exists());
    assert!(!team_path.exists());
    fs::remove_dir(nested).expect("cleanup nested fixture directory");
    fs::remove_dir(directory).expect("cleanup fixture directory");
}
