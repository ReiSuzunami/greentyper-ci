use greentyper_core::agent_team::{
    AgentSession, AgentStatus, Capability, CapabilitySnapshot, CommandOutcome, CommitDurability,
    CompletionCapsule, MessageRecipient, RecoveryError, ResourceBudget, TaskScope, TaskSpec,
    TaskStatus, TeamCommand, TeamError, TeamEventKind, TeamRuntime,
};

fn scope(labels: &[&str]) -> TaskScope {
    TaskScope::from_labels(labels.iter().copied())
}

fn capabilities(values: &[Capability]) -> CapabilitySnapshot {
    CapabilitySnapshot::from_capabilities(values.iter().cloned())
}

fn budget(token_units: u64, tool_calls: u32) -> ResourceBudget {
    ResourceBudget::new(token_units, tool_calls)
}

fn task(title: &str, labels: &[&str]) -> TaskSpec {
    TaskSpec::new(title, scope(labels))
}

fn admitted_root(team: &mut TeamRuntime, active_limit: usize) -> AgentSession {
    assert_eq!(team.max_active_agents(), active_limit);
    let commit = team
        .dispatch(TeamCommand::AdmitRoot {
            task: task("coordinate implementation", &["repo", "src", "tests"]),
            budget: budget(1_000, 8),
            capabilities: capabilities(&[
                Capability::WorkspaceRead,
                Capability::WorkspaceWrite,
                Capability::Process,
            ]),
        })
        .expect("root admission should succeed");

    match commit.outcome {
        CommandOutcome::RootAdmitted { session, .. } => session,
        other => panic!("unexpected admission outcome: {other:?}"),
    }
}

fn delegated_child(
    team: &mut TeamRuntime,
    parent: AgentSession,
    title: &str,
    dependencies: &[greentyper_core::agent_team::TaskId],
) -> AgentSession {
    let commit = team
        .dispatch(TeamCommand::Delegate {
            parent,
            task: task(title, &["src"]).with_dependencies(dependencies.iter().copied()),
            budget: budget(200, 1),
            capabilities: capabilities(&[Capability::WorkspaceRead]),
        })
        .expect("delegation should succeed");

    match commit.outcome {
        CommandOutcome::Delegated { session, .. } => session,
        other => panic!("unexpected delegation outcome: {other:?}"),
    }
}

#[test]
fn root_admission_is_one_atomic_ordered_transaction() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let commit = team
        .dispatch(TeamCommand::AdmitRoot {
            task: task("root", &["repo"]),
            budget: budget(100, 2),
            capabilities: capabilities(&[Capability::WorkspaceRead]),
        })
        .expect("root admission should succeed");

    assert_eq!(commit.events.len(), 6);
    assert_eq!(commit.durability, CommitDurability::Volatile);
    assert!(matches!(
        commit.events[0].kind,
        TeamEventKind::TaskCreated { .. }
    ));
    assert!(matches!(
        commit.events[1].kind,
        TeamEventKind::AgentCreated { .. }
    ));
    assert!(matches!(
        commit.events[2].kind,
        TeamEventKind::TaskOwnerAssigned { .. }
    ));
    assert!(matches!(
        commit.events[3].kind,
        TeamEventKind::TaskReady { .. }
    ));
    assert!(matches!(
        commit.events[4].kind,
        TeamEventKind::AgentActivated { .. }
    ));
    assert!(matches!(
        commit.events[5].kind,
        TeamEventKind::TaskStarted { .. }
    ));

    for (index, event) in commit.events.iter().enumerate() {
        assert_eq!(event.sequence.get(), (index + 1) as u64);
        assert_eq!(event.transaction, commit.transaction);
        assert_eq!(event.index_in_transaction, index as u32);
        assert_eq!(event.events_in_transaction, 6);
    }

    let snapshot = team.snapshot();
    assert_eq!(snapshot.active_agent_count(), 1);
    assert_eq!(snapshot.tasks[0].status, TaskStatus::Running);
    assert_eq!(snapshot.agents[0].status, AgentStatus::Active);
}

