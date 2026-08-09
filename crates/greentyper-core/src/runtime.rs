//! Single-Agent Runtime Kernel with durable admission, output preparation, and acknowledgement.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::agent_team::{
    AgentSession, DurableTeamError, DurableTeamRuntime, TeamCommand, TeamError,
    TeamOperationAcknowledgeOutcome, TeamOperationCommit, TeamOperationId, TeamOperationRecord,
    TeamOperationStatus, TeamSnapshot,
};
use crate::config::{ConfigEpoch, ConfigError, ConfigLayer, ConfigLayers, ConfigSource};
use crate::ledger::{
    DurabilityReceipt, EventData, FileLedger, LedgerError, LedgerHead, StoredEvent,
};
use crate::model::{
    CanonicalItem, ConfigEpochId, DeliveryId, ItemId, ItemRole, ModelError, ProviderEpochId,
    ThreadId, TurnId,
};
use crate::provider::{
    MAX_SERVICE_TIER_BYTES, ProviderEpoch, ProviderError, ProviderEvent, ProviderRequest,
    ProviderRuntime, ProviderToolCall, ProviderToolOutput, UsageRecord,
};
use crate::schema::SchemaKind;
use crate::tool_runtime::{
    ApprovalDecision, DurableToolRuntime, ToolApprovalRequest, ToolArguments, ToolArgumentsHash,
    ToolCallId, ToolCallOutcome, ToolCallRecord, ToolCallStatus, ToolEffectExecutor, ToolIntent,
    ToolPrincipal, ToolReconciliationDecision, ToolRequestOutcome, ToolResources, ToolRuntimeError,
    ToolSnapshot,
};

