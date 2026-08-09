//! Deterministic Agent Team orchestration policy.
//!
//! [`TeamRuntime`] is the process-local policy interface. Commands become
//! atomic event transactions before their resulting state is visible.
//! [`DurableTeamRuntime`] adds the file Ledger adapter without exposing storage
//! mechanics through the policy interface.

mod persistence;

pub use persistence::{DurableTeamError, DurableTeamRuntime};

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_REASON_BYTES: usize = 8 * 1024;
const MAX_CAPSULE_BYTES: usize = 256 * 1024;
const MAX_TASK_TITLE_BYTES: usize = 1024;
const MAX_SCOPE_LABEL_BYTES: usize = 256;
const MAX_SCOPE_LABELS: usize = 64;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_CAPABILITIES: usize = 64;
const MAX_TASK_DEPENDENCIES: usize = 256;
const MAX_CAPSULE_ENTRIES: usize = 1024;
static NEXT_RUNTIME_AUTHORITY: AtomicU64 = AtomicU64::new(1);

macro_rules! identifier {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

identifier!(TaskId);
identifier!(AgentId);
identifier!(MessageId);
identifier!(EventSeq);
identifier!(TransactionId);

/// Process-local authority to act as one Agent.
///
/// The fields are deliberately private: canonical Agent identifiers are safe to
/// inspect, but they are not execution authority. Sessions are never persisted
/// and are invalidated when a Team is recovered into a new Runtime instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentSession {
    agent: AgentId,
    runtime_authority: u64,
}

