use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::agent_team::{
    AgentSession, AgentStatus, Capability, CapabilitySnapshot, CommandOutcome, CompletionCapsule,
    DurableTeamError, MessageRecipient, ResourceBudget, TaskScope, TaskSpec, TeamCommand,
    TeamError,
};
use greentyper_core::ledger::LedgerError;
use greentyper_core::runtime::{RuntimeError, RuntimeKernel};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str, ledger: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-runtime-team-{name}-{ledger}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn root_spec() -> TaskSpec {
    TaskSpec::new(
        "coordinate runtime recovery",
        TaskScope::from_labels(["repo", "src", "tests"]),
    )
}

fn root_budget() -> ResourceBudget {
    ResourceBudget::new(1_000, 8)
}

fn root_capabilities() -> CapabilitySnapshot {
    CapabilitySnapshot::from_capabilities([
        Capability::WorkspaceRead,
        Capability::WorkspaceWrite,
        Capability::Process,
    ])
}

fn root_session(commit: greentyper_core::agent_team::TeamCommit) -> AgentSession {
    match commit.outcome {
        CommandOutcome::RootAdmitted { session, .. } => session,
        other => panic!("unexpected root admission outcome: {other:?}"),
    }
}

fn session_for_test(sessions: &[AgentSession], agent: AgentSession) -> AgentSession {
    sessions
        .iter()
        .copied()
        .find(|session| session.agent() == agent.agent())
        .expect("Kernel automatically rebound the persisted owner")
}

#[test]
fn kernel_rebinds_every_nonterminal_owner_without_an_id_conversion_interface() {
    let runtime_path = temp_path("rebind", "runtime");
    let team_path = temp_path("rebind", "team");
    let (mut kernel, initial_recovery) =
        RuntimeKernel::open_with_team(&runtime_path, &team_path, 2).expect("open Team Kernel");
    assert!(initial_recovery.into_sessions().is_empty());

    let root = root_session(
        kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: root_spec(),
                budget: root_budget(),
                capabilities: root_capabilities(),
            })
            .expect("Kernel root admission"),
    );
    let blocker = kernel
        .dispatch_team(TeamCommand::Delegate {
            parent: root,
            task: TaskSpec::new("blocker", TaskScope::from_labels(["src"])),
            budget: ResourceBudget::new(200, 1),
            capabilities: CapabilitySnapshot::from_capabilities([Capability::WorkspaceRead]),
        })
        .expect("delegate blocker");
    let (blocker_task, blocker_session) = match blocker.outcome {
        CommandOutcome::Delegated { task, session, .. } => (task, session),
        other => panic!("unexpected blocker delegation outcome: {other:?}"),
    };
    let dependent = kernel
        .dispatch_team(TeamCommand::Delegate {
            parent: root,
            task: TaskSpec::new("dependent", TaskScope::from_labels(["src"]))
                .with_dependencies([blocker_task]),
            budget: ResourceBudget::new(200, 1),
            capabilities: CapabilitySnapshot::from_capabilities([Capability::WorkspaceRead]),
        })
        .expect("delegate dependent");
    let dependent_session = match dependent.outcome {
        CommandOutcome::Delegated { session, .. } => session,
        other => panic!("unexpected dependent delegation outcome: {other:?}"),
    };

    let before_restart = kernel.team_snapshot().expect("Team state");
    assert_eq!(before_restart.projection.agents.len(), 3);
    assert_eq!(
        before_restart
            .projection
            .agent(dependent_session.agent())
            .expect("dependent Agent")
            .status,
        AgentStatus::Dormant
    );
    drop(kernel);

    let (mut recovered, rebound) =
        RuntimeKernel::open_with_team(&runtime_path, &team_path, 2).expect("recover Team Kernel");
    assert_eq!(rebound.snapshot(), &before_restart);
    let rebound_sessions = rebound.into_sessions();
    assert_eq!(rebound_sessions.len(), 3);

    let before_stale_command = recovered.team_snapshot().expect("recovered Team state");
    assert!(matches!(
        recovered.dispatch_team(TeamCommand::SendMessage {
            from: root,
            recipient: MessageRecipient::Team,
            body: "stale authority".into(),
        }),
        Err(RuntimeError::Team(DurableTeamError::Team(
            TeamError::InvalidAgentSession { agent }
        ))) if agent == root.agent()
    ));
    assert_eq!(
        recovered
            .team_snapshot()
            .expect("Team state after rejected command"),
        before_stale_command
    );

    let fresh_blocker = session_for_test(&rebound_sessions, blocker_session);
    recovered
        .dispatch_team(TeamCommand::Fail {
            agent: fresh_blocker,
            reason: "deterministic failure".into(),
        })
        .expect("fresh rebound session can make a durable transition");
    let blocked = recovered.team_snapshot().expect("blocked Team state");
    assert_eq!(
        blocked
            .projection
            .agent(blocker_session.agent())
            .expect("failed blocker")
            .status,
        AgentStatus::Failed
    );
    assert_eq!(
        blocked
            .projection
            .agent(dependent_session.agent())
            .expect("blocked dependent")
            .status,
        AgentStatus::Blocked
    );
    drop(recovered);

    let (mut recovered_again, rebound_again) =
        RuntimeKernel::open_with_team(&runtime_path, &team_path, 2).expect("recover blocked Team");
    assert_eq!(rebound_again.snapshot(), &blocked);
    let rebound_again_sessions = rebound_again.into_sessions();
    assert_eq!(rebound_again_sessions.len(), 2);
    assert!(
        rebound_again_sessions
            .iter()
            .all(|session| session.agent() != blocker_session.agent())
    );
    let fresh_root = session_for_test(&rebound_again_sessions, root);
    let fresh_dependent = session_for_test(&rebound_again_sessions, dependent_session);
    recovered_again
        .dispatch_team(TeamCommand::Cancel {
            agent: fresh_dependent,
            reason: "dependency will not recover".into(),
        })
        .expect("Kernel can cancel a rebound Blocked owner");
    recovered_again
        .dispatch_team(TeamCommand::Complete {
            agent: fresh_root,
            capsule: CompletionCapsule::new("recovery complete"),
        })
        .expect("finish root after children become terminal");
    let completed = recovered_again
        .team_snapshot()
        .expect("completed Team state");
    assert!(completed.projection.agents.iter().all(|agent| matches!(
        agent.status,
        AgentStatus::Succeeded | AgentStatus::Failed | AgentStatus::Cancelled
    )));
    assert_eq!(
        completed
            .projection
            .agent(root.agent())
            .expect("succeeded root")
            .status,
        AgentStatus::Succeeded
    );
    assert_eq!(
        completed
            .projection
            .agent(dependent_session.agent())
            .expect("cancelled dependent")
            .status,
        AgentStatus::Cancelled
    );
    drop(recovered_again);

    let (final_kernel, final_recovery) =
        RuntimeKernel::open_with_team(&runtime_path, &team_path, 2).expect("final recovery");
    assert_eq!(final_recovery.snapshot(), &completed);
    assert!(final_recovery.into_sessions().is_empty());
    drop(final_kernel);
    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
}

