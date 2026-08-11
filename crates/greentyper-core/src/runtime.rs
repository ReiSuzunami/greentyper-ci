//! Single-Agent Runtime Kernel with durable admission, output preparation, and acknowledgement.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::agent_team::{
    AgentId, AgentSession, DurableTeamError, DurableTeamRuntime, TeamCommand, TeamError,
    TeamOperationAcknowledgeOutcome, TeamOperationCommit, TeamOperationId, TeamOperationRecord,
    TeamOperationStatus, TeamSnapshot,
};
use crate::config::{
    ConfigEpoch, ConfigError, ConfigLayer, ConfigLayers, ConfigSource, MAX_CONFIG_ID_BYTES,
    MAX_CONFIG_STRING_BYTES, ReasoningEffort, ServiceTier,
};
use crate::context::{
    ContextAdmissionDecision, ContextArtifactRef, ContextEventRange, ContextPressureSnapshot,
    ContextReductionPolicy, ContextViewError, ContextViewItem, ContextViewRole,
    MAX_CONTEXT_VIEW_ITEMS, ReducedContextView,
};
use crate::ledger::{
    DurabilityReceipt, EventData, FileLedger, LedgerError, LedgerHead, StoredEvent,
};
use crate::model::{
    CanonicalItem, ConfigEpochId, DeliveryId, ItemId, ItemRole, ModelError, ProviderEpochId,
    ThreadId, TurnId,
};
use crate::pricing::{
    CostEstimateOutcome, CostEstimateUnknownReason, MAX_PRICE_SCHEDULES, PriceSchedule,
    PriceScheduleBook, PriceScheduleDefinition, PriceScheduleSource, TokenRates,
};
use crate::provider::{
    MAX_PROVIDER_ID_BYTES, MAX_SERVICE_TIER_BYTES, ProviderDialect, ProviderEpoch, ProviderError,
    ProviderEvent, ProviderPricingSource, ProviderProfileSnapshot, ProviderRequest,
    ProviderRuntime, ProviderToolCall, ProviderToolOutput, ProviderUnavailableStage, UsageAccuracy,
    UsageRecord,
};
use crate::schema::SchemaKind;
use crate::tool_runtime::{
    ApprovalDecision, DurableToolRuntime, ToolApprovalRequest, ToolArguments, ToolArgumentsHash,
    ToolCallId, ToolCallOutcome, ToolCallRecord, ToolCallStatus, ToolEffectExecutor, ToolIntent,
    ToolPrincipal, ToolReconciliationDecision, ToolRequestOutcome, ToolResources, ToolRuntimeError,
    ToolSnapshot,
};
use crate::usage::{
    MAX_USAGE_WINDOWS, RuntimeUsageQuery, RuntimeUsageReport, RuntimeUsageSnapshot, UsageAttempt,
    UsageAttemptOutcome, UsageError, UsageProjection, UsageRevision, UsageTimestamp,
    UsageTimezoneSource, UsageWeekday, UsageWindow,
};

pub const RUNTIME_EVENT_SCHEMA: u16 = SchemaKind::RuntimeEvent.current().get();
pub const MAX_INPUT_BYTES: usize = 512 * 1024;
const MAX_BLOCK_REASON_BYTES: usize = 4096;
const MAX_USAGE_RECORDS_PER_TURN: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryStatus {
    Ready,
    ResumeRequired {
        turn: TurnId,
    },
    ReconciliationRequired {
        turn: TurnId,
        delivery: DeliveryId,
    },
    Blocked {
        turn: TurnId,
        reason: String,
        retryable: bool,
    },
}