#[test]
fn delegation_can_only_reduce_scope_capability_and_unreserved_budget() {
    let mut team = TeamRuntime::new(4).expect("valid active limit");
    let root = admitted_root(&mut team, 4);

    delegated_child(&mut team, root, "valid child", &[]);

    let capability_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task("network child", &["src"]),
            budget: budget(100, 1),
            capabilities: capabilities(&[Capability::Network]),
        })
        .expect_err("network authority was not held by the parent");
    assert_eq!(
        capability_error,
        TeamError::CapabilityExpansion {
            parent: root.agent()
        }
    );

    let scope_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task("docs child", &["docs"]),
            budget: budget(100, 1),
            capabilities: capabilities(&[Capability::WorkspaceRead]),
        })
        .expect_err("docs scope was not held by the parent");
    assert_eq!(
        scope_error,
        TeamError::ScopeExpansion {
            parent: root.agent()
        }
    );

    let budget_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task("oversized child", &["src"]),
            budget: budget(900, 8),
            capabilities: capabilities(&[Capability::WorkspaceRead]),
        })
        .expect_err("the first child already reserved part of the parent budget");
    assert_eq!(
        budget_error,
        TeamError::BudgetExpansion {
            parent: root.agent()
        }
    );

    let snapshot = team.snapshot();
    let root_view = snapshot
        .agents
        .iter()
        .find(|agent| agent.id == root.agent())
        .expect("root should remain present");
    assert_eq!(root_view.reserved_budget, budget(200, 1));
}

#[test]
fn dormant_agent_activates_when_an_active_child_completes() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let first = delegated_child(&mut team, root, "first", &[]);
    let second = delegated_child(&mut team, root, "second", &[]);

    let before = team.snapshot();
    assert_eq!(before.active_agent_count(), 2);
    assert_eq!(
        before
            .agent(second.agent())
            .expect("second child exists")
            .status,
        AgentStatus::Dormant
    );
    assert_eq!(
        before
            .task(
                before
                    .agent(second.agent())
                    .expect("second child exists")
                    .task,
            )
            .expect("second task exists")
            .status,
        TaskStatus::Ready
    );

    team.dispatch(TeamCommand::Complete {
        agent: first,
        capsule: CompletionCapsule::new("first child done"),
    })
    .expect("first child should complete");

    let after = team.snapshot();
    assert_eq!(after.active_agent_count(), 2);
    assert_eq!(
        after
            .agent(first.agent())
            .expect("first child exists")
            .status,
        AgentStatus::Succeeded
    );
    assert_eq!(
        after
            .agent(second.agent())
            .expect("second child exists")
            .status,
        AgentStatus::Active
    );
}

#[test]
fn failed_dependency_blocks_waiting_task_without_polling() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let first = delegated_child(&mut team, root, "producer", &[]);
    let first_task = team
        .snapshot()
        .agent(first.agent())
        .expect("producer exists")
        .task;
    let second = delegated_child(&mut team, root, "consumer", &[first_task]);

    team.dispatch(TeamCommand::Fail {
        agent: first,
        reason: "fixture failure".into(),
    })
    .expect("active producer may fail explicitly");

    let snapshot = team.snapshot();
    assert_eq!(
        snapshot
            .agent(second.agent())
            .expect("consumer exists")
            .status,
        AgentStatus::Blocked
    );
    assert_eq!(
        snapshot
            .task(
                snapshot
                    .agent(second.agent())
                    .expect("consumer exists")
                    .task,
            )
            .expect("consumer task exists")
            .status,
        TaskStatus::Blocked {
            blocked_by: first_task
        }
    );

    let parent_error = team
        .dispatch(TeamCommand::Complete {
            agent: root,
            capsule: CompletionCapsule::new("cannot skip blocked child"),
        })
        .expect_err("Blocked children remain accountable until explicitly resolved");
    assert_eq!(
        parent_error,
        TeamError::OutstandingChildren {
            parent: root.agent()
        }
    );
    team.dispatch(TeamCommand::Cancel {
        agent: second,
        reason: "dependency cannot recover in this slice".into(),
    })
    .expect("Blocked child can be explicitly cancelled");
    team.dispatch(TeamCommand::Complete {
        agent: root,
        capsule: CompletionCapsule::new("failure accounted for"),
    })
    .expect("parent can finish after all children are terminal");
}

