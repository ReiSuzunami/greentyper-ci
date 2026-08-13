use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::agent_team::{
    AgentSession, Capability, CapabilitySnapshot, CommandOutcome, CommitDurability,
    CompletionCapsule, DurableTeamError, DurableTeamRuntime, InheritedModelPreset,
    MessageRecipient, ResourceBudget, TaskScope, TaskSpec, TeamCommand, TeamError, TeamEventKind,
};
use greentyper_core::ledger::{EventData, FileLedger, LedgerError, LedgerHead};
use greentyper_core::schema::SchemaKind;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-durable-team-{name}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn root_command() -> TeamCommand {
    TeamCommand::AdmitRoot {
        task: TaskSpec::new(
            "coordinate implementation",
            TaskScope::from_labels(["repo", "src", "tests"]),
        ),
        budget: ResourceBudget::new(1_000, 8),
        capabilities: CapabilitySnapshot::from_capabilities([
            Capability::WorkspaceRead,
            Capability::WorkspaceWrite,
            Capability::Process,
        ]),
    }
}

fn admit_root(team: &mut DurableTeamRuntime) -> AgentSession {
    let commit = team
        .dispatch(root_command())
        .expect("durable root admission");
    let receipt = match commit.durability {
        CommitDurability::Synchronous(receipt) => receipt,
        CommitDurability::Volatile => panic!("durable adapter returned a volatile commit"),
    };
    assert_eq!(receipt.transaction, commit.transaction);
    assert_eq!(receipt.first_sequence, commit.events[0].sequence);
    assert_eq!(receipt.last_sequence, commit.revision);
    assert_eq!(receipt.event_count as usize, commit.events.len());
    match commit.outcome {
        CommandOutcome::RootAdmitted { session, .. } => session,
        other => panic!("unexpected admission outcome: {other:?}"),
    }
}

#[test]
fn invalid_active_limit_fails_before_creating_a_ledger() {
    let path = temp_path("invalid-limit");
    assert!(matches!(
        DurableTeamRuntime::open(&path, 0),
        Err(DurableTeamError::Team(TeamError::InvalidActiveAgentLimit))
    ));
    assert!(matches!(
        DurableTeamRuntime::inspect(&path, 0),
        Err(DurableTeamError::Team(TeamError::InvalidActiveAgentLimit))
    ));
    assert!(!path.exists());
}

#[test]
fn durable_commit_reopens_exactly_and_invalidates_old_sessions() {
    let path = temp_path("reopen");
    let mut team = DurableTeamRuntime::open(&path, 2).expect("create durable Team");
    let root = admit_root(&mut team);
    let expected_snapshot = team.snapshot();
    let expected_events = team.event_log().to_vec();
    let expected_head = team.ledger_head();
    drop(team);

    let mut recovered = DurableTeamRuntime::open(&path, 2).expect("recover durable Team");
    assert_eq!(recovered.snapshot(), expected_snapshot);
    assert_eq!(recovered.event_log(), expected_events);
    assert_eq!(recovered.ledger_head(), expected_head);
    assert_eq!(recovered.recovered_tail_bytes(), 0);

    let before = recovered.snapshot();
    let error = recovered
        .dispatch(TeamCommand::SendMessage {
            from: root,
            recipient: MessageRecipient::Team,
            body: "stale authority".into(),
        })
        .expect_err("sessions are process-local and invalid after recovery");
    assert!(matches!(
        error,
        DurableTeamError::Team(TeamError::InvalidAgentSession { agent })
            if agent == root.agent()
    ));
    assert_eq!(recovered.snapshot(), before);
    assert_eq!(recovered.ledger_head(), expected_head);
    drop(recovered);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn durable_requeue_reopens_with_the_child_runnable_again() {
    let path = temp_path("requeue-reopen");
    let mut team = DurableTeamRuntime::open(&path, 2).expect("create durable Team");
    let root = admit_root(&mut team);
    let child_commit = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: TaskSpec::new("retry child", TaskScope::default()),
            budget: ResourceBudget::new(100, 1),
            capabilities: CapabilitySnapshot::default(),
        })
        .expect("delegate retry child");
    let child = match child_commit.outcome {
        CommandOutcome::Delegated { session, .. } => session,
        other => panic!("unexpected delegation outcome: {other:?}"),
    };
    team.dispatch(TeamCommand::Fail {
        agent: child,
        reason: "provider unavailable".into(),
    })
    .expect("fail child");
    team.dispatch(TeamCommand::Retry {
        requester: root,
        agent: child.agent(),
    })
    .expect("requeue child");
    let expected = team.snapshot();
    drop(team);

    let recovered = DurableTeamRuntime::open(&path, 2).expect("reopen durable Team");
    assert_eq!(recovered.snapshot(), expected);
    let recovered_snapshot = recovered.snapshot();
    let child_view = recovered_snapshot
        .agents
        .iter()
        .find(|agent| agent.id == child.agent())
        .expect("requeued child");
    assert_eq!(
        child_view.status,
        greentyper_core::agent_team::AgentStatus::Active
    );
    drop(recovered);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn read_only_team_inspection_reports_a_torn_tail_without_repairing_it() {
    let path = temp_path("inspect-tail");
    let mut team = DurableTeamRuntime::open(&path, 1).expect("create durable Team");
    admit_root(&mut team);
    let expected_snapshot = team.snapshot();
    let expected_head = team.ledger_head();
    drop(team);

    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open Team Ledger tail")
        .write_all(b"xyz")
        .expect("append incomplete Team frame");
    let before = fs::read(&path).expect("read Team Ledger before inspection");

    let inspection = DurableTeamRuntime::inspect(&path, 1).expect("inspect durable Team");
    assert_eq!(inspection.snapshot(), &expected_snapshot);
    assert_eq!(inspection.ledger_head(), expected_head);
    assert_eq!(inspection.recovered_tail_bytes(), 3);
    assert!(inspection.operation_records().is_empty());
    assert_eq!(fs::read(&path).expect("reread Team Ledger"), before);

    fs::remove_file(path).expect("cleanup inspected Team Ledger");
}