impl AgentSession {
    #[must_use]
    pub const fn agent(&self) -> AgentId {
        self.agent
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    WorkspaceRead,
    WorkspaceWrite,
    Process,
    Network,
    Tool(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilitySnapshot {
    capabilities: BTreeSet<Capability>,
}

impl CapabilitySnapshot {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            capabilities: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn from_capabilities(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn contains(&self, capability: &Capability) -> bool {
        self.capabilities.contains(capability)
    }

    #[must_use]
    pub fn is_subset_of(&self, parent: &Self) -> bool {
        self.capabilities.is_subset(&parent.capabilities)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.capabilities.iter()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskScope {
    labels: BTreeSet<String>,
}

impl TaskScope {
    #[must_use]
    pub fn from_labels(labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            labels: labels.into_iter().map(Into::into).collect(),
        }
    }

    #[must_use]
    pub fn is_subset_of(&self, parent: &Self) -> bool {
        self.labels.is_subset(&parent.labels)
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.labels.iter().map(String::as_str)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceBudget {
    pub token_units: u64,
    pub tool_calls: u32,
}

impl ResourceBudget {
    #[must_use]
    pub const fn new(token_units: u64, tool_calls: u32) -> Self {
        Self {
            token_units,
            tool_calls,
        }
    }

    #[must_use]
    pub const fn contains(self, child: Self) -> bool {
        child.token_units <= self.token_units && child.tool_calls <= self.tool_calls
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            token_units: self.token_units.checked_add(other.token_units)?,
            tool_calls: self.tool_calls.checked_add(other.tool_calls)?,
        })
    }

    fn remaining_after(self, reserved: Self) -> Option<Self> {
        Some(Self {
            token_units: self.token_units.checked_sub(reserved.token_units)?,
            tool_calls: self.tool_calls.checked_sub(reserved.tool_calls)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSpec {
    title: String,
    scope: TaskScope,
    dependencies: Vec<TaskId>,
}

impl TaskSpec {
    #[must_use]
    pub fn new(title: impl Into<String>, scope: TaskScope) -> Self {
        Self {
            title: title.into(),
            scope,
            dependencies: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_dependencies(mut self, dependencies: impl IntoIterator<Item = TaskId>) -> Self {
        self.dependencies = dependencies.into_iter().collect();
        self
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionCapsule {
    pub outcome: String,
    pub evidence: Vec<String>,
    pub changes: Vec<String>,
    pub tests: Vec<String>,
    pub decisions: Vec<String>,
    pub blockers: Vec<String>,
    pub artifacts: Vec<String>,
    pub residual_risks: Vec<String>,
}

impl CompletionCapsule {
    #[must_use]
    pub fn new(outcome: impl Into<String>) -> Self {
        Self {
            outcome: outcome.into(),
            ..Self::default()
        }
    }

    fn approximate_bytes(&self) -> Option<usize> {
        let mut total = self.outcome.len();
        for values in [
            &self.evidence,
            &self.changes,
            &self.tests,
            &self.decisions,
            &self.blockers,
            &self.artifacts,
            &self.residual_risks,
        ] {
            for value in values {
                total = total.checked_add(value.len())?;
            }
        }
        Some(total)
    }

    fn entry_count(&self) -> Option<usize> {
        [
            &self.evidence,
            &self.changes,
            &self.tests,
            &self.decisions,
            &self.blockers,
            &self.artifacts,
            &self.residual_risks,
        ]
        .into_iter()
        .try_fold(0_usize, |total, values| total.checked_add(values.len()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageRecipient {
    Agent(AgentId),
    Team,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TeamCommand {
    AdmitRoot {
        task: TaskSpec,
        budget: ResourceBudget,
        capabilities: CapabilitySnapshot,
    },
    Delegate {
        parent: AgentSession,
        task: TaskSpec,
        budget: ResourceBudget,
        capabilities: CapabilitySnapshot,
    },
    SendMessage {
        from: AgentSession,
        recipient: MessageRecipient,
        body: String,
    },
    Complete {
        agent: AgentSession,
        capsule: CompletionCapsule,
    },
    Fail {
        agent: AgentSession,
        reason: String,
    },
    Cancel {
        agent: AgentSession,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStatus {
    Dormant,
    Active,
    Blocked,
    Succeeded,
    Failed,
    Cancelled,
}

impl AgentStatus {
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    Pending,
    Ready,
    Running,
    Blocked { blocked_by: TaskId },
    Succeeded,
    Failed { reason: String },
    Cancelled { reason: String },
}

impl TaskStatus {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }

    fn blocks_dependents(&self) -> bool {
        matches!(
            self,
            Self::Blocked { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskView {
    pub id: TaskId,
    pub title: String,
    pub scope: TaskScope,
    pub dependencies: Vec<TaskId>,
    pub owner: Option<AgentId>,
    pub status: TaskStatus,
    pub completion: Option<CompletionCapsule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentView {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    pub task: TaskId,
    pub status: AgentStatus,
    pub budget: ResourceBudget,
    pub reserved_budget: ResourceBudget,
    pub capabilities: CapabilitySnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageView {
    pub id: MessageId,
    pub from: AgentId,
    pub recipient: MessageRecipient,
    pub body: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamSnapshot {
    pub revision: EventSeq,
    pub tasks: Vec<TaskView>,
    pub agents: Vec<AgentView>,
    pub messages: Vec<MessageView>,
}

impl TeamSnapshot {
    #[must_use]
    pub fn active_agent_count(&self) -> usize {
        self.agents
            .iter()
            .filter(|agent| agent.status == AgentStatus::Active)
            .count()
    }

    #[must_use]
    pub fn agent(&self, id: AgentId) -> Option<&AgentView> {
        self.agents.iter().find(|agent| agent.id == id)
    }

    #[must_use]
    pub fn task(&self, id: TaskId) -> Option<&TaskView> {
        self.tasks.iter().find(|task| task.id == id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TeamEventKind {
    TaskCreated {
        task: TaskId,
        spec: TaskSpec,
    },
    AgentCreated {
        agent: AgentId,
        task: TaskId,
        parent: Option<AgentId>,
        budget: ResourceBudget,
        capabilities: CapabilitySnapshot,
    },
    TaskOwnerAssigned {
        task: TaskId,
        agent: AgentId,
    },
    DelegationGranted {
        parent: AgentId,
        child: AgentId,
    },
    TaskReady {
        task: TaskId,
    },
    AgentActivated {
        agent: AgentId,
    },
    TaskStarted {
        task: TaskId,
    },
    MessageSent {
        message: MessageId,
        from: AgentId,
        recipient: MessageRecipient,
        body: String,
    },
    CompletionCapsuleSubmitted {
        task: TaskId,
        agent: AgentId,
        capsule: CompletionCapsule,
    },
    TaskSucceeded {
        task: TaskId,
    },
    AgentSucceeded {
        agent: AgentId,
    },
    TaskFailed {
        task: TaskId,
        reason: String,
    },
    AgentFailed {
        agent: AgentId,
    },
    TaskCancelled {
        task: TaskId,
        reason: String,
    },
    AgentCancelled {
        agent: AgentId,
    },
    TaskBlocked {
        task: TaskId,
        blocked_by: TaskId,
    },
    AgentBlocked {
        agent: AgentId,
        blocked_by: TaskId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamEvent {
    pub sequence: EventSeq,
    pub transaction: TransactionId,
    pub index_in_transaction: u32,
    pub events_in_transaction: u32,
    pub kind: TeamEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    RootAdmitted {
        task: TaskId,
        agent: AgentId,
        session: AgentSession,
    },
    Delegated {
        task: TaskId,
        agent: AgentId,
        session: AgentSession,
    },
    MessageAccepted {
        message: MessageId,
    },
    StateChanged {
        task: TaskId,
        agent: AgentId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamCommit {
    pub transaction: TransactionId,
    pub revision: EventSeq,
    pub durability: CommitDurability,
    pub events: Vec<TeamEvent>,
    pub outcome: CommandOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeamDurabilityReceipt {
    pub transaction: TransactionId,
    pub first_sequence: EventSeq,
    pub last_sequence: EventSeq,
    pub event_count: u32,
    pub transaction_crc32c: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitDurability {
    Volatile,
    Synchronous(TeamDurabilityReceipt),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TeamError {
    InvalidActiveAgentLimit,
    RootAlreadyAdmitted,
    RootDependenciesNotAllowed,
    InvalidTaskTitle,
    TaskTitleTooLarge,
    InvalidScope,
    ScopeLabelTooLarge,
    TooManyScopeLabels,
    InvalidCapability,
    ToolNameTooLarge,
    TooManyCapabilities,
    InvalidBudget,
    TooManyDependencies,
    DuplicateDependency {
        task: TaskId,
    },
    DependencyCycle {
        dependency: TaskId,
    },
    UnknownTask {
        task: TaskId,
    },
    UnknownAgent {
        agent: AgentId,
    },
    InvalidAgentSession {
        agent: AgentId,
    },
    AgentNotActive {
        agent: AgentId,
    },
    ScopeExpansion {
        parent: AgentId,
    },
    CapabilityExpansion {
        parent: AgentId,
    },
    BudgetExpansion {
        parent: AgentId,
    },
    InvalidMessage,
    MessageTooLarge,
    InvalidCompletionCapsule,
    CompletionCapsuleTooLarge,
    TooManyCompletionCapsuleEntries,
    InvalidReason,
    ReasonTooLarge,
    OutstandingChildren {
        parent: AgentId,
    },
    InvalidTransition {
        agent: AgentId,
        status: AgentStatus,
        operation: &'static str,
    },
    IdentifierExhausted,
    InvariantViolation {
        detail: String,
    },
}

impl fmt::Display for TeamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidActiveAgentLimit => {
                write!(formatter, "active Agent limit must be non-zero")
            }
            Self::RootAlreadyAdmitted => write!(formatter, "Agent Team already has a root"),
            Self::RootDependenciesNotAllowed => {
                write!(formatter, "root Task cannot have dependencies")
            }
            Self::InvalidTaskTitle => write!(formatter, "Task title must be non-empty"),
            Self::TaskTitleTooLarge => {
                write!(formatter, "Task title exceeds {MAX_TASK_TITLE_BYTES} bytes")
            }
            Self::InvalidScope => write!(formatter, "Task scope contains an invalid label"),
            Self::ScopeLabelTooLarge => {
                write!(
                    formatter,
                    "Task scope label exceeds {MAX_SCOPE_LABEL_BYTES} bytes"
                )
            }
            Self::TooManyScopeLabels => {
                write!(formatter, "Task scope exceeds {MAX_SCOPE_LABELS} labels")
            }
            Self::InvalidCapability => write!(
                formatter,
                "Capability Snapshot contains an invalid capability"
            ),
            Self::ToolNameTooLarge => {
                write!(
                    formatter,
                    "Tool capability name exceeds {MAX_TOOL_NAME_BYTES} bytes"
                )
            }
            Self::TooManyCapabilities => write!(
                formatter,
                "Capability Snapshot exceeds {MAX_CAPABILITIES} entries"
            ),
            Self::InvalidBudget => write!(formatter, "resource budget must include token units"),
            Self::TooManyDependencies => write!(
                formatter,
                "Task exceeds {MAX_TASK_DEPENDENCIES} dependencies"
            ),
            Self::DuplicateDependency { task } => {
                write!(formatter, "duplicate Task dependency {}", task.get())
            }
            Self::DependencyCycle { dependency } => write!(
                formatter,
                "Task dependency {} creates a Delegation cycle",
                dependency.get()
            ),
            Self::UnknownTask { task } => write!(formatter, "unknown Task {}", task.get()),
            Self::UnknownAgent { agent } => write!(formatter, "unknown Agent {}", agent.get()),
            Self::InvalidAgentSession { agent } => {
                write!(
                    formatter,
                    "Agent {} session is not valid for this Runtime",
                    agent.get()
                )
            }
            Self::AgentNotActive { agent } => {
                write!(formatter, "Agent {} is not Active", agent.get())
            }
            Self::ScopeExpansion { parent } => write!(
                formatter,
                "Delegation expands Agent {} Task scope",
                parent.get()
            ),
            Self::CapabilityExpansion { parent } => write!(
                formatter,
                "Delegation expands Agent {} capabilities",
                parent.get()
            ),
            Self::BudgetExpansion { parent } => write!(
                formatter,
                "Delegation exceeds Agent {} unreserved budget",
                parent.get()
            ),
            Self::InvalidMessage => write!(formatter, "Agent message must be non-empty"),
            Self::MessageTooLarge => {
                write!(formatter, "Agent message exceeds {MAX_MESSAGE_BYTES} bytes")
            }
            Self::InvalidCompletionCapsule => {
                write!(formatter, "Completion Capsule outcome must be non-empty")
            }
            Self::CompletionCapsuleTooLarge => write!(
                formatter,
                "Completion Capsule exceeds {MAX_CAPSULE_BYTES} bytes"
            ),
            Self::TooManyCompletionCapsuleEntries => write!(
                formatter,
                "Completion Capsule exceeds {MAX_CAPSULE_ENTRIES} list entries"
            ),
            Self::InvalidReason => write!(formatter, "terminal reason must be non-empty"),
            Self::ReasonTooLarge => write!(
                formatter,
                "terminal reason exceeds {MAX_REASON_BYTES} bytes"
            ),
            Self::OutstandingChildren { parent } => write!(
                formatter,
                "Agent {} still has non-terminal children",
                parent.get()
            ),
            Self::InvalidTransition {
                agent,
                status,
                operation,
            } => write!(
                formatter,
                "Agent {} cannot {operation} while {status:?}",
                agent.get()
            ),
            Self::IdentifierExhausted => write!(formatter, "Agent Team identifier space exhausted"),
            Self::InvariantViolation { detail } => {
                write!(formatter, "Agent Team invariant violated: {detail}")
            }
        }
    }
}

impl Error for TeamError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryError {
    InvalidActiveAgentLimit,
    RuntimeAuthorityExhausted,
    SequenceMismatch {
        expected: u64,
        actual: u64,
    },
    TransactionMismatch {
        expected: u64,
        actual: u64,
    },
    InvalidTransactionPosition {
        transaction: TransactionId,
        expected_index: u32,
        actual_index: u32,
    },
    InvalidTransactionSize {
        transaction: TransactionId,
    },
    IncompleteTransaction {
        transaction: TransactionId,
        expected_events: u32,
        available_events: usize,
    },
    InvalidEvent {
        sequence: EventSeq,
        source: TeamError,
    },
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidActiveAgentLimit => {
                write!(formatter, "active Agent limit must be non-zero")
            }
            Self::RuntimeAuthorityExhausted => {
                write!(formatter, "Runtime authority identifier space exhausted")
            }
            Self::SequenceMismatch { expected, actual } => write!(
                formatter,
                "expected Event sequence {expected}, found {actual}"
            ),
            Self::TransactionMismatch { expected, actual } => {
                write!(formatter, "expected transaction {expected}, found {actual}")
            }
            Self::InvalidTransactionPosition {
                transaction,
                expected_index,
                actual_index,
            } => write!(
                formatter,
                "transaction {} expected event index {expected_index}, found {actual_index}",
                transaction.get()
            ),
            Self::InvalidTransactionSize { transaction } => write!(
                formatter,
                "transaction {} has an invalid event count",
                transaction.get()
            ),
            Self::IncompleteTransaction {
                transaction,
                expected_events,
                available_events,
            } => write!(
                formatter,
                "transaction {} expected {expected_events} events, found {available_events}",
                transaction.get()
            ),
            Self::InvalidEvent { sequence, source } => write!(
                formatter,
                "Event {} cannot be replayed: {source}",
                sequence.get()
            ),
        }
    }
}

impl Error for RecoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidEvent { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct TeamRuntime {
    max_active_agents: usize,
    runtime_authority: u64,
    state: TeamState,
    event_log: Vec<TeamEvent>,
    next_task: u64,
    next_agent: u64,
    next_message: u64,
    next_transaction: u64,
}

impl TeamRuntime {
    pub fn new(max_active_agents: usize) -> Result<Self, TeamError> {
        if max_active_agents == 0 {
            return Err(TeamError::InvalidActiveAgentLimit);
        }
        let runtime_authority = NEXT_RUNTIME_AUTHORITY
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| TeamError::IdentifierExhausted)?;

        Ok(Self {
            max_active_agents,
            runtime_authority,
            state: TeamState::default(),
            event_log: Vec::new(),
            next_task: 1,
            next_agent: 1,
            next_message: 1,
            next_transaction: 1,
        })
    }

    #[must_use]
    pub const fn max_active_agents(&self) -> usize {
        self.max_active_agents
    }

    pub fn dispatch(&mut self, command: TeamCommand) -> Result<TeamCommit, TeamError> {
        let prepared = self.prepare(command)?;
        Ok(self.publish(prepared, CommitDurability::Volatile))
    }

    fn prepare(&self, command: TeamCommand) -> Result<PreparedTeamTransaction, TeamError> {
        let (events, outcome) = match command {
            TeamCommand::AdmitRoot {
                task,
                budget,
                capabilities,
            } => self.plan_root(task, budget, capabilities)?,
            TeamCommand::Delegate {
                parent,
                task,
                budget,
                capabilities,
            } => {
                let parent = self.authenticate(parent)?;
                self.plan_delegation(parent, task, budget, capabilities)?
            }
            TeamCommand::SendMessage {
                from,
                recipient,
                body,
            } => {
                let from = self.authenticate(from)?;
                self.plan_message(from, recipient, body)?
            }
            TeamCommand::Complete { agent, capsule } => {
                let agent = self.authenticate(agent)?;
                self.plan_completion(agent, capsule)?
            }
            TeamCommand::Fail { agent, reason } => {
                let agent = self.authenticate(agent)?;
                self.plan_failure(agent, reason)?
            }
            TeamCommand::Cancel { agent, reason } => {
                let agent = self.authenticate(agent)?;
                self.plan_cancellation(agent, reason)?
            }
        };

        self.prepare_transaction(events, outcome)
    }

    #[must_use]
    pub fn snapshot(&self) -> TeamSnapshot {
        TeamSnapshot {
            revision: self
                .event_log
                .last()
                .map_or(EventSeq(0), |event| event.sequence),
            tasks: self
                .state
                .tasks
                .values()
                .map(|task| TaskView {
                    id: task.id,
                    title: task.spec.title.clone(),
                    scope: task.spec.scope.clone(),
                    dependencies: task.spec.dependencies.clone(),
                    owner: task.owner,
                    status: task.status.clone(),
                    completion: task.completion.clone(),
                })
                .collect(),
            agents: self
                .state
                .agents
                .values()
                .map(|agent| AgentView {
                    id: agent.id,
                    parent: agent.parent,
                    task: agent.task,
                    status: agent.status,
                    budget: agent.budget,
                    reserved_budget: agent.reserved_budget,
                    capabilities: agent.capabilities.clone(),
                })
                .collect(),
            messages: self.state.messages.clone(),
        }
    }

    #[must_use]
    pub fn event_log(&self) -> &[TeamEvent] {
        &self.event_log
    }

    pub fn recover(
        max_active_agents: usize,
        events: impl IntoIterator<Item = TeamEvent>,
    ) -> Result<Self, RecoveryError> {
        let mut runtime = Self::new(max_active_agents).map_err(|source| match source {
            TeamError::InvalidActiveAgentLimit => RecoveryError::InvalidActiveAgentLimit,
            TeamError::IdentifierExhausted => RecoveryError::RuntimeAuthorityExhausted,
            _ => unreachable!("TeamRuntime::new only validates limit and Runtime authority"),
        })?;
        let events: Vec<_> = events.into_iter().collect();
        let mut cursor = 0_usize;
        let mut expected_sequence = 1_u64;
        let mut expected_transaction = 1_u64;

        while cursor < events.len() {
            let first = &events[cursor];
            if first.sequence.get() != expected_sequence {
                return Err(RecoveryError::SequenceMismatch {
                    expected: expected_sequence,
                    actual: first.sequence.get(),
                });
            }
            if first.transaction.get() != expected_transaction {
                return Err(RecoveryError::TransactionMismatch {
                    expected: expected_transaction,
                    actual: first.transaction.get(),
                });
            }
            if first.index_in_transaction != 0 {
                return Err(RecoveryError::InvalidTransactionPosition {
                    transaction: first.transaction,
                    expected_index: 0,
                    actual_index: first.index_in_transaction,
                });
            }
            if first.events_in_transaction == 0 {
                return Err(RecoveryError::InvalidTransactionSize {
                    transaction: first.transaction,
                });
            }

            let event_count = first.events_in_transaction as usize;
            let available = events.len() - cursor;
            if available < event_count {
                return Err(RecoveryError::IncompleteTransaction {
                    transaction: first.transaction,
                    expected_events: first.events_in_transaction,
                    available_events: available,
                });
            }

            let mut candidate = runtime.state.clone();
            for offset in 0..event_count {
                let event = &events[cursor + offset];
                let expected_event_sequence = expected_sequence.checked_add(offset as u64).ok_or(
                    RecoveryError::InvalidEvent {
                        sequence: event.sequence,
                        source: TeamError::IdentifierExhausted,
                    },
                )?;
                if event.sequence.get() != expected_event_sequence {
                    return Err(RecoveryError::SequenceMismatch {
                        expected: expected_event_sequence,
                        actual: event.sequence.get(),
                    });
                }
                if event.transaction != first.transaction {
                    return Err(RecoveryError::TransactionMismatch {
                        expected: first.transaction.get(),
                        actual: event.transaction.get(),
                    });
                }
                if event.index_in_transaction != offset as u32 {
                    return Err(RecoveryError::InvalidTransactionPosition {
                        transaction: first.transaction,
                        expected_index: offset as u32,
                        actual_index: event.index_in_transaction,
                    });
                }
                if event.events_in_transaction != first.events_in_transaction {
                    return Err(RecoveryError::InvalidTransactionSize {
                        transaction: first.transaction,
                    });
                }
                candidate
                    .apply(&event.kind, max_active_agents)
                    .map_err(|source| RecoveryError::InvalidEvent {
                        sequence: event.sequence,
                        source,
                    })?;
            }
            candidate.validate(max_active_agents).map_err(|source| {
                RecoveryError::InvalidEvent {
                    sequence: events[cursor + event_count - 1].sequence,
                    source,
                }
            })?;
            candidate
                .validate_quiescent(max_active_agents)
                .map_err(|source| RecoveryError::InvalidEvent {
                    sequence: events[cursor + event_count - 1].sequence,
                    source,
                })?;

            runtime.state = candidate;
            runtime
                .event_log
                .extend_from_slice(&events[cursor..cursor + event_count]);
            cursor += event_count;
            expected_sequence = expected_sequence.checked_add(event_count as u64).ok_or(
                RecoveryError::InvalidEvent {
                    sequence: first.sequence,
                    source: TeamError::IdentifierExhausted,
                },
            )?;
            expected_transaction =
                expected_transaction
                    .checked_add(1)
                    .ok_or(RecoveryError::InvalidEvent {
                        sequence: first.sequence,
                        source: TeamError::IdentifierExhausted,
                    })?;
        }

        runtime.next_transaction = expected_transaction;
        runtime
            .refresh_identifiers()
            .map_err(|source| RecoveryError::InvalidEvent {
                sequence: runtime
                    .event_log
                    .last()
                    .map_or(EventSeq(0), |event| event.sequence),
                source,
            })?;
        Ok(runtime)
    }

    fn plan_root(
        &self,
        task: TaskSpec,
        budget: ResourceBudget,
        capabilities: CapabilitySnapshot,
    ) -> Result<(Vec<TeamEventKind>, CommandOutcome), TeamError> {
        if self.state.root.is_some() {
            return Err(TeamError::RootAlreadyAdmitted);
        }
        let task = self.validate_task_spec(task)?;
        if !task.dependencies.is_empty() {
            return Err(TeamError::RootDependenciesNotAllowed);
        }
        validate_budget(budget)?;
        validate_capabilities(&capabilities)?;
        let task_id = self.fresh_task_id()?;
        let agent_id = self.fresh_agent_id()?;

        Ok((
            vec![
                TeamEventKind::TaskCreated {
                    task: task_id,
                    spec: task,
                },
                TeamEventKind::AgentCreated {
                    agent: agent_id,
                    task: task_id,
                    parent: None,
                    budget,
                    capabilities,
                },
                TeamEventKind::TaskOwnerAssigned {
                    task: task_id,
                    agent: agent_id,
                },
            ],
            CommandOutcome::RootAdmitted {
                task: task_id,
                agent: agent_id,
                session: self.session(agent_id),
            },
        ))
    }

    fn plan_delegation(
        &self,
        parent: AgentId,
        task: TaskSpec,
        budget: ResourceBudget,
        capabilities: CapabilitySnapshot,
    ) -> Result<(Vec<TeamEventKind>, CommandOutcome), TeamError> {
        let parent_agent = self
            .state
            .agents
            .get(&parent)
            .ok_or(TeamError::UnknownAgent { agent: parent })?;
        if parent_agent.status != AgentStatus::Active {
            return Err(TeamError::AgentNotActive { agent: parent });
        }
        let parent_task =
            self.state
                .tasks
                .get(&parent_agent.task)
                .ok_or(TeamError::UnknownTask {
                    task: parent_agent.task,
                })?;
        let task = self.validate_task_spec(task)?;
        if let Some(dependency) = self.ancestor_dependency(parent, &task.dependencies)? {
            return Err(TeamError::DependencyCycle { dependency });
        }
        if !task.scope.is_subset_of(&parent_task.spec.scope) {
            return Err(TeamError::ScopeExpansion { parent });
        }
        validate_capabilities(&capabilities)?;
        if !capabilities.is_subset_of(&parent_agent.capabilities) {
            return Err(TeamError::CapabilityExpansion { parent });
        }
        validate_budget(budget)?;
        let remaining = parent_agent
            .budget
            .remaining_after(parent_agent.reserved_budget)
            .ok_or(TeamError::InvariantViolation {
                detail: format!("Agent {} reserved budget exceeds its budget", parent.get()),
            })?;
        if !remaining.contains(budget) {
            return Err(TeamError::BudgetExpansion { parent });
        }
        let task_id = self.fresh_task_id()?;
        let agent_id = self.fresh_agent_id()?;

        Ok((
            vec![
                TeamEventKind::TaskCreated {
                    task: task_id,
                    spec: task,
                },
                TeamEventKind::AgentCreated {
                    agent: agent_id,
                    task: task_id,
                    parent: Some(parent),
                    budget,
                    capabilities,
                },
                TeamEventKind::TaskOwnerAssigned {
                    task: task_id,
                    agent: agent_id,
                },
                TeamEventKind::DelegationGranted {
                    parent,
                    child: agent_id,
                },
            ],
            CommandOutcome::Delegated {
                task: task_id,
                agent: agent_id,
                session: self.session(agent_id),
            },
        ))
    }

    fn plan_message(
        &self,
        from: AgentId,
        recipient: MessageRecipient,
        body: String,
    ) -> Result<(Vec<TeamEventKind>, CommandOutcome), TeamError> {
        self.require_active(from)?;
        if body.trim().is_empty() {
            return Err(TeamError::InvalidMessage);
        }
        if body.len() > MAX_MESSAGE_BYTES {
            return Err(TeamError::MessageTooLarge);
        }
        if let MessageRecipient::Agent(agent) = recipient {
            let target = self
                .state
                .agents
                .get(&agent)
                .ok_or(TeamError::UnknownAgent { agent })?;
            if target.status.is_terminal() {
                return Err(TeamError::InvalidTransition {
                    agent,
                    status: target.status,
                    operation: "receive a message",
                });
            }
        }
        let message = self.fresh_message_id()?;
        Ok((
            vec![TeamEventKind::MessageSent {
                message,
                from,
                recipient,
                body,
            }],
            CommandOutcome::MessageAccepted { message },
        ))
    }

    fn plan_completion(
        &self,
        agent: AgentId,
        capsule: CompletionCapsule,
    ) -> Result<(Vec<TeamEventKind>, CommandOutcome), TeamError> {
        let task = self.require_active(agent)?.task;
        self.require_no_outstanding_children(agent)?;
        validate_capsule(&capsule)?;
        Ok((
            vec![
                TeamEventKind::CompletionCapsuleSubmitted {
                    task,
                    agent,
                    capsule,
                },
                TeamEventKind::TaskSucceeded { task },
                TeamEventKind::AgentSucceeded { agent },
            ],
            CommandOutcome::StateChanged { task, agent },
        ))
    }

    fn plan_failure(
        &self,
        agent: AgentId,
        reason: String,
    ) -> Result<(Vec<TeamEventKind>, CommandOutcome), TeamError> {
        let task = self.require_active(agent)?.task;
        self.require_no_outstanding_children(agent)?;
        validate_reason(&reason)?;
        Ok((
            vec![
                TeamEventKind::TaskFailed { task, reason },
                TeamEventKind::AgentFailed { agent },
            ],
            CommandOutcome::StateChanged { task, agent },
        ))
    }

    fn plan_cancellation(
        &self,
        agent: AgentId,
        reason: String,
    ) -> Result<(Vec<TeamEventKind>, CommandOutcome), TeamError> {
        let record = self
            .state
            .agents
            .get(&agent)
            .ok_or(TeamError::UnknownAgent { agent })?;
        if record.status.is_terminal() {
            return Err(TeamError::InvalidTransition {
                agent,
                status: record.status,
                operation: "cancel",
            });
        }
        self.require_no_outstanding_children(agent)?;
        validate_reason(&reason)?;
        Ok((
            vec![
                TeamEventKind::TaskCancelled {
                    task: record.task,
                    reason,
                },
                TeamEventKind::AgentCancelled { agent },
            ],
            CommandOutcome::StateChanged {
                task: record.task,
                agent,
            },
        ))
    }

    fn prepare_transaction(
        &self,
        base_events: Vec<TeamEventKind>,
        outcome: CommandOutcome,
    ) -> Result<PreparedTeamTransaction, TeamError> {
        let transaction = self.fresh_transaction_id()?;
        let mut candidate = self.state.clone();
        let mut event_kinds = Vec::with_capacity(base_events.len() + 8);
        for event in base_events {
            candidate.apply(&event, self.max_active_agents)?;
            event_kinds.push(event);
        }
        reconcile(&mut candidate, &mut event_kinds, self.max_active_agents)?;
        candidate.validate(self.max_active_agents)?;
        candidate.validate_quiescent(self.max_active_agents)?;

        let event_count =
            u32::try_from(event_kinds.len()).map_err(|_| TeamError::IdentifierExhausted)?;
        let first_sequence = u64::try_from(self.event_log.len())
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(TeamError::IdentifierExhausted)?;
        let last_sequence = first_sequence
            .checked_add(u64::from(event_count).saturating_sub(1))
            .ok_or(TeamError::IdentifierExhausted)?;
        let events: Vec<_> = event_kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| TeamEvent {
                sequence: EventSeq(first_sequence + index as u64),
                transaction,
                index_in_transaction: index as u32,
                events_in_transaction: event_count,
                kind,
            })
            .collect();
        let next_transaction = self
            .next_transaction
            .checked_add(1)
            .ok_or(TeamError::IdentifierExhausted)?;
        let next_task = next_identifier(candidate.tasks.keys().map(|id| id.get()))?;
        let next_agent = next_identifier(candidate.agents.keys().map(|id| id.get()))?;
        let next_message =
            next_identifier(candidate.messages.iter().map(|message| message.id.get()))?;

        Ok(PreparedTeamTransaction {
            candidate,
            transaction,
            revision: EventSeq(last_sequence),
            events,
            outcome,
            next_transaction,
            next_task,
            next_agent,
            next_message,
        })
    }

    fn publish(
        &mut self,
        prepared: PreparedTeamTransaction,
        durability: CommitDurability,
    ) -> TeamCommit {
        let PreparedTeamTransaction {
            candidate,
            transaction,
            revision,
            events,
            outcome,
            next_transaction,
            next_task,
            next_agent,
            next_message,
        } = prepared;

        self.event_log.extend(events.iter().cloned());
        self.state = candidate;
        self.next_transaction = next_transaction;
        self.next_task = next_task;
        self.next_agent = next_agent;
        self.next_message = next_message;

        TeamCommit {
            transaction,
            revision,
            durability,
            events,
            outcome,
        }
    }

    fn validate_task_spec(&self, mut task: TaskSpec) -> Result<TaskSpec, TeamError> {
        let title = task.title.trim();
        if title.is_empty() {
            return Err(TeamError::InvalidTaskTitle);
        }
        if title.len() > MAX_TASK_TITLE_BYTES {
            return Err(TeamError::TaskTitleTooLarge);
        }
        task.title = title.to_owned();
        validate_scope(&task.scope)?;
        if task.dependencies.len() > MAX_TASK_DEPENDENCIES {
            return Err(TeamError::TooManyDependencies);
        }
        task.dependencies.sort_unstable();
        for pair in task.dependencies.windows(2) {
            if pair[0] == pair[1] {
                return Err(TeamError::DuplicateDependency { task: pair[0] });
            }
        }
        for dependency in &task.dependencies {
            if !self.state.tasks.contains_key(dependency) {
                return Err(TeamError::UnknownTask { task: *dependency });
            }
        }
        Ok(task)
    }

    fn require_active(&self, agent: AgentId) -> Result<&AgentRecord, TeamError> {
        let record = self
            .state
            .agents
            .get(&agent)
            .ok_or(TeamError::UnknownAgent { agent })?;
        if record.status != AgentStatus::Active {
            return Err(TeamError::AgentNotActive { agent });
        }
        Ok(record)
    }

    fn authenticate(&self, session: AgentSession) -> Result<AgentId, TeamError> {
        if session.runtime_authority != self.runtime_authority {
            return Err(TeamError::InvalidAgentSession {
                agent: session.agent,
            });
        }
        if !self.state.agents.contains_key(&session.agent) {
            return Err(TeamError::UnknownAgent {
                agent: session.agent,
            });
        }
        Ok(session.agent)
    }

    fn trusted_rebind_nonterminal_sessions(&self) -> Vec<AgentSession> {
        self.state
            .agents
            .values()
            .filter(|agent| !agent.status.is_terminal())
            .map(|agent| self.session(agent.id))
            .collect()
    }

    const fn session(&self, agent: AgentId) -> AgentSession {
        AgentSession {
            agent,
            runtime_authority: self.runtime_authority,
        }
    }

    fn require_no_outstanding_children(&self, parent: AgentId) -> Result<(), TeamError> {
        if self
            .state
            .agents
            .values()
            .any(|agent| agent.parent == Some(parent) && !agent.status.is_terminal())
        {
            return Err(TeamError::OutstandingChildren { parent });
        }
        Ok(())
    }

    fn ancestor_dependency(
        &self,
        parent: AgentId,
        dependencies: &[TaskId],
    ) -> Result<Option<TaskId>, TeamError> {
        let mut current = Some(parent);
        while let Some(agent) = current {
            let record = self
                .state
                .agents
                .get(&agent)
                .ok_or(TeamError::UnknownAgent { agent })?;
            if dependencies.contains(&record.task) {
                return Ok(Some(record.task));
            }
            current = record.parent;
        }
        Ok(None)
    }

    fn fresh_task_id(&self) -> Result<TaskId, TeamError> {
        fresh_identifier(self.next_task).map(TaskId)
    }

    fn fresh_agent_id(&self) -> Result<AgentId, TeamError> {
        fresh_identifier(self.next_agent).map(AgentId)
    }

    fn fresh_message_id(&self) -> Result<MessageId, TeamError> {
        fresh_identifier(self.next_message).map(MessageId)
    }

    fn fresh_transaction_id(&self) -> Result<TransactionId, TeamError> {
        fresh_identifier(self.next_transaction).map(TransactionId)
    }

    fn refresh_identifiers(&mut self) -> Result<(), TeamError> {
        self.next_task = next_identifier(self.state.tasks.keys().map(|id| id.get()))?;
        self.next_agent = next_identifier(self.state.agents.keys().map(|id| id.get()))?;
        self.next_message =
            next_identifier(self.state.messages.iter().map(|message| message.id.get()))?;
        Ok(())
    }
}

struct PreparedTeamTransaction {
    candidate: TeamState,
    transaction: TransactionId,
    revision: EventSeq,
    events: Vec<TeamEvent>,
    outcome: CommandOutcome,
    next_transaction: u64,
    next_task: u64,
    next_agent: u64,
    next_message: u64,
}

#[derive(Clone, Debug, Default)]
struct TeamState {
    root: Option<AgentId>,
    tasks: BTreeMap<TaskId, TaskRecord>,
    agents: BTreeMap<AgentId, AgentRecord>,
    messages: Vec<MessageView>,
}

#[derive(Clone, Debug)]
struct TaskRecord {
    id: TaskId,
    spec: TaskSpec,
    owner: Option<AgentId>,
    status: TaskStatus,
    completion: Option<CompletionCapsule>,
}

#[derive(Clone, Debug)]
struct AgentRecord {
    id: AgentId,
    parent: Option<AgentId>,
    task: TaskId,
    status: AgentStatus,
    budget: ResourceBudget,
    reserved_budget: ResourceBudget,
    capabilities: CapabilitySnapshot,
}

impl TeamState {
    fn apply(&mut self, event: &TeamEventKind, max_active_agents: usize) -> Result<(), TeamError> {
        match event {
            TeamEventKind::TaskCreated { task, spec } => {
                validate_stored_identifier(task.get(), "Task")?;
                let expected = next_identifier(self.tasks.keys().map(|id| id.get()))?;
                if task.get() != expected {
                    return invariant(format!(
                        "expected Task identifier {expected}, found {}",
                        task.get()
                    ));
                }
                if self.tasks.contains_key(task) {
                    return invariant(format!("Task {} was created twice", task.get()));
                }
                validate_stored_task_spec(spec)?;
                for dependency in &spec.dependencies {
                    if !self.tasks.contains_key(dependency) {
                        return invariant(format!(
                            "Task {} references unknown dependency {}",
                            task.get(),
                            dependency.get()
                        ));
                    }
                }
                self.tasks.insert(
                    *task,
                    TaskRecord {
                        id: *task,
                        spec: spec.clone(),
                        owner: None,
                        status: TaskStatus::Pending,
                        completion: None,
                    },
                );
            }
            TeamEventKind::AgentCreated {
                agent,
                task,
                parent,
                budget,
                capabilities,
            } => {
                validate_stored_identifier(agent.get(), "Agent")?;
                let expected = next_identifier(self.agents.keys().map(|id| id.get()))?;
                if agent.get() != expected {
                    return invariant(format!(
                        "expected Agent identifier {expected}, found {}",
                        agent.get()
                    ));
                }
                if self.agents.contains_key(agent) {
                    return invariant(format!("Agent {} was created twice", agent.get()));
                }
                let task_record = self
                    .tasks
                    .get(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if task_record.owner.is_some() {
                    return invariant(format!("Task {} already has an Owner", task.get()));
                }
                validate_budget(*budget)?;
                validate_capabilities(capabilities)?;
                match parent {
                    Some(parent_id) => {
                        let parent_agent = self
                            .agents
                            .get(parent_id)
                            .ok_or(TeamError::UnknownAgent { agent: *parent_id })?;
                        if parent_agent.status != AgentStatus::Active {
                            return invariant(format!(
                                "parent Agent {} was not Active at Delegation",
                                parent_id.get()
                            ));
                        }
                        let parent_task =
                            self.tasks
                                .get(&parent_agent.task)
                                .ok_or(TeamError::UnknownTask {
                                    task: parent_agent.task,
                                })?;
                        if !task_record.spec.scope.is_subset_of(&parent_task.spec.scope) {
                            return Err(TeamError::ScopeExpansion { parent: *parent_id });
                        }
                        if !capabilities.is_subset_of(&parent_agent.capabilities) {
                            return Err(TeamError::CapabilityExpansion { parent: *parent_id });
                        }
                        if let Some(dependency) =
                            self.ancestor_dependency(*parent_id, &task_record.spec.dependencies)?
                        {
                            return Err(TeamError::DependencyCycle { dependency });
                        }
                    }
                    None => {
                        if self.root.is_some() {
                            return Err(TeamError::RootAlreadyAdmitted);
                        }
                        if !task_record.spec.dependencies.is_empty() {
                            return Err(TeamError::RootDependenciesNotAllowed);
                        }
                        self.root = Some(*agent);
                    }
                }
                self.agents.insert(
                    *agent,
                    AgentRecord {
                        id: *agent,
                        parent: *parent,
                        task: *task,
                        status: AgentStatus::Dormant,
                        budget: *budget,
                        reserved_budget: ResourceBudget::default(),
                        capabilities: capabilities.clone(),
                    },
                );
            }
            TeamEventKind::TaskOwnerAssigned { task, agent } => {
                let agent_record = self
                    .agents
                    .get(agent)
                    .ok_or(TeamError::UnknownAgent { agent: *agent })?;
                if agent_record.task != *task {
                    return invariant(format!(
                        "Agent {} was assigned to the wrong Task",
                        agent.get()
                    ));
                }
                let task_record = self
                    .tasks
                    .get_mut(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if task_record.owner.replace(*agent).is_some() {
                    return invariant(format!("Task {} received more than one Owner", task.get()));
                }
            }
            TeamEventKind::DelegationGranted { parent, child } => {
                let child_record = self
                    .agents
                    .get(child)
                    .ok_or(TeamError::UnknownAgent { agent: *child })?;
                if child_record.parent != Some(*parent) {
                    return invariant(format!("Agent {} has a different parent", child.get()));
                }
                let child_budget = child_record.budget;
                let parent_record = self
                    .agents
                    .get_mut(parent)
                    .ok_or(TeamError::UnknownAgent { agent: *parent })?;
                if parent_record.status != AgentStatus::Active {
                    return Err(TeamError::AgentNotActive { agent: *parent });
                }
                let reserved = parent_record
                    .reserved_budget
                    .checked_add(child_budget)
                    .ok_or(TeamError::BudgetExpansion { parent: *parent })?;
                if !parent_record.budget.contains(reserved) {
                    return Err(TeamError::BudgetExpansion { parent: *parent });
                }
                parent_record.reserved_budget = reserved;
            }
            TeamEventKind::TaskReady { task } => {
                let ready = self.dependencies_succeeded(*task)?;
                let record = self
                    .tasks
                    .get_mut(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if record.status != TaskStatus::Pending || !ready {
                    return invariant(format!("Task {} cannot become Ready", task.get()));
                }
                record.status = TaskStatus::Ready;
            }
            TeamEventKind::AgentActivated { agent } => {
                if self.active_agent_count() >= max_active_agents {
                    return invariant("Active Agent limit exceeded".into());
                }
                let record = self
                    .agents
                    .get_mut(agent)
                    .ok_or(TeamError::UnknownAgent { agent: *agent })?;
                if record.status != AgentStatus::Dormant {
                    return invariant(format!(
                        "Agent {} cannot become Active from {:?}",
                        agent.get(),
                        record.status
                    ));
                }
                let task = self
                    .tasks
                    .get(&record.task)
                    .ok_or(TeamError::UnknownTask { task: record.task })?;
                if task.status != TaskStatus::Ready || task.owner != Some(*agent) {
                    return invariant(format!("Agent {} does not own a Ready Task", agent.get()));
                }
                record.status = AgentStatus::Active;
            }
            TeamEventKind::TaskStarted { task } => {
                let record = self
                    .tasks
                    .get_mut(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if record.status != TaskStatus::Ready {
                    return invariant(format!(
                        "Task {} cannot start from {:?}",
                        task.get(),
                        record.status
                    ));
                }
                let owner = record.owner.ok_or_else(|| TeamError::InvariantViolation {
                    detail: format!("Task {} has no Owner", task.get()),
                })?;
                let agent = self
                    .agents
                    .get(&owner)
                    .ok_or(TeamError::UnknownAgent { agent: owner })?;
                if agent.status != AgentStatus::Active {
                    return invariant(format!("Task {} Owner is not Active", task.get()));
                }
                record.status = TaskStatus::Running;
            }
            TeamEventKind::MessageSent {
                message,
                from,
                recipient,
                body,
            } => {
                validate_stored_identifier(message.get(), "Message")?;
                let expected = next_identifier(self.messages.iter().map(|entry| entry.id.get()))?;
                if message.get() != expected {
                    return invariant(format!(
                        "expected Message identifier {expected}, found {}",
                        message.get()
                    ));
                }
                let sender = self
                    .agents
                    .get(from)
                    .ok_or(TeamError::UnknownAgent { agent: *from })?;
                if sender.status != AgentStatus::Active {
                    return Err(TeamError::AgentNotActive { agent: *from });
                }
                if body.trim().is_empty() || body.len() > MAX_MESSAGE_BYTES {
                    return Err(TeamError::InvalidMessage);
                }
                if self.messages.iter().any(|existing| existing.id == *message) {
                    return invariant(format!("Message {} was recorded twice", message.get()));
                }
                if let MessageRecipient::Agent(target) = recipient {
                    let target = self
                        .agents
                        .get(target)
                        .ok_or(TeamError::UnknownAgent { agent: *target })?;
                    if target.status.is_terminal() {
                        return invariant("terminal Agent received a message".into());
                    }
                }
                self.messages.push(MessageView {
                    id: *message,
                    from: *from,
                    recipient: *recipient,
                    body: body.clone(),
                });
            }
            TeamEventKind::CompletionCapsuleSubmitted {
                task,
                agent,
                capsule,
            } => {
                validate_capsule(capsule)?;
                let record = self
                    .tasks
                    .get_mut(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if record.status != TaskStatus::Running
                    || record.owner != Some(*agent)
                    || record.completion.is_some()
                {
                    return invariant(format!(
                        "Task {} cannot accept this Completion Capsule",
                        task.get()
                    ));
                }
                let owner = self
                    .agents
                    .get(agent)
                    .ok_or(TeamError::UnknownAgent { agent: *agent })?;
                if owner.status != AgentStatus::Active {
                    return Err(TeamError::AgentNotActive { agent: *agent });
                }
                record.completion = Some(capsule.clone());
            }
            TeamEventKind::TaskSucceeded { task } => {
                let record = self
                    .tasks
                    .get_mut(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if record.status != TaskStatus::Running || record.completion.is_none() {
                    return invariant(format!("Task {} cannot succeed", task.get()));
                }
                record.status = TaskStatus::Succeeded;
            }
            TeamEventKind::AgentSucceeded { agent } => {
                let record = self
                    .agents
                    .get_mut(agent)
                    .ok_or(TeamError::UnknownAgent { agent: *agent })?;
                if record.status != AgentStatus::Active {
                    return Err(TeamError::AgentNotActive { agent: *agent });
                }
                let task = self
                    .tasks
                    .get(&record.task)
                    .ok_or(TeamError::UnknownTask { task: record.task })?;
                if task.status != TaskStatus::Succeeded {
                    return invariant(format!("Agent {} Task did not succeed", agent.get()));
                }
                record.status = AgentStatus::Succeeded;
            }
            TeamEventKind::TaskFailed { task, reason } => {
                validate_reason(reason)?;
                let record = self
                    .tasks
                    .get_mut(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if record.status != TaskStatus::Running {
                    return invariant(format!(
                        "Task {} cannot fail from {:?}",
                        task.get(),
                        record.status
                    ));
                }
                record.status = TaskStatus::Failed {
                    reason: reason.clone(),
                };
            }
            TeamEventKind::AgentFailed { agent } => {
                let record = self
                    .agents
                    .get_mut(agent)
                    .ok_or(TeamError::UnknownAgent { agent: *agent })?;
                if record.status != AgentStatus::Active {
                    return Err(TeamError::AgentNotActive { agent: *agent });
                }
                let task = self
                    .tasks
                    .get(&record.task)
                    .ok_or(TeamError::UnknownTask { task: record.task })?;
                if !matches!(task.status, TaskStatus::Failed { .. }) {
                    return invariant(format!("Agent {} Task did not fail", agent.get()));
                }
                record.status = AgentStatus::Failed;
            }
            TeamEventKind::TaskCancelled { task, reason } => {
                validate_reason(reason)?;
                let record = self
                    .tasks
                    .get_mut(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if record.status.is_terminal() {
                    return invariant(format!("terminal Task {} cannot be cancelled", task.get()));
                }
                record.status = TaskStatus::Cancelled {
                    reason: reason.clone(),
                };
            }
            TeamEventKind::AgentCancelled { agent } => {
                let record = self
                    .agents
                    .get_mut(agent)
                    .ok_or(TeamError::UnknownAgent { agent: *agent })?;
                if record.status.is_terminal() {
                    return invariant(format!(
                        "terminal Agent {} cannot be cancelled",
                        agent.get()
                    ));
                }
                let task = self
                    .tasks
                    .get(&record.task)
                    .ok_or(TeamError::UnknownTask { task: record.task })?;
                if !matches!(task.status, TaskStatus::Cancelled { .. }) {
                    return invariant(format!("Agent {} Task was not cancelled", agent.get()));
                }
                record.status = AgentStatus::Cancelled;
            }
            TeamEventKind::TaskBlocked { task, blocked_by } => {
                let blocker = self
                    .tasks
                    .get(blocked_by)
                    .ok_or(TeamError::UnknownTask { task: *blocked_by })?;
                if !blocker.status.blocks_dependents() {
                    return invariant(format!(
                        "Task {} does not block dependents",
                        blocked_by.get()
                    ));
                }
                let record = self
                    .tasks
                    .get_mut(task)
                    .ok_or(TeamError::UnknownTask { task: *task })?;
                if !matches!(record.status, TaskStatus::Pending | TaskStatus::Ready)
                    || !record.spec.dependencies.contains(blocked_by)
                {
                    return invariant(format!(
                        "Task {} cannot be blocked by Task {}",
                        task.get(),
                        blocked_by.get()
                    ));
                }
                record.status = TaskStatus::Blocked {
                    blocked_by: *blocked_by,
                };
            }
            TeamEventKind::AgentBlocked { agent, blocked_by } => {
                let record = self
                    .agents
                    .get_mut(agent)
                    .ok_or(TeamError::UnknownAgent { agent: *agent })?;
                if record.status != AgentStatus::Dormant {
                    return invariant(format!(
                        "Agent {} cannot become Blocked from {:?}",
                        agent.get(),
                        record.status
                    ));
                }
                let task = self
                    .tasks
                    .get(&record.task)
                    .ok_or(TeamError::UnknownTask { task: record.task })?;
                if task.status
                    != (TaskStatus::Blocked {
                        blocked_by: *blocked_by,
                    })
                {
                    return invariant(format!(
                        "Agent {} Task has a different blocker",
                        agent.get()
                    ));
                }
                record.status = AgentStatus::Blocked;
            }
        }
        Ok(())
    }

    fn active_agent_count(&self) -> usize {
        self.agents
            .values()
            .filter(|agent| agent.status == AgentStatus::Active)
            .count()
    }

    fn dependencies_succeeded(&self, task: TaskId) -> Result<bool, TeamError> {
        let record = self
            .tasks
            .get(&task)
            .ok_or(TeamError::UnknownTask { task })?;
        record
            .spec
            .dependencies
            .iter()
            .map(|dependency| {
                self.tasks
                    .get(dependency)
                    .map(|task| task.status == TaskStatus::Succeeded)
                    .ok_or(TeamError::UnknownTask { task: *dependency })
            })
            .try_fold(true, |all, succeeded| succeeded.map(|value| all && value))
    }

    fn first_failed_dependency(&self, task: TaskId) -> Result<Option<TaskId>, TeamError> {
        let record = self
            .tasks
            .get(&task)
            .ok_or(TeamError::UnknownTask { task })?;
        for dependency in &record.spec.dependencies {
            let dependency_record = self
                .tasks
                .get(dependency)
                .ok_or(TeamError::UnknownTask { task: *dependency })?;
            if dependency_record.status.blocks_dependents() {
                return Ok(Some(*dependency));
            }
        }
        Ok(None)
    }

    fn ancestor_dependency(
        &self,
        parent: AgentId,
        dependencies: &[TaskId],
    ) -> Result<Option<TaskId>, TeamError> {
        let mut current = Some(parent);
        while let Some(agent) = current {
            let record = self
                .agents
                .get(&agent)
                .ok_or(TeamError::UnknownAgent { agent })?;
            if dependencies.contains(&record.task) {
                return Ok(Some(record.task));
            }
            current = record.parent;
        }
        Ok(None)
    }

    fn validate(&self, max_active_agents: usize) -> Result<(), TeamError> {
        if self.active_agent_count() > max_active_agents {
            return invariant("Active Agent limit exceeded".into());
        }
        if self
            .root
            .is_some_and(|root| !self.agents.contains_key(&root))
        {
            return invariant("root Agent is missing".into());
        }
        for task in self.tasks.values() {
            let owner = task.owner.ok_or_else(|| TeamError::InvariantViolation {
                detail: format!("Task {} has no Owner", task.id.get()),
            })?;
            let agent = self
                .agents
                .get(&owner)
                .ok_or(TeamError::UnknownAgent { agent: owner })?;
            if agent.task != task.id {
                return invariant(format!("Task {} Owner points elsewhere", task.id.get()));
            }
            let paired = matches!(
                (&task.status, agent.status),
                (
                    TaskStatus::Pending | TaskStatus::Ready,
                    AgentStatus::Dormant
                ) | (TaskStatus::Running, AgentStatus::Active)
                    | (TaskStatus::Blocked { .. }, AgentStatus::Blocked)
                    | (TaskStatus::Succeeded, AgentStatus::Succeeded)
                    | (TaskStatus::Failed { .. }, AgentStatus::Failed)
                    | (TaskStatus::Cancelled { .. }, AgentStatus::Cancelled)
            );
            if !paired {
                return invariant(format!(
                    "Task {} and Agent {} states disagree",
                    task.id.get(),
                    agent.id.get()
                ));
            }
        }
        for agent in self.agents.values() {
            let task = self
                .tasks
                .get(&agent.task)
                .ok_or(TeamError::UnknownTask { task: agent.task })?;
            if task.owner != Some(agent.id) {
                return invariant(format!(
                    "Agent {} is not the Owner of Task {}",
                    agent.id.get(),
                    agent.task.get()
                ));
            }
            if let Some(parent) = agent.parent
                && !self.agents.contains_key(&parent)
            {
                return Err(TeamError::UnknownAgent { agent: parent });
            }
            if !agent.budget.contains(agent.reserved_budget) {
                return invariant(format!("Agent {} over-reserved its budget", agent.id.get()));
            }
            let expected_reserved = self
                .agents
                .values()
                .filter(|child| child.parent == Some(agent.id))
                .try_fold(ResourceBudget::default(), |reserved, child| {
                    reserved.checked_add(child.budget)
                })
                .ok_or(TeamError::BudgetExpansion { parent: agent.id })?;
            if agent.reserved_budget != expected_reserved {
                return invariant(format!(
                    "Agent {} reserved budget does not match its Delegations",
                    agent.id.get()
                ));
            }
            if agent.status.is_terminal()
                && self
                    .agents
                    .values()
                    .any(|child| child.parent == Some(agent.id) && !child.status.is_terminal())
            {
                return Err(TeamError::OutstandingChildren { parent: agent.id });
            }
        }
        Ok(())
    }

    fn validate_quiescent(&self, max_active_agents: usize) -> Result<(), TeamError> {
        for task in self
            .tasks
            .values()
            .filter(|task| task.status == TaskStatus::Pending)
        {
            if self.first_failed_dependency(task.id)?.is_some()
                || self.dependencies_succeeded(task.id)?
            {
                return invariant(format!(
                    "Task {} is Pending despite a resolved dependency state",
                    task.id.get()
                ));
            }
        }
        if self.active_agent_count() < max_active_agents
            && self
                .tasks
                .values()
                .any(|task| task.status == TaskStatus::Ready)
        {
            return invariant("Ready Task was not activated despite available capacity".into());
        }
        Ok(())
    }
}

fn reconcile(
    state: &mut TeamState,
    events: &mut Vec<TeamEventKind>,
    max_active_agents: usize,
) -> Result<(), TeamError> {
    let pending: Vec<_> = state
        .tasks
        .values()
        .filter(|task| task.status == TaskStatus::Pending)
        .map(|task| task.id)
        .collect();
    for task in pending {
        if let Some(blocked_by) = state.first_failed_dependency(task)? {
            apply_planned(
                state,
                events,
                TeamEventKind::TaskBlocked { task, blocked_by },
                max_active_agents,
            )?;
            let agent = state
                .tasks
                .get(&task)
                .and_then(|task| task.owner)
                .ok_or_else(|| TeamError::InvariantViolation {
                    detail: format!("Task {} has no Owner", task.get()),
                })?;
            apply_planned(
                state,
                events,
                TeamEventKind::AgentBlocked { agent, blocked_by },
                max_active_agents,
            )?;
        } else if state.dependencies_succeeded(task)? {
            apply_planned(
                state,
                events,
                TeamEventKind::TaskReady { task },
                max_active_agents,
            )?;
        }
    }

    while state.active_agent_count() < max_active_agents {
        let Some(task) = state
            .tasks
            .values()
            .find(|task| task.status == TaskStatus::Ready)
            .map(|task| task.id)
        else {
            break;
        };
        let agent = state
            .tasks
            .get(&task)
            .and_then(|task| task.owner)
            .ok_or_else(|| TeamError::InvariantViolation {
                detail: format!("Task {} has no Owner", task.get()),
            })?;
        apply_planned(
            state,
            events,
            TeamEventKind::AgentActivated { agent },
            max_active_agents,
        )?;
        apply_planned(
            state,
            events,
            TeamEventKind::TaskStarted { task },
            max_active_agents,
        )?;
    }
    Ok(())
}

fn apply_planned(
    state: &mut TeamState,
    events: &mut Vec<TeamEventKind>,
    event: TeamEventKind,
    max_active_agents: usize,
) -> Result<(), TeamError> {
    state.apply(&event, max_active_agents)?;
    events.push(event);
    Ok(())
}

fn validate_budget(budget: ResourceBudget) -> Result<(), TeamError> {
    if budget.token_units == 0 {
        return Err(TeamError::InvalidBudget);
    }
    Ok(())
}

fn validate_scope(scope: &TaskScope) -> Result<(), TeamError> {
    if scope.labels.len() > MAX_SCOPE_LABELS {
        return Err(TeamError::TooManyScopeLabels);
    }
    if scope
        .iter()
        .any(|label| label.trim().is_empty() || label.trim() != label)
    {
        return Err(TeamError::InvalidScope);
    }
    if scope
        .iter()
        .any(|label| label.len() > MAX_SCOPE_LABEL_BYTES)
    {
        return Err(TeamError::ScopeLabelTooLarge);
    }
    Ok(())
}

fn validate_capabilities(capabilities: &CapabilitySnapshot) -> Result<(), TeamError> {
    if capabilities.capabilities.len() > MAX_CAPABILITIES {
        return Err(TeamError::TooManyCapabilities);
    }
    if capabilities.iter().any(|capability| {
        matches!(capability, Capability::Tool(name) if name.trim().is_empty() || name.trim() != name)
    }) {
        return Err(TeamError::InvalidCapability);
    }
    if capabilities.iter().any(
        |capability| matches!(capability, Capability::Tool(name) if name.len() > MAX_TOOL_NAME_BYTES),
    ) {
        return Err(TeamError::ToolNameTooLarge);
    }
    Ok(())
}

fn validate_capsule(capsule: &CompletionCapsule) -> Result<(), TeamError> {
    if capsule.outcome.trim().is_empty() {
        return Err(TeamError::InvalidCompletionCapsule);
    }
    if capsule
        .approximate_bytes()
        .is_none_or(|bytes| bytes > MAX_CAPSULE_BYTES)
    {
        return Err(TeamError::CompletionCapsuleTooLarge);
    }
    if capsule
        .entry_count()
        .is_none_or(|entries| entries > MAX_CAPSULE_ENTRIES)
    {
        return Err(TeamError::TooManyCompletionCapsuleEntries);
    }
    Ok(())
}

fn validate_reason(reason: &str) -> Result<(), TeamError> {
    if reason.trim().is_empty() {
        return Err(TeamError::InvalidReason);
    }
    if reason.len() > MAX_REASON_BYTES {
        return Err(TeamError::ReasonTooLarge);
    }
    Ok(())
}

fn validate_stored_task_spec(spec: &TaskSpec) -> Result<(), TeamError> {
    if spec.title.trim().is_empty() || spec.title.trim() != spec.title {
        return Err(TeamError::InvalidTaskTitle);
    }
    if spec.title.len() > MAX_TASK_TITLE_BYTES {
        return Err(TeamError::TaskTitleTooLarge);
    }
    validate_scope(&spec.scope)?;
    if spec.dependencies.len() > MAX_TASK_DEPENDENCIES {
        return Err(TeamError::TooManyDependencies);
    }
    for pair in spec.dependencies.windows(2) {
        if pair[0] >= pair[1] {
            if pair[0] == pair[1] {
                return Err(TeamError::DuplicateDependency { task: pair[0] });
            }
            return invariant("Task dependencies are not canonically ordered".into());
        }
    }
    Ok(())
}

fn validate_stored_identifier(value: u64, kind: &str) -> Result<(), TeamError> {
    if value == 0 || value == u64::MAX {
        return invariant(format!("{kind} identifier {value} is invalid"));
    }
    Ok(())
}

fn fresh_identifier(value: u64) -> Result<u64, TeamError> {
    validate_stored_identifier(value, "next")?;
    Ok(value)
}

fn next_identifier(values: impl Iterator<Item = u64>) -> Result<u64, TeamError> {
    values
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(TeamError::IdentifierExhausted)
}

fn invariant<T>(detail: String) -> Result<T, TeamError> {
    Err(TeamError::InvariantViolation { detail })
}