#[test]
fn active_parent_can_requeue_failed_child_without_replaying_effects() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let child = delegated_child(&mut team, root, "retryable child", &[]);
    team.dispatch(TeamCommand::Fail {
        agent: child,
        reason: "provider unavailable".into(),
    })
    .expect("child failure should persist");
    let commit = team
        .dispatch(TeamCommand::Retry {
            requester: root,
            agent: child.agent(),
        })
        .expect("active parent may explicitly requeue child");
    assert!(
        commit
            .events
            .iter()
            .any(|event| matches!(event.kind, TeamEventKind::TaskRetryRequested { .. }))
    );
    assert!(
        commit
            .events
            .iter()
            .any(|event| matches!(event.kind, TeamEventKind::AgentRetryRequested { .. }))
    );
    let snapshot = team.snapshot();
    assert_eq!(
        snapshot.agent(child.agent()).expect("child exists").status,
        AgentStatus::Active
    );
    assert_eq!(
        snapshot
            .task(snapshot.agent(child.agent()).expect("child exists").task)
            .expect("task exists")
            .status,
        TaskStatus::Running
    );
}

#[test]
fn active_agent_messages_are_ledgered_and_dormant_agents_cannot_send() {
    let mut team = TeamRuntime::new(1).expect("valid active limit");
    let root = admitted_root(&mut team, 1);
    let child = delegated_child(&mut team, root, "queued child", &[]);

    let commit = team
        .dispatch(TeamCommand::SendMessage {
            from: root,
            recipient: MessageRecipient::Team,
            body: "root evidence".into(),
        })
        .expect("active root may send");
    assert!(matches!(
        commit.outcome,
        CommandOutcome::MessageAccepted { .. }
    ));
    assert_eq!(team.snapshot().messages.len(), 1);

    let error = team
        .dispatch(TeamCommand::SendMessage {
            from: child,
            recipient: MessageRecipient::Agent(root.agent()),
            body: "should not run while dormant".into(),
        })
        .expect_err("dormant Agents do not execute commands");
    assert_eq!(
        error,
        TeamError::AgentNotActive {
            agent: child.agent()
        }
    );
}

#[test]
fn parent_cannot_finish_before_children_reach_terminal_states() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let child = delegated_child(&mut team, root, "child", &[]);

    let error = team
        .dispatch(TeamCommand::Complete {
            agent: root,
            capsule: CompletionCapsule::new("too early"),
        })
        .expect_err("root must account for its child");
    assert_eq!(
        error,
        TeamError::OutstandingChildren {
            parent: root.agent()
        }
    );

    team.dispatch(TeamCommand::Cancel {
        agent: child,
        reason: "superseded".into(),
    })
    .expect("leaf child may be cancelled");
    team.dispatch(TeamCommand::Complete {
        agent: root,
        capsule: CompletionCapsule::new("root done"),
    })
    .expect("terminal child no longer blocks parent completion");
}

#[test]
fn delegation_cannot_depend_on_the_parent_task() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let root_task = team
        .snapshot()
        .agent(root.agent())
        .expect("root exists")
        .task;

    let error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task("cyclic child", &["src"]).with_dependencies([root_task]),
            budget: budget(100, 1),
            capabilities: capabilities(&[Capability::WorkspaceRead]),
        })
        .expect_err("parent waits for child, so child cannot wait for parent");
    assert_eq!(
        error,
        TeamError::DependencyCycle {
            dependency: root_task
        }
    );
}

#[test]
fn delegation_cannot_depend_on_any_ancestor_task() {
    let mut team = TeamRuntime::new(3).expect("valid active limit");
    let root = admitted_root(&mut team, 3);
    let root_task = team
        .snapshot()
        .agent(root.agent())
        .expect("root exists")
        .task;
    let child = delegated_child(&mut team, root, "child", &[]);

    let error = team
        .dispatch(TeamCommand::Delegate {
            parent: child,
            task: task("cyclic grandchild", &["src"]).with_dependencies([root_task]),
            budget: budget(50, 0),
            capabilities: capabilities(&[Capability::WorkspaceRead]),
        })
        .expect_err("descendant cannot wait on any ancestor");
    assert_eq!(
        error,
        TeamError::DependencyCycle {
            dependency: root_task
        }
    );
}