pub const RUNTIME_EVENT_SCHEMA: u16 = SchemaKind::RuntimeEvent.current().get();
pub const MAX_INPUT_BYTES: usize = 512 * 1024;
const MAX_BLOCK_REASON_BYTES: usize = 4096;
const MAX_USAGE_RECORDS_PER_TURN: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryStatus {
    Ready,
    ResumeRequired { turn: TurnId },
    ReconciliationRequired { turn: TurnId, delivery: DeliveryId },
    Blocked { turn: TurnId, reason: String },
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
            Self::Blocked { turn, reason } => {
                write!(formatter, "blocked turn={} reason={reason}", turn.get())
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
    pub recovered_tail_bytes: u64,
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

    pub fn inspect(path: impl AsRef<Path>) -> Result<RuntimeSnapshot, RuntimeError> {
        let report = match FileLedger::inspect(path) {
            Ok(report) => report,
            Err(LedgerError::Io(source)) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RuntimeSnapshot {
                    head: LedgerHead::default(),
                    thread: None,
                    items: Vec::new(),
                    status: RecoveryStatus::Ready,
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
            recovered_tail_bytes: report.truncated_tail_bytes,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            head: self.ledger.head(),
            thread: self.state.thread,
            items: self.state.items.clone(),
            status: self.state.status(),
            recovered_tail_bytes: self.recovered_tail_bytes,
        }
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
        self.admit_turn(layers, input.into())?;
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
        self.require_provider_session(session)?;
        self.admit_turn(layers, input.into())?;
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
        let provider_events =
            match provider.continue_after_tool(&provider_request, &provider_output) {
                Ok(events) => events,
                Err(source) => {
                    self.block_pending(turn, provider_block_reason(&source))?;
                    return Err(RuntimeError::Provider(source));
                }
            };
        match validate_provider_events(&provider_events, self.pending_max_output_bytes(turn)?) {
            Ok(ValidatedProviderStep::Completed {
                deltas,
                usage_record,
            }) => {
                leading_deltas.extend(deltas);
                usage_records.push(usage_record);
                self.prepare_output(turn, leading_deltas, usage_records)
            }
            Ok(ValidatedProviderStep::ToolCall { .. }) => {
                self.block_pending(turn, "Provider continuation requested another Tool call")?;
                Err(RuntimeError::InvalidProviderOutput(
                    "Provider continuation requested more than one Tool call",
                ))
            }
            Err(reason) => {
                self.block_pending(turn, reason)?;
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

    fn admit_turn(&mut self, layers: &ConfigLayers, input: String) -> Result<(), RuntimeError> {
        self.require_ready()?;
        self.require_no_tool_reconciliation()?;
        validate_input(&input)?;

        let thread = match self.state.thread {
            Some(thread) => thread,
            None => ThreadId::new(self.state.next_thread).map_err(RuntimeError::Model)?,
        };
        let turn = TurnId::new(self.state.next_turn).map_err(RuntimeError::Model)?;
        let user_item = ItemId::new(self.state.next_item).map_err(RuntimeError::Model)?;
        let config_id = ConfigEpochId::new(self.state.next_config).map_err(RuntimeError::Model)?;
        let provider_id =
            ProviderEpochId::new(self.state.next_provider).map_err(RuntimeError::Model)?;
        let config = ConfigEpoch::freeze(config_id, layers).map_err(RuntimeError::Config)?;
        let provider_epoch = ProviderEpoch::new(
            provider_id,
            config.resolved().provider_profile().value().clone(),
            config.resolved().provider_model().value().clone(),
        )
        .map_err(RuntimeError::Provider)?;

        let mut admission = Vec::new();
        if self.state.thread.is_none() {
            admission.push(RuntimeEvent::ThreadCreated { thread });
        }
        admission.push(RuntimeEvent::ConfigFrozen {
            epoch: config.clone(),
        });
        admission.push(RuntimeEvent::ProviderFrozen {
            epoch: provider_epoch,
        });
        admission.push(RuntimeEvent::TurnAdmitted {
            thread,
            turn,
            user_item,
            config: config_id,
            provider: provider_id,
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
        let provider_events = match provider.run(&request) {
            Ok(events) => events,
            Err(source) => {
                self.block_pending(pending.turn, provider_block_reason(&source))?;
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
            }) => self.prepare_output(pending.turn, deltas, vec![usage_record]),
            Ok(ValidatedProviderStep::ToolCall { .. }) => {
                self.block_pending(pending.turn, "Provider requested an unavailable Tool")?;
                Err(RuntimeError::InvalidProviderOutput(
                    "Provider requested a Tool through the text-only interface",
                ))
            }
            Err(reason) => {
                self.block_pending(pending.turn, reason)?;
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
        let provider_events = match provider.run(&request) {
            Ok(events) => events,
            Err(source) => {
                self.block_pending(pending.turn, provider_block_reason(&source))?;
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
                .prepare_output(pending.turn, deltas, vec![usage_record])
                .map(ProviderTurnOutcome::Prepared),
            Ok(ValidatedProviderStep::ToolCall {
                deltas,
                call,
                usage_record,
            }) => {
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
                self.block_pending(pending.turn, reason)?;
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
    ) -> Result<PreparedOutput, RuntimeError> {
        if usage_records.is_empty() || usage_records.len() > MAX_USAGE_RECORDS_PER_TURN {
            self.block_pending(turn, "Provider usage record count is invalid")?;
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
                self.block_pending(
                    turn,
                    "Provider output exceeds the frozen Config Epoch limit",
                )?;
                return Err(RuntimeError::InvalidProviderOutput(
                    "Provider output exceeds the frozen Config Epoch limit",
                ));
            }
            text.push_str(delta);
        }
        if text.trim().is_empty() {
            self.block_pending(turn, "Provider output cannot be empty")?;
            return Err(RuntimeError::InvalidProviderOutput(
                "Provider output cannot be empty",
            ));
        }

        let assistant_item = ItemId::new(self.state.next_item).map_err(RuntimeError::Model)?;
        let delivery = DeliveryId::new(self.state.next_delivery).map_err(RuntimeError::Model)?;
        let mut events = Vec::with_capacity(deltas.len() + 2);
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

    fn block_pending(&mut self, turn: TurnId, reason: &str) -> Result<(), RuntimeError> {
        let reason = bounded_reason(reason);
        self.commit(&[RuntimeEvent::TurnBlocked { turn, reason }])?;
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
enum RuntimeEvent {
    ThreadCreated {
        thread: ThreadId,
    },
    ConfigFrozen {
        epoch: ConfigEpoch,
    },
    ProviderFrozen {
        epoch: ProviderEpoch,
    },
    TurnAdmitted {
        thread: ThreadId,
        turn: TurnId,
        user_item: ItemId,
        config: ConfigEpochId,
        provider: ProviderEpochId,
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
    },
}

impl RuntimeEvent {
    fn encode(&self) -> Result<EventData, RuntimeError> {
        let mut payload = Encoder::default();
        let kind = match self {
            Self::ThreadCreated { thread } => {
                payload.u64(thread.get());
                1
            }
            Self::ConfigFrozen { epoch } => {
                encode_config_epoch(&mut payload, epoch)?;
                2
            }
            Self::ProviderFrozen { epoch } => {
                payload.u64(epoch.id().get());
                payload.string(epoch.profile())?;
                payload.string(epoch.model())?;
                3
            }
            Self::TurnAdmitted {
                thread,
                turn,
                user_item,
                config,
                provider,
                input,
            } => {
                payload.u64(thread.get());
                payload.u64(turn.get());
                payload.u64(user_item.get());
                payload.u64(config.get());
                payload.u64(provider.get());
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
            Self::TurnBlocked { turn, reason } => {
                payload.u64(turn.get());
                payload.string(reason)?;
                10
            }
        };
        Ok(EventData {
            schema: RUNTIME_EVENT_SCHEMA,
            kind,
            payload: payload.finish(),
        })
    }

    fn decode(event: &StoredEvent) -> Result<Self, RuntimeError> {
        if event.data.schema != 1 && event.data.schema != RUNTIME_EVENT_SCHEMA {
            return Err(RuntimeError::UnsupportedRuntimeEventSchema {
                supported: RUNTIME_EVENT_SCHEMA,
                actual: event.data.schema,
            });
        }
        let mut payload = Decoder::new(&event.data.payload);
        let decoded = match event.data.kind {
            1 => Self::ThreadCreated {
                thread: ThreadId::new(payload.u64()?).map_err(RuntimeError::Model)?,
            },
            2 => Self::ConfigFrozen {
                epoch: decode_config_epoch(&mut payload)?,
            },
            3 => Self::ProviderFrozen {
                epoch: ProviderEpoch::new(
                    ProviderEpochId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                    payload.string(MAX_BLOCK_REASON_BYTES)?,
                    payload.string(MAX_BLOCK_REASON_BYTES)?,
                )
                .map_err(RuntimeError::Provider)?,
            },
            4 => Self::TurnAdmitted {
                thread: ThreadId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                turn: TurnId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                user_item: ItemId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                config: ConfigEpochId::new(payload.u64()?).map_err(RuntimeError::Model)?,
                provider: ProviderEpochId::new(payload.u64()?).map_err(RuntimeError::Model)?,
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
                        .map(|_| decode_usage_record(&mut payload))
                        .collect::<Result<Vec<_>, _>>()?
                },
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
            },
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
    assistant_item: Option<ItemId>,
    delivery: Option<DeliveryId>,
    completed: bool,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingTurn {
    turn: TurnId,
    config: ConfigEpochId,
    provider: ProviderEpochId,
    input: String,
    phase: PendingPhase,
    assistant_item: Option<ItemId>,
    streamed_text: String,
    prepared: Option<PreparedState>,
    acknowledged: bool,
    blocked_reason: Option<String>,
}

#[derive(Clone, Debug)]
struct RuntimeState {
    thread: Option<ThreadId>,
    configs: BTreeMap<ConfigEpochId, ConfigEpoch>,
    providers: BTreeMap<ProviderEpochId, ProviderEpoch>,
    turns: BTreeMap<TurnId, TurnRecord>,
    items: Vec<CanonicalItem>,
    pending: Option<PendingTurn>,
    acknowledged: BTreeSet<DeliveryId>,
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
            acknowledged: BTreeSet::new(),
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
            },
            PendingPhase::Streaming => RecoveryStatus::Blocked {
                turn: pending.turn,
                reason: "incomplete output transaction".to_owned(),
            },
        }
    }

    fn apply(&mut self, event: RuntimeEvent) -> Result<(), RuntimeError> {
        match event {
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
                if self.providers.insert(id, epoch).is_some() {
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
                input,
            } => {
                if self.thread != Some(thread) || self.pending.is_some() {
                    return Err(RuntimeError::CorruptState("invalid Turn admission"));
                }
                if !self.configs.contains_key(&config) || !self.providers.contains_key(&provider) {
                    return Err(RuntimeError::CorruptState("Turn snapshot is missing"));
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
                        assistant_item: None,
                        delivery: None,
                        completed: false,
                    },
                );
                self.pending = Some(PendingTurn {
                    turn,
                    config,
                    provider,
                    input,
                    phase: PendingPhase::Admitted,
                    assistant_item: None,
                    streamed_text: String::new(),
                    prepared: None,
                    acknowledged: false,
                    blocked_reason: None,
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
            RuntimeEvent::TurnBlocked { turn, reason } => {
                if reason.trim().is_empty()
                    || reason.len() > MAX_BLOCK_REASON_BYTES
                    || reason.chars().any(char::is_control)
                {
                    return Err(RuntimeError::CorruptState("invalid blocked reason"));
                }
                let pending = self.pending_for(turn)?;
                if pending.phase != PendingPhase::Admitted {
                    return Err(RuntimeError::CorruptState("invalid blocked transition"));
                }
                pending.phase = PendingPhase::Blocked;
                pending.blocked_reason = Some(reason);
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
}

fn replay_runtime(events: &[StoredEvent]) -> Result<RuntimeState, RuntimeError> {
    let mut state = RuntimeState::default();
    let mut index = 0;
    while index < events.len() {
        let transaction = events[index].transaction;
        let mut candidate = state.clone();
        while index < events.len() && events[index].transaction == transaction {
            candidate.apply(RuntimeEvent::decode(&events[index])?)?;
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
        ProviderError::Unavailable { .. } => "Provider became unavailable",
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
    Ok(())
}

fn decode_usage_record(decoder: &mut Decoder<'_>) -> Result<UsageRecord, RuntimeError> {
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
    UsageRecord::new(
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        service_tier,
    )
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

fn decode_optional_u64(decoder: &mut Decoder<'_>) -> Result<Option<u64>, RuntimeError> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decoder.u64()?)),
        _ => Err(RuntimeError::CorruptEvent("invalid optional integer tag")),
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
    Ok(())
}

fn decode_config_epoch(decoder: &mut Decoder<'_>) -> Result<ConfigEpoch, RuntimeError> {
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
    let epoch = ConfigEpoch::freeze(id, &layers).map_err(RuntimeError::Config)?;
    if epoch.fingerprint() != fingerprint {
        return Err(RuntimeError::CorruptEvent(
            "Config Epoch fingerprint mismatch",
        ));
    }
    Ok(epoch)
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

    fn string(&mut self, value: &str) -> Result<(), RuntimeError> {
        let length = u32::try_from(value.len()).map_err(|_| RuntimeError::IntegerOverflow)?;
        self.u32(length);
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
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
    Busy(RecoveryStatus),
    UnknownDelivery(DeliveryId),
    InvalidInput(&'static str),
    InvalidProviderOutput(&'static str),
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
            Self::Busy(status) => write!(formatter, "Runtime requires reconciliation: {status}"),
            Self::UnknownDelivery(delivery) => {
                write!(formatter, "unknown output delivery {}", delivery.get())
            }
            Self::InvalidInput(reason) => write!(formatter, "invalid input: {reason}"),
            Self::InvalidProviderOutput(reason) => {
                write!(formatter, "invalid provider output: {reason}")
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
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            RuntimeEvent::ProviderFrozen { epoch: provider },
            RuntimeEvent::TurnAdmitted {
                thread,
                turn,
                user_item,
                config: config_id,
                provider: provider_id,
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
            }),
            Err(RuntimeError::CorruptState(
                "prepared output exceeds the frozen Config Epoch limit"
            ))
        ));
    }
}