#[test]
fn kernel_owns_root_admission_and_duplicate_rejection() {
    let runtime_path = temp_path("root", "runtime");
    let team_path = temp_path("root", "team");
    let (mut kernel, initial_recovery) =
        RuntimeKernel::open_with_team(&runtime_path, &team_path, 1).expect("open Team Kernel");
    assert!(initial_recovery.into_sessions().is_empty());

    kernel
        .dispatch_team(TeamCommand::AdmitRoot {
            task: root_spec(),
            budget: root_budget(),
            capabilities: root_capabilities(),
        })
        .expect("trusted Kernel admission succeeds");
    let admitted = kernel.team_snapshot().expect("Team state after admission");
    assert_eq!(admitted.projection.agents.len(), 1);
    assert!(matches!(
        kernel.dispatch_team(TeamCommand::AdmitRoot {
            task: root_spec(),
            budget: root_budget(),
            capabilities: root_capabilities(),
        }),
        Err(RuntimeError::Team(DurableTeamError::Team(
            TeamError::RootAlreadyAdmitted
        )))
    ));
    assert_eq!(
        kernel
            .team_snapshot()
            .expect("Team state after duplicate root rejection"),
        admitted
    );
    drop(kernel);
    fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
}

#[test]
fn kernel_owns_one_exclusive_team_writer() {
    let runtime_a_path = temp_path("writer", "runtime-a");
    let runtime_b_path = temp_path("writer", "runtime-b");
    let team_path = temp_path("writer", "team");
    let (first, recovery) = RuntimeKernel::open_with_team(&runtime_a_path, &team_path, 1)
        .expect("open first Team Kernel");
    assert!(recovery.into_sessions().is_empty());

    assert!(matches!(
        RuntimeKernel::open_with_team(&runtime_b_path, &team_path, 1),
        Err(RuntimeError::Team(DurableTeamError::Ledger(
            LedgerError::Locked
        )))
    ));
    drop(first);

    let (reopened, recovery) = RuntimeKernel::open_with_team(&runtime_b_path, &team_path, 1)
        .expect("Team writer lock releases with Kernel owner");
    assert!(recovery.into_sessions().is_empty());
    drop(reopened);
    fs::remove_file(runtime_a_path).expect("cleanup first Runtime Ledger");
    fs::remove_file(runtime_b_path).expect("cleanup second Runtime Ledger");
    fs::remove_file(team_path).expect("cleanup Team Ledger");
}

#[test]
fn disabled_or_invalid_team_configuration_fails_before_team_use() {
    let runtime_path = temp_path("disabled", "runtime");
    let mut runtime = RuntimeKernel::open(&runtime_path).expect("open single-Agent Runtime");
    assert!(runtime.team_snapshot().is_none());
    assert!(matches!(
        runtime.dispatch_team(TeamCommand::AdmitRoot {
            task: root_spec(),
            budget: root_budget(),
            capabilities: root_capabilities(),
        }),
        Err(RuntimeError::TeamUnavailable)
    ));
    drop(runtime);
    fs::remove_file(&runtime_path).expect("cleanup single-Agent Runtime Ledger");

    let same_path = temp_path("same-path", "ledger");
    assert!(matches!(
        RuntimeKernel::open_with_team(&same_path, &same_path, 1),
        Err(RuntimeError::InvalidTeamConfiguration(_))
    ));
    assert!(!same_path.exists());

    let invalid_runtime_path = temp_path("invalid-limit", "runtime");
    let invalid_team_path = temp_path("invalid-limit", "team");
    assert!(matches!(
        RuntimeKernel::open_with_team(&invalid_runtime_path, &invalid_team_path, 0),
        Err(RuntimeError::Team(DurableTeamError::Team(
            TeamError::InvalidActiveAgentLimit
        )))
    ));
    assert!(!invalid_runtime_path.exists());
    assert!(!invalid_team_path.exists());
}