#[test]
fn recovery_rebuilds_the_same_snapshot_and_rejects_partial_transactions() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let child = delegated_child(&mut team, root, "child", &[]);
    team.dispatch(TeamCommand::SendMessage {
        from: child,
        recipient: MessageRecipient::Agent(root.agent()),
        body: "evidence ready".into(),
    })
    .expect("active child may send");
    team.dispatch(TeamCommand::Complete {
        agent: child,
        capsule: CompletionCapsule::new("child done"),
    })
    .expect("child should complete");

    let events = team.event_log().to_vec();
    let mut recovered = TeamRuntime::recover(2, events.clone()).expect("complete ledger recovers");
    assert_eq!(recovered.snapshot(), team.snapshot());
    assert_eq!(recovered.event_log(), events);

    let mut partial = events;
    partial.pop();
    assert!(matches!(
        TeamRuntime::recover(2, partial),
        Err(RecoveryError::IncompleteTransaction { .. })
    ));

    let old_session_error = recovered
        .dispatch(TeamCommand::SendMessage {
            from: root,
            recipient: MessageRecipient::Team,
            body: "stale session".into(),
        })
        .expect_err("process-local sessions do not survive recovery");
    assert_eq!(
        old_session_error,
        TeamError::InvalidAgentSession {
            agent: root.agent()
        }
    );
}

#[test]
fn recovery_accepts_a_higher_active_limit_without_rewriting_old_scheduling() {
    let mut old = TeamRuntime::new(1).expect("old active limit");
    let root = admitted_root(&mut old, 1);
    let child = delegated_child(&mut old, root, "old dormant child", &[]);
    assert_eq!(
        old.snapshot()
            .agent(child.agent())
            .expect("old child")
            .status,
        AgentStatus::Dormant
    );

    let recovered = TeamRuntime::recover(2, old.event_log().iter().cloned())
        .expect("higher active limit replays old scheduling");
    assert_eq!(recovered.max_active_agents(), 2);
    assert_eq!(recovered.snapshot(), old.snapshot());
}

#[test]
fn every_complete_transaction_prefix_replays_deterministically() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let first_snapshot = team.snapshot();
    let first_revision = first_snapshot.revision.get() as usize;

    let child = delegated_child(&mut team, root, "child", &[]);
    let second_snapshot = team.snapshot();
    let second_revision = second_snapshot.revision.get() as usize;

    team.dispatch(TeamCommand::Complete {
        agent: child,
        capsule: CompletionCapsule::new("done"),
    })
    .expect("child should complete");
    let third_snapshot = team.snapshot();
    let events = team.event_log();

    for (end, expected) in [
        (first_revision, first_snapshot),
        (second_revision, second_snapshot),
        (events.len(), third_snapshot),
    ] {
        let replayed = TeamRuntime::recover(2, events[..end].iter().cloned())
            .expect("complete transaction prefix should replay");
        assert_eq!(replayed.snapshot(), expected);
    }
}