#[test]
fn read_only_team_inspection_never_creates_missing_state() {
    let path = temp_path("inspect-missing");
    assert!(matches!(
        DurableTeamRuntime::inspect(&path, 1),
        Err(DurableTeamError::Ledger(LedgerError::Io(source)))
            if source.kind() == std::io::ErrorKind::NotFound
    ));
    assert!(!path.exists());
}

#[test]
fn failed_planning_does_not_mutate_the_ledger_or_projection() {
    let path = temp_path("planning");
    let mut team = DurableTeamRuntime::open(&path, 2).expect("create durable Team");
    let root = admit_root(&mut team);
    let before_snapshot = team.snapshot();
    let before_events = team.event_log().to_vec();
    let before_head = team.ledger_head();

    assert!(matches!(
        team.dispatch(root_command()),
        Err(DurableTeamError::Team(TeamError::RootAlreadyAdmitted))
    ));
    assert_eq!(team.snapshot(), before_snapshot);
    assert_eq!(team.event_log(), before_events);
    assert_eq!(team.ledger_head(), before_head);

    let next = team
        .dispatch(TeamCommand::SendMessage {
            from: root,
            recipient: MessageRecipient::Team,
            body: "next valid transaction".into(),
        })
        .expect("planning failure must not consume identifiers");
    assert_eq!(next.transaction.get(), before_head.transaction + 1);
    assert_eq!(next.events[0].sequence.get(), before_head.sequence + 1);
    drop(team);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn one_durable_team_owns_the_ledger_writer() {
    let path = temp_path("lock");
    let team = DurableTeamRuntime::open(&path, 1).expect("create durable Team");
    assert!(matches!(
        DurableTeamRuntime::open(&path, 1),
        Err(DurableTeamError::Ledger(LedgerError::Locked))
    ));
    drop(team);
    let reopened = DurableTeamRuntime::open(&path, 1).expect("lock releases with owner");
    drop(reopened);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn torn_final_team_transaction_recovers_only_the_complete_prefix() {
    let path = temp_path("tail");
    let mut team = DurableTeamRuntime::open(&path, 1).expect("create durable Team");
    let root = admit_root(&mut team);
    let prefix_snapshot = team.snapshot();
    let prefix_events = team.event_log().to_vec();
    let prefix_length = fs::metadata(&path).expect("ledger metadata").len();
    team.dispatch(TeamCommand::SendMessage {
        from: root,
        recipient: MessageRecipient::Team,
        body: "transaction to tear".into(),
    })
    .expect("append second transaction");
    drop(team);
    let full_length = fs::metadata(&path).expect("ledger metadata").len();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open ledger for truncation")
        .set_len(full_length - 3)
        .expect("truncate commit marker");

    let recovered = DurableTeamRuntime::open(&path, 1).expect("repair torn tail");
    assert_eq!(recovered.snapshot(), prefix_snapshot);
    assert_eq!(recovered.event_log(), prefix_events);
    assert!(recovered.recovered_tail_bytes() > 0);
    assert_eq!(
        fs::metadata(&path).expect("ledger metadata").len(),
        prefix_length
    );
    drop(recovered);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn unsupported_team_schema_and_unknown_kind_fail_closed() {
    for (name, event, expected_schema_error) in [
        (
            "schema",
            EventData {
                schema: SchemaKind::TeamEvent.current().get() + 1,
                kind: 5,
                payload: 1_u64.to_le_bytes().to_vec(),
            },
            true,
        ),
        (
            "kind",
            EventData {
                schema: SchemaKind::TeamEvent.current().get(),
                kind: 99,
                payload: Vec::new(),
            },
            false,
        ),
    ] {
        let path = temp_path(name);
        let (mut ledger, _) = FileLedger::open(&path).expect("create raw ledger");
        ledger
            .append(LedgerHead::default(), &[event])
            .expect("append raw Team event");
        drop(ledger);

        let result = DurableTeamRuntime::open(&path, 1);
        if expected_schema_error {
            assert!(matches!(
                result,
                Err(DurableTeamError::UnsupportedTeamEventSchema { .. })
            ));
        } else {
            assert!(matches!(
                result,
                Err(DurableTeamError::CorruptEvent("unknown Team Event kind"))
            ));
        }
        fs::remove_file(path).expect("cleanup ledger");
    }
}

#[test]
fn checksum_tampering_is_not_reclassified_as_a_recoverable_tail() {
    let path = temp_path("checksum");
    let mut team = DurableTeamRuntime::open(&path, 1).expect("create durable Team");
    admit_root(&mut team);
    drop(team);

    let length = fs::metadata(&path).expect("ledger metadata").len();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open ledger for tampering");
    file.seek(SeekFrom::Start(length - 5))
        .expect("seek checksum byte");
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).expect("read checksum byte");
    byte[0] ^= 0x80;
    file.seek(SeekFrom::Start(length - 5))
        .expect("seek checksum byte");
    file.write_all(&byte).expect("tamper checksum byte");
    file.sync_all().expect("sync tampering");
    drop(file);
    let corrupted = fs::read(&path).expect("read corrupted Team Ledger");

    assert!(matches!(
        DurableTeamRuntime::inspect(&path, 1),
        Err(DurableTeamError::Ledger(LedgerError::Corrupt { .. }))
    ));
    assert_eq!(
        fs::read(&path).expect("reread corrupted Team Ledger"),
        corrupted
    );

    assert!(matches!(
        DurableTeamRuntime::open(&path, 1),
        Err(DurableTeamError::Ledger(LedgerError::Corrupt { .. }))
    ));
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn every_team_transition_survives_durable_replay() {
    let path = temp_path("all-transitions");
    let mut team = DurableTeamRuntime::open(&path, 2).expect("create durable Team");
    let root = admit_root(&mut team);

    let blocker = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: TaskSpec::new("blocker", TaskScope::from_labels(["src"])),
            budget: ResourceBudget::new(200, 1),
            capabilities: CapabilitySnapshot::from_capabilities([Capability::WorkspaceRead]),
        })
        .expect("delegate blocker");
    let (blocker_task, blocker_session) = match blocker.outcome {
        CommandOutcome::Delegated { task, session, .. } => (task, session),
        other => panic!("unexpected delegation outcome: {other:?}"),
    };

    let dependent = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: TaskSpec::new("dependent", TaskScope::from_labels(["src"]))
                .with_dependencies([blocker_task]),
            budget: ResourceBudget::new(200, 1),
            capabilities: CapabilitySnapshot::from_capabilities([Capability::WorkspaceRead]),
        })
        .expect("delegate dependent");
    let dependent_session = match dependent.outcome {
        CommandOutcome::Delegated { session, .. } => session,
        other => panic!("unexpected delegation outcome: {other:?}"),
    };

    team.dispatch(TeamCommand::SendMessage {
        from: blocker_session,
        recipient: MessageRecipient::Agent(root.agent()),
        body: "failure incoming".into(),
    })
    .expect("persist Agent-targeted message");
    team.dispatch(TeamCommand::Fail {
        agent: blocker_session,
        reason: "deterministic failure".into(),
    })
    .expect("persist failure and blocked propagation");
    team.dispatch(TeamCommand::Cancel {
        agent: dependent_session,
        reason: "blocked dependency will not recover".into(),
    })
    .expect("persist blocked Agent cancellation");

    let mut capsule = CompletionCapsule::new("coordination complete");
    capsule.evidence.push("durable Ledger".into());
    capsule.changes.push("Team adapter".into());
    capsule.tests.push("replay".into());
    capsule.decisions.push("fail closed".into());
    capsule.blockers.push("trusted rebind pending".into());
    capsule.artifacts.push("ledger".into());
    capsule.residual_risks.push("fault matrix pending".into());
    team.dispatch(TeamCommand::Complete {
        agent: root,
        capsule,
    })
    .expect("persist root completion");

    let expected_snapshot = team.snapshot();
    let expected_events = team.event_log().to_vec();
    let mut seen = [false; 17];
    for event in &expected_events {
        let index = match event.kind {
            TeamEventKind::TaskCreated { .. } => 0,
            TeamEventKind::AgentCreated { .. } => 1,
            TeamEventKind::TaskOwnerAssigned { .. } => 2,
            TeamEventKind::DelegationGranted { .. } => 3,
            TeamEventKind::TaskReady { .. } => 4,
            TeamEventKind::AgentActivated { .. } => 5,
            TeamEventKind::TaskStarted { .. } => 6,
            TeamEventKind::MessageSent { .. } => 7,
            TeamEventKind::CompletionCapsuleSubmitted { .. } => 8,
            TeamEventKind::TaskSucceeded { .. } => 9,
            TeamEventKind::AgentSucceeded { .. } => 10,
            TeamEventKind::TaskFailed { .. } => 11,
            TeamEventKind::AgentFailed { .. } => 12,
            TeamEventKind::TaskCancelled { .. } => 13,
            TeamEventKind::AgentCancelled { .. } => 14,
            TeamEventKind::TaskBlocked { .. } => 15,
            TeamEventKind::AgentBlocked { .. } => 16,
            TeamEventKind::OperationCommitted { .. }
            | TeamEventKind::OperationAcknowledged { .. } => {
                panic!("standalone Durable Team must not emit Kernel operation events")
            }
            TeamEventKind::TaskRetryRequested { .. }
            | TeamEventKind::AgentRetryRequested { .. } => {
                panic!("retry events are not part of this transition fixture")
            }
        };
        seen[index] = true;
    }
    assert!(seen.into_iter().all(|present| present));
    drop(team);

    let recovered = DurableTeamRuntime::open(&path, 2).expect("replay every Team transition");
    assert_eq!(recovered.snapshot(), expected_snapshot);
    assert_eq!(recovered.event_log(), expected_events);
    drop(recovered);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn delegated_agent_inherited_model_preset_survives_reopen() {
    let path = temp_path("inherited-model-preset");
    let mut team = DurableTeamRuntime::open(&path, 2).expect("create durable Team");
    let root = admit_root(&mut team);
    let commit = team
        .dispatch(TeamCommand::DelegateWithModelPreset {
            parent: root,
            task: TaskSpec::new("child provider turn", TaskScope::from_labels(["src"])),
            budget: ResourceBudget::new(200, 1),
            capabilities: CapabilitySnapshot::from_capabilities([Capability::WorkspaceRead]),
            inherited_model_preset: Some(
                InheritedModelPreset::new("frontier").expect("valid Model Preset"),
            ),
        })
        .expect("delegate child with inherited Model Preset");
    let child = match commit.outcome {
        CommandOutcome::Delegated { agent, .. } => agent,
        other => panic!("unexpected delegation outcome: {other:?}"),
    };
    let expected = team.snapshot();
    assert_eq!(
        expected
            .agent(child)
            .and_then(|agent| agent.inherited_model_preset.as_ref())
            .map(InheritedModelPreset::id),
        Some("frontier")
    );
    drop(team);

    let recovered = DurableTeamRuntime::open(&path, 2).expect("reopen durable Team");
    assert_eq!(recovered.snapshot(), expected);
    assert_eq!(
        recovered
            .snapshot()
            .agent(child)
            .and_then(|agent| agent.inherited_model_preset.as_ref())
            .map(InheritedModelPreset::id),
        Some("frontier")
    );
    drop(recovered);
    fs::remove_file(path).expect("cleanup ledger");
}