impl fmt::Display for RecoveryStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => write!(formatter, "ready"),
            Self::ResumeRequired { turn } => {
                write!(formatter, "resume-required turn={}", turn.get())
            }
            Self::ReconciliationRequired { turn, delivery } => write!(
                formatter,
                "reconciliation-required turn={} delivery={}",
                turn.get(),
                delivery.get()
            ),
            Self::Blocked {
                turn,
                reason,
                retryable,
            } => {
                write!(
                    formatter,
                    "blocked turn={} retryable={retryable} reason={reason}",
                    turn.get()
                )
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSnapshot {
    pub head: LedgerHead,
    pub thread: Option<ThreadId>,
    pub items: Vec<CanonicalItem>,
    pub status: RecoveryStatus,
    pub pending_model_selection: Option<PendingModelSelection>,
    pub recovered_tail_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCheckpointDraft {
    view: ReducedContextView,
}

impl ContextCheckpointDraft {
    #[must_use]
    pub const fn source(&self) -> ContextEventRange {
        self.view.source()
    }

    #[must_use]
    pub const fn view(&self) -> &ReducedContextView {
        &self.view
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCheckpoint {
    view: ReducedContextView,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextInspection {
    head: LedgerHead,
    checkpoint: Option<ContextCheckpoint>,
    recovered_tail_bytes: u64,
}

impl ContextInspection {
    #[must_use]
    pub const fn head(&self) -> LedgerHead {
        self.head
    }

    #[must_use]
    pub const fn checkpoint(&self) -> Option<&ContextCheckpoint> {
        self.checkpoint.as_ref()
    }

    #[must_use]
    pub const fn recovered_tail_bytes(&self) -> u64 {
        self.recovered_tail_bytes
    }
}

impl ContextCheckpoint {
    #[must_use]
    pub const fn source(&self) -> ContextEventRange {
        self.view.source()
    }

    #[must_use]
    pub const fn view(&self) -> &ReducedContextView {
        &self.view
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelection {
    preset_id: String,
    config_fingerprint: u64,
    provider_profile: String,
    provider_model: String,
    preferred_dialect: ProviderDialect,
}

impl ModelSelection {
    pub fn new(
        preset_id: impl Into<String>,
        config_fingerprint: u64,
        provider_profile: impl Into<String>,
        provider_model: impl Into<String>,
        preferred_dialect: ProviderDialect,
    ) -> Result<Self, RuntimeError> {
        let selection = Self {
            preset_id: preset_id.into(),
            config_fingerprint,
            provider_profile: provider_profile.into(),
            provider_model: provider_model.into(),
            preferred_dialect,
        };
        selection.validate()?;
        Ok(selection)
    }

    #[must_use]
    pub fn preset_id(&self) -> &str {
        &self.preset_id
    }

    #[must_use]
    pub const fn config_fingerprint(&self) -> u64 {
        self.config_fingerprint
    }

    #[must_use]
    pub fn provider_profile(&self) -> &str {
        &self.provider_profile
    }

    #[must_use]
    pub fn provider_model(&self) -> &str {
        &self.provider_model
    }

    #[must_use]
    pub const fn preferred_dialect(&self) -> ProviderDialect {
        self.preferred_dialect
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        let id = self.preset_id.as_str();
        if id.is_empty()
            || id.len() > MAX_CONFIG_ID_BYTES
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        {
            return Err(RuntimeError::InvalidModelSelection("Preset ID is invalid"));
        }
        for value in [&self.provider_profile, &self.provider_model] {
            if value.trim().is_empty()
                || value.trim() != value
                || value.len() > MAX_PROVIDER_ID_BYTES
                || value.chars().any(char::is_control)
            {
                return Err(RuntimeError::InvalidModelSelection(
                    "Provider identity is invalid",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingModelSelection {
    agent: AgentId,
    selection: ModelSelection,
}

impl PendingModelSelection {
    #[must_use]
    pub const fn agent(&self) -> AgentId {
        self.agent
    }

    #[must_use]
    pub const fn selection(&self) -> &ModelSelection {
        &self.selection
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelTeamSnapshot {
    pub projection: TeamSnapshot,
    pub ledger_head: LedgerHead,
    pub recovered_tail_bytes: u64,
    pub operations: Vec<TeamOperationRecord>,
}

/// Per-open execution authority rebound from a validated Agent Team replay.
///
/// The Runtime Kernel returns the complete non-terminal set at open. The
/// interface deliberately offers no caller-selected Agent ID conversion and no
/// later session-minting lookup.
pub struct KernelTeamRecovery {
    snapshot: KernelTeamSnapshot,
    sessions: Vec<AgentSession>,
}

impl KernelTeamRecovery {
    #[must_use]
    pub const fn snapshot(&self) -> &KernelTeamSnapshot {
        &self.snapshot
    }

    #[must_use]
    pub fn into_sessions(self) -> Vec<AgentSession> {
        self.sessions
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PreparedOutput {
    delivery: DeliveryId,
    turn: TurnId,
    text: String,
    usage_records: Vec<UsageRecord>,
    receipt: DurabilityReceipt,
}

impl fmt::Debug for PreparedOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedOutput")
            .field("delivery", &self.delivery)
            .field("turn", &self.turn)
            .field("text_bytes", &self.text.len())
            .field("usage_record_count", &self.usage_records.len())
            .field("receipt", &self.receipt)
            .finish()
    }
}

impl PreparedOutput {
    #[must_use]
    pub const fn delivery(&self) -> DeliveryId {
        self.delivery
    }

    #[must_use]
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn usage_records(&self) -> &[UsageRecord] {
        &self.usage_records
    }

    #[must_use]
    pub const fn receipt(&self) -> DurabilityReceipt {
        self.receipt
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcknowledgeOutcome {
    Durable(DurabilityReceipt),
    AlreadyAcknowledged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelTurnOutcome {
    Durable(DurabilityReceipt),
    AlreadyCancelled,
}

pub struct ProviderToolApproval {
    request: ToolApprovalRequest,
    provider_request: ProviderRequest,
    provider_call_id: String,
    leading_deltas: Vec<String>,
    usage_records: Vec<UsageRecord>,
}

impl ProviderToolApproval {
    #[must_use]
    pub const fn call(&self) -> ToolCallId {
        self.request.call()
    }

    #[must_use]
    pub fn tool(&self) -> &str {
        self.request.tool()
    }

    #[must_use]
    pub fn identity(&self) -> &str {
        self.request.identity()
    }

    #[must_use]
    pub const fn arguments(&self) -> &ToolArguments {
        self.request.arguments()
    }

    #[must_use]
    pub const fn resources(&self) -> &ToolResources {
        self.request.resources()
    }

    #[must_use]
    pub const fn arguments_hash(&self) -> ToolArgumentsHash {
        self.request.arguments_hash()
    }
}

impl fmt::Debug for ProviderToolApproval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderToolApproval")
            .field("call", &self.request.call())
            .field("tool", &self.request.tool())
            .field("provider_call_id_bytes", &self.provider_call_id.len())
            .field("leading_delta_count", &self.leading_deltas.len())
            .field("usage_record_count", &self.usage_records.len())
            .finish_non_exhaustive()
    }
}

pub enum ProviderTurnOutcome {
    Prepared(PreparedOutput),
    ApprovalRequired(Box<ProviderToolApproval>),
}

impl fmt::Debug for ProviderTurnOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepared(output) => formatter
                .debug_struct("Prepared")
                .field("turn", &output.turn)
                .field("delivery", &output.delivery)
                .field("text_bytes", &output.text.len())
                .field("usage_record_count", &output.usage_records.len())
                .finish(),
            Self::ApprovalRequired(approval) => formatter
                .debug_tuple("ApprovalRequired")
                .field(approval)
                .finish(),
        }
    }
}

pub struct RuntimeKernel {
    ledger: FileLedger,
    state: RuntimeState,
    recovered_tail_bytes: u64,
    team: Option<KernelTeam>,
    tools: Option<DurableToolRuntime>,
}

struct TurnAdmission {
    usage_windows: Vec<UsageWindow>,
    price_schedules: PriceScheduleBook,
    context_pressure: Option<ContextPressureSnapshot>,
    input: String,
    provider_snapshot: Option<ProviderProfileSnapshot>,
    provider_dialect: Option<ProviderDialect>,
    agent: Option<AgentId>,
}

struct KernelTeam {
    runtime: DurableTeamRuntime,
}

impl KernelTeam {
    fn open(
        path: impl AsRef<Path>,
        max_active_agents: usize,
    ) -> Result<(Self, KernelTeamRecovery), DurableTeamError> {
        let runtime = DurableTeamRuntime::open(path, max_active_agents)?;
        let sessions = runtime.trusted_rebind_nonterminal_sessions();
        let recovery = KernelTeamRecovery {
            snapshot: Self::snapshot_runtime(&runtime),
            sessions,
        };
        Ok((Self { runtime }, recovery))
    }

    fn open_existing_strict(
        path: impl AsRef<Path>,
        max_active_agents: usize,
    ) -> Result<(Self, KernelTeamRecovery), DurableTeamError> {
        let runtime = DurableTeamRuntime::open_existing_strict(path, max_active_agents)?;
        let sessions = runtime.trusted_rebind_nonterminal_sessions();
        let recovery = KernelTeamRecovery {
            snapshot: Self::snapshot_runtime(&runtime),
            sessions,
        };
        Ok((Self { runtime }, recovery))
    }

    fn snapshot(&self) -> KernelTeamSnapshot {
        Self::snapshot_runtime(&self.runtime)
    }

    fn snapshot_runtime(runtime: &DurableTeamRuntime) -> KernelTeamSnapshot {
        KernelTeamSnapshot {
            projection: runtime.snapshot(),
            ledger_head: runtime.ledger_head(),
            recovered_tail_bytes: runtime.recovered_tail_bytes(),
            operations: runtime.operation_records(),
        }
    }

    fn dispatch(&mut self, command: TeamCommand) -> Result<TeamOperationCommit, RuntimeError> {
        self.require_ready()?;
        let operation = self
            .runtime
            .next_operation_id()
            .map_err(RuntimeError::Team)?;
        self.runtime
            .dispatch_operation(operation, command)
            .map_err(RuntimeError::Team)
    }

    fn require_ready(&self) -> Result<(), RuntimeError> {
        if let Some(record) =
            self.runtime.operation_records().into_iter().find(|record| {
                record.status == TeamOperationStatus::CommittedAwaitingAcknowledgement
            })
        {
            return Err(RuntimeError::TeamOperationReconciliationRequired(
                record.operation,
            ));
        }
        Ok(())
    }

    fn active_agent_context(
        &self,
        session: AgentSession,
    ) -> Result<crate::agent_team::AgentExecutionContext, RuntimeError> {
        self.runtime
            .trusted_active_agent_context(session)
            .map_err(RuntimeError::Team)
    }

    fn acknowledge(
        &mut self,
        operation: TeamOperationId,
    ) -> Result<TeamOperationAcknowledgeOutcome, RuntimeError> {
        self.runtime
            .acknowledge_operation(operation)
            .map_err(RuntimeError::Team)
    }
}

impl RuntimeKernel {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let (ledger, report) = FileLedger::open(path).map_err(RuntimeError::Ledger)?;
        Self::from_opened_ledger(ledger, report)
    }

    pub fn open_existing_strict(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let (ledger, report) =
            FileLedger::open_existing_strict(path).map_err(RuntimeError::Ledger)?;
        Self::from_opened_ledger(ledger, report)
    }

    fn from_opened_ledger(
        ledger: FileLedger,
        report: crate::ledger::ReplayReport,
    ) -> Result<Self, RuntimeError> {
        let state = replay_runtime(&report.events)?;
        Ok(Self {
            ledger,
            state,
            recovered_tail_bytes: report.truncated_tail_bytes,
            team: None,
            tools: None,
        })
    }

    pub fn open_with_team(
        runtime_path: impl AsRef<Path>,
        team_path: impl AsRef<Path>,
        max_active_agents: usize,
    ) -> Result<(Self, KernelTeamRecovery), RuntimeError> {
        let runtime_path = runtime_path.as_ref();
        let team_path = team_path.as_ref();
        if ledger_paths_may_alias(runtime_path, team_path) {
            return Err(RuntimeError::InvalidTeamConfiguration(
                "Runtime and Team Ledgers must use different paths",
            ));
        }
        if max_active_agents == 0 {
            return Err(RuntimeError::Team(DurableTeamError::Team(
                TeamError::InvalidActiveAgentLimit,
            )));
        }

        let mut kernel = Self::open(runtime_path)?;
        let (team, recovery) =
            KernelTeam::open(team_path, max_active_agents).map_err(RuntimeError::Team)?;
        kernel.team = Some(team);
        Ok((kernel, recovery))
    }

    pub fn open_with_team_and_tools(
        runtime_path: impl AsRef<Path>,
        team_path: impl AsRef<Path>,
        tool_path: impl AsRef<Path>,
        max_active_agents: usize,
    ) -> Result<(Self, KernelTeamRecovery), RuntimeError> {
        let runtime_path = runtime_path.as_ref();
        let team_path = team_path.as_ref();
        let tool_path = tool_path.as_ref();
        if ledger_paths_may_alias(runtime_path, team_path) {
            return Err(RuntimeError::InvalidTeamConfiguration(
                "Runtime and Team Ledgers must use different paths",
            ));
        }
        if ledger_paths_may_alias(runtime_path, tool_path)
            || ledger_paths_may_alias(team_path, tool_path)
        {
            return Err(RuntimeError::InvalidToolConfiguration(
                "Tool Ledger must use a path distinct from Runtime and Team Ledgers",
            ));
        }
        if max_active_agents == 0 {
            return Err(RuntimeError::Team(DurableTeamError::Team(
                TeamError::InvalidActiveAgentLimit,
            )));
        }

        let mut kernel = Self::open(runtime_path)?;
        let (team, recovery) =
            KernelTeam::open(team_path, max_active_agents).map_err(RuntimeError::Team)?;
        let tools = DurableToolRuntime::open(tool_path).map_err(RuntimeError::Tool)?;
        kernel.team = Some(team);
        kernel.tools = Some(tools);
        Ok((kernel, recovery))
    }

    pub fn open_with_team_and_tools_existing_strict(
        runtime_path: impl AsRef<Path>,
        team_path: impl AsRef<Path>,
        tool_path: impl AsRef<Path>,
        max_active_agents: usize,
    ) -> Result<(Self, KernelTeamRecovery), RuntimeError> {
        let runtime_path = runtime_path.as_ref();
        let team_path = team_path.as_ref();
        let tool_path = tool_path.as_ref();
        if ledger_paths_may_alias(runtime_path, team_path) {
            return Err(RuntimeError::InvalidTeamConfiguration(
                "Runtime and Team Ledgers must use different paths",
            ));
        }
        if ledger_paths_may_alias(runtime_path, tool_path)
            || ledger_paths_may_alias(team_path, tool_path)
        {
            return Err(RuntimeError::InvalidToolConfiguration(
                "Tool Ledger must use a path distinct from Runtime and Team Ledgers",
            ));
        }
        if max_active_agents == 0 {
            return Err(RuntimeError::Team(DurableTeamError::Team(
                TeamError::InvalidActiveAgentLimit,
            )));
        }

        let mut kernel = Self::open_existing_strict(runtime_path)?;
        let (team, recovery) = KernelTeam::open_existing_strict(team_path, max_active_agents)
            .map_err(RuntimeError::Team)?;
        let tools =
            DurableToolRuntime::open_existing_strict(tool_path).map_err(RuntimeError::Tool)?;
        kernel.team = Some(team);
        kernel.tools = Some(tools);
        Ok((kernel, recovery))
    }

    pub fn inspect(path: impl AsRef<Path>) -> Result<RuntimeSnapshot, RuntimeError> {
        let report = match FileLedger::inspect(path) {
            Ok(report) => report,
            Err(LedgerError::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RuntimeSnapshot {
                    head: LedgerHead::default(),
                    thread: None,
                    items: Vec::new(),
                    status: RecoveryStatus::Ready,
                    pending_model_selection: None,
                    recovered_tail_bytes: 0,
                });
            }
            Err(source) => return Err(RuntimeError::Ledger(source)),
        };
        let state = replay_runtime(&report.events)?;
        let status = state.status();
        Ok(RuntimeSnapshot {
            head: report.head,
            thread: state.thread,
            items: state.items,
            status,
            pending_model_selection: state.pending_model_selection,
            recovered_tail_bytes: report.truncated_tail_bytes,
        })
    }

    pub fn inspect_context(path: impl AsRef<Path>) -> Result<ContextInspection, RuntimeError> {
        let report = match FileLedger::inspect(path) {
            Ok(report) => report,
            Err(LedgerError::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ContextInspection {
                    head: LedgerHead::default(),
                    checkpoint: None,
                    recovered_tail_bytes: 0,
                });
            }
            Err(source) => return Err(RuntimeError::Ledger(source)),
        };
        let state = replay_runtime(&report.events)?;
        Ok(ContextInspection {
            head: report.head,
            checkpoint: state.context_checkpoint,
            recovered_tail_bytes: report.truncated_tail_bytes,
        })
    }

    pub fn inspect_usage(
        path: impl AsRef<Path>,
        as_of: UsageTimestamp,
    ) -> Result<RuntimeUsageSnapshot, RuntimeError> {
        let report = match FileLedger::inspect(path) {
            Ok(report) => report,
            Err(LedgerError::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(UsageProjection::default().snapshot(None, as_of));
            }
            Err(source) => return Err(RuntimeError::Ledger(source)),
        };
        let state = replay_runtime(&report.events)?;
        Ok(state.usage.snapshot(state.thread.map(ThreadId::get), as_of))
    }

    pub fn inspect_usage_report(
        path: impl AsRef<Path>,
        as_of: UsageTimestamp,
        query: RuntimeUsageQuery,
    ) -> Result<RuntimeUsageReport, RuntimeError> {
        let report = match FileLedger::inspect(path) {
            Ok(report) => report,
            Err(LedgerError::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
                return UsageProjection::default()
                    .report(None, as_of, UsageRevision::default(), query)
                    .map_err(RuntimeError::Usage);
            }
            Err(source) => return Err(RuntimeError::Ledger(source)),
        };
        let state = replay_runtime(&report.events)?;
        state
            .usage
            .report(
                state.thread.map(ThreadId::get),
                as_of,
                UsageRevision::new(report.head.transaction, report.head.sequence),
                query,
            )
            .map_err(RuntimeError::Usage)
    }

    #[must_use]
    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            head: self.ledger.head(),
            thread: self.state.thread,
            items: self.state.items.clone(),
            status: self.state.status(),
            pending_model_selection: self.state.pending_model_selection.clone(),
            recovered_tail_bytes: self.recovered_tail_bytes,
        }
    }

    #[must_use]
    pub fn pending_model_selection(&self) -> Option<&PendingModelSelection> {
        self.state.pending_model_selection.as_ref()
    }

    #[must_use]
    pub const fn context_checkpoint(&self) -> Option<&ContextCheckpoint> {
        self.state.context_checkpoint.as_ref()
    }

    pub fn prepare_context_checkpoint(
        &self,
        policy: ContextReductionPolicy,
    ) -> Result<ContextCheckpointDraft, RuntimeError> {
        self.require_context_safe_barrier()?;
        let view = ReducedContextView::from_items(self.ledger.head(), &self.state.items, policy)
            .map_err(RuntimeError::Context)?;
        Ok(ContextCheckpointDraft { view })
    }

    pub fn publish_context_checkpoint(
        &mut self,
        draft: ContextCheckpointDraft,
    ) -> Result<DurabilityReceipt, RuntimeError> {
        self.require_context_safe_barrier()?;
        let expected = draft.source().head();
        let actual = self.ledger.head();
        if expected != actual {
            return Err(RuntimeError::StaleContextCheckpoint { expected, actual });
        }
        self.commit(&[RuntimeEvent::ContextCheckpointPublished {
            checkpoint: ContextCheckpoint { view: draft.view },
        }])
    }

    pub fn stage_model_selection(
        &mut self,
        session: AgentSession,
        selection: ModelSelection,
    ) -> Result<DurabilityReceipt, RuntimeError> {
        self.require_provider_session(session)?;
        self.require_ready()?;
        selection.validate()?;
        if self
            .state
            .pending_model_selection
            .as_ref()
            .is_some_and(|pending| pending.agent != session.agent())
        {
            return Err(RuntimeError::InvalidModelSelection(
                "another Agent already owns the pending selection",
            ));
        }
        self.commit(&[RuntimeEvent::ModelSelectionStaged {
            agent: session.agent(),
            selection,
        }])
    }

    #[must_use]
    pub fn usage_snapshot(&self, as_of: UsageTimestamp) -> RuntimeUsageSnapshot {
        self.state
            .usage
            .snapshot(self.state.thread.map(ThreadId::get), as_of)
    }

    pub fn usage_report(
        &self,
        as_of: UsageTimestamp,
        query: RuntimeUsageQuery,
    ) -> Result<RuntimeUsageReport, RuntimeError> {
        let head = self.ledger.head();
        self.state
            .usage
            .report(
                self.state.thread.map(ThreadId::get),
                as_of,
                UsageRevision::new(head.transaction, head.sequence),
                query,
            )
            .map_err(RuntimeError::Usage)
    }

    /// Returns the frozen Provider Epoch for the current non-terminal Turn.
    ///
    /// Product composition uses this after recovery to reconstruct the exact
    /// Provider adapter without consulting mutable configuration.
    #[must_use]
    pub fn pending_provider_epoch(&self) -> Option<&ProviderEpoch> {
        let pending = self.state.pending.as_ref()?;
        self.state.providers.get(&pending.provider)
    }

    #[must_use]
    pub fn team_snapshot(&self) -> Option<KernelTeamSnapshot> {
        self.team.as_ref().map(KernelTeam::snapshot)
    }

    #[must_use]
    pub fn tool_snapshot(&self) -> Option<ToolSnapshot> {
        self.tools.as_ref().map(DurableToolRuntime::snapshot)
    }

    pub fn dispatch_team(
        &mut self,
        command: TeamCommand,
    ) -> Result<TeamOperationCommit, RuntimeError> {
        self.dispatch_team_command(command)
    }

    pub fn acknowledge_team_operation(
        &mut self,
        operation: TeamOperationId,
    ) -> Result<TeamOperationAcknowledgeOutcome, RuntimeError> {
        let team = self.team.as_mut().ok_or(RuntimeError::TeamUnavailable)?;
        team.acknowledge(operation)
    }

    pub fn request_tool_call(
        &mut self,
        session: AgentSession,
        intent: ToolIntent,
    ) -> Result<ToolRequestOutcome, RuntimeError> {
        self.require_no_tool_reconciliation()?;
        let context = {
            let team = self.team.as_ref().ok_or(RuntimeError::TeamUnavailable)?;
            team.require_ready()?;
            team.active_agent_context(session)?
        };
        let tools = self.tools.as_mut().ok_or(RuntimeError::ToolUnavailable)?;
        tools
            .request(ToolPrincipal::new(session, context), intent)
            .map_err(RuntimeError::Tool)
    }

    pub fn resolve_tool_call(
        &mut self,
        request: ToolApprovalRequest,
        decision: ApprovalDecision,
        executor: &mut impl ToolEffectExecutor,
    ) -> Result<ToolCallOutcome, RuntimeError> {
        self.require_no_tool_reconciliation()?;
        let session = request.session();
        let context = {
            let team = self.team.as_ref().ok_or(RuntimeError::TeamUnavailable)?;
            team.require_ready()?;
            team.active_agent_context(session)?
        };
        let tools = self.tools.as_mut().ok_or(RuntimeError::ToolUnavailable)?;
        tools
            .resolve(
                ToolPrincipal::new(session, context),
                request,
                decision,
                executor,
            )
            .map_err(RuntimeError::Tool)
    }

    pub fn reconcile_tool_call(
        &mut self,
        session: AgentSession,
        call: ToolCallId,
        decision: ToolReconciliationDecision,
    ) -> Result<ToolCallRecord, RuntimeError> {
        let context = {
            let team = self.team.as_ref().ok_or(RuntimeError::TeamUnavailable)?;
            team.active_agent_context(session)?
        };
        let tools = self.tools.as_mut().ok_or(RuntimeError::ToolUnavailable)?;
        tools
            .reconcile(ToolPrincipal::new(session, context), call, decision)
            .map_err(RuntimeError::Tool)
    }

    pub fn execute(
        &mut self,
        layers: &ConfigLayers,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
        self.execute_with_usage_windows(layers, Vec::new(), input, provider)
    }

    pub fn execute_with_usage_windows(
        &mut self,
        layers: &ConfigLayers,
        usage_windows: Vec<UsageWindow>,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
        self.execute_with_observability(
            layers,
            usage_windows,
            PriceScheduleBook::default(),
            input,
            provider,
        )
    }

    pub fn execute_with_observability(
        &mut self,
        layers: &ConfigLayers,
        usage_windows: Vec<UsageWindow>,
        price_schedules: PriceScheduleBook,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
        self.admit_turn(
            layers,
            TurnAdmission {
                usage_windows,
                price_schedules,
                context_pressure: None,
                input: input.into(),
                provider_snapshot: provider.profile_snapshot().cloned(),
                provider_dialect: provider.dialect(),
                agent: None,
            },
        )?;
        self.drive_pending(provider)
    }

    /// Executes one Turn with an immutable Context Pressure projection.
    ///
    /// A hard projection stops before admission or Provider execution. Soft
    /// pressure publishes a bounded checkpoint at the current Safe Barrier
    /// before admission. Unknown pressure preserves the existing path without
    /// inventing Context facts.
    pub fn execute_with_context_pressure(
        &mut self,
        layers: &ConfigLayers,
        pressure: ContextPressureSnapshot,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
        let input = input.into();
        if pressure.admission() == ContextAdmissionDecision::Reduce {
            validate_input(&input)?;
            let checkpoint = self.prepare_context_checkpoint(ContextReductionPolicy::default())?;
            self.publish_context_checkpoint(checkpoint)?;
        }
        self.admit_turn(
            layers,
            TurnAdmission {
                usage_windows: Vec::new(),
                price_schedules: PriceScheduleBook::default(),
                context_pressure: Some(pressure),
                input,
                provider_snapshot: provider.profile_snapshot().cloned(),
                provider_dialect: provider.dialect(),
                agent: None,
            },
        )?;
        self.drive_pending(provider)
    }

    /// Drives one provider-neutral Turn that may request one Tool.
    ///
    /// `map_resources` is a deterministic data mapping. It must not perform I/O
    /// or external effects because recovery may call it again before Tool
    /// Runtime resolves the durable identity.
    pub fn execute_provider_turn<ResolveResources>(
        &mut self,
        session: AgentSession,
        layers: &ConfigLayers,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
        map_resources: ResolveResources,
    ) -> Result<ProviderTurnOutcome, RuntimeError>
    where
        ResolveResources: FnOnce(&ProviderToolCall) -> Result<ToolResources, RuntimeError>,
    {
        self.execute_provider_turn_with_usage_windows(
            session,
            layers,
            Vec::new(),
            input,
            provider,
            map_resources,
        )
    }

    pub fn execute_provider_turn_with_usage_windows<ResolveResources>(
        &mut self,
        session: AgentSession,
        layers: &ConfigLayers,
        usage_windows: Vec<UsageWindow>,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
        map_resources: ResolveResources,
    ) -> Result<ProviderTurnOutcome, RuntimeError>
    where
        ResolveResources: FnOnce(&ProviderToolCall) -> Result<ToolResources, RuntimeError>,
    {
        self.execute_provider_turn_with_observability(
            session,
            layers,
            usage_windows,
            PriceScheduleBook::default(),
            input,
            provider,
            map_resources,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_provider_turn_with_observability<ResolveResources>(
        &mut self,
        session: AgentSession,
        layers: &ConfigLayers,
        usage_windows: Vec<UsageWindow>,
        price_schedules: PriceScheduleBook,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
        map_resources: ResolveResources,
    ) -> Result<ProviderTurnOutcome, RuntimeError>
    where
        ResolveResources: FnOnce(&ProviderToolCall) -> Result<ToolResources, RuntimeError>,
    {
        self.require_provider_session(session)?;
        self.admit_turn(
            layers,
            TurnAdmission {
                usage_windows,
                price_schedules,
                context_pressure: None,
                input: input.into(),
                provider_snapshot: provider.profile_snapshot().cloned(),
                provider_dialect: provider.dialect(),
                agent: Some(session.agent()),
            },
        )?;
        self.drive_provider_turn(session, provider, map_resources)
    }

    /// Explicitly resumes a provider-neutral Turn admitted before recovery.
    ///
    /// `map_resources` has the same pure, replayable contract as in
    /// [`Self::execute_provider_turn`].
    pub fn resume_provider_turn<ResolveResources>(
        &mut self,
        session: AgentSession,
        provider: &mut impl ProviderRuntime,
        map_resources: ResolveResources,
    ) -> Result<ProviderTurnOutcome, RuntimeError>
    where
        ResolveResources: FnOnce(&ProviderToolCall) -> Result<ToolResources, RuntimeError>,
    {
        self.require_provider_session(session)?;
        match self.state.status() {
            RecoveryStatus::ResumeRequired { .. } => {
                self.drive_provider_turn(session, provider, map_resources)
            }
            status => Err(RuntimeError::Busy(status)),
        }
    }

    pub fn cancel_blocked_turn(&mut self, turn: TurnId) -> Result<CancelTurnOutcome, RuntimeError> {
        self.cancel_provider_blocked_turn(None, turn)
    }

    pub fn request_blocked_turn_retry(
        &mut self,
        turn: TurnId,
    ) -> Result<DurabilityReceipt, RuntimeError> {
        self.require_no_tool_reconciliation()?;
        self.request_provider_retry(None, turn)
    }

    pub fn cancel_blocked_provider_turn(
        &mut self,
        session: AgentSession,
        turn: TurnId,
    ) -> Result<CancelTurnOutcome, RuntimeError> {
        self.require_provider_session(session)?;
        self.cancel_provider_blocked_turn(Some(session.agent()), turn)
    }

    pub fn request_blocked_provider_turn_retry(
        &mut self,
        session: AgentSession,
        turn: TurnId,
    ) -> Result<DurabilityReceipt, RuntimeError> {
        self.require_provider_session(session)?;
        self.request_provider_retry(Some(session.agent()), turn)
    }

    pub fn resolve_provider_tool_call(
        &mut self,
        approval: Box<ProviderToolApproval>,
        decision: ApprovalDecision,
        executor: &mut impl ToolEffectExecutor,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
        let ProviderToolApproval {
            request,
            provider_request,
            provider_call_id,
            mut leading_deltas,
            mut usage_records,
        } = *approval;
        require_provider_snapshot(provider, &provider_request.provider)?;
        let turn = provider_request.turn;
        let outcome = self.resolve_tool_call(request, decision, executor)?;
        let output = match outcome {
            ToolCallOutcome::Succeeded { output, .. } => output,
            ToolCallOutcome::Denied(record) | ToolCallOutcome::Failed(record) => {
                self.block_pending(turn, "Provider Tool call did not succeed")?;
                return Err(RuntimeError::ProviderToolCallTerminated {
                    call: record.call,
                    status: record.status,
                });
            }
            ToolCallOutcome::ReconciliationRequired(record) => {
                return Err(RuntimeError::ToolReconciliationRequired(record.call));
            }
        };
        let output = match String::from_utf8(output) {
            Ok(output) => output,
            Err(_) => {
                self.block_pending(turn, "Provider Tool output is not UTF-8")?;
                return Err(RuntimeError::InvalidProviderOutput(
                    "Provider Tool output is not UTF-8",
                ));
            }
        };
        let provider_output = match ProviderToolOutput::new(provider_call_id, output) {
            Ok(output) => output,
            Err(source) => {
                self.block_pending(turn, provider_block_reason(&source))?;
                return Err(RuntimeError::Provider(source));
            }
        };
        let attempt = self.begin_usage_attempt(turn)?;
        let provider_events =
            match provider.continue_after_tool(&provider_request, &provider_output) {
                Ok(events) => events,
                Err(source) => {
                    self.finish_usage_attempt_and_block(
                        turn,
                        attempt,
                        UsageAttemptOutcome::Failed,
                        None,
                        provider_block_reason(&source),
                    )?;
                    return Err(RuntimeError::Provider(source));
                }
            };
        match validate_provider_events(&provider_events, self.pending_max_output_bytes(turn)?) {
            Ok(ValidatedProviderStep::Completed {
                deltas,
                usage_record,
            }) => {
                leading_deltas.extend(deltas);
                usage_records.push(usage_record.clone());
                self.prepare_output(turn, leading_deltas, usage_records, attempt, usage_record)
            }
            Ok(ValidatedProviderStep::ToolCall { .. }) => {
                self.finish_usage_attempt_and_block(
                    turn,
                    attempt,
                    UsageAttemptOutcome::Succeeded,
                    provider_events.iter().find_map(|event| match event {
                        ProviderEvent::Completed(usage) => Some(usage.clone()),
                        _ => None,
                    }),
                    "Provider continuation requested another Tool call",
                )?;
                Err(RuntimeError::InvalidProviderOutput(
                    "Provider continuation requested more than one Tool call",
                ))
            }
            Err(reason) => {
                self.finish_usage_attempt_and_block(
                    turn,
                    attempt,
                    UsageAttemptOutcome::Failed,
                    None,
                    reason,
                )?;
                Err(RuntimeError::InvalidProviderOutput(reason))
            }
        }
    }

    pub fn resume(
        &mut self,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
        self.require_no_tool_reconciliation()?;
        match self.state.status() {
            RecoveryStatus::ResumeRequired { .. } => self.drive_pending(provider),
            status => Err(RuntimeError::Busy(status)),
        }
    }

    pub fn acknowledge(
        &mut self,
        delivery: DeliveryId,
    ) -> Result<AcknowledgeOutcome, RuntimeError> {
        if self.state.acknowledged.contains(&delivery) {
            return Ok(AcknowledgeOutcome::AlreadyAcknowledged);
        }
        let pending = self
            .state
            .pending
            .as_ref()
            .ok_or(RuntimeError::UnknownDelivery(delivery))?;
        let prepared = pending
            .prepared
            .as_ref()
            .ok_or(RuntimeError::UnknownDelivery(delivery))?;
        if prepared.delivery != delivery {
            return Err(RuntimeError::UnknownDelivery(delivery));
        }
        let receipt = self.commit(&[
            RuntimeEvent::OutputAcknowledged {
                turn: pending.turn,
                delivery,
            },
            RuntimeEvent::TurnCompleted { turn: pending.turn },
        ])?;
        Ok(AcknowledgeOutcome::Durable(receipt))
    }

    fn require_ready(&self) -> Result<(), RuntimeError> {
        match self.state.status() {
            RecoveryStatus::Ready => Ok(()),
            status => Err(RuntimeError::Busy(status)),
        }
    }

    fn require_context_safe_barrier(&self) -> Result<(), RuntimeError> {
        self.require_ready()?;
        self.require_no_tool_reconciliation()?;
        if self.tools.as_ref().is_some_and(|tools| {
            tools
                .snapshot()
                .calls
                .iter()
                .any(|record| record.status == ToolCallStatus::AwaitingApproval)
        }) {
            return Err(RuntimeError::ContextCheckpointNotAtSafeBarrier);
        }
        if let Some(team) = &self.team {
            team.require_ready()?;
        }
        Ok(())
    }

    fn dispatch_team_command(
        &mut self,
        command: TeamCommand,
    ) -> Result<TeamOperationCommit, RuntimeError> {
        self.require_no_tool_reconciliation()?;
        let team = self.team.as_mut().ok_or(RuntimeError::TeamUnavailable)?;
        team.dispatch(command)
    }

    fn require_no_tool_reconciliation(&self) -> Result<(), RuntimeError> {
        if let Some(call) = self
            .tools
            .as_ref()
            .and_then(DurableToolRuntime::pending_reconciliation)
        {
            return Err(RuntimeError::ToolReconciliationRequired(call));
        }
        Ok(())
    }

    fn require_provider_session(&self, session: AgentSession) -> Result<(), RuntimeError> {
        self.require_no_tool_reconciliation()?;
        if self.tools.is_none() {
            return Err(RuntimeError::ToolUnavailable);
        }
        let team = self.team.as_ref().ok_or(RuntimeError::TeamUnavailable)?;
        team.require_ready()?;
        team.active_agent_context(session)?;
        Ok(())
    }

    fn cancel_provider_blocked_turn(
        &mut self,
        agent: Option<AgentId>,
        turn: TurnId,
    ) -> Result<CancelTurnOutcome, RuntimeError> {
        let record = self
            .state
            .turns
            .get(&turn)
            .ok_or(RuntimeError::UnknownTurn(turn))?;
        if record.agent != agent {
            return Err(RuntimeError::TurnCancellationNotAllowed(turn));
        }
        if record.cancelled {
            return Ok(CancelTurnOutcome::AlreadyCancelled);
        }
        let pending = self
            .state
            .pending
            .as_ref()
            .filter(|pending| pending.turn == turn)
            .ok_or(RuntimeError::TurnCancellationNotAllowed(turn))?;
        if pending.phase != PendingPhase::Blocked
            || pending.block_origin != Some(TurnBlockOrigin::Provider)
        {
            return Err(RuntimeError::TurnCancellationNotAllowed(turn));
        }
        let receipt = self.commit(&[RuntimeEvent::TurnCancelled { turn }])?;
        Ok(CancelTurnOutcome::Durable(receipt))
    }

    fn request_provider_retry(
        &mut self,
        agent: Option<AgentId>,
        turn: TurnId,
    ) -> Result<DurabilityReceipt, RuntimeError> {
        let record = self
            .state
            .turns
            .get(&turn)
            .ok_or(RuntimeError::UnknownTurn(turn))?;
        if record.agent != agent || record.completed || record.cancelled {
            return Err(RuntimeError::TurnRetryNotAllowed(turn));
        }
        let pending = self
            .state
            .pending
            .as_ref()
            .filter(|pending| pending.turn == turn)
            .ok_or(RuntimeError::TurnRetryNotAllowed(turn))?;
        if pending.phase != PendingPhase::Blocked
            || pending.block_origin != Some(TurnBlockOrigin::Provider)
            || !provider_stage_is_retryable(pending.provider_unavailable_stage)
        {
            return Err(RuntimeError::TurnRetryNotAllowed(turn));
        }
        self.commit(&[RuntimeEvent::TurnRetryRequested { turn }])
    }

    fn admit_turn(
        &mut self,
        layers: &ConfigLayers,
        admission: TurnAdmission,
    ) -> Result<(), RuntimeError> {
        let TurnAdmission {
            usage_windows,
            price_schedules,
            context_pressure,
            input,
            provider_snapshot,
            provider_dialect,
            agent,
        } = admission;
        self.require_ready()?;
        self.require_no_tool_reconciliation()?;
        validate_input(&input)?;
        if context_pressure
            .is_some_and(|pressure| pressure.admission() == ContextAdmissionDecision::Stop)
        {
            return Err(RuntimeError::ContextAdmissionBlocked {
                pressure: context_pressure.expect("hard pressure is present"),
            });
        }

        let thread = match self.state.thread {
            Some(thread) => thread,
            None => ThreadId::new(self.state.next_thread).map_err(RuntimeError::Model)?,
        };
        let turn = TurnId::new(self.state.next_turn).map_err(RuntimeError::Model)?;
        let user_item = ItemId::new(self.state.next_item).map_err(RuntimeError::Model)?;
        let config_id = ConfigEpochId::new(self.state.next_config).map_err(RuntimeError::Model)?;
        let provider_id =
            ProviderEpochId::new(self.state.next_provider).map_err(RuntimeError::Model)?;
        for window in &usage_windows {
            window
                .require_current_ruleset()
                .map_err(RuntimeError::Usage)?;
        }
        let config = ConfigEpoch::freeze_with_observability(
            config_id,
            layers,
            usage_windows,
            price_schedules,
        )
        .map_err(RuntimeError::Config)?;
        if let Some(pending) = &self.state.pending_model_selection {
            let resolved = config.resolved();
            if agent != Some(pending.agent)
                || config.fingerprint() != pending.selection.config_fingerprint()
                || resolved.provider_profile().value() != pending.selection.provider_profile()
                || resolved.provider_model().value() != pending.selection.provider_model()
            {
                return Err(RuntimeError::InvalidModelSelection(
                    "pending Preset no longer matches the next Turn",
                ));
            }
        }
        let profile = config.resolved().provider_profile().value().clone();
        let model = config.resolved().provider_model().value().clone();
        let provider_epoch = match provider_snapshot {
            Some(snapshot) => ProviderEpoch::with_profile_snapshot_and_dialect(
                provider_id,
                profile,
                model,
                snapshot,
                provider_dialect,
            ),
            None if profile == "simulator" && provider_dialect.is_none() => {
                ProviderEpoch::new(provider_id, profile, model)
            }
            None if profile == "simulator" => Err(ProviderError::InvalidConfiguration(
                "simulator Provider cannot select a wire dialect",
            )),
            None => Err(ProviderError::InvalidConfiguration(
                "non-simulator provider requires a frozen Provider Profile snapshot",
            )),
        }
        .map_err(RuntimeError::Provider)?;

        let mut admission = Vec::new();
        if self.state.thread.is_none() {
            admission.push(RuntimeEvent::ThreadCreated { thread });
        }
        admission.push(RuntimeEvent::ConfigFrozen {
            epoch: config.clone(),
        });
        admission.push(RuntimeEvent::ProviderFrozen {
            epoch: Box::new(provider_epoch),
        });
        admission.push(RuntimeEvent::TurnAdmitted {
            thread,
            turn,
            user_item,
            config: config_id,
            provider: provider_id,
            agent,
            input,
        });
        self.commit(&admission)?;
        Ok(())
    }

    fn drive_pending(
        &mut self,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
        let (pending, config, request) = self.pending_provider_context()?;
        require_provider_snapshot(provider, &request.provider)?;
        let attempt = self.begin_usage_attempt(pending.turn)?;
        let provider_events = match provider.run(&request) {
            Ok(events) => events,
            Err(source) => {
                self.finish_usage_attempt_and_block_for_provider_error(
                    pending.turn,
                    attempt,
                    &source,
                )?;
                return Err(RuntimeError::Provider(source));
            }
        };
        match validate_provider_events(
            &provider_events,
            *config.resolved().max_output_bytes().value() as usize,
        ) {
            Ok(ValidatedProviderStep::Completed {
                deltas,
                usage_record,
            }) => self.prepare_output(
                pending.turn,
                deltas,
                vec![usage_record.clone()],
                attempt,
                usage_record,
            ),
            Ok(ValidatedProviderStep::ToolCall { usage_record, .. }) => {
                self.finish_usage_attempt_and_block(
                    pending.turn,
                    attempt,
                    UsageAttemptOutcome::Succeeded,
                    Some(usage_record),
                    "Provider requested an unavailable Tool",
                )?;
                Err(RuntimeError::InvalidProviderOutput(
                    "Provider requested a Tool through the text-only interface",
                ))
            }
            Err(reason) => {
                self.finish_usage_attempt_and_block(
                    pending.turn,
                    attempt,
                    UsageAttemptOutcome::Failed,
                    None,
                    reason,
                )?;
                Err(RuntimeError::InvalidProviderOutput(reason))
            }
        }
    }

    fn drive_provider_turn<ResolveResources>(
        &mut self,
        session: AgentSession,
        provider: &mut impl ProviderRuntime,
        map_resources: ResolveResources,
    ) -> Result<ProviderTurnOutcome, RuntimeError>
    where
        ResolveResources: FnOnce(&ProviderToolCall) -> Result<ToolResources, RuntimeError>,
    {
        let (pending, config, request) = self.pending_provider_context()?;
        require_provider_snapshot(provider, &request.provider)?;
        let attempt = self.begin_usage_attempt(pending.turn)?;
        let provider_events = match provider.run(&request) {
            Ok(events) => events,
            Err(source) => {
                self.finish_usage_attempt_and_block_for_provider_error(
                    pending.turn,
                    attempt,
                    &source,
                )?;
                return Err(RuntimeError::Provider(source));
            }
        };
        match validate_provider_events(
            &provider_events,
            *config.resolved().max_output_bytes().value() as usize,
        ) {
            Ok(ValidatedProviderStep::Completed {
                deltas,
                usage_record,
            }) => self
                .prepare_output(
                    pending.turn,
                    deltas,
                    vec![usage_record.clone()],
                    attempt,
                    usage_record,
                )
                .map(ProviderTurnOutcome::Prepared),
            Ok(ValidatedProviderStep::ToolCall {
                deltas,
                call,
                usage_record,
            }) => {
                self.finish_usage_attempt(
                    pending.turn,
                    attempt,
                    UsageAttemptOutcome::Succeeded,
                    Some(usage_record.clone()),
                )?;
                let arguments = match ToolArguments::parse(call.arguments_json()) {
                    Ok(arguments) => arguments,
                    Err(error) => {
                        self.block_pending(pending.turn, "Provider Tool arguments were rejected")?;
                        return Err(RuntimeError::Tool(error));
                    }
                };
                let resources = match map_resources(&call) {
                    Ok(resources) => resources,
                    Err(error) => {
                        self.block_pending(pending.turn, "Provider Tool resources were rejected")?;
                        return Err(error);
                    }
                };
                let intent = match ToolIntent::new(
                    provider_tool_identity(pending.turn, call.call_id()),
                    call.tool(),
                    arguments,
                    resources,
                ) {
                    Ok(intent) => intent,
                    Err(error) => {
                        self.block_pending(pending.turn, "Provider Tool intent was rejected")?;
                        return Err(RuntimeError::Tool(error));
                    }
                };
                match self.request_tool_call(session, intent)? {
                    ToolRequestOutcome::ApprovalRequired(tool_request) => Ok(
                        ProviderTurnOutcome::ApprovalRequired(Box::new(ProviderToolApproval {
                            request: tool_request,
                            provider_request: request,
                            provider_call_id: call.call_id().to_owned(),
                            leading_deltas: deltas,
                            usage_records: vec![usage_record],
                        })),
                    ),
                    ToolRequestOutcome::Existing(record)
                        if record.status == ToolCallStatus::ReconciliationRequired =>
                    {
                        Err(RuntimeError::ToolReconciliationRequired(record.call))
                    }
                    ToolRequestOutcome::Existing(record)
                        if record.status == ToolCallStatus::Succeeded =>
                    {
                        self.block_pending(
                            pending.turn,
                            "Provider Tool result is unavailable after recovery",
                        )?;
                        Err(RuntimeError::ProviderToolResultUnavailable(record.call))
                    }
                    ToolRequestOutcome::Existing(record) => {
                        self.block_pending(pending.turn, "Provider Tool call already terminated")?;
                        Err(RuntimeError::ProviderToolCallTerminated {
                            call: record.call,
                            status: record.status,
                        })
                    }
                }
            }
            Err(reason) => {
                self.finish_usage_attempt_and_block(
                    pending.turn,
                    attempt,
                    UsageAttemptOutcome::Failed,
                    None,
                    reason,
                )?;
                Err(RuntimeError::InvalidProviderOutput(reason))
            }
        }
    }

    fn pending_provider_context(
        &self,
    ) -> Result<(PendingTurn, ConfigEpoch, ProviderRequest), RuntimeError> {
        let pending = self
            .state
            .pending
            .as_ref()
            .filter(|pending| pending.phase == PendingPhase::Admitted)
            .cloned()
            .ok_or_else(|| RuntimeError::Busy(self.state.status()))?;
        let config =
            self.state
                .configs
                .get(&pending.config)
                .cloned()
                .ok_or(RuntimeError::CorruptState(
                    "pending Config Epoch is missing",
                ))?;
        let provider_epoch = self.state.providers.get(&pending.provider).cloned().ok_or(
            RuntimeError::CorruptState("pending Provider Epoch is missing"),
        )?;
        let thread = self
            .state
            .thread
            .ok_or(RuntimeError::CorruptState("pending Thread is missing"))?;
        let request = ProviderRequest {
            thread,
            turn: pending.turn,
            config: config.clone(),
            provider: provider_epoch,
            input: pending.input.clone(),
        };
        Ok((pending, config, request))
    }

    fn pending_max_output_bytes(&self, turn: TurnId) -> Result<usize, RuntimeError> {
        let pending = self
            .state
            .pending
            .as_ref()
            .filter(|pending| pending.turn == turn && pending.phase == PendingPhase::Admitted)
            .ok_or_else(|| RuntimeError::Busy(self.state.status()))?;
        Ok(*self
            .state
            .configs
            .get(&pending.config)
            .ok_or(RuntimeError::CorruptState(
                "pending Config Epoch is missing",
            ))?
            .resolved()
            .max_output_bytes()
            .value() as usize)
    }

    fn prepare_output(
        &mut self,
        turn: TurnId,
        deltas: Vec<String>,
        usage_records: Vec<UsageRecord>,
        attempt: u32,
        current_usage: UsageRecord,
    ) -> Result<PreparedOutput, RuntimeError> {
        if usage_records.is_empty() || usage_records.len() > MAX_USAGE_RECORDS_PER_TURN {
            self.finish_usage_attempt_and_block(
                turn,
                attempt,
                UsageAttemptOutcome::Succeeded,
                Some(current_usage),
                "Provider usage record count is invalid",
            )?;
            return Err(RuntimeError::InvalidProviderOutput(
                "Provider usage record count is invalid",
            ));
        }
        let max_output_bytes = self.pending_max_output_bytes(turn)?;
        let mut text = String::new();
        for delta in &deltas {
            let next_length = text
                .len()
                .checked_add(delta.len())
                .ok_or(RuntimeError::IntegerOverflow)?;
            if next_length > max_output_bytes {
                self.finish_usage_attempt_and_block(
                    turn,
                    attempt,
                    UsageAttemptOutcome::Succeeded,
                    Some(current_usage),
                    "Provider output exceeds the frozen Config Epoch limit",
                )?;
                return Err(RuntimeError::InvalidProviderOutput(
                    "Provider output exceeds the frozen Config Epoch limit",
                ));
            }
            text.push_str(delta);
        }
        if text.trim().is_empty() {
            self.finish_usage_attempt_and_block(
                turn,
                attempt,
                UsageAttemptOutcome::Succeeded,
                Some(current_usage),
                "Provider output cannot be empty",
            )?;
            return Err(RuntimeError::InvalidProviderOutput(
                "Provider output cannot be empty",
            ));
        }

        let assistant_item = ItemId::new(self.state.next_item).map_err(RuntimeError::Model)?;
        let delivery = DeliveryId::new(self.state.next_delivery).map_err(RuntimeError::Model)?;
        let completed_at = UsageTimestamp::now().map_err(RuntimeError::Usage)?;
        let finish = self.usage_attempt_finish_events(
            turn,
            attempt,
            completed_at,
            UsageAttemptOutcome::Succeeded,
            Some(current_usage),
        )?;
        let mut events = Vec::with_capacity(deltas.len() + 4);
        events.extend(finish);
        events.push(RuntimeEvent::AssistantItemStarted {
            turn,
            item: assistant_item,
        });
        for delta in deltas {
            events.push(RuntimeEvent::AssistantTextDelta {
                turn,
                item: assistant_item,
                delta,
            });
        }
        events.push(RuntimeEvent::OutputPrepared {
            turn,
            item: assistant_item,
            delivery,
            text: text.clone(),
            usage_records: usage_records.clone(),
            legacy_usage: false,
        });
        let receipt = self.commit(&events)?;
        Ok(PreparedOutput {
            delivery,
            turn,
            text,
            usage_records,
            receipt,
        })
    }

    fn begin_usage_attempt(&mut self, turn: TurnId) -> Result<u32, RuntimeError> {
        let started_at = UsageTimestamp::now().map_err(RuntimeError::Usage)?;
        let pending = self
            .state
            .pending
            .as_ref()
            .filter(|pending| pending.turn == turn && pending.phase == PendingPhase::Admitted)
            .ok_or_else(|| RuntimeError::Busy(self.state.status()))?;
        let config = self
            .state
            .configs
            .get(&pending.config)
            .ok_or(RuntimeError::CorruptState(
                "pending Config Epoch is missing",
            ))?;
        for window in config.usage_windows() {
            window
                .require_current_ruleset()
                .map_err(RuntimeError::Usage)?;
        }
        let attempt = pending.next_usage_attempt;
        let mut events = Vec::with_capacity(2);
        if let Some(open) = pending.open_usage_attempt {
            events.extend(self.usage_attempt_finish_events(
                turn,
                open.attempt,
                started_at,
                UsageAttemptOutcome::Interrupted,
                None,
            )?);
        }
        events.push(RuntimeEvent::UsageAttemptStarted {
            turn,
            attempt,
            started_at,
        });
        self.commit(&events)?;
        Ok(attempt)
    }

    fn finish_usage_attempt(
        &mut self,
        turn: TurnId,
        attempt: u32,
        outcome: UsageAttemptOutcome,
        usage: Option<UsageRecord>,
    ) -> Result<(), RuntimeError> {
        let completed_at = UsageTimestamp::now().map_err(RuntimeError::Usage)?;
        let events =
            self.usage_attempt_finish_events(turn, attempt, completed_at, outcome, usage)?;
        self.commit(&events)?;
        Ok(())
    }

    fn finish_usage_attempt_and_block(
        &mut self,
        turn: TurnId,
        attempt: u32,
        outcome: UsageAttemptOutcome,
        usage: Option<UsageRecord>,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        self.finish_usage_attempt_and_block_with_stage(turn, attempt, outcome, usage, reason, None)
    }

    fn finish_usage_attempt_and_block_for_provider_error(
        &mut self,
        turn: TurnId,
        attempt: u32,
        source: &ProviderError,
    ) -> Result<(), RuntimeError> {
        self.finish_usage_attempt_and_block_with_stage(
            turn,
            attempt,
            UsageAttemptOutcome::Failed,
            None,
            provider_block_reason(source),
            source.unavailable_stage(),
        )
    }

    fn finish_usage_attempt_and_block_with_stage(
        &mut self,
        turn: TurnId,
        attempt: u32,
        outcome: UsageAttemptOutcome,
        usage: Option<UsageRecord>,
        reason: &str,
        provider_unavailable_stage: Option<ProviderUnavailableStage>,
    ) -> Result<(), RuntimeError> {
        let completed_at = UsageTimestamp::now().map_err(RuntimeError::Usage)?;
        let mut events =
            self.usage_attempt_finish_events(turn, attempt, completed_at, outcome, usage)?;
        events.push(RuntimeEvent::TurnBlocked {
            turn,
            reason: bounded_reason(reason),
            origin: TurnBlockOrigin::Provider,
            provider_unavailable_stage,
        });
        self.commit(&events)?;
        Ok(())
    }

    fn usage_attempt_finish_events(
        &self,
        turn: TurnId,
        attempt: u32,
        completed_at: UsageTimestamp,
        outcome: UsageAttemptOutcome,
        usage: Option<UsageRecord>,
    ) -> Result<Vec<RuntimeEvent>, RuntimeError> {
        let pending = self
            .state
            .pending
            .as_ref()
            .filter(|pending| pending.turn == turn && pending.phase == PendingPhase::Admitted)
            .ok_or_else(|| RuntimeError::Busy(self.state.status()))?;
        let open = pending
            .open_usage_attempt
            .filter(|open| open.attempt == attempt)
            .ok_or(RuntimeError::CorruptState("usage attempt is not active"))?;
        let completed_at = completed_at.max(open.started_at);
        let config = self
            .state
            .configs
            .get(&pending.config)
            .ok_or(RuntimeError::CorruptState(
                "pending Config Epoch is missing",
            ))?;
        let mut named_windows = Vec::new();
        for window in config.usage_windows() {
            if window
                .contains(open.started_at)
                .map_err(RuntimeError::Usage)?
            {
                named_windows.push(window.id().to_owned());
            }
        }
        let context = self.state.usage_context(turn)?;
        let cost = usage.as_ref().map_or(
            CostEstimateOutcome::Unknown(CostEstimateUnknownReason::MissingUsageRecord),
            |usage| {
                config.price_schedules().estimate_attempt(
                    &context.profile,
                    &context.model,
                    context.dialect,
                    open.started_at,
                    usage,
                )
            },
        );
        Ok(vec![
            RuntimeEvent::UsageAttemptFinished {
                turn,
                attempt,
                completed_at,
                outcome,
                usage,
                named_windows,
                cost_evaluation_required: true,
            },
            RuntimeEvent::UsageAttemptCostEvaluated {
                turn,
                attempt,
                evaluation: FrozenCostEvaluation::from_outcome(&cost),
            },
        ])
    }

    fn block_pending(&mut self, turn: TurnId, reason: &str) -> Result<(), RuntimeError> {
        let reason = bounded_reason(reason);
        self.commit(&[RuntimeEvent::TurnBlocked {
            turn,
            reason,
            origin: TurnBlockOrigin::Other,
            provider_unavailable_stage: None,
        }])?;
        Ok(())
    }

    fn commit(&mut self, events: &[RuntimeEvent]) -> Result<DurabilityReceipt, RuntimeError> {
        let mut candidate = self.state.clone();
        for event in events {
            candidate.apply(event.clone())?;
        }
        candidate.validate_quiescent()?;
        let encoded = events
            .iter()
            .map(RuntimeEvent::encode)
            .collect::<Result<Vec<_>, _>>()?;
        let receipt = self
            .ledger
            .append(self.ledger.head(), &encoded)
            .map_err(RuntimeError::Ledger)?;
        self.state = candidate;
        self.recovered_tail_bytes = 0;
        Ok(receipt)
    }
}

fn ledger_paths_may_alias(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let (Some(left), Some(right)) = (ledger_path_key(left), ledger_path_key(right)) else {
        return false;
    };
    path_keys_equal(&left, &right)
}

fn ledger_path_key(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }
    let file_name = path.file_name()?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    Some(parent.canonicalize().ok()?.join(file_name))
}

#[cfg(any(windows, target_os = "macos"))]
fn path_keys_equal(left: &Path, right: &Path) -> bool {
    left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn path_keys_equal(left: &Path, right: &Path) -> bool {
    left == right
}

fn validate_input(input: &str) -> Result<(), RuntimeError> {
    if input.trim().is_empty() {
        return Err(RuntimeError::InvalidInput("input cannot be empty"));
    }
    if input.len() > MAX_INPUT_BYTES {
        return Err(RuntimeError::InvalidInput("input is too large"));
    }
    Ok(())
}

enum ValidatedProviderStep {
    Completed {
        deltas: Vec<String>,
        usage_record: UsageRecord,
    },
    ToolCall {
        deltas: Vec<String>,
        call: ProviderToolCall,
        usage_record: UsageRecord,
    },
}

fn validate_provider_events(
    events: &[ProviderEvent],
    max_output_bytes: usize,
) -> Result<ValidatedProviderStep, &'static str> {
    if events.is_empty() {
        return Err("provider emitted no events");
    }
    let mut deltas = Vec::new();
    let mut text = String::new();
    let mut usage = None;
    let mut tool_call = None;
    for (index, event) in events.iter().enumerate() {
        match event {
            ProviderEvent::TextDelta(delta) => {
                if usage.is_some() {
                    return Err("provider emitted text after completion");
                }
                if delta.is_empty() {
                    return Err("provider emitted an empty text delta");
                }
                let next_length = text
                    .len()
                    .checked_add(delta.len())
                    .ok_or("provider output length overflow")?;
                if next_length > max_output_bytes {
                    return Err("provider output exceeds the frozen Config Epoch limit");
                }
                text.push_str(delta);
                deltas.push(delta.clone());
            }
            ProviderEvent::FunctionCall(call) => {
                if usage.is_some() {
                    return Err("provider emitted a Tool call after completion");
                }
                if tool_call.replace(call.clone()).is_some() {
                    return Err("provider emitted more than one Tool call");
                }
            }
            ProviderEvent::Completed(record) => {
                if usage.replace(record.clone()).is_some() {
                    return Err("provider emitted completion more than once");
                }
                if index + 1 != events.len() {
                    return Err("provider completion must be the final event");
                }
            }
        }
    }
    let usage = usage.ok_or("provider did not emit completion")?;
    if tool_call.is_none() && text.trim().is_empty() {
        return Err("provider output cannot be empty");
    }
    Ok(match tool_call {
        Some(call) => ValidatedProviderStep::ToolCall {
            deltas,
            call,
            usage_record: usage,
        },
        None => ValidatedProviderStep::Completed {
            deltas,
            usage_record: usage,
        },
    })
}

fn require_provider_snapshot(
    provider: &impl ProviderRuntime,
    epoch: &ProviderEpoch,
) -> Result<(), RuntimeError> {
    if provider.profile_snapshot() == epoch.profile_snapshot()
        && provider.dialect() == epoch.dialect()
    {
        Ok(())
    } else {
        Err(RuntimeError::Provider(ProviderError::InvalidConfiguration(
            "Provider Runtime does not match the frozen Provider Profile snapshot",
        )))
    }
}

fn provider_tool_identity(turn: TurnId, call_id: &str) -> String {
    let digest = Sha256::digest(call_id.as_bytes());
    let mut identity = format!("provider-turn-{}-", turn.get());
    for byte in digest {
        write!(&mut identity, "{byte:02x}").expect("writing to String cannot fail");
    }
    identity
}

fn bounded_reason(reason: &str) -> String {
    let mut bounded = String::new();
    for character in reason.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if bounded.len() + character.len_utf8() > MAX_BLOCK_REASON_BYTES {
            break;
        }
        bounded.push(character);
    }
    if bounded.trim().is_empty() {
        "provider failure".to_owned()
    } else {
        bounded
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FrozenCostEvaluation {
    Known {
        schedule_id: String,
        schedule_fingerprint: u64,
        amount_pico_units: u64,
    },
    Unknown(CostEstimateUnknownReason),
}

impl FrozenCostEvaluation {
    fn from_outcome(outcome: &CostEstimateOutcome) -> Self {
        match outcome {
            CostEstimateOutcome::Known(estimate) => Self::Known {
                schedule_id: estimate.schedule().id().to_owned(),
                schedule_fingerprint: estimate.schedule().fingerprint(),
                amount_pico_units: estimate.amount_pico_units(),
            },
            CostEstimateOutcome::Unknown(reason) => Self::Unknown(*reason),
        }
    }

    fn matches(&self, outcome: &CostEstimateOutcome) -> bool {
        match (self, outcome) {
            (
                Self::Known {
                    schedule_id,
                    schedule_fingerprint,
                    amount_pico_units,
                },
                CostEstimateOutcome::Known(estimate),
            ) => {
                schedule_id == estimate.schedule().id()
                    && *schedule_fingerprint == estimate.schedule().fingerprint()
                    && *amount_pico_units == estimate.amount_pico_units()
            }
            (Self::Unknown(left), CostEstimateOutcome::Unknown(right)) => left == right,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RuntimeEvent {
    ContextCheckpointPublished {
        checkpoint: ContextCheckpoint,
    },
    ModelSelectionStaged {
        agent: AgentId,
        selection: ModelSelection,
    },
    ThreadCreated {
        thread: ThreadId,
    },
    ConfigFrozen {
        epoch: ConfigEpoch,
    },
    ProviderFrozen {
        epoch: Box<ProviderEpoch>,
    },
    TurnAdmitted {
        thread: ThreadId,
        turn: TurnId,
        user_item: ItemId,
        config: ConfigEpochId,
        provider: ProviderEpochId,
        agent: Option<AgentId>,
        input: String,
    },
    AssistantItemStarted {
        turn: TurnId,
        item: ItemId,
    },
    AssistantTextDelta {
        turn: TurnId,
        item: ItemId,
        delta: String,
    },
    OutputPrepared {
        turn: TurnId,
        item: ItemId,
        delivery: DeliveryId,
        text: String,
        usage_records: Vec<UsageRecord>,
        legacy_usage: bool,
    },
    OutputAcknowledged {
        turn: TurnId,
        delivery: DeliveryId,
    },
    TurnCompleted {
        turn: TurnId,
    },
    TurnBlocked {
        turn: TurnId,
        reason: String,
        origin: TurnBlockOrigin,
        provider_unavailable_stage: Option<ProviderUnavailableStage>,
    },
    TurnCancelled {
        turn: TurnId,
    },
    TurnRetryRequested {
        turn: TurnId,
    },
    UsageAttemptStarted {
        turn: TurnId,
        attempt: u32,
        started_at: UsageTimestamp,
    },
    UsageAttemptFinished {
        turn: TurnId,
        attempt: u32,
        completed_at: UsageTimestamp,
        outcome: UsageAttemptOutcome,
        usage: Option<UsageRecord>,
        named_windows: Vec<String>,
        cost_evaluation_required: bool,
    },
    UsageAttemptCostEvaluated {
        turn: TurnId,
        attempt: u32,
        evaluation: FrozenCostEvaluation,
    },
}

impl RuntimeEvent {
    fn encode(&self) -> Result<EventData, RuntimeError> {
        let mut payload = Encoder::default();
        let kind = match self {
            Self::ContextCheckpointPublished { checkpoint } => {
                encode_context_checkpoint(&mut payload, checkpoint)?;
                17
            }
            Self::ModelSelectionStaged { agent, selection } => {
                payload.u64(agent.get());
                payload.string(selection.preset_id())?;
                payload.u64(selection.config_fingerprint());
                payload.string(selection.provider_profile())?;
                payload.string(selection.provider_model())?;
                payload.u8(provider_dialect_tag(selection.preferred_dialect()));
                14
            }
            Self::ThreadCreated { thread } => {
                payload.u64(thread.get());
                1
            }
            Self::ConfigFrozen { epoch } => {
                encode_config_epoch(&mut payload, epoch)?;
                2
            }
            Self::ProviderFrozen { epoch } => {
                encode_provider_epoch(&mut payload, epoch)?;
                3
            }
            Self::TurnAdmitted {
                thread,
                turn,
                user_item,
                config,
                provider,
                agent,
                input,
            } => {
                payload.u64(thread.get());
                payload.u64(turn.get());
                payload.u64(user_item.get());
                payload.u64(config.get());
                payload.u64(provider.get());
                encode_optional_agent(&mut payload, *agent);
                payload.string(input)?;
                4
            }
            Self::AssistantItemStarted { turn, item } => {
                payload.u64(turn.get());
                payload.u64(item.get());
                5
            }
            Self::AssistantTextDelta { turn, item, delta } => {
                payload.u64(turn.get());
                payload.u64(item.get());
                payload.string(delta)?;
                6
            }
            Self::OutputPrepared {
                turn,
                item,
                delivery,
                text,
                usage_records,
                legacy_usage: _,
            } => {
                payload.u64(turn.get());
                payload.u64(item.get());
                payload.u64(delivery.get());
                payload.string(text)?;
                payload.u32(
                    u32::try_from(usage_records.len())
                        .map_err(|_| RuntimeError::IntegerOverflow)?,
                );
                for usage in usage_records {
                    encode_usage_record(&mut payload, usage)?;
                }
                7
            }
            Self::OutputAcknowledged { turn, delivery } => {
                payload.u64(turn.get());
                payload.u64(delivery.get());
                8
            }
            Self::TurnCompleted { turn } => {
                payload.u64(turn.get());
                9
            }
            Self::TurnBlocked {
                turn,
                reason,
                origin,
                provider_unavailable_stage,
            } => {
                payload.u64(turn.get());
                payload.string(reason)?;
                payload.u8(turn_block_origin_tag(*origin)?);
                encode_optional_provider_unavailable_stage(
                    &mut payload,
                    *provider_unavailable_stage,
                );
                10
            }
            Self::TurnCancelled { turn } => {
                payload.u64(turn.get());
                15
            }
            Self::TurnRetryRequested { turn } => {
                payload.u64(turn.get());
                16
            }
            Self::UsageAttemptStarted {
                turn,
                attempt,
                started_at,
            } => {
                payload.u64(turn.get());
                payload.u32(*attempt);
                payload.i64(started_at.unix_millis());
                11
            }
            Self::UsageAttemptFinished {
                turn,
                attempt,
                completed_at,
                outcome,
                usage,
                named_windows,
                cost_evaluation_required: _,
            } => {
                payload.u64(turn.get());
                payload.u32(*attempt);
                payload.i64(completed_at.unix_millis());
                payload.u8(usage_attempt_outcome_tag(*outcome));
                match usage {
                    None => payload.u8(0),
                    Some(usage) => {
                        payload.u8(1);
                        encode_usage_record(&mut payload, usage)?;
                    }
                }
                payload.u32(
                    u32::try_from(named_windows.len())
                        .map_err(|_| RuntimeError::IntegerOverflow)?,
                );
                for window in named_windows {
                    payload.string(window)?;
                }
                12
            }
            Self::UsageAttemptCostEvaluated {
                turn,
                attempt,
                evaluation,
            } => {
                payload.u64(turn.get());
                payload.u32(*attempt);
                match evaluation {
                    FrozenCostEvaluation::Known {
                        schedule_id,
                        schedule_fingerprint,
                        amount_pico_units,
                    } => {
                        payload.u8(1);
                        payload.string(schedule_id)?;
                        payload.u64(*schedule_fingerprint);
                        payload.u64(*amount_pico_units);
                    }
                    FrozenCostEvaluation::Unknown(reason) => {
                        payload.u8(2);
                        payload.u8(cost_unknown_reason_tag(*reason));
                    }
                }
                13
            }
        };
        Ok(EventData {
            schema: RUNTIME_EVENT_SCHEMA,
            kind,
            payload: payload.finish(),
        })
    }

    fn decode(event: &StoredEvent) -> Result<Self, RuntimeError> {
        if !(1..=RUNTIME_EVENT_SCHEMA).contains(&event.data.schema) {
            return Err(RuntimeError::UnsupportedRuntimeEventSchema {
                supported: RUNTIME_EVENT_SCHEMA,
                actual: event.data.schema,
            });
        }
        let mut payload = Decoder::new(&event.data.payload);
        let decoded = match event.data.kind {
            17 if event.data.schema >= 12 => Self::ContextCheckpointPublished {
                checkpoint: decode_context_checkpoint(&mut payload)?,
            },
            14 if event.data.schema >= 9 => Self::ModelSelectionStaged {
                agent: AgentId::from_stored(payload.u64()?).ok_or(RuntimeError::CorruptEvent(
                    "invalid Agent ID in Model selection",
                ))?,
                selection: ModelSelection::new(
                    payload.string(MAX_CONFIG_ID_BYTES)?,
                    payload.u64()?,
                    payload.string(MAX_PROVIDER_ID_BYTES)?,
                    payload.string(MAX_PROVIDER_ID_BYTES)?,
                    decode_provider_dialect(payload.u8()?)?,
                )
                .map_err(|_| RuntimeError::CorruptEvent("invalid Model selection"))?,
            },
            1 => Self::ThreadCreated {
                thread: ThreadId::new(payload.u64()?).map_err(RuntimeError::Model)?,
            },
            2 => Self::ConfigFrozen {
                epoch: decode_config_epoch(&mut payload, event.data.schema)?,
            },
            3 => Self::ProviderFrozen {
                epoch: Box::new(decode_provider_epoch(&mut payload, event.data.schema)?),
            },
            4 => Self::TurnAdmitted {
                thread: ThreadId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                user_item: ItemId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                config: ConfigEpochId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                provider: ProviderEpochId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                agent: if event.data.schema >= 4 {
                    decode_optional_agent(&mut payload)?
                } else {
                    None
                },
                input: payload.string(MAX_INPUT_BYTES)?,
            },
            5 => Self::AssistantItemStarted {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                item: ItemId::new(payload.u64()?).map_err(RuntimeError::Model)?,
            },
            6 => Self::AssistantTextDelta {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                item: ItemId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                delta: payload.string(MAX_INPUT_BYTES)?,
            },
            7 => Self::OutputPrepared {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                item: ItemId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                delivery: DeliveryId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                text: payload.string(MAX_INPUT_BYTES)?,
                usage_records: if event.data.schema == 1 {
                    vec![UsageRecord::estimated(payload.u32()?, payload.u32()?)]
                } else {
                    let count = payload.u32()? as usize;
                    if count == 0 || count > MAX_USAGE_RECORDS_PER_TURN {
                        return Err(RuntimeError::CorruptEvent(
                            "Runtime usage record count is invalid",
                        ));
                    }
                    (0..count)
                        .map(|_| decode_usage_record(&mut payload, event.data.schema))
                        .collect::<Result<Vec<_>, _>>()?
                },
                legacy_usage: event.data.schema < 4,
            },
            8 => Self::OutputAcknowledged {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                delivery: DeliveryId::new(payload.u64()?).map_err(RuntimeError::Model)?,
            },
            9 => Self::TurnCompleted {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
            },
            10 => Self::TurnBlocked {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                reason: payload.string(MAX_BLOCK_REASON_BYTES)?,
                origin: if event.data.schema >= 10 {
                    decode_turn_block_origin(payload.u8()?)?
                } else {
                    TurnBlockOrigin::Legacy
                },
                provider_unavailable_stage: if event.data.schema >= 11 {
                    decode_optional_provider_unavailable_stage(&mut payload)?
                } else {
                    None
                },
            },
            15 if event.data.schema >= 10 => Self::TurnCancelled {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
            },
            16 if event.data.schema >= 11 => Self::TurnRetryRequested {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
            },
            11 if event.data.schema >= 4 => Self::UsageAttemptStarted {
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                attempt: payload.u32()?,
                started_at: UsageTimestamp::from_unix_millis(payload.i64()?)
                    .map_err(RuntimeError::Usage)?,
            },
            12 if event.data.schema >= 4 => {
                let turn = TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?;
                let attempt = payload.u32()?;
                let completed_at = UsageTimestamp::from_unix_millis(payload.i64()?)
                    .map_err(RuntimeError::Usage)?;
                let outcome = decode_usage_attempt_outcome(payload.u8()?)?;
                let usage = match payload.u8()? {
                    0 => None,
                    1 => Some(decode_usage_record(&mut payload, event.data.schema)?),
                    _ => {
                        return Err(RuntimeError::CorruptEvent(
                            "invalid optional usage record tag",
                        ));
                    }
                };
                let count = payload.u32()? as usize;
                if count > MAX_USAGE_WINDOWS {
                    return Err(RuntimeError::CorruptEvent(
                        "usage attempt window count is invalid",
                    ));
                }
                let named_windows = (0..count)
                    .map(|_| payload.string(MAX_PROVIDER_ID_BYTES))
                    .collect::<Result<Vec<_>, _>>()?;
                Self::UsageAttemptFinished {
                    turn,
                    attempt,
                    completed_at,
                    outcome,
                    usage,
                    named_windows,
                    cost_evaluation_required: event.data.schema >= 5,
                }
            }
            13 if event.data.schema >= 5 => {
                let turn = TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?;
                let attempt = payload.u32()?;
                let evaluation = match payload.u8()? {
                    1 => FrozenCostEvaluation::Known {
                        schedule_id: payload.string(MAX_PROVIDER_ID_BYTES)?,
                        schedule_fingerprint: payload.u64()?,
                        amount_pico_units: payload.u64()?,
                    },
                    2 => FrozenCostEvaluation::Unknown(decode_cost_unknown_reason(payload.u8()?)?),
                    _ => {
                        return Err(RuntimeError::CorruptEvent(
                            "invalid Cost Estimate outcome tag",
                        ));
                    }
                };
                Self::UsageAttemptCostEvaluated {
                    turn,
                    attempt,
                    evaluation,
                }
            }
            _ => return Err(RuntimeError::CorruptEvent("unknown Runtime Event kind")),
        };
        payload.finish()?;
        Ok(decoded)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TurnRecord {
    user_item: ItemId,
    config: ConfigEpochId,
    provider: ProviderEpochId,
    agent: Option<AgentId>,
    assistant_item: Option<ItemId>,
    delivery: Option<DeliveryId>,
    completed: bool,
    cancelled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnBlockOrigin {
    Legacy,
    Provider,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingPhase {
    Admitted,
    Streaming,
    Prepared,
    Blocked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedState {
    item: ItemId,
    delivery: DeliveryId,
    usage_records: Vec<UsageRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenUsageAttempt {
    attempt: u32,
    started_at: UsageTimestamp,
}

struct UsageContext {
    thread: ThreadId,
    agent: Option<AgentId>,
    profile: String,
    model: String,
    dialect: Option<ProviderDialect>,
    reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<ServiceTier>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingTurn {
    turn: TurnId,
    config: ConfigEpochId,
    provider: ProviderEpochId,
    agent: Option<AgentId>,
    input: String,
    phase: PendingPhase,
    assistant_item: Option<ItemId>,
    streamed_text: String,
    prepared: Option<PreparedState>,
    acknowledged: bool,
    blocked_reason: Option<String>,
    block_origin: Option<TurnBlockOrigin>,
    provider_unavailable_stage: Option<ProviderUnavailableStage>,
    next_usage_attempt: u32,
    open_usage_attempt: Option<OpenUsageAttempt>,
}

#[derive(Clone, Debug)]
struct RuntimeState {
    thread: Option<ThreadId>,
    configs: BTreeMap<ConfigEpochId, ConfigEpoch>,
    providers: BTreeMap<ProviderEpochId, ProviderEpoch>,
    turns: BTreeMap<TurnId, TurnRecord>,
    items: Vec<CanonicalItem>,
    pending: Option<PendingTurn>,
    pending_model_selection: Option<PendingModelSelection>,
    context_checkpoint: Option<ContextCheckpoint>,
    acknowledged: BTreeSet<DeliveryId>,
    usage: UsageProjection,
    pending_cost_evaluation: Option<(TurnId, u32)>,
    next_thread: u64,
    next_turn: u64,
    next_item: u64,
    next_delivery: u64,
    next_config: u64,
    next_provider: u64,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            thread: None,
            configs: BTreeMap::new(),
            providers: BTreeMap::new(),
            turns: BTreeMap::new(),
            items: Vec::new(),
            pending: None,
            pending_model_selection: None,
            context_checkpoint: None,
            acknowledged: BTreeSet::new(),
            usage: UsageProjection::default(),
            pending_cost_evaluation: None,
            next_thread: 1,
            next_turn: 1,
            next_item: 1,
            next_delivery: 1,
            next_config: 1,
            next_provider: 1,
        }
    }
}

impl RuntimeState {
    fn status(&self) -> RecoveryStatus {
        let Some(pending) = &self.pending else {
            return RecoveryStatus::Ready;
        };
        match pending.phase {
            PendingPhase::Admitted => RecoveryStatus::ResumeRequired { turn: pending.turn },
            PendingPhase::Prepared => RecoveryStatus::ReconciliationRequired {
                turn: pending.turn,
                delivery: pending.prepared.as_ref().expect("prepared phase").delivery,
            },
            PendingPhase::Blocked => RecoveryStatus::Blocked {
                turn: pending.turn,
                reason: pending
                    .blocked_reason
                    .clone()
                    .expect("blocked phase has reason"),
                retryable: provider_stage_is_retryable(pending.provider_unavailable_stage),
            },
            PendingPhase::Streaming => RecoveryStatus::Blocked {
                turn: pending.turn,
                reason: "incomplete output transaction".to_owned(),
                retryable: false,
            },
        }
    }

    fn apply(&mut self, event: RuntimeEvent) -> Result<(), RuntimeError> {
        if let Some((pending_turn, pending_attempt)) = self.pending_cost_evaluation
            && !matches!(
                &event,
                RuntimeEvent::UsageAttemptCostEvaluated { turn, attempt, .. }
                    if *turn == pending_turn && *attempt == pending_attempt
            )
        {
            return Err(RuntimeError::CorruptState(
                "Usage cost evaluation must immediately follow its attempt",
            ));
        }
        match event {
            RuntimeEvent::ContextCheckpointPublished { checkpoint } => {
                if self.pending.is_some() {
                    return Err(RuntimeError::CorruptState(
                        "Context checkpoint was published outside a Safe Barrier",
                    ));
                }
                checkpoint
                    .view
                    .validate_against_items(&self.items)
                    .map_err(RuntimeError::Context)?;
                self.context_checkpoint = Some(checkpoint);
            }
            RuntimeEvent::ModelSelectionStaged { agent, selection } => {
                if self.pending.is_some() {
                    return Err(RuntimeError::CorruptState(
                        "Model selection was staged during an active Turn",
                    ));
                }
                if self
                    .pending_model_selection
                    .as_ref()
                    .is_some_and(|pending| pending.agent != agent)
                {
                    return Err(RuntimeError::CorruptState(
                        "Model selection changed Agent ownership",
                    ));
                }
                self.pending_model_selection = Some(PendingModelSelection { agent, selection });
            }
            RuntimeEvent::ThreadCreated { thread } => {
                if self.thread.replace(thread).is_some() || !self.turns.is_empty() {
                    return Err(RuntimeError::CorruptState(
                        "Thread was created more than once",
                    ));
                }
                observe_id(&mut self.next_thread, thread.get())?;
            }
            RuntimeEvent::ConfigFrozen { epoch } => {
                let id = epoch.id();
                if self.configs.insert(id, epoch).is_some() {
                    return Err(RuntimeError::CorruptState("duplicate Config Epoch"));
                }
                observe_id(&mut self.next_config, id.get())?;
            }
            RuntimeEvent::ProviderFrozen { epoch } => {
                let id = epoch.id();
                if self.providers.insert(id, *epoch).is_some() {
                    return Err(RuntimeError::CorruptState("duplicate Provider Epoch"));
                }
                observe_id(&mut self.next_provider, id.get())?;
            }
            RuntimeEvent::TurnAdmitted {
                thread,
                turn,
                user_item,
                config,
                provider,
                agent,
                input,
            } => {
                if self.thread != Some(thread) || self.pending.is_some() {
                    return Err(RuntimeError::CorruptState("invalid Turn admission"));
                }
                if !self.configs.contains_key(&config) || !self.providers.contains_key(&provider) {
                    return Err(RuntimeError::CorruptState("Turn snapshot is missing"));
                }
                if let Some(pending) = &self.pending_model_selection {
                    let config_epoch = self
                        .configs
                        .get(&config)
                        .ok_or(RuntimeError::CorruptState("Turn snapshot is missing"))?;
                    let resolved = config_epoch.resolved();
                    if agent != Some(pending.agent)
                        || config_epoch.fingerprint() != pending.selection.config_fingerprint()
                        || resolved.provider_profile().value()
                            != pending.selection.provider_profile()
                        || resolved.provider_model().value() != pending.selection.provider_model()
                    {
                        return Err(RuntimeError::CorruptState(
                            "Turn does not match pending Model selection",
                        ));
                    }
                    self.pending_model_selection = None;
                }
                if self.turns.contains_key(&turn) || self.item_exists(user_item) {
                    return Err(RuntimeError::CorruptState("duplicate Turn or Item id"));
                }
                validate_input(&input)?;
                self.items.push(
                    CanonicalItem::new(user_item, turn, ItemRole::User, input.clone())
                        .map_err(RuntimeError::Model)?,
                );
                self.turns.insert(
                    turn,
                    TurnRecord {
                        user_item,
                        config,
                        provider,
                        agent,
                        assistant_item: None,
                        delivery: None,
                        completed: false,
                        cancelled: false,
                    },
                );
                self.pending = Some(PendingTurn {
                    turn,
                    config,
                    provider,
                    agent,
                    input,
                    phase: PendingPhase::Admitted,
                    assistant_item: None,
                    streamed_text: String::new(),
                    prepared: None,
                    acknowledged: false,
                    blocked_reason: None,
                    block_origin: None,
                    provider_unavailable_stage: None,
                    next_usage_attempt: 1,
                    open_usage_attempt: None,
                });
                observe_id(&mut self.next_turn, turn.get())?;
                observe_id(&mut self.next_item, user_item.get())?;
            }
            RuntimeEvent::AssistantItemStarted { turn, item } => {
                if self.item_exists(item) {
                    return Err(RuntimeError::CorruptState("duplicate Assistant Item id"));
                }
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Admitted || pending.assistant_item.is_some() {
                    return Err(RuntimeError::CorruptState("invalid Assistant Item start"));
                }
                pending.phase = PendingPhase::Streaming;
                pending.assistant_item = Some(item);
                observe_id(&mut self.next_item, item.get())?;
            }
            RuntimeEvent::AssistantTextDelta { turn, item, delta } => {
                if delta.is_empty() {
                    return Err(RuntimeError::CorruptState("empty Assistant text delta"));
                }
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Streaming || pending.assistant_item != Some(item)
                {
                    return Err(RuntimeError::CorruptState("invalid Assistant text delta"));
                }
                let next_length = pending
                    .streamed_text
                    .len()
                    .checked_add(delta.len())
                    .ok_or(RuntimeError::IntegerOverflow)?;
                if next_length > MAX_INPUT_BYTES {
                    return Err(RuntimeError::CorruptState("Assistant output is too large"));
                }
                pending.streamed_text.push_str(&delta);
            }
            RuntimeEvent::OutputPrepared {
                turn,
                item,
                delivery,
                text,
                usage_records,
                legacy_usage,
            } => {
                if self.acknowledged.contains(&delivery) {
                    return Err(RuntimeError::CorruptState("duplicate Delivery id"));
                }
                if usage_records.is_empty() || usage_records.len() > MAX_USAGE_RECORDS_PER_TURN {
                    return Err(RuntimeError::CorruptState(
                        "invalid prepared usage record count",
                    ));
                }
                let config = {
                    let pending = self.pending_for(turn)?;
                    if pending.phase != PendingPhase::Streaming
                        || pending.assistant_item != Some(item)
                        || pending.streamed_text != text
                        || pending.prepared.is_some()
                    {
                        return Err(RuntimeError::CorruptState("invalid prepared output"));
                    }
                    pending.config
                };
                let max_output_bytes = *self
                    .configs
                    .get(&config)
                    .ok_or(RuntimeError::CorruptState(
                        "prepared Config Epoch is missing",
                    ))?
                    .resolved()
                    .max_output_bytes()
                    .value() as usize;
                if text.trim().is_empty() {
                    return Err(RuntimeError::CorruptState(
                        "prepared output cannot be empty",
                    ));
                }
                if text.len() > max_output_bytes {
                    return Err(RuntimeError::CorruptState(
                        "prepared output exceeds the frozen Config Epoch limit",
                    ));
                }
                if legacy_usage {
                    let context = self.usage_context(turn)?;
                    let next_attempt = self
                        .pending
                        .as_ref()
                        .ok_or(RuntimeError::CorruptState("prepared Turn is missing"))?
                        .next_usage_attempt;
                    for (offset, usage) in usage_records.iter().cloned().enumerate() {
                        let attempt = next_attempt
                            .checked_add(
                                u32::try_from(offset).map_err(|_| RuntimeError::IntegerOverflow)?,
                            )
                            .ok_or(RuntimeError::IntegerOverflow)?;
                        self.usage
                            .record(
                                UsageAttempt::new(
                                    attempt,
                                    context.thread.get(),
                                    turn.get(),
                                    context.agent.map(AgentId::get),
                                    context.profile.clone(),
                                    context.model.clone(),
                                    context.dialect,
                                    None,
                                    None,
                                    UsageAttemptOutcome::Succeeded,
                                    Some(usage),
                                    Vec::new(),
                                )
                                .map_err(RuntimeError::Usage)?
                                .with_requested_policy(
                                    context.reasoning_effort.map(ReasoningEffort::as_str),
                                    context.service_tier.map(ServiceTier::as_str),
                                ),
                            )
                            .map_err(RuntimeError::Usage)?;
                    }
                    let count = u32::try_from(usage_records.len())
                        .map_err(|_| RuntimeError::IntegerOverflow)?;
                    let pending = self.pending_for(turn)?;
                    pending.next_usage_attempt = pending
                        .next_usage_attempt
                        .checked_add(count)
                        .ok_or(RuntimeError::IntegerOverflow)?;
                } else if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.open_usage_attempt.is_some())
                {
                    return Err(RuntimeError::CorruptState(
                        "prepared output has an unfinished usage attempt",
                    ));
                }
                self.items.push(
                    CanonicalItem::new(item, turn, ItemRole::Assistant, text)
                        .map_err(RuntimeError::Model)?,
                );
                let pending = self.pending_for(turn)?;
                pending.phase = PendingPhase::Prepared;
                pending.prepared = Some(PreparedState {
                    item,
                    delivery,
                    usage_records,
                });
                let record = self
                    .turns
                    .get_mut(&turn)
                    .ok_or(RuntimeError::CorruptState("prepared Turn is missing"))?;
                record.assistant_item = Some(item);
                record.delivery = Some(delivery);
                observe_id(&mut self.next_delivery, delivery.get())?;
            }
            RuntimeEvent::OutputAcknowledged { turn, delivery } => {
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Prepared
                    || pending.acknowledged
                    || pending.prepared.as_ref().map(|value| value.delivery) != Some(delivery)
                {
                    return Err(RuntimeError::CorruptState("invalid output acknowledgement"));
                }
                pending.acknowledged = true;
                if !self.acknowledged.insert(delivery) {
                    return Err(RuntimeError::CorruptState(
                        "duplicate output acknowledgement",
                    ));
                }
            }
            RuntimeEvent::TurnCompleted { turn } => {
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Prepared || !pending.acknowledged {
                    return Err(RuntimeError::CorruptState(
                        "Turn completed before output ack",
                    ));
                }
                let record = self
                    .turns
                    .get_mut(&turn)
                    .ok_or(RuntimeError::CorruptState("completed Turn is missing"))?;
                record.completed = true;
                self.pending = None;
            }
            RuntimeEvent::TurnBlocked {
                turn,
                reason,
                origin,
                provider_unavailable_stage,
            } => {
                if reason.trim().is_empty()
                    || reason.len() > MAX_BLOCK_REASON_BYTES
                    || reason.chars().any(char::is_control)
                    || (provider_unavailable_stage.is_some() && origin != TurnBlockOrigin::Provider)
                {
                    return Err(RuntimeError::CorruptState("invalid blocked reason"));
                }
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Admitted || pending.open_usage_attempt.is_some() {
                    return Err(RuntimeError::CorruptState("invalid blocked transition"));
                }
                pending.phase = PendingPhase::Blocked;
                pending.blocked_reason = Some(reason);
                pending.block_origin = Some(origin);
                pending.provider_unavailable_stage = provider_unavailable_stage;
            }
            RuntimeEvent::TurnCancelled { turn } => {
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Blocked
                    || pending.block_origin != Some(TurnBlockOrigin::Provider)
                    || pending.open_usage_attempt.is_some()
                    || pending.prepared.is_some()
                    || pending.assistant_item.is_some()
                    || !pending.streamed_text.is_empty()
                    || pending.acknowledged
                {
                    return Err(RuntimeError::CorruptState("invalid Turn cancellation"));
                }
                let record = self
                    .turns
                    .get_mut(&turn)
                    .ok_or(RuntimeError::CorruptState("cancelled Turn is missing"))?;
                if record.completed || record.cancelled {
                    return Err(RuntimeError::CorruptState("invalid Turn cancellation"));
                }
                record.cancelled = true;
                self.pending = None;
            }
            RuntimeEvent::TurnRetryRequested { turn } => {
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Blocked
                    || pending.block_origin != Some(TurnBlockOrigin::Provider)
                    || !provider_stage_is_retryable(pending.provider_unavailable_stage)
                    || pending.open_usage_attempt.is_some()
                    || pending.prepared.is_some()
                    || pending.assistant_item.is_some()
                    || !pending.streamed_text.is_empty()
                    || pending.acknowledged
                {
                    return Err(RuntimeError::CorruptState("invalid Turn retry"));
                }
                pending.phase = PendingPhase::Admitted;
                pending.blocked_reason = None;
                pending.block_origin = None;
                pending.provider_unavailable_stage = None;
            }
            RuntimeEvent::UsageAttemptStarted {
                turn,
                attempt,
                started_at,
            } => {
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Admitted
                    || pending.open_usage_attempt.is_some()
                    || attempt != pending.next_usage_attempt
                    || attempt == 0
                {
                    return Err(RuntimeError::CorruptState("invalid usage attempt start"));
                }
                pending.next_usage_attempt = pending
                    .next_usage_attempt
                    .checked_add(1)
                    .ok_or(RuntimeError::IntegerOverflow)?;
                pending.open_usage_attempt = Some(OpenUsageAttempt {
                    attempt,
                    started_at,
                });
            }
            RuntimeEvent::UsageAttemptFinished {
                turn,
                attempt,
                completed_at,
                outcome,
                usage,
                named_windows,
                cost_evaluation_required,
            } => {
                if matches!(outcome, UsageAttemptOutcome::Succeeded) != usage.is_some() {
                    return Err(RuntimeError::CorruptState(
                        "usage attempt outcome and record disagree",
                    ));
                }
                let open = self
                    .pending
                    .as_ref()
                    .filter(|pending| {
                        pending.turn == turn && pending.phase == PendingPhase::Admitted
                    })
                    .and_then(|pending| pending.open_usage_attempt)
                    .filter(|open| open.attempt == attempt)
                    .ok_or(RuntimeError::CorruptState("invalid usage attempt finish"))?;
                if completed_at < open.started_at {
                    return Err(RuntimeError::CorruptState(
                        "usage attempt completed before it started",
                    ));
                }
                let config = self
                    .pending
                    .as_ref()
                    .filter(|pending| pending.turn == turn)
                    .map(|pending| pending.config)
                    .ok_or(RuntimeError::CorruptState("usage Turn is missing"))?;
                let windows = self
                    .configs
                    .get(&config)
                    .ok_or(RuntimeError::CorruptState("usage Config Epoch is missing"))?
                    .usage_windows();
                let mut seen = BTreeSet::new();
                let mut resolved_windows = Vec::with_capacity(named_windows.len());
                for window in &named_windows {
                    let Some(resolved) = windows.iter().find(|candidate| candidate.id() == window)
                    else {
                        return Err(RuntimeError::CorruptState(
                            "usage attempt named window is invalid",
                        ));
                    };
                    if !seen.insert(window.as_str()) {
                        return Err(RuntimeError::CorruptState(
                            "usage attempt named window is invalid",
                        ));
                    }
                    resolved_windows.push(resolved.clone());
                }
                let context = self.usage_context(turn)?;
                let usage_attempt = UsageAttempt::new(
                    attempt,
                    context.thread.get(),
                    turn.get(),
                    context.agent.map(AgentId::get),
                    context.profile,
                    context.model,
                    context.dialect,
                    Some(open.started_at),
                    Some(completed_at),
                    outcome,
                    usage,
                    resolved_windows,
                )
                .map_err(RuntimeError::Usage)?
                .with_requested_policy(
                    context.reasoning_effort.map(ReasoningEffort::as_str),
                    context.service_tier.map(ServiceTier::as_str),
                );
                self.pending_for(turn)?.open_usage_attempt = None;
                self.usage
                    .record(usage_attempt)
                    .map_err(RuntimeError::Usage)?;
                if cost_evaluation_required {
                    self.pending_cost_evaluation = Some((turn, attempt));
                }
            }
            RuntimeEvent::UsageAttemptCostEvaluated {
                turn,
                attempt,
                evaluation,
            } => {
                if self.pending_cost_evaluation != Some((turn, attempt)) {
                    return Err(RuntimeError::CorruptState(
                        "Cost Estimate has no pending Usage Attempt",
                    ));
                }
                let context = self.usage_context(turn)?;
                let config = self
                    .turns
                    .get(&turn)
                    .map(|record| record.config)
                    .and_then(|config| self.configs.get(&config))
                    .ok_or(RuntimeError::CorruptState(
                        "Cost Estimate Config Epoch is missing",
                    ))?;
                let attempt_record =
                    self.usage
                        .attempt(turn.get(), attempt)
                        .ok_or(RuntimeError::CorruptState(
                            "Cost Estimate usage attempt is missing",
                        ))?;
                let expected = match attempt_record.usage() {
                    None => {
                        CostEstimateOutcome::Unknown(CostEstimateUnknownReason::MissingUsageRecord)
                    }
                    Some(usage) => {
                        let started_at =
                            attempt_record
                                .started_at()
                                .ok_or(RuntimeError::CorruptState(
                                    "Cost Estimate usage attempt has no start instant",
                                ))?;
                        config.price_schedules().estimate_attempt(
                            &context.profile,
                            &context.model,
                            context.dialect,
                            started_at,
                            usage,
                        )
                    }
                };
                if !evaluation.matches(&expected) {
                    return Err(RuntimeError::CorruptState(
                        "Cost Estimate does not match frozen usage and pricing evidence",
                    ));
                }
                self.usage
                    .record_cost_evaluation(turn.get(), attempt, expected)
                    .map_err(RuntimeError::Usage)?;
                self.pending_cost_evaluation = None;
            }
        }
        Ok(())
    }

    fn validate_quiescent(&self) -> Result<(), RuntimeError> {
        if self.turns.is_empty() != self.thread.is_none() {
            return Err(RuntimeError::CorruptState(
                "Thread and Turn history disagree",
            ));
        }
        if self.configs.len() != self.turns.len() || self.providers.len() != self.turns.len() {
            return Err(RuntimeError::CorruptState(
                "snapshot and Turn counts disagree",
            ));
        }
        if matches!(
            self.pending.as_ref().map(|pending| pending.phase),
            Some(PendingPhase::Streaming)
        ) {
            return Err(RuntimeError::CorruptState(
                "output transaction ended while streaming",
            ));
        }
        if self.pending_cost_evaluation.is_some() {
            return Err(RuntimeError::CorruptState(
                "Usage transaction ended before cost evaluation",
            ));
        }
        for (turn, record) in &self.turns {
            if !self.configs.contains_key(&record.config)
                || !self.providers.contains_key(&record.provider)
                || !self
                    .items
                    .iter()
                    .any(|item| item.id() == record.user_item && item.turn() == *turn)
            {
                return Err(RuntimeError::CorruptState("Turn record is incomplete"));
            }
            if record.completed && record.delivery.is_none() {
                return Err(RuntimeError::CorruptState("completed Turn has no delivery"));
            }
            if record.completed && record.cancelled {
                return Err(RuntimeError::CorruptState(
                    "Turn has conflicting terminal states",
                ));
            }
            if record.cancelled && (record.assistant_item.is_some() || record.delivery.is_some()) {
                return Err(RuntimeError::CorruptState(
                    "cancelled Turn has prepared output",
                ));
            }
            let is_pending = self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.turn == *turn);
            if is_pending == (record.completed || record.cancelled) {
                return Err(RuntimeError::CorruptState(
                    "Turn terminal state disagrees with pending state",
                ));
            }
        }
        Ok(())
    }

    fn pending_for(&mut self, turn: TurnId) -> Result<&mut PendingTurn, RuntimeError> {
        self.pending
            .as_mut()
            .filter(|pending| pending.turn == turn)
            .ok_or(RuntimeError::CorruptState(
                "event targets a non-pending Turn",
            ))
    }

    fn item_exists(&self, item: ItemId) -> bool {
        self.items.iter().any(|candidate| candidate.id() == item)
            || self
                .pending
                .as_ref()
                .and_then(|pending| pending.assistant_item)
                == Some(item)
    }

    fn usage_context(&self, turn: TurnId) -> Result<UsageContext, RuntimeError> {
        let thread = self
            .thread
            .ok_or(RuntimeError::CorruptState("usage Thread is missing"))?;
        let record = self
            .turns
            .get(&turn)
            .ok_or(RuntimeError::CorruptState("usage Turn is missing"))?;
        let provider = self
            .providers
            .get(&record.provider)
            .ok_or(RuntimeError::CorruptState(
                "usage Provider Epoch is missing",
            ))?;
        let config = self
            .configs
            .get(&record.config)
            .ok_or(RuntimeError::CorruptState("usage Config Epoch is missing"))?;
        Ok(UsageContext {
            thread,
            agent: record.agent,
            profile: provider.profile().to_owned(),
            model: provider.model().to_owned(),
            dialect: provider.dialect(),
            reasoning_effort: config
                .resolved()
                .reasoning_effort()
                .map(|value| *value.value()),
            service_tier: config.resolved().service_tier().map(|value| *value.value()),
        })
    }
}

fn replay_runtime(events: &[StoredEvent]) -> Result<RuntimeState, RuntimeError> {
    let mut state = RuntimeState::default();
    let mut index = 0;
    while index < events.len() {
        let prior_head = index
            .checked_sub(1)
            .map_or_else(LedgerHead::default, |prior| LedgerHead {
                transaction: events[prior].transaction,
                sequence: events[prior].sequence,
            });
        let transaction = events[index].transaction;
        let mut candidate = state.clone();
        while index < events.len() && events[index].transaction == transaction {
            let event = RuntimeEvent::decode(&events[index])?;
            if let RuntimeEvent::ContextCheckpointPublished { checkpoint } = &event
                && (events[index].events_in_transaction != 1
                    || checkpoint.source().head() != prior_head)
            {
                return Err(RuntimeError::CorruptEvent(
                    "Context checkpoint is not bound to the prior Ledger head",
                ));
            }
            candidate.apply(event)?;
            index += 1;
        }
        candidate.validate_quiescent()?;
        state = candidate;
    }
    Ok(state)
}

fn observe_id(next: &mut u64, observed: u64) -> Result<(), RuntimeError> {
    let candidate = observed
        .checked_add(1)
        .ok_or(RuntimeError::IntegerOverflow)?;
    *next = (*next).max(candidate);
    Ok(())
}

fn provider_block_reason(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::InvalidConfiguration(_) => "Provider configuration was rejected",
        ProviderError::InvalidRequest(_) => "Provider request was rejected",
        ProviderError::InvalidResponse(_) => "Provider response was rejected",
        ProviderError::Unavailable {
            stage: ProviderUnavailableStage::BeforeResponse,
            ..
        } => "Provider became unavailable before a response",
        ProviderError::Unavailable {
            stage: ProviderUnavailableStage::BeforeFirstEvent,
            ..
        } => "Provider stream failed before its first event",
        ProviderError::Unavailable {
            stage: ProviderUnavailableStage::AfterFirstEvent,
            ..
        } => "Provider stream was interrupted after a partial response",
    }
}

fn encode_context_checkpoint(
    encoder: &mut Encoder,
    checkpoint: &ContextCheckpoint,
) -> Result<(), RuntimeError> {
    let head = checkpoint.source().head();
    encoder.u64(head.transaction);
    encoder.u64(head.sequence);
    encoder.u32(
        u32::try_from(checkpoint.view.artifacts().len())
            .map_err(|_| RuntimeError::IntegerOverflow)?,
    );
    for artifact in checkpoint.view.artifacts() {
        encoder.u64(artifact.item());
        encoder.u64(artifact.turn());
        encoder.u8(context_view_role_tag(artifact.role()));
        encoder.u64(artifact.byte_len());
        encoder.u64(artifact.estimated_tokens());
        encoder.raw(artifact.digest());
    }
    encoder.u32(
        u32::try_from(checkpoint.view.recent_items().len())
            .map_err(|_| RuntimeError::IntegerOverflow)?,
    );
    for item in checkpoint.view.recent_items() {
        encoder.u64(item.item());
        encoder.u64(item.turn());
        encoder.u8(context_view_role_tag(item.role()));
        encoder.string(item.text())?;
    }
    Ok(())
}

fn decode_context_checkpoint(decoder: &mut Decoder<'_>) -> Result<ContextCheckpoint, RuntimeError> {
    let transaction = decoder.u64()?;
    let sequence = decoder.u64()?;
    if (transaction == 0) != (sequence == 0) {
        return Err(RuntimeError::CorruptEvent(
            "Context checkpoint source head is invalid",
        ));
    }
    let source = ContextEventRange::from_head(LedgerHead {
        transaction,
        sequence,
    });
    let artifact_count = decoder.u32()? as usize;
    if artifact_count > MAX_CONTEXT_VIEW_ITEMS {
        return Err(RuntimeError::CorruptEvent(
            "Context checkpoint artifact count is invalid",
        ));
    }
    let mut artifacts = Vec::with_capacity(artifact_count);
    for _ in 0..artifact_count {
        let item = decoder.u64()?;
        let turn = decoder.u64()?;
        let role = decode_context_view_role(decoder.u8()?)?;
        let byte_len = decoder.u64()?;
        let estimated_tokens = decoder.u64()?;
        let digest = decoder
            .bytes(32)?
            .try_into()
            .expect("fixed Context artifact digest");
        artifacts.push(
            ContextArtifactRef::from_stored(item, turn, role, byte_len, estimated_tokens, digest)
                .map_err(RuntimeError::Context)?,
        );
    }
    let recent_count = decoder.u32()? as usize;
    if artifact_count
        .checked_add(recent_count)
        .is_none_or(|count| count > MAX_CONTEXT_VIEW_ITEMS)
    {
        return Err(RuntimeError::CorruptEvent(
            "Context checkpoint Item count is invalid",
        ));
    }
    let mut recent_items = Vec::with_capacity(recent_count);
    for _ in 0..recent_count {
        recent_items.push(
            ContextViewItem::from_stored(
                decoder.u64()?,
                decoder.u64()?,
                decode_context_view_role(decoder.u8()?)?,
                decoder.string(crate::context::MAX_CONTEXT_VIEW_BYTES)?,
            )
            .map_err(RuntimeError::Context)?,
        );
    }
    let view = ReducedContextView::from_stored(source, artifacts, recent_items)
        .map_err(RuntimeError::Context)?;
    Ok(ContextCheckpoint { view })
}

const fn context_view_role_tag(role: ContextViewRole) -> u8 {
    match role {
        ContextViewRole::User => 1,
        ContextViewRole::Assistant => 2,
    }
}

fn decode_context_view_role(tag: u8) -> Result<ContextViewRole, RuntimeError> {
    match tag {
        1 => Ok(ContextViewRole::User),
        2 => Ok(ContextViewRole::Assistant),
        _ => Err(RuntimeError::CorruptEvent(
            "Context checkpoint role is invalid",
        )),
    }
}

fn encode_usage_record(encoder: &mut Encoder, usage: &UsageRecord) -> Result<(), RuntimeError> {
    encode_optional_u64(encoder, usage.input_tokens());
    encode_optional_u64(encoder, usage.cached_input_tokens());
    encode_optional_u64(encoder, usage.cache_write_input_tokens());
    encode_optional_u64(encoder, usage.output_tokens());
    encode_optional_u64(encoder, usage.reasoning_output_tokens());
    encode_optional_u64(encoder, usage.total_tokens());
    match usage.service_tier() {
        Some(service_tier) => {
            encoder.u8(1);
            encoder.string(service_tier)?;
        }
        None => encoder.u8(0),
    }
    encoder.u8(match usage.accuracy() {
        UsageAccuracy::Exact => 1,
        UsageAccuracy::Estimated => 2,
    });
    Ok(())
}

fn decode_usage_record(
    decoder: &mut Decoder<'_>,
    schema: u16,
) -> Result<UsageRecord, RuntimeError> {
    let input_tokens = decode_optional_u64(decoder)?;
    let cached_input_tokens = decode_optional_u64(decoder)?;
    let cache_write_input_tokens = decode_optional_u64(decoder)?;
    let output_tokens = decode_optional_u64(decoder)?;
    let reasoning_output_tokens = decode_optional_u64(decoder)?;
    let total_tokens = decode_optional_u64(decoder)?;
    let service_tier = match decoder.u8()? {
        0 => None,
        1 => Some(decoder.string(MAX_SERVICE_TIER_BYTES)?),
        _ => {
            return Err(RuntimeError::CorruptEvent(
                "invalid optional service tier tag",
            ));
        }
    };
    let accuracy = if schema >= 4 {
        match decoder.u8()? {
            1 => UsageAccuracy::Exact,
            2 => UsageAccuracy::Estimated,
            _ => return Err(RuntimeError::CorruptEvent("invalid usage accuracy tag")),
        }
    } else {
        UsageAccuracy::Exact
    };
    UsageRecord::new(
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        service_tier,
    )
    .map(|usage| usage.with_accuracy(accuracy))
    .map_err(RuntimeError::Provider)
}

fn encode_optional_u64(encoder: &mut Encoder, value: Option<u64>) {
    match value {
        Some(value) => {
            encoder.u8(1);
            encoder.u64(value);
        }
        None => encoder.u8(0),
    }
}

fn encode_optional_i64(encoder: &mut Encoder, value: Option<i64>) {
    match value {
        Some(value) => {
            encoder.u8(1);
            encoder.i64(value);
        }
        None => encoder.u8(0),
    }
}

fn decode_optional_u64(decoder: &mut Decoder<'_>) -> Result<Option<u64>, RuntimeError> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decoder.u64()?)),
        _ => Err(RuntimeError::CorruptEvent("invalid optional integer tag")),
    }
}

fn decode_optional_i64(decoder: &mut Decoder<'_>) -> Result<Option<i64>, RuntimeError> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decoder.i64()?)),
        _ => Err(RuntimeError::CorruptEvent("invalid optional integer tag")),
    }
}

fn encode_provider_epoch(encoder: &mut Encoder, epoch: &ProviderEpoch) -> Result<(), RuntimeError> {
    encoder.u64(epoch.id().get());
    encoder.u64(epoch.fingerprint());
    encoder.string(epoch.profile())?;
    encoder.string(epoch.model())?;
    match epoch.profile_snapshot() {
        None => encoder.u8(0),
        Some(snapshot) => {
            encoder.u8(1);
            encode_provider_profile_snapshot(encoder, snapshot)?;
        }
    }
    encoder.u8(match epoch.dialect() {
        None => 0,
        Some(dialect) => provider_dialect_tag(dialect),
    });
    Ok(())
}

fn decode_provider_epoch(
    decoder: &mut Decoder<'_>,
    schema: u16,
) -> Result<ProviderEpoch, RuntimeError> {
    let id = ProviderEpochId::new(decoder.u64()?).map_err(RuntimeError::Model)?;
    if schema < 3 {
        return ProviderEpoch::new(
            id,
            decoder.string(MAX_PROVIDER_ID_BYTES)?,
            decoder.string(MAX_PROVIDER_ID_BYTES)?,
        )
        .map_err(RuntimeError::Provider);
    }
    let fingerprint = decoder.u64()?;
    let profile = decoder.string(MAX_PROVIDER_ID_BYTES)?;
    let model = decoder.string(MAX_PROVIDER_ID_BYTES)?;
    let snapshot = match decoder.u8()? {
        0 => None,
        1 => Some(decode_provider_profile_snapshot(decoder, &profile, schema)?),
        _ => {
            return Err(RuntimeError::CorruptEvent(
                "invalid Provider Profile snapshot tag",
            ));
        }
    };
    let dialect = if schema >= 4 {
        match decoder.u8()? {
            0 => None,
            tag => Some(decode_provider_dialect(tag)?),
        }
    } else {
        None
    };
    let epoch = match snapshot {
        None if dialect.is_none() => ProviderEpoch::new(id, profile, model),
        Some(snapshot) => {
            ProviderEpoch::with_profile_snapshot_and_dialect(id, profile, model, snapshot, dialect)
        }
        None => Err(ProviderError::InvalidConfiguration(
            "Provider dialect requires a Profile snapshot",
        )),
    }
    .map_err(RuntimeError::Provider)?;
    if epoch.fingerprint() != fingerprint {
        return Err(RuntimeError::CorruptEvent(
            "Provider Epoch fingerprint mismatch",
        ));
    }
    Ok(epoch)
}

fn encode_provider_profile_snapshot(
    encoder: &mut Encoder,
    snapshot: &ProviderProfileSnapshot,
) -> Result<(), RuntimeError> {
    encoder.u64(snapshot.fingerprint());
    encoder.string(snapshot.template())?;
    encode_optional_string(encoder, snapshot.credential_reference())?;
    encode_optional_string(encoder, snapshot.base_url())?;
    encode_optional_string(encoder, snapshot.route(ProviderDialect::Responses))?;
    encode_optional_string(encoder, snapshot.route(ProviderDialect::ChatCompletions))?;
    encode_optional_string(encoder, snapshot.route(ProviderDialect::Messages))?;
    encode_optional_string(encoder, snapshot.models_route())?;
    encoder
        .u32(u32::try_from(snapshot.dialects().len()).map_err(|_| RuntimeError::IntegerOverflow)?);
    for dialect in snapshot.dialects() {
        encoder.u8(provider_dialect_tag(dialect));
    }
    encoder.u8(match snapshot.pricing_source() {
        None => 0,
        Some(ProviderPricingSource::Unknown) => 1,
        Some(ProviderPricingSource::Template) => 2,
        Some(ProviderPricingSource::Manual) => 3,
        Some(ProviderPricingSource::ProviderReported) => 4,
        Some(ProviderPricingSource::TemplateMirror) => 5,
    });
    encoder.u8(u8::from(snapshot.allow_insecure_loopback()));
    Ok(())
}

fn decode_provider_profile_snapshot(
    decoder: &mut Decoder<'_>,
    profile: &str,
    schema: u16,
) -> Result<ProviderProfileSnapshot, RuntimeError> {
    let fingerprint = decoder.u64()?;
    let template = decoder.string(MAX_PROVIDER_ID_BYTES)?;
    let credential_reference = decode_optional_string(decoder, MAX_PROVIDER_ID_BYTES)?;
    let base_url = decode_optional_string(decoder, MAX_PROVIDER_ID_BYTES)?;
    let responses_route = decode_optional_string(decoder, MAX_PROVIDER_ID_BYTES)?;
    let chat_completions_route = decode_optional_string(decoder, MAX_PROVIDER_ID_BYTES)?;
    let messages_route = decode_optional_string(decoder, MAX_PROVIDER_ID_BYTES)?;
    let models_route = decode_optional_string(decoder, MAX_PROVIDER_ID_BYTES)?;
    let dialect_count = decoder.u32()? as usize;
    if dialect_count == 0 || dialect_count > 3 {
        return Err(RuntimeError::CorruptEvent(
            "Provider Profile dialect count is invalid",
        ));
    }
    let mut dialects = Vec::with_capacity(dialect_count);
    for _ in 0..dialect_count {
        let dialect = decode_provider_dialect(decoder.u8()?)?;
        if dialects.contains(&dialect) {
            return Err(RuntimeError::CorruptEvent(
                "Provider Profile contains a duplicate dialect",
            ));
        }
        dialects.push(dialect);
    }
    let pricing_source = match decoder.u8()? {
        0 => None,
        1 => Some(ProviderPricingSource::Unknown),
        2 => Some(ProviderPricingSource::Template),
        3 => Some(ProviderPricingSource::Manual),
        4 => Some(ProviderPricingSource::ProviderReported),
        5 if schema >= 8 => Some(ProviderPricingSource::TemplateMirror),
        _ => {
            return Err(RuntimeError::CorruptEvent(
                "invalid Provider pricing source tag",
            ));
        }
    };
    let allow_insecure_loopback = match decoder.u8()? {
        0 => false,
        1 => true,
        _ => {
            return Err(RuntimeError::CorruptEvent(
                "invalid Provider loopback permission tag",
            ));
        }
    };
    let raw_base_url = base_url.clone();
    let raw_routes = [
        responses_route.clone(),
        chat_completions_route.clone(),
        messages_route.clone(),
        models_route.clone(),
    ];
    let snapshot = ProviderProfileSnapshot::from_parts(
        profile,
        template,
        credential_reference,
        base_url,
        responses_route,
        chat_completions_route,
        messages_route,
        models_route,
        dialects,
        pricing_source,
        allow_insecure_loopback,
    )
    .map_err(RuntimeError::Provider)?;
    let canonical_routes = [
        snapshot
            .route(ProviderDialect::Responses)
            .map(str::to_owned),
        snapshot
            .route(ProviderDialect::ChatCompletions)
            .map(str::to_owned),
        snapshot.route(ProviderDialect::Messages).map(str::to_owned),
        snapshot.models_route().map(str::to_owned),
    ];
    if snapshot.base_url().map(str::to_owned) != raw_base_url || canonical_routes != raw_routes {
        return Err(RuntimeError::CorruptEvent(
            "Provider Profile snapshot is not canonical",
        ));
    }
    if snapshot.fingerprint() != fingerprint {
        return Err(RuntimeError::CorruptEvent(
            "Provider Profile fingerprint mismatch",
        ));
    }
    Ok(snapshot)
}

fn encode_optional_string(encoder: &mut Encoder, value: Option<&str>) -> Result<(), RuntimeError> {
    match value {
        None => encoder.u8(0),
        Some(value) => {
            encoder.u8(1);
            encoder.string(value)?;
        }
    }
    Ok(())
}

fn decode_optional_string(
    decoder: &mut Decoder<'_>,
    max_bytes: usize,
) -> Result<Option<String>, RuntimeError> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => decoder.string(max_bytes).map(Some),
        _ => Err(RuntimeError::CorruptEvent("invalid optional string tag")),
    }
}

const fn provider_dialect_tag(dialect: ProviderDialect) -> u8 {
    match dialect {
        ProviderDialect::Responses => 1,
        ProviderDialect::ChatCompletions => 2,
        ProviderDialect::Messages => 3,
    }
}

fn decode_provider_dialect(tag: u8) -> Result<ProviderDialect, RuntimeError> {
    match tag {
        1 => Ok(ProviderDialect::Responses),
        2 => Ok(ProviderDialect::ChatCompletions),
        3 => Ok(ProviderDialect::Messages),
        _ => Err(RuntimeError::CorruptEvent("invalid Provider dialect tag")),
    }
}

fn turn_block_origin_tag(origin: TurnBlockOrigin) -> Result<u8, RuntimeError> {
    match origin {
        TurnBlockOrigin::Provider => Ok(1),
        TurnBlockOrigin::Other => Ok(2),
        TurnBlockOrigin::Legacy => Err(RuntimeError::CorruptState(
            "legacy blocked origin cannot be encoded",
        )),
    }
}

fn decode_turn_block_origin(tag: u8) -> Result<TurnBlockOrigin, RuntimeError> {
    match tag {
        1 => Ok(TurnBlockOrigin::Provider),
        2 => Ok(TurnBlockOrigin::Other),
        _ => Err(RuntimeError::CorruptEvent("invalid Turn block origin tag")),
    }
}

const fn provider_stage_is_retryable(stage: Option<ProviderUnavailableStage>) -> bool {
    matches!(
        stage,
        Some(ProviderUnavailableStage::BeforeResponse | ProviderUnavailableStage::BeforeFirstEvent)
    )
}

fn encode_optional_provider_unavailable_stage(
    encoder: &mut Encoder,
    stage: Option<ProviderUnavailableStage>,
) {
    encoder.u8(match stage {
        None => 0,
        Some(ProviderUnavailableStage::BeforeResponse) => 1,
        Some(ProviderUnavailableStage::BeforeFirstEvent) => 2,
        Some(ProviderUnavailableStage::AfterFirstEvent) => 3,
    });
}

fn decode_optional_provider_unavailable_stage(
    decoder: &mut Decoder<'_>,
) -> Result<Option<ProviderUnavailableStage>, RuntimeError> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => Ok(Some(ProviderUnavailableStage::BeforeResponse)),
        2 => Ok(Some(ProviderUnavailableStage::BeforeFirstEvent)),
        3 => Ok(Some(ProviderUnavailableStage::AfterFirstEvent)),
        _ => Err(RuntimeError::CorruptEvent(
            "invalid optional Provider unavailable stage tag",
        )),
    }
}

const fn price_schedule_source_tag(source: PriceScheduleSource) -> u8 {
    match source {
        PriceScheduleSource::Template => 1,
        PriceScheduleSource::Manual => 2,
        PriceScheduleSource::ProviderReported => 3,
        PriceScheduleSource::TemplateMirror => 4,
    }
}

fn decode_price_schedule_source(tag: u8, schema: u16) -> Result<PriceScheduleSource, RuntimeError> {
    match tag {
        1 => Ok(PriceScheduleSource::Template),
        2 => Ok(PriceScheduleSource::Manual),
        3 => Ok(PriceScheduleSource::ProviderReported),
        4 if schema >= 8 => Ok(PriceScheduleSource::TemplateMirror),
        _ => Err(RuntimeError::CorruptEvent(
            "invalid Price Schedule source tag",
        )),
    }
}

fn encode_optional_agent(encoder: &mut Encoder, agent: Option<AgentId>) {
    match agent {
        None => encoder.u8(0),
        Some(agent) => {
            encoder.u8(1);
            encoder.u64(agent.get());
        }
    }
}

fn decode_optional_agent(decoder: &mut Decoder<'_>) -> Result<Option<AgentId>, RuntimeError> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => AgentId::from_stored(decoder.u64()?)
            .map(Some)
            .ok_or(RuntimeError::CorruptEvent(
                "invalid Agent ID in Runtime Event",
            )),
        _ => Err(RuntimeError::CorruptEvent("invalid optional Agent ID tag")),
    }
}

const fn usage_attempt_outcome_tag(outcome: UsageAttemptOutcome) -> u8 {
    match outcome {
        UsageAttemptOutcome::Succeeded => 1,
        UsageAttemptOutcome::Failed => 2,
        UsageAttemptOutcome::Interrupted => 3,
    }
}

fn decode_usage_attempt_outcome(tag: u8) -> Result<UsageAttemptOutcome, RuntimeError> {
    match tag {
        1 => Ok(UsageAttemptOutcome::Succeeded),
        2 => Ok(UsageAttemptOutcome::Failed),
        3 => Ok(UsageAttemptOutcome::Interrupted),
        _ => Err(RuntimeError::CorruptEvent("invalid usage attempt outcome")),
    }
}

const fn cost_unknown_reason_tag(reason: CostEstimateUnknownReason) -> u8 {
    match reason {
        CostEstimateUnknownReason::MissingUsageRecord => 1,
        CostEstimateUnknownReason::NoMatchingSchedule => 2,
        CostEstimateUnknownReason::MissingInputTokens => 3,
        CostEstimateUnknownReason::MissingCachedInputTokens => 4,
        CostEstimateUnknownReason::MissingCacheWriteInputTokens => 5,
        CostEstimateUnknownReason::MissingOutputTokens => 6,
        CostEstimateUnknownReason::MissingReasoningOutputTokens => 7,
        CostEstimateUnknownReason::InconsistentInputAccounting => 8,
        CostEstimateUnknownReason::InconsistentOutputAccounting => 9,
        CostEstimateUnknownReason::ArithmeticOverflow => 10,
        CostEstimateUnknownReason::MissingServiceTier => 11,
    }
}

fn decode_cost_unknown_reason(tag: u8) -> Result<CostEstimateUnknownReason, RuntimeError> {
    match tag {
        1 => Ok(CostEstimateUnknownReason::MissingUsageRecord),
        2 => Ok(CostEstimateUnknownReason::NoMatchingSchedule),
        3 => Ok(CostEstimateUnknownReason::MissingInputTokens),
        4 => Ok(CostEstimateUnknownReason::MissingCachedInputTokens),
        5 => Ok(CostEstimateUnknownReason::MissingCacheWriteInputTokens),
        6 => Ok(CostEstimateUnknownReason::MissingOutputTokens),
        7 => Ok(CostEstimateUnknownReason::MissingReasoningOutputTokens),
        8 => Ok(CostEstimateUnknownReason::InconsistentInputAccounting),
        9 => Ok(CostEstimateUnknownReason::InconsistentOutputAccounting),
        10 => Ok(CostEstimateUnknownReason::ArithmeticOverflow),
        11 => Ok(CostEstimateUnknownReason::MissingServiceTier),
        _ => Err(RuntimeError::CorruptEvent(
            "invalid Cost Estimate unknown reason",
        )),
    }
}

const fn usage_weekday_tag(day: UsageWeekday) -> u8 {
    match day {
        UsageWeekday::Mon => 1,
        UsageWeekday::Tue => 2,
        UsageWeekday::Wed => 3,
        UsageWeekday::Thu => 4,
        UsageWeekday::Fri => 5,
        UsageWeekday::Sat => 6,
        UsageWeekday::Sun => 7,
    }
}

fn decode_usage_weekday(tag: u8) -> Result<UsageWeekday, RuntimeError> {
    match tag {
        1 => Ok(UsageWeekday::Mon),
        2 => Ok(UsageWeekday::Tue),
        3 => Ok(UsageWeekday::Wed),
        4 => Ok(UsageWeekday::Thu),
        5 => Ok(UsageWeekday::Fri),
        6 => Ok(UsageWeekday::Sat),
        7 => Ok(UsageWeekday::Sun),
        _ => Err(RuntimeError::CorruptEvent("invalid usage window weekday")),
    }
}

fn encode_config_epoch(encoder: &mut Encoder, epoch: &ConfigEpoch) -> Result<(), RuntimeError> {
    encoder.u64(epoch.id().get());
    encoder.u64(epoch.fingerprint());
    let resolved = epoch.resolved();
    encoder.string(resolved.provider_profile().value())?;
    encoder.u8(source_tag(resolved.provider_profile().source()));
    encoder.string(resolved.provider_model().value())?;
    encoder.u8(source_tag(resolved.provider_model().source()));
    encoder.u32(*resolved.max_output_bytes().value());
    encoder.u8(source_tag(resolved.max_output_bytes().source()));
    encoder.u32(
        u32::try_from(epoch.usage_windows().len()).map_err(|_| RuntimeError::IntegerOverflow)?,
    );
    for window in epoch.usage_windows() {
        encode_usage_window(encoder, window)?;
    }
    encoder.u32(
        u32::try_from(epoch.price_schedules().schedules().len())
            .map_err(|_| RuntimeError::IntegerOverflow)?,
    );
    for schedule in epoch.price_schedules().schedules() {
        encode_price_schedule(encoder, schedule)?;
    }
    match resolved.max_output_tokens() {
        None => encoder.u8(0),
        Some(value) => {
            encoder.u8(1);
            encoder.u32(*value.value());
            encoder.u8(source_tag(value.source()));
        }
    }
    match resolved.reasoning_effort() {
        None => encoder.u8(0),
        Some(value) => {
            encoder.u8(1);
            encoder.string(value.value().as_str())?;
            encoder.u8(source_tag(value.source()));
        }
    }
    match resolved.service_tier() {
        None => encoder.u8(0),
        Some(value) => {
            encoder.u8(1);
            encoder.string(value.value().as_str())?;
            encoder.u8(source_tag(value.source()));
        }
    }
    Ok(())
}

fn decode_config_epoch(
    decoder: &mut Decoder<'_>,
    schema: u16,
) -> Result<ConfigEpoch, RuntimeError> {
    let id = ConfigEpochId::new(decoder.u64()?).map_err(RuntimeError::Model)?;
    let fingerprint = decoder.u64()?;
    let profile = decoder.string(MAX_BLOCK_REASON_BYTES)?;
    let profile_source = decode_source(decoder.u8()?)?;
    let model = decoder.string(MAX_BLOCK_REASON_BYTES)?;
    let model_source = decode_source(decoder.u8()?)?;
    let max_output = decoder.u32()?;
    let max_output_source = decode_source(decoder.u8()?)?;
    let mut layers = ConfigLayers {
        built_in: ConfigLayer::default(),
        user: ConfigLayer::default(),
        project: ConfigLayer::default(),
        cli: ConfigLayer::default(),
    };
    layer_mut(&mut layers, profile_source).provider_profile = Some(profile);
    layer_mut(&mut layers, model_source).provider_model = Some(model);
    layer_mut(&mut layers, max_output_source).max_output_bytes = Some(max_output);
    let usage_windows = if schema >= 4 {
        let count = decoder.u32()? as usize;
        if count > MAX_USAGE_WINDOWS {
            return Err(RuntimeError::CorruptEvent(
                "Config Epoch usage window count is invalid",
            ));
        }
        (0..count)
            .map(|_| decode_usage_window(decoder))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let price_schedules = if schema >= 5 {
        let count = decoder.u32()? as usize;
        if count > MAX_PRICE_SCHEDULES {
            return Err(RuntimeError::CorruptEvent(
                "Config Epoch Price Schedule count is invalid",
            ));
        }
        PriceScheduleBook::new(
            (0..count)
                .map(|_| decode_price_schedule(decoder, schema))
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map_err(|_| RuntimeError::CorruptEvent("invalid Config Epoch Price Schedules"))?
    } else {
        PriceScheduleBook::default()
    };
    if schema >= 6 {
        match decoder.u8()? {
            0 => {}
            1 => {
                let value = decoder.u32()?;
                let source = decode_source(decoder.u8()?)?;
                layer_mut(&mut layers, source).max_output_tokens = Some(value);
            }
            _ => {
                return Err(RuntimeError::CorruptEvent(
                    "invalid Config Epoch output-token limit",
                ));
            }
        }
    }
    if schema >= 7 {
        match decoder.u8()? {
            0 => {}
            1 => {
                let value = decoder.string(MAX_CONFIG_STRING_BYTES)?;
                let value = ReasoningEffort::parse(&value).ok_or(RuntimeError::CorruptEvent(
                    "invalid Config Epoch reasoning effort",
                ))?;
                let source = decode_source(decoder.u8()?)?;
                layer_mut(&mut layers, source).reasoning_effort = Some(value);
            }
            _ => {
                return Err(RuntimeError::CorruptEvent(
                    "invalid Config Epoch reasoning effort",
                ));
            }
        }
        match decoder.u8()? {
            0 => {}
            1 => {
                let value = decoder.string(MAX_CONFIG_STRING_BYTES)?;
                let value = ServiceTier::parse(&value).ok_or(RuntimeError::CorruptEvent(
                    "invalid Config Epoch service tier",
                ))?;
                let source = decode_source(decoder.u8()?)?;
                layer_mut(&mut layers, source).service_tier = Some(value);
            }
            _ => {
                return Err(RuntimeError::CorruptEvent(
                    "invalid Config Epoch service tier",
                ));
            }
        }
    }
    let epoch = ConfigEpoch::freeze_with_observability(id, &layers, usage_windows, price_schedules)
        .map_err(RuntimeError::Config)?;
    if epoch.fingerprint() != fingerprint {
        return Err(RuntimeError::CorruptEvent(
            "Config Epoch fingerprint mismatch",
        ));
    }
    Ok(epoch)
}

fn encode_price_schedule(
    encoder: &mut Encoder,
    schedule: &PriceSchedule,
) -> Result<(), RuntimeError> {
    encoder.u64(schedule.fingerprint());
    encoder.string(schedule.id())?;
    encoder.string(schedule.version())?;
    encoder.string(schedule.currency())?;
    encoder.string(schedule.provider_profile())?;
    encoder.string(schedule.model())?;
    encoder.u8(schedule.dialect().map_or(0, provider_dialect_tag));
    encode_optional_string(encoder, schedule.service_tier())?;
    encoder.u64(schedule.minimum_context_tokens());
    encode_optional_u64(encoder, schedule.maximum_context_tokens());
    encoder.i64(schedule.effective_from().unix_millis());
    encode_optional_i64(
        encoder,
        schedule.effective_until().map(UsageTimestamp::unix_millis),
    );
    encoder.u8(price_schedule_source_tag(schedule.source()));
    encoder.string(schedule.source_ref())?;
    let rates = schedule.rates();
    encoder.u64(rates.input_micros_per_million());
    encoder.u64(rates.cached_input_micros_per_million());
    encoder.u64(rates.cache_write_micros_per_million());
    encoder.u64(rates.output_micros_per_million());
    encoder.u64(rates.reasoning_output_micros_per_million());
    Ok(())
}

fn decode_price_schedule(
    decoder: &mut Decoder<'_>,
    schema: u16,
) -> Result<PriceSchedule, RuntimeError> {
    let fingerprint = decoder.u64()?;
    let schedule = PriceSchedule::new_trusted(PriceScheduleDefinition {
        id: decoder.string(MAX_PROVIDER_ID_BYTES)?,
        version: decoder.string(MAX_CONFIG_STRING_BYTES)?,
        currency: decoder.string(3)?,
        provider_profile: decoder.string(MAX_CONFIG_STRING_BYTES)?,
        model: decoder.string(MAX_CONFIG_STRING_BYTES)?,
        dialect: match decoder.u8()? {
            0 => None,
            tag => Some(decode_provider_dialect(tag)?),
        },
        service_tier: decode_optional_string(decoder, MAX_SERVICE_TIER_BYTES)?,
        minimum_context_tokens: decoder.u64()?,
        maximum_context_tokens: decode_optional_u64(decoder)?,
        effective_from: UsageTimestamp::from_unix_millis(decoder.i64()?)
            .map_err(RuntimeError::Usage)?,
        effective_until: decode_optional_i64(decoder)?
            .map(UsageTimestamp::from_unix_millis)
            .transpose()
            .map_err(RuntimeError::Usage)?,
        source: decode_price_schedule_source(decoder.u8()?, schema)?,
        source_ref: decoder.string(MAX_CONFIG_STRING_BYTES)?,
        rates: TokenRates::new(
            decoder.u64()?,
            decoder.u64()?,
            decoder.u64()?,
            decoder.u64()?,
            decoder.u64()?,
        ),
    })
    .map_err(|_| RuntimeError::CorruptEvent("invalid Config Epoch Price Schedule"))?;
    if schedule.fingerprint() != fingerprint {
        return Err(RuntimeError::CorruptEvent(
            "Price Schedule fingerprint mismatch",
        ));
    }
    Ok(schedule)
}

fn encode_usage_window(encoder: &mut Encoder, window: &UsageWindow) -> Result<(), RuntimeError> {
    encoder.string(window.id())?;
    encoder.u32(u32::from(window.start_minute()));
    encoder.u32(u32::from(window.end_minute()));
    encoder.u32(u32::try_from(window.days().len()).map_err(|_| RuntimeError::IntegerOverflow)?);
    for day in window.days() {
        encoder.u8(usage_weekday_tag(day));
    }
    encoder.string(window.timezone())?;
    encoder.u8(match window.timezone_source() {
        UsageTimezoneSource::Explicit => 1,
        UsageTimezoneSource::LocalSystem => 2,
    });
    encoder.string(window.ruleset_version())?;
    Ok(())
}

fn decode_usage_window(decoder: &mut Decoder<'_>) -> Result<UsageWindow, RuntimeError> {
    let id = decoder.string(MAX_PROVIDER_ID_BYTES)?;
    let start_minute = u16::try_from(decoder.u32()?)
        .map_err(|_| RuntimeError::CorruptEvent("usage window start is invalid"))?;
    let end_minute = u16::try_from(decoder.u32()?)
        .map_err(|_| RuntimeError::CorruptEvent("usage window end is invalid"))?;
    let day_count = decoder.u32()? as usize;
    if day_count == 0 || day_count > 7 {
        return Err(RuntimeError::CorruptEvent(
            "usage window day count is invalid",
        ));
    }
    let mut days = BTreeSet::new();
    for _ in 0..day_count {
        if !days.insert(decode_usage_weekday(decoder.u8()?)?) {
            return Err(RuntimeError::CorruptEvent(
                "usage window contains a duplicate day",
            ));
        }
    }
    let timezone = decoder.string(MAX_PROVIDER_ID_BYTES)?;
    let timezone_source = match decoder.u8()? {
        1 => UsageTimezoneSource::Explicit,
        2 => UsageTimezoneSource::LocalSystem,
        _ => {
            return Err(RuntimeError::CorruptEvent(
                "usage window timezone source is invalid",
            ));
        }
    };
    let ruleset_version = decoder.string(MAX_PROVIDER_ID_BYTES)?;
    UsageWindow::from_resolved_parts(
        id,
        start_minute,
        end_minute,
        days,
        timezone,
        timezone_source,
        ruleset_version,
    )
    .map_err(RuntimeError::Usage)
}

fn layer_mut(layers: &mut ConfigLayers, source: ConfigSource) -> &mut ConfigLayer {
    match source {
        ConfigSource::BuiltIn => &mut layers.built_in,
        ConfigSource::User => &mut layers.user,
        ConfigSource::Project => &mut layers.project,
        ConfigSource::Cli => &mut layers.cli,
    }
}

const fn source_tag(source: ConfigSource) -> u8 {
    match source {
        ConfigSource::BuiltIn => 1,
        ConfigSource::User => 2,
        ConfigSource::Project => 3,
        ConfigSource::Cli => 4,
    }
}

fn decode_source(tag: u8) -> Result<ConfigSource, RuntimeError> {
    match tag {
        1 => Ok(ConfigSource::BuiltIn),
        2 => Ok(ConfigSource::User),
        3 => Ok(ConfigSource::Project),
        4 => Ok(ConfigSource::Cli),
        _ => Err(RuntimeError::CorruptEvent("invalid Config source tag")),
    }
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) -> Result<(), RuntimeError> {
        let length = u32::try_from(value.len()).map_err(|_| RuntimeError::IntegerOverflow)?;
        self.u32(length);
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8], RuntimeError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(RuntimeError::IntegerOverflow)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(RuntimeError::CorruptEvent("truncated Runtime Event"))?;
        self.position = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, RuntimeError> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, RuntimeError> {
        Ok(u32::from_le_bytes(
            self.bytes(4)?.try_into().expect("fixed integer slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64, RuntimeError> {
        Ok(u64::from_le_bytes(
            self.bytes(8)?.try_into().expect("fixed integer slice"),
        ))
    }

    fn i64(&mut self) -> Result<i64, RuntimeError> {
        Ok(i64::from_le_bytes(
            self.bytes(8)?.try_into().expect("fixed integer slice"),
        ))
    }

    fn string(&mut self, max_bytes: usize) -> Result<String, RuntimeError> {
        let length = self.u32()? as usize;
        if length > max_bytes {
            return Err(RuntimeError::CorruptEvent(
                "Runtime Event string is too large",
            ));
        }
        let bytes = self.bytes(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| RuntimeError::CorruptEvent("Runtime Event string is not UTF-8"))?;
        Ok(value.to_owned())
    }

    fn finish(self) -> Result<(), RuntimeError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(RuntimeError::CorruptEvent(
                "Runtime Event has trailing bytes",
            ))
        }
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Ledger(LedgerError),
    Team(DurableTeamError),
    Tool(ToolRuntimeError),
    Config(ConfigError),
    Model(ModelError),
    Provider(ProviderError),
    Usage(UsageError),
    Context(ContextViewError),
    ContextCheckpointNotAtSafeBarrier,
    ContextAdmissionBlocked {
        pressure: ContextPressureSnapshot,
    },
    Busy(RecoveryStatus),
    UnknownTurn(TurnId),
    TurnCancellationNotAllowed(TurnId),
    TurnRetryNotAllowed(TurnId),
    StaleContextCheckpoint {
        expected: LedgerHead,
        actual: LedgerHead,
    },
    UnknownDelivery(DeliveryId),
    InvalidInput(&'static str),
    InvalidProviderOutput(&'static str),
    InvalidModelSelection(&'static str),
    TeamUnavailable,
    ToolUnavailable,
    TeamOperationReconciliationRequired(TeamOperationId),
    ToolReconciliationRequired(ToolCallId),
    ProviderToolCallTerminated {
        call: ToolCallId,
        status: ToolCallStatus,
    },
    ProviderToolResultUnavailable(ToolCallId),
    InvalidTeamConfiguration(&'static str),
    InvalidToolConfiguration(&'static str),
    UnsupportedRuntimeEventSchema {
        supported: u16,
        actual: u16,
    },
    CorruptEvent(&'static str),
    CorruptState(&'static str),
    IntegerOverflow,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ledger(source) => write!(formatter, "{source}"),
            Self::Team(source) => write!(formatter, "{source}"),
            Self::Tool(source) => write!(formatter, "{source}"),
            Self::Config(source) => write!(formatter, "{source}"),
            Self::Model(source) => write!(formatter, "{source}"),
            Self::Provider(source) => write!(formatter, "{source}"),
            Self::Usage(source) => write!(formatter, "{source}"),
            Self::Context(source) => write!(formatter, "{source}"),
            Self::ContextCheckpointNotAtSafeBarrier => {
                write!(formatter, "Context checkpoint requires a Safe Barrier")
            }
            Self::ContextAdmissionBlocked { pressure } => write!(
                formatter,
                "Context Pressure stopped Turn admission at {}%",
                pressure
                    .occupancy_percent()
                    .expect("hard Context Pressure has a known occupancy")
            ),
            Self::Busy(status) => write!(formatter, "Runtime requires reconciliation: {status}"),
            Self::UnknownTurn(turn) => write!(formatter, "unknown Turn {}", turn.get()),
            Self::TurnCancellationNotAllowed(turn) => {
                write!(formatter, "Turn {} cannot be cancelled", turn.get())
            }
            Self::TurnRetryNotAllowed(turn) => {
                write!(formatter, "Turn {} cannot be retried", turn.get())
            }
            Self::StaleContextCheckpoint { expected, actual } => write!(
                formatter,
                "stale Context checkpoint: expected Ledger head {}/{}, found {}/{}",
                expected.transaction, expected.sequence, actual.transaction, actual.sequence
            ),
            Self::UnknownDelivery(delivery) => {
                write!(formatter, "unknown output delivery {}", delivery.get())
            }
            Self::InvalidInput(reason) => write!(formatter, "invalid input: {reason}"),
            Self::InvalidProviderOutput(reason) => {
                write!(formatter, "invalid provider output: {reason}")
            }
            Self::InvalidModelSelection(reason) => {
                write!(formatter, "invalid Model selection: {reason}")
            }
            Self::TeamUnavailable => write!(formatter, "Runtime Kernel has no Agent Team"),
            Self::ToolUnavailable => write!(formatter, "Runtime Kernel has no Tool Runtime"),
            Self::TeamOperationReconciliationRequired(operation) => write!(
                formatter,
                "Team operation {} requires acknowledgement reconciliation",
                operation.get()
            ),
            Self::ToolReconciliationRequired(call) => {
                write!(
                    formatter,
                    "Tool call {} requires reconciliation",
                    call.get()
                )
            }
            Self::ProviderToolCallTerminated { call, status } => write!(
                formatter,
                "Provider Tool call {} terminated with status {status:?}",
                call.get()
            ),
            Self::ProviderToolResultUnavailable(call) => write!(
                formatter,
                "Provider Tool call {} completed without a resumable raw result",
                call.get()
            ),
            Self::InvalidTeamConfiguration(reason) => {
                write!(formatter, "invalid Agent Team configuration: {reason}")
            }
            Self::InvalidToolConfiguration(reason) => {
                write!(formatter, "invalid Tool Runtime configuration: {reason}")
            }
            Self::UnsupportedRuntimeEventSchema { supported, actual } => write!(
                formatter,
                "unsupported Runtime Event schema {actual}; expected {supported}"
            ),
            Self::CorruptEvent(reason) => write!(formatter, "corrupt Runtime Event: {reason}"),
            Self::CorruptState(reason) => write!(formatter, "corrupt Runtime state: {reason}"),
            Self::IntegerOverflow => write!(formatter, "Runtime integer overflow"),
        }
    }
}

impl Error for RuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ledger(source) => Some(source),
            Self::Team(source) => Some(source),
            Self::Tool(source) => Some(source),
            Self::Config(source) => Some(source),
            Self::Model(source) => Some(source),
            Self::Provider(source) => Some(source),
            Self::Usage(source) => Some(source),
            Self::Context(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_runtime_event(data: EventData) -> StoredEvent {
        StoredEvent {
            sequence: 1,
            transaction: 1,
            index_in_transaction: 0,
            events_in_transaction: 1,
            data,
        }
    }

    fn profile_snapshot() -> ProviderProfileSnapshot {
        ProviderProfileSnapshot::from_parts(
            "edge",
            "openai-compatible",
            Some("edge-credential".to_owned()),
            Some("https://gateway.example.com/v1".to_owned()),
            Some("/responses".to_owned()),
            Some("/chat/completions".to_owned()),
            None,
            Some("/models".to_owned()),
            [ProviderDialect::Responses, ProviderDialect::ChatCompletions],
            Some(ProviderPricingSource::Unknown),
            false,
        )
        .expect("valid Provider Profile snapshot")
    }

    #[test]
    fn context_checkpoint_event_rejects_a_non_prior_source_head() {
        let view = ReducedContextView::from_items(
            LedgerHead {
                transaction: 1,
                sequence: 1,
            },
            &[],
            ContextReductionPolicy::new(64, 1).expect("policy"),
        )
        .expect("empty reduced View");
        let event = RuntimeEvent::ContextCheckpointPublished {
            checkpoint: ContextCheckpoint { view },
        }
        .encode()
        .expect("encode checkpoint");

        assert!(matches!(
            replay_runtime(&[stored_runtime_event(event)]),
            Err(RuntimeError::CorruptEvent(
                "Context checkpoint is not bound to the prior Ledger head"
            ))
        ));
    }

    #[test]
    fn context_checkpoint_event_rejects_tampered_artifact_evidence() {
        let artifact = ContextArtifactRef::from_stored(1, 1, ContextViewRole::User, 4, 1, [0; 32])
            .expect("bounded artifact");
        let view = ReducedContextView::from_stored(
            ContextEventRange::from_head(LedgerHead::default()),
            vec![artifact],
            Vec::new(),
        )
        .expect("structurally valid reduced View");
        let event = RuntimeEvent::ContextCheckpointPublished {
            checkpoint: ContextCheckpoint { view },
        }
        .encode()
        .expect("encode checkpoint");

        assert!(matches!(
            replay_runtime(&[stored_runtime_event(event)]),
            Err(RuntimeError::Context(ContextViewError::ArtifactMismatch))
        ));
    }

    #[test]
    fn current_schema_records_provider_retry_stage_and_preserves_legacy_blocks() {
        let turn = TurnId::new(1).expect("Turn ID");
        let blocked = RuntimeEvent::TurnBlocked {
            turn,
            reason: "Provider stream failed before its first event".to_owned(),
            origin: TurnBlockOrigin::Provider,
            provider_unavailable_stage: Some(ProviderUnavailableStage::BeforeFirstEvent),
        };
        let encoded = blocked.encode().expect("encode provider block");
        assert_eq!(encoded.schema, 12);
        assert_eq!(encoded.kind, 10);
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded.clone()))
                .expect("decode provider block"),
            blocked
        );

        let mut schema_ten = encoded;
        schema_ten.schema = 10;
        assert_eq!(schema_ten.payload.pop(), Some(2));
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(schema_ten.clone()))
                .expect("decode schema-ten block"),
            RuntimeEvent::TurnBlocked {
                turn,
                reason: "Provider stream failed before its first event".to_owned(),
                origin: TurnBlockOrigin::Provider,
                provider_unavailable_stage: None,
            }
        );

        schema_ten.schema = 9;
        assert_eq!(schema_ten.payload.pop(), Some(1));
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(schema_ten)).expect("decode legacy block"),
            RuntimeEvent::TurnBlocked {
                turn,
                reason: "Provider stream failed before its first event".to_owned(),
                origin: TurnBlockOrigin::Legacy,
                provider_unavailable_stage: None,
            }
        );

        let cancelled = RuntimeEvent::TurnCancelled { turn };
        let encoded = cancelled.encode().expect("encode Turn cancellation");
        assert_eq!(encoded.schema, 12);
        assert_eq!(encoded.kind, 15);
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded)).expect("decode Turn cancellation"),
            cancelled
        );

        let retry = RuntimeEvent::TurnRetryRequested { turn };
        let encoded = retry.encode().expect("encode Turn retry request");
        assert_eq!(encoded.schema, 12);
        assert_eq!(encoded.kind, 16);
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded))
                .expect("decode Turn retry request"),
            retry
        );
    }

    #[test]
    fn schema_two_provider_epoch_remains_replayable() {
        let mut payload = Encoder::default();
        payload.u64(1);
        payload.string("simulator").expect("profile");
        payload.string("deterministic-v1").expect("model");
        let decoded = RuntimeEvent::decode(&stored_runtime_event(EventData {
            schema: 2,
            kind: 3,
            payload: payload.finish(),
        }))
        .expect("decode schema-two Provider Epoch");
        let RuntimeEvent::ProviderFrozen { epoch } = decoded else {
            panic!("decoded the wrong Runtime Event")
        };
        assert_eq!(epoch.profile(), "simulator");
        assert_eq!(epoch.model(), "deterministic-v1");
        assert_eq!(epoch.profile_snapshot(), None);
    }

    #[test]
    fn schema_eight_config_epoch_preserves_schema_seven_policy_and_schema_six_replay() {
        let legacy_epoch = ConfigEpoch::freeze(
            ConfigEpochId::new(1).expect("Config Epoch id"),
            &ConfigLayers::default(),
        )
        .expect("legacy-compatible Config Epoch");
        let mut schema_six = RuntimeEvent::ConfigFrozen {
            epoch: legacy_epoch.clone(),
        }
        .encode()
        .expect("encode Config Epoch");
        assert_eq!(schema_six.schema, RUNTIME_EVENT_SCHEMA);
        assert_eq!(schema_six.payload.pop(), Some(0));
        assert_eq!(schema_six.payload.pop(), Some(0));
        schema_six.schema = 6;
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(schema_six))
                .expect("decode schema-six Config Epoch"),
            RuntimeEvent::ConfigFrozen {
                epoch: legacy_epoch
            }
        );

        let mut layers = ConfigLayers::default();
        layers.cli.max_output_tokens = Some(8_192);
        layers.cli.reasoning_effort = Some(crate::config::ReasoningEffort::Low);
        layers.cli.service_tier = Some(crate::config::ServiceTier::Priority);
        let epoch = ConfigEpoch::freeze(ConfigEpochId::new(2).expect("Config Epoch id"), &layers)
            .expect("request-policy Config Epoch");
        let encoded = RuntimeEvent::ConfigFrozen {
            epoch: epoch.clone(),
        }
        .encode()
        .expect("encode request-policy Config Epoch");
        let mut schema_seven = encoded.clone();
        schema_seven.schema = 7;
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(schema_seven))
                .expect("decode schema-seven request policy"),
            RuntimeEvent::ConfigFrozen {
                epoch: epoch.clone()
            }
        );
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded.clone()))
                .expect("decode request-policy Config Epoch"),
            RuntimeEvent::ConfigFrozen { epoch }
        );

        let mut tampered = encoded;
        let reasoning_start = tampered
            .payload
            .windows(3)
            .position(|window| window == b"low")
            .expect("encoded reasoning effort");
        tampered.payload[reasoning_start..reasoning_start + 3].copy_from_slice(b"max");
        assert!(matches!(
            RuntimeEvent::decode(&stored_runtime_event(tampered)),
            Err(RuntimeError::CorruptEvent(
                "Config Epoch fingerprint mismatch"
            ))
        ));
    }

    #[test]
    fn schema_two_and_three_usage_records_replay_as_exact_legacy_usage() {
        for schema in [2, 3] {
            let mut payload = Encoder::default();
            payload.u64(1);
            payload.u64(2);
            payload.u64(1);
            payload.string("done").expect("text");
            payload.u32(1);
            for value in [Some(3), Some(1), None, Some(2), None, Some(5)] {
                encode_optional_u64(&mut payload, value);
            }
            encode_optional_string(&mut payload, Some("standard")).expect("service tier");

            let decoded = RuntimeEvent::decode(&stored_runtime_event(EventData {
                schema,
                kind: 7,
                payload: payload.finish(),
            }))
            .expect("decode historical OutputPrepared");
            let RuntimeEvent::OutputPrepared {
                usage_records,
                legacy_usage,
                ..
            } = decoded
            else {
                panic!("decoded the wrong Runtime Event")
            };
            assert!(legacy_usage);
            assert_eq!(usage_records.len(), 1);
            assert_eq!(usage_records[0].accuracy(), UsageAccuracy::Exact);
            assert_eq!(usage_records[0].total_tokens(), Some(5));
        }
    }

    #[test]
    fn schema_three_provider_epoch_replays_and_current_schema_rejects_fingerprint_tampering() {
        let epoch = ProviderEpoch::with_profile_snapshot(
            ProviderEpochId::new(1).expect("Provider Epoch id"),
            "edge",
            "fixture-model",
            profile_snapshot(),
        )
        .expect("freeze Provider Epoch");
        let encoded = RuntimeEvent::ProviderFrozen {
            epoch: Box::new(epoch.clone()),
        }
        .encode()
        .expect("encode Provider Epoch");
        assert_eq!(encoded.schema, RUNTIME_EVENT_SCHEMA);
        let mut schema_three = encoded.clone();
        schema_three.schema = 3;
        assert_eq!(schema_three.payload.pop(), Some(0));
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(schema_three))
                .expect("decode schema-three Provider Epoch"),
            RuntimeEvent::ProviderFrozen {
                epoch: Box::new(epoch.clone())
            }
        );
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded.clone()))
                .expect("decode Provider Epoch"),
            RuntimeEvent::ProviderFrozen {
                epoch: Box::new(epoch)
            }
        );

        let mut epoch_tamper = encoded.clone();
        epoch_tamper.payload[8] ^= 1;
        assert!(matches!(
            RuntimeEvent::decode(&stored_runtime_event(epoch_tamper)),
            Err(RuntimeError::CorruptEvent(
                "Provider Epoch fingerprint mismatch"
            ))
        ));

        let mut snapshot_tamper = encoded;
        let profile_length = "edge".len();
        let model_length = "fixture-model".len();
        let snapshot_fingerprint = 8 + 8 + 4 + profile_length + 4 + model_length + 1;
        snapshot_tamper.payload[snapshot_fingerprint] ^= 1;
        assert!(matches!(
            RuntimeEvent::decode(&stored_runtime_event(snapshot_tamper)),
            Err(RuntimeError::CorruptEvent(
                "Provider Profile fingerprint mismatch"
            ))
        ));
    }

    #[test]
    fn schema_eight_round_trips_template_mirror_without_relabeling_historical_sources() {
        let mirrored_profile = ProviderProfileSnapshot::from_parts(
            "gateway",
            "deepseek",
            Some("gateway-credential".to_owned()),
            Some("https://gateway.example.com".to_owned()),
            Some("/responses".to_owned()),
            Some("/chat/completions".to_owned()),
            Some("/anthropic/v1/messages".to_owned()),
            Some("/models".to_owned()),
            [ProviderDialect::Responses, ProviderDialect::ChatCompletions],
            Some(ProviderPricingSource::TemplateMirror),
            false,
        )
        .expect("mirrored Provider Profile");
        let epoch = ProviderEpoch::with_profile_snapshot_and_dialect(
            ProviderEpochId::new(1).expect("Provider Epoch id"),
            "gateway",
            "deepseek-v4-flash",
            mirrored_profile,
            Some(ProviderDialect::Responses),
        )
        .expect("mirrored Provider Epoch");
        let encoded = RuntimeEvent::ProviderFrozen {
            epoch: Box::new(epoch.clone()),
        }
        .encode()
        .expect("encode mirrored Provider Epoch");
        assert_eq!(encoded.schema, RUNTIME_EVENT_SCHEMA);
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded.clone()))
                .expect("decode mirrored Provider Epoch"),
            RuntimeEvent::ProviderFrozen {
                epoch: Box::new(epoch)
            }
        );

        let mut falsely_historical = encoded;
        falsely_historical.schema = 7;
        assert!(matches!(
            RuntimeEvent::decode(&stored_runtime_event(falsely_historical)),
            Err(RuntimeError::CorruptEvent(
                "invalid Provider pricing source tag"
            ))
        ));

        let schedule = PriceSchedule::new_trusted(PriceScheduleDefinition {
            id: "release-mirror".to_owned(),
            version: "2026-08-10.1".to_owned(),
            currency: "USD".to_owned(),
            provider_profile: "gateway".to_owned(),
            model: "deepseek-v4-flash".to_owned(),
            dialect: Some(ProviderDialect::Responses),
            service_tier: None,
            minimum_context_tokens: 0,
            maximum_context_tokens: None,
            effective_from: UsageTimestamp::from_unix_millis(0).expect("timestamp"),
            effective_until: None,
            source: PriceScheduleSource::TemplateMirror,
            source_ref: "https://api-docs.deepseek.com/quick_start/pricing/".to_owned(),
            rates: TokenRates::new(140_000, 2_800, 0, 280_000, 280_000),
        })
        .expect("mirrored Price Schedule");
        let config = ConfigEpoch::freeze_with_observability(
            ConfigEpochId::new(2).expect("Config Epoch id"),
            &ConfigLayers::default(),
            Vec::new(),
            PriceScheduleBook::new(vec![schedule]).expect("Price Schedule book"),
        )
        .expect("mirrored Config Epoch");
        let encoded = RuntimeEvent::ConfigFrozen {
            epoch: config.clone(),
        }
        .encode()
        .expect("encode mirrored Config Epoch");
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded.clone()))
                .expect("decode mirrored Config Epoch"),
            RuntimeEvent::ConfigFrozen { epoch: config }
        );

        let mut falsely_historical = encoded;
        falsely_historical.schema = 7;
        assert!(matches!(
            RuntimeEvent::decode(&stored_runtime_event(falsely_historical)),
            Err(RuntimeError::CorruptEvent(
                "invalid Price Schedule source tag"
            ))
        ));
    }

    #[test]
    fn schema_four_and_current_usage_attempt_codec_is_bounded_and_fail_closed() {
        let turn = TurnId::new(1).expect("Turn id");
        let started = RuntimeEvent::UsageAttemptStarted {
            turn,
            attempt: 1,
            started_at: UsageTimestamp::from_unix_millis(1_000).expect("timestamp"),
        };
        let encoded_start = started.clone().encode().expect("encode attempt start");
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded_start.clone()))
                .expect("decode attempt start"),
            started
        );

        let finished = RuntimeEvent::UsageAttemptFinished {
            turn,
            attempt: 1,
            completed_at: UsageTimestamp::from_unix_millis(2_000).expect("timestamp"),
            outcome: UsageAttemptOutcome::Succeeded,
            usage: Some(UsageRecord::estimated(2, 3)),
            named_windows: vec!["workday".to_owned()],
            cost_evaluation_required: true,
        };
        let encoded_finish = finished.clone().encode().expect("encode attempt finish");
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded_finish.clone()))
                .expect("decode attempt finish"),
            finished
        );

        let mut invalid_time = encoded_start;
        invalid_time.payload[12..20].copy_from_slice(&i64::MAX.to_le_bytes());
        assert!(matches!(
            RuntimeEvent::decode(&stored_runtime_event(invalid_time)),
            Err(RuntimeError::Usage(UsageError::TimestampRange))
        ));

        let mut invalid_outcome = encoded_finish;
        invalid_outcome.payload[20] = u8::MAX;
        assert!(matches!(
            RuntimeEvent::decode(&stored_runtime_event(invalid_outcome)),
            Err(RuntimeError::CorruptEvent("invalid usage attempt outcome"))
        ));
    }

    #[test]
    fn schema_five_cost_evaluation_rejects_tampered_amount_against_frozen_evidence() {
        let thread = ThreadId::new(1).expect("Thread id");
        let turn = TurnId::new(1).expect("Turn id");
        let config_id = ConfigEpochId::new(1).expect("Config Epoch id");
        let provider_id = ProviderEpochId::new(1).expect("Provider Epoch id");
        let schedule = PriceSchedule::new(PriceScheduleDefinition {
            id: "simulator-standard".to_owned(),
            version: "2026-08-10".to_owned(),
            currency: "USD".to_owned(),
            provider_profile: "simulator".to_owned(),
            model: "deterministic-v1".to_owned(),
            dialect: None,
            service_tier: None,
            minimum_context_tokens: 0,
            maximum_context_tokens: None,
            effective_from: UsageTimestamp::from_unix_millis(0).expect("timestamp"),
            effective_until: None,
            source: PriceScheduleSource::Manual,
            source_ref: "synthetic-runtime-rate-card".to_owned(),
            rates: TokenRates::new(1, 0, 0, 0, 0),
        })
        .expect("Price Schedule");
        let schedule_fingerprint = schedule.fingerprint();
        let config = ConfigEpoch::freeze_with_observability(
            config_id,
            &ConfigLayers::default(),
            Vec::new(),
            PriceScheduleBook::new(vec![schedule]).expect("Price Schedule book"),
        )
        .expect("Config Epoch");
        let provider = ProviderEpoch::new(provider_id, "simulator", "deterministic-v1")
            .expect("Provider Epoch");
        let mut state = RuntimeState::default();
        for event in [
            RuntimeEvent::ThreadCreated { thread },
            RuntimeEvent::ConfigFrozen { epoch: config },
            RuntimeEvent::ProviderFrozen {
                epoch: Box::new(provider),
            },
            RuntimeEvent::TurnAdmitted {
                thread,
                turn,
                user_item: ItemId::new(1).expect("Item id"),
                config: config_id,
                provider: provider_id,
                agent: None,
                input: "input".to_owned(),
            },
            RuntimeEvent::UsageAttemptStarted {
                turn,
                attempt: 1,
                started_at: UsageTimestamp::from_unix_millis(1).expect("timestamp"),
            },
            RuntimeEvent::UsageAttemptFinished {
                turn,
                attempt: 1,
                completed_at: UsageTimestamp::from_unix_millis(2).expect("timestamp"),
                outcome: UsageAttemptOutcome::Succeeded,
                usage: Some(
                    UsageRecord::new(Some(1), Some(0), Some(0), Some(0), Some(0), Some(1), None)
                        .expect("usage"),
                ),
                named_windows: Vec::new(),
                cost_evaluation_required: true,
            },
        ] {
            state.apply(event).expect("valid Runtime transition");
        }
        assert!(matches!(
            state.validate_quiescent(),
            Err(RuntimeError::CorruptState(
                "Usage transaction ended before cost evaluation"
            ))
        ));
        let tampered = RuntimeEvent::UsageAttemptCostEvaluated {
            turn,
            attempt: 1,
            evaluation: FrozenCostEvaluation::Known {
                schedule_id: "simulator-standard".to_owned(),
                schedule_fingerprint,
                amount_pico_units: 2,
            },
        };
        let encoded = tampered.clone().encode().expect("encode Cost Estimate");
        assert_eq!(encoded.schema, RUNTIME_EVENT_SCHEMA);
        assert_eq!(encoded.kind, 13);
        assert_eq!(
            RuntimeEvent::decode(&stored_runtime_event(encoded)).expect("decode Cost Estimate"),
            tampered
        );
        assert!(matches!(
            state.apply(tampered),
            Err(RuntimeError::CorruptState(
                "Cost Estimate does not match frozen usage and pricing evidence"
            ))
        ));
        state
            .apply(RuntimeEvent::UsageAttemptCostEvaluated {
                turn,
                attempt: 1,
                evaluation: FrozenCostEvaluation::Known {
                    schedule_id: "simulator-standard".to_owned(),
                    schedule_fingerprint,
                    amount_pico_units: 1,
                },
            })
            .expect("matching Cost Estimate completes the Usage transaction");
        state
            .validate_quiescent()
            .expect("completed Usage transaction is quiescent");
    }

    #[test]
    fn prepared_output_rechecks_the_frozen_config_limit() {
        let thread = ThreadId::new(1).expect("Thread id");
        let turn = TurnId::new(1).expect("Turn id");
        let user_item = ItemId::new(1).expect("User Item id");
        let assistant_item = ItemId::new(2).expect("Assistant Item id");
        let delivery = DeliveryId::new(1).expect("Delivery id");
        let config_id = ConfigEpochId::new(1).expect("Config Epoch id");
        let provider_id = ProviderEpochId::new(1).expect("Provider Epoch id");
        let layers = ConfigLayers {
            cli: ConfigLayer {
                max_output_bytes: Some(3),
                ..ConfigLayer::default()
            },
            ..ConfigLayers::default()
        };
        let config = ConfigEpoch::freeze(config_id, &layers).expect("freeze Config");
        let provider = ProviderEpoch::new(
            provider_id,
            config.resolved().provider_profile().value().clone(),
            config.resolved().provider_model().value().clone(),
        )
        .expect("freeze Provider");
        let mut state = RuntimeState::default();
        for event in [
            RuntimeEvent::ThreadCreated { thread },
            RuntimeEvent::ConfigFrozen { epoch: config },
            RuntimeEvent::ProviderFrozen {
                epoch: Box::new(provider),
            },
            RuntimeEvent::TurnAdmitted {
                thread,
                turn,
                user_item,
                config: config_id,
                provider: provider_id,
                agent: None,
                input: "input".to_owned(),
            },
            RuntimeEvent::AssistantItemStarted {
                turn,
                item: assistant_item,
            },
            RuntimeEvent::AssistantTextDelta {
                turn,
                item: assistant_item,
                delta: "four".to_owned(),
            },
        ] {
            state.apply(event).expect("valid setup event");
        }

        assert!(matches!(
            state.apply(RuntimeEvent::OutputPrepared {
                turn,
                item: assistant_item,
                delivery,
                text: "four".to_owned(),
                usage_records: vec![UsageRecord::default()],
                legacy_usage: true,
            }),
            Err(RuntimeError::CorruptState(
                "prepared output exceeds the frozen Config Epoch limit"
            ))
        ));
    }
}