#[test]
fn event_payloads_are_bounded_before_entering_the_ledger() {
    let mut team = TeamRuntime::new(1).expect("valid active limit");
    let root = admitted_root(&mut team, 1);
    let initial_event_count = team.event_log().len();
    let root_task = team
        .snapshot()
        .agent(root.agent())
        .expect("root exists")
        .task;

    let title_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task(&"t".repeat(1025), &["src"]),
            budget: budget(10, 0),
            capabilities: CapabilitySnapshot::empty(),
        })
        .expect_err("Task titles are bounded");
    assert_eq!(title_error, TeamError::TaskTitleTooLarge);

    let label_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: TaskSpec::new(
                "oversized scope label",
                TaskScope::from_labels(["s".repeat(257)]),
            ),
            budget: budget(10, 0),
            capabilities: CapabilitySnapshot::empty(),
        })
        .expect_err("individual scope labels are bounded");
    assert_eq!(label_error, TeamError::ScopeLabelTooLarge);

    let scope_count_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: TaskSpec::new(
                "too many scope labels",
                TaskScope::from_labels((0..65).map(|index| format!("scope-{index}"))),
            ),
            budget: budget(10, 0),
            capabilities: CapabilitySnapshot::empty(),
        })
        .expect_err("scope label count is bounded");
    assert_eq!(scope_count_error, TeamError::TooManyScopeLabels);

    let tool_name_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task("oversized tool name", &["src"]),
            budget: budget(10, 0),
            capabilities: capabilities(&[Capability::Tool("x".repeat(257))]),
        })
        .expect_err("Tool capability names are bounded");
    assert_eq!(tool_name_error, TeamError::ToolNameTooLarge);

    let capability_count_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task("too many capabilities", &["src"]),
            budget: budget(10, 0),
            capabilities: CapabilitySnapshot::from_capabilities(
                (0..65).map(|index| Capability::Tool(format!("tool-{index}"))),
            ),
        })
        .expect_err("Capability count is bounded before subset evaluation");
    assert_eq!(capability_count_error, TeamError::TooManyCapabilities);

    let dependency_error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task("too many dependencies", &["src"])
                .with_dependencies(std::iter::repeat_n(root_task, 257)),
            budget: budget(10, 0),
            capabilities: CapabilitySnapshot::empty(),
        })
        .expect_err("dependency count is bounded before canonicalization");
    assert_eq!(dependency_error, TeamError::TooManyDependencies);

    let message_error = team
        .dispatch(TeamCommand::SendMessage {
            from: root,
            recipient: MessageRecipient::Team,
            body: "m".repeat(64 * 1024 + 1),
        })
        .expect_err("oversized messages consume unbounded resident memory");
    assert_eq!(message_error, TeamError::MessageTooLarge);

    let completion_error = team
        .dispatch(TeamCommand::Complete {
            agent: root,
            capsule: CompletionCapsule::new("c".repeat(256 * 1024 + 1)),
        })
        .expect_err("oversized capsules should become Artifacts later");
    assert_eq!(completion_error, TeamError::CompletionCapsuleTooLarge);

    let mut entry_heavy_capsule = CompletionCapsule::new("bounded bytes");
    entry_heavy_capsule.evidence = vec![String::new(); 1025];
    let entry_count_error = team
        .dispatch(TeamCommand::Complete {
            agent: root,
            capsule: entry_heavy_capsule,
        })
        .expect_err("empty list entries cannot evade Capsule memory accounting");
    assert_eq!(
        entry_count_error,
        TeamError::TooManyCompletionCapsuleEntries
    );

    let reason_error = team
        .dispatch(TeamCommand::Fail {
            agent: root,
            reason: "r".repeat(8 * 1024 + 1),
        })
        .expect_err("terminal reasons are bounded too");
    assert_eq!(reason_error, TeamError::ReasonTooLarge);
    assert_eq!(team.event_log().len(), initial_event_count);
    assert_eq!(team.snapshot().revision.get() as usize, initial_event_count);
}

#[test]
fn failed_commands_leave_projection_identifiers_and_ledger_unchanged() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let before = team.snapshot();
    let before_events = team.event_log().to_vec();
    let previous_transaction = before_events
        .last()
        .expect("admission event")
        .transaction
        .get();

    let error = team
        .dispatch(TeamCommand::Delegate {
            parent: root,
            task: task("unauthorized network child", &["src"]),
            budget: budget(100, 1),
            capabilities: capabilities(&[Capability::Network]),
        })
        .expect_err("planning failure must be atomic");
    assert_eq!(
        error,
        TeamError::CapabilityExpansion {
            parent: root.agent()
        }
    );
    assert_eq!(team.snapshot(), before);
    assert_eq!(team.event_log(), before_events);

    let next = team
        .dispatch(TeamCommand::SendMessage {
            from: root,
            recipient: MessageRecipient::Team,
            body: "next valid command".into(),
        })
        .expect("failed command must not consume identifiers");
    assert_eq!(next.transaction.get(), previous_transaction + 1);
    assert_eq!(
        next.events[0].sequence.get(),
        before_events.len() as u64 + 1
    );
}

#[test]
fn sessions_cannot_cross_runtime_boundaries() {
    let mut first = TeamRuntime::new(1).expect("valid active limit");
    let first_root = admitted_root(&mut first, 1);
    let mut second = TeamRuntime::new(1).expect("valid active limit");
    admitted_root(&mut second, 1);
    let before = second.snapshot();

    let error = second
        .dispatch(TeamCommand::SendMessage {
            from: first_root,
            recipient: MessageRecipient::Team,
            body: "cross-team impersonation".into(),
        })
        .expect_err("Agent session belongs to exactly one Runtime instance");
    assert_eq!(
        error,
        TeamError::InvalidAgentSession {
            agent: first_root.agent()
        }
    );
    assert_eq!(second.snapshot(), before);
}

#[test]
fn recovery_rejects_transaction_metadata_and_transition_tampering() {
    let mut team = TeamRuntime::new(1).expect("valid active limit");
    let root = admitted_root(&mut team, 1);
    team.dispatch(TeamCommand::SendMessage {
        from: root,
        recipient: MessageRecipient::Team,
        body: "second transaction".into(),
    })
    .expect("message supplies another transaction identifier");
    let events = team.event_log().to_vec();

    let mut bad_sequence = events.clone();
    bad_sequence[1].sequence = bad_sequence[0].sequence;
    assert!(matches!(
        TeamRuntime::recover(1, bad_sequence),
        Err(RecoveryError::SequenceMismatch { .. })
    ));

    let mut bad_transaction = events.clone();
    bad_transaction[1].transaction = events[6].transaction;
    assert!(matches!(
        TeamRuntime::recover(1, bad_transaction),
        Err(RecoveryError::TransactionMismatch { .. })
    ));

    let mut bad_position = events.clone();
    bad_position[1].index_in_transaction = 0;
    assert!(matches!(
        TeamRuntime::recover(1, bad_position),
        Err(RecoveryError::InvalidTransactionPosition { .. })
    ));

    let mut bad_size = events.clone();
    bad_size[1].events_in_transaction -= 1;
    assert!(matches!(
        TeamRuntime::recover(1, bad_size),
        Err(RecoveryError::InvalidTransactionSize { .. })
    ));

    let root_task = team
        .snapshot()
        .agent(root.agent())
        .expect("root exists")
        .task;
    let mut invalid_transition = events;
    invalid_transition[3].kind = TeamEventKind::TaskSucceeded { task: root_task };
    assert!(matches!(
        TeamRuntime::recover(1, invalid_transition),
        Err(RecoveryError::InvalidEvent { .. })
    ));

    let mut oversized_payload = team.event_log().to_vec();
    oversized_payload[0].kind = TeamEventKind::TaskCreated {
        task: root_task,
        spec: task(&"t".repeat(1025), &["repo"]),
    };
    assert!(matches!(
        TeamRuntime::recover(1, oversized_payload),
        Err(RecoveryError::InvalidEvent {
            source: TeamError::TaskTitleTooLarge,
            ..
        })
    ));
}

#[test]
fn recovery_rejects_terminal_parent_with_non_terminal_child() {
    let mut team = TeamRuntime::new(2).expect("valid active limit");
    let root = admitted_root(&mut team, 2);
    let root_task = team
        .snapshot()
        .agent(root.agent())
        .expect("root exists")
        .task;
    let child = delegated_child(&mut team, root, "child", &[]);
    team.dispatch(TeamCommand::Cancel {
        agent: child,
        reason: "generate terminal transaction shape".into(),
    })
    .expect("leaf child can be cancelled");

    let mut forged = team.event_log().to_vec();
    let terminal_start = forged.len() - 2;
    forged[terminal_start].kind = TeamEventKind::TaskCancelled {
        task: root_task,
        reason: "forged parent cancellation".into(),
    };
    forged[terminal_start + 1].kind = TeamEventKind::AgentCancelled {
        agent: root.agent(),
    };

    assert!(matches!(
        TeamRuntime::recover(2, forged),
        Err(RecoveryError::InvalidEvent {
            source: TeamError::OutstandingChildren { .. },
            ..
        })
    ));
}

#[test]
fn invalid_active_limit_and_duplicate_root_fail_closed() {
    assert_eq!(
        TeamRuntime::new(0).expect_err("zero active slots are invalid"),
        TeamError::InvalidActiveAgentLimit
    );

    let mut team = TeamRuntime::new(1).expect("valid active limit");
    admitted_root(&mut team, 1);
    let error = team
        .dispatch(TeamCommand::AdmitRoot {
            task: task("another root", &["repo"]),
            budget: budget(10, 0),
            capabilities: CapabilitySnapshot::empty(),
        })
        .expect_err("one runtime represents one Agent Team");
    assert_eq!(error, TeamError::RootAlreadyAdmitted);
}
