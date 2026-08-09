use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::Path;

use crate::ledger::{
    DurabilityReceipt, EventData, FileLedger, LedgerError, LedgerHead, StoredEvent,
};
use crate::schema::SchemaKind;

use super::*;

const TEAM_EVENT_SCHEMA: u16 = SchemaKind::TeamEvent.current().get();
const TEAM_EVENT_SCHEMA_V1: u16 = 1;

/// File-backed adapter for [`TeamRuntime`].
///
/// One adapter owns one exclusive Ledger writer. A command is fully planned
/// and validated, synchronously appended, and only then published to the Team
/// projection.
pub struct DurableTeamRuntime {
    runtime: TeamRuntime,
    ledger: FileLedger,
    recovered_tail_bytes: u64,
}

impl DurableTeamRuntime {
    pub fn open(
        path: impl AsRef<Path>,
        max_active_agents: usize,
    ) -> Result<Self, DurableTeamError> {
        if max_active_agents == 0 {
            return Err(DurableTeamError::Team(TeamError::InvalidActiveAgentLimit));
        }
        let (ledger, report) = FileLedger::open(path).map_err(DurableTeamError::Ledger)?;
        let events = report
            .events
            .iter()
            .map(decode_stored_event)
            .collect::<Result<Vec<_>, _>>()?;
        let runtime =
            TeamRuntime::recover(max_active_agents, events).map_err(DurableTeamError::Recovery)?;
        let team_head = runtime_head(&runtime);
        if team_head != report.head {
            return Err(DurableTeamError::HeadMismatch {
                ledger: report.head,
                team: team_head,
            });
        }

        Ok(Self {
            runtime,
            ledger,
            recovered_tail_bytes: report.truncated_tail_bytes,
        })
    }

    #[must_use]
    pub const fn max_active_agents(&self) -> usize {
        self.runtime.max_active_agents()
    }

    #[must_use]
    pub fn snapshot(&self) -> TeamSnapshot {
        self.runtime.snapshot()
    }

    #[must_use]
    pub fn event_log(&self) -> &[TeamEvent] {
        self.runtime.event_log()
    }

    #[must_use]
    pub const fn ledger_head(&self) -> LedgerHead {
        self.ledger.head()
    }

    #[must_use]
    pub const fn recovered_tail_bytes(&self) -> u64 {
        self.recovered_tail_bytes
    }

    pub(crate) fn trusted_rebind_nonterminal_sessions(&self) -> Vec<AgentSession> {
        self.runtime.trusted_rebind_nonterminal_sessions()
    }

    pub(crate) fn trusted_active_agent_context(
        &self,
        session: AgentSession,
    ) -> Result<AgentExecutionContext, DurableTeamError> {
        self.runtime
            .trusted_active_agent_context(session)
            .map_err(DurableTeamError::Team)
    }

    pub(crate) fn next_operation_id(&self) -> Result<TeamOperationId, DurableTeamError> {
        self.runtime
            .next_operation_id()
            .map_err(DurableTeamError::Team)
    }

    pub(crate) fn operation_records(&self) -> Vec<TeamOperationRecord> {
        self.runtime.operation_records()
    }

    /// Dispatches through the standalone durable policy adapter.
    ///
    /// This low-level seam intentionally has no Kernel operation identity or
    /// acknowledgement. Product Team driving goes through
    /// [`crate::runtime::RuntimeKernel::dispatch_team`].
    pub fn dispatch(&mut self, command: TeamCommand) -> Result<TeamCommit, DurableTeamError> {
        self.dispatch_with(command, FileLedger::append)
    }

    pub(crate) fn dispatch_operation(
        &mut self,
        operation: TeamOperationId,
        command: TeamCommand,
    ) -> Result<TeamOperationCommit, DurableTeamError> {
        self.dispatch_operation_with(operation, command, FileLedger::append)
    }

    fn dispatch_operation_with<F>(
        &mut self,
        operation: TeamOperationId,
        command: TeamCommand,
        append: F,
    ) -> Result<TeamOperationCommit, DurableTeamError>
    where
        F: FnOnce(
            &mut FileLedger,
            LedgerHead,
            &[EventData],
        ) -> Result<DurabilityReceipt, LedgerError>,
    {
        let prepared = self
            .runtime
            .prepare_operation(operation, command)
            .map_err(DurableTeamError::Team)?;
        let receipt = self.append_prepared(&prepared.prepared, append)?;
        let commit = self
            .runtime
            .publish(prepared, CommitDurability::Synchronous(receipt));
        Ok(TeamOperationCommit { operation, commit })
    }

    pub(crate) fn acknowledge_operation(
        &mut self,
        operation: TeamOperationId,
    ) -> Result<TeamOperationAcknowledgeOutcome, DurableTeamError> {
        self.acknowledge_operation_with(operation, FileLedger::append)
    }

    fn acknowledge_operation_with<F>(
        &mut self,
        operation: TeamOperationId,
        append: F,
    ) -> Result<TeamOperationAcknowledgeOutcome, DurableTeamError>
    where
        F: FnOnce(
            &mut FileLedger,
            LedgerHead,
            &[EventData],
        ) -> Result<DurabilityReceipt, LedgerError>,
    {
        let record = self
            .runtime
            .operation_records()
            .into_iter()
            .find(|record| record.operation == operation)
            .ok_or(DurableTeamError::Team(TeamError::UnknownOperation {
                operation,
            }))?;
        if record.status == TeamOperationStatus::Acknowledged {
            return Ok(TeamOperationAcknowledgeOutcome::AlreadyAcknowledged);
        }

        let prepared = self
            .runtime
            .prepare_operation_acknowledgement(operation)
            .map_err(DurableTeamError::Team)?;
        let receipt = self.append_prepared(&prepared, append)?;
        self.runtime.publish_events(prepared);
        Ok(TeamOperationAcknowledgeOutcome::Durable(receipt))
    }

    fn dispatch_with<F>(
        &mut self,
        command: TeamCommand,
        append: F,
    ) -> Result<TeamCommit, DurableTeamError>
    where
        F: FnOnce(
            &mut FileLedger,
            LedgerHead,
            &[EventData],
        ) -> Result<DurabilityReceipt, LedgerError>,
    {
        let prepared = self
            .runtime
            .prepare(command)
            .map_err(DurableTeamError::Team)?;
        let receipt = self.append_prepared(&prepared.prepared, append)?;
        Ok(self
            .runtime
            .publish(prepared, CommitDurability::Synchronous(receipt)))
    }

    fn append_prepared<F>(
        &mut self,
        prepared: &PreparedTeamEvents,
        append: F,
    ) -> Result<TeamDurabilityReceipt, DurableTeamError>
    where
        F: FnOnce(
            &mut FileLedger,
            LedgerHead,
            &[EventData],
        ) -> Result<DurabilityReceipt, LedgerError>,
    {
        let team_head = runtime_head(&self.runtime);
        let ledger_head = self.ledger.head();
        if ledger_head != team_head {
            return Err(DurableTeamError::HeadMismatch {
                ledger: ledger_head,
                team: team_head,
            });
        }
        let encoded = prepared
            .events
            .iter()
            .map(|event| encode_event_data(&event.kind))
            .collect::<Result<Vec<_>, _>>()?;
        let receipt =
            append(&mut self.ledger, team_head, &encoded).map_err(DurableTeamError::Ledger)?;
        Ok(team_receipt(receipt))
    }

    #[cfg(test)]
    fn dispatch_with_test_io<F>(
        &mut self,
        command: TeamCommand,
        write_frame: F,
    ) -> Result<TeamCommit, DurableTeamError>
    where
        F: FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    {
        self.dispatch_with(command, |ledger, head, events| {
            ledger.append_with_test_io(head, events, write_frame)
        })
    }

    #[cfg(test)]
    fn dispatch_operation_with_test_io<F>(
        &mut self,
        operation: TeamOperationId,
        command: TeamCommand,
        write_frame: F,
    ) -> Result<TeamOperationCommit, DurableTeamError>
    where
        F: FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    {
        self.dispatch_operation_with(operation, command, |ledger, head, events| {
            ledger.append_with_test_io(head, events, write_frame)
        })
    }

    #[cfg(test)]
    fn acknowledge_operation_with_test_io<F>(
        &mut self,
        operation: TeamOperationId,
        write_frame: F,
    ) -> Result<TeamOperationAcknowledgeOutcome, DurableTeamError>
    where
        F: FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    {
        self.acknowledge_operation_with(operation, |ledger, head, events| {
            ledger.append_with_test_io(head, events, write_frame)
        })
    }
}

fn runtime_head(runtime: &TeamRuntime) -> LedgerHead {
    runtime
        .event_log()
        .last()
        .map_or_else(LedgerHead::default, |event| LedgerHead {
            transaction: event.transaction.get(),
            sequence: event.sequence.get(),
        })
}

const fn team_receipt(receipt: DurabilityReceipt) -> TeamDurabilityReceipt {
    TeamDurabilityReceipt {
        transaction: TransactionId(receipt.transaction),
        first_sequence: EventSeq(receipt.first_sequence),
        last_sequence: EventSeq(receipt.last_sequence),
        event_count: receipt.event_count,
        transaction_crc32c: receipt.transaction_crc32c,
    }
}

fn decode_stored_event(stored: &StoredEvent) -> Result<TeamEvent, DurableTeamError> {
    Ok(TeamEvent {
        sequence: EventSeq(decode_identifier(stored.sequence)?),
        transaction: TransactionId(decode_identifier(stored.transaction)?),
        index_in_transaction: stored.index_in_transaction,
        events_in_transaction: stored.events_in_transaction,
        kind: decode_event_data(&stored.data)?,
    })
}

fn encode_event_data(kind: &TeamEventKind) -> Result<EventData, DurableTeamError> {
    let mut encoder = Encoder::default();
    let tag = match kind {
        TeamEventKind::TaskCreated { task, spec } => {
            encoder.identifier(task.get());
            encode_task_spec(&mut encoder, spec)?;
            1
        }
        TeamEventKind::AgentCreated {
            agent,
            task,
            parent,
            budget,
            capabilities,
        } => {
            encoder.identifier(agent.get());
            encoder.identifier(task.get());
            encode_optional_agent(&mut encoder, *parent);
            encode_budget(&mut encoder, *budget);
            encode_capabilities(&mut encoder, capabilities)?;
            2
        }
        TeamEventKind::TaskOwnerAssigned { task, agent } => {
            encoder.identifier(task.get());
            encoder.identifier(agent.get());
            3
        }
        TeamEventKind::DelegationGranted { parent, child } => {
            encoder.identifier(parent.get());
            encoder.identifier(child.get());
            4
        }
        TeamEventKind::TaskReady { task } => {
            encoder.identifier(task.get());
            5
        }
        TeamEventKind::AgentActivated { agent } => {
            encoder.identifier(agent.get());
            6
        }
        TeamEventKind::TaskStarted { task } => {
            encoder.identifier(task.get());
            7
        }
        TeamEventKind::MessageSent {
            message,
            from,
            recipient,
            body,
        } => {
            encoder.identifier(message.get());
            encoder.identifier(from.get());
            encode_recipient(&mut encoder, *recipient);
            encoder.string(body)?;
            8
        }
        TeamEventKind::CompletionCapsuleSubmitted {
            task,
            agent,
            capsule,
        } => {
            encoder.identifier(task.get());
            encoder.identifier(agent.get());
            encode_capsule(&mut encoder, capsule)?;
            9
        }
        TeamEventKind::TaskSucceeded { task } => {
            encoder.identifier(task.get());
            10
        }
        TeamEventKind::AgentSucceeded { agent } => {
            encoder.identifier(agent.get());
            11
        }
        TeamEventKind::TaskFailed { task, reason } => {
            encoder.identifier(task.get());
            encoder.string(reason)?;
            12
        }
        TeamEventKind::AgentFailed { agent } => {
            encoder.identifier(agent.get());
            13
        }
        TeamEventKind::TaskCancelled { task, reason } => {
            encoder.identifier(task.get());
            encoder.string(reason)?;
            14
        }
        TeamEventKind::AgentCancelled { agent } => {
            encoder.identifier(agent.get());
            15
        }
        TeamEventKind::TaskBlocked { task, blocked_by } => {
            encoder.identifier(task.get());
            encoder.identifier(blocked_by.get());
            16
        }
        TeamEventKind::AgentBlocked { agent, blocked_by } => {
            encoder.identifier(agent.get());
            encoder.identifier(blocked_by.get());
            17
        }
        TeamEventKind::OperationCommitted {
            operation,
            transaction,
        } => {
            encoder.identifier(operation.get());
            encoder.identifier(transaction.get());
            18
        }
        TeamEventKind::OperationAcknowledged {
            operation,
            committed_transaction,
            acknowledgement_transaction,
        } => {
            encoder.identifier(operation.get());
            encoder.identifier(committed_transaction.get());
            encoder.identifier(acknowledgement_transaction.get());
            19
        }
    };

    Ok(EventData {
        schema: TEAM_EVENT_SCHEMA,
        kind: tag,
        payload: encoder.finish(),
    })
}

fn decode_event_data(data: &EventData) -> Result<TeamEventKind, DurableTeamError> {
    if !matches!(data.schema, TEAM_EVENT_SCHEMA_V1 | TEAM_EVENT_SCHEMA) {
        return Err(DurableTeamError::UnsupportedTeamEventSchema {
            supported: TEAM_EVENT_SCHEMA,
            actual: data.schema,
        });
    }
    if data.schema == TEAM_EVENT_SCHEMA_V1 && data.kind > 17 {
        return Err(DurableTeamError::CorruptEvent(
            "Team Event kind is unavailable in schema one",
        ));
    }
    let mut decoder = Decoder::new(&data.payload);
    let kind = match data.kind {
        1 => TeamEventKind::TaskCreated {
            task: decode_task(&mut decoder)?,
            spec: decode_task_spec(&mut decoder)?,
        },
        2 => TeamEventKind::AgentCreated {
            agent: decode_agent(&mut decoder)?,
            task: decode_task(&mut decoder)?,
            parent: decode_optional_agent(&mut decoder)?,
            budget: decode_budget(&mut decoder)?,
            capabilities: decode_capabilities(&mut decoder)?,
        },
        3 => TeamEventKind::TaskOwnerAssigned {
            task: decode_task(&mut decoder)?,
            agent: decode_agent(&mut decoder)?,
        },
        4 => TeamEventKind::DelegationGranted {
            parent: decode_agent(&mut decoder)?,
            child: decode_agent(&mut decoder)?,
        },
        5 => TeamEventKind::TaskReady {
            task: decode_task(&mut decoder)?,
        },
        6 => TeamEventKind::AgentActivated {
            agent: decode_agent(&mut decoder)?,
        },
        7 => TeamEventKind::TaskStarted {
            task: decode_task(&mut decoder)?,
        },
        8 => TeamEventKind::MessageSent {
            message: decode_message(&mut decoder)?,
            from: decode_agent(&mut decoder)?,
            recipient: decode_recipient(&mut decoder)?,
            body: decoder.string(MAX_MESSAGE_BYTES)?,
        },
        9 => TeamEventKind::CompletionCapsuleSubmitted {
            task: decode_task(&mut decoder)?,
            agent: decode_agent(&mut decoder)?,
            capsule: decode_capsule(&mut decoder)?,
        },
        10 => TeamEventKind::TaskSucceeded {
            task: decode_task(&mut decoder)?,
        },
        11 => TeamEventKind::AgentSucceeded {
            agent: decode_agent(&mut decoder)?,
        },
        12 => TeamEventKind::TaskFailed {
            task: decode_task(&mut decoder)?,
            reason: decoder.string(MAX_REASON_BYTES)?,
        },
        13 => TeamEventKind::AgentFailed {
            agent: decode_agent(&mut decoder)?,
        },
        14 => TeamEventKind::TaskCancelled {
            task: decode_task(&mut decoder)?,
            reason: decoder.string(MAX_REASON_BYTES)?,
        },
        15 => TeamEventKind::AgentCancelled {
            agent: decode_agent(&mut decoder)?,
        },
        16 => TeamEventKind::TaskBlocked {
            task: decode_task(&mut decoder)?,
            blocked_by: decode_task(&mut decoder)?,
        },
        17 => TeamEventKind::AgentBlocked {
            agent: decode_agent(&mut decoder)?,
            blocked_by: decode_task(&mut decoder)?,
        },
        18 => TeamEventKind::OperationCommitted {
            operation: decode_operation(&mut decoder)?,
            transaction: decode_transaction(&mut decoder)?,
        },
        19 => TeamEventKind::OperationAcknowledged {
            operation: decode_operation(&mut decoder)?,
            committed_transaction: decode_transaction(&mut decoder)?,
            acknowledgement_transaction: decode_transaction(&mut decoder)?,
        },
        _ => return Err(DurableTeamError::CorruptEvent("unknown Team Event kind")),
    };
    decoder.finish()?;
    Ok(kind)
}

fn encode_task_spec(encoder: &mut Encoder, spec: &TaskSpec) -> Result<(), DurableTeamError> {
    encoder.string(&spec.title)?;
    encoder.count(spec.scope.labels.len())?;
    for label in &spec.scope.labels {
        encoder.string(label)?;
    }
    encoder.count(spec.dependencies.len())?;
    for dependency in &spec.dependencies {
        encoder.identifier(dependency.get());
    }
    Ok(())
}

fn decode_task_spec(decoder: &mut Decoder<'_>) -> Result<TaskSpec, DurableTeamError> {
    let title = decoder.string(MAX_TASK_TITLE_BYTES)?;
    let label_count = decoder.count(MAX_SCOPE_LABELS)?;
    let mut labels = BTreeSet::new();
    for _ in 0..label_count {
        if !labels.insert(decoder.string(MAX_SCOPE_LABEL_BYTES)?) {
            return Err(DurableTeamError::CorruptEvent("duplicate Team scope label"));
        }
    }
    let dependency_count = decoder.count(MAX_TASK_DEPENDENCIES)?;
    let mut dependencies = Vec::with_capacity(dependency_count);
    for _ in 0..dependency_count {
        dependencies.push(decode_task(decoder)?);
    }
    Ok(TaskSpec {
        title,
        scope: TaskScope { labels },
        dependencies,
    })
}

fn encode_budget(encoder: &mut Encoder, budget: ResourceBudget) {
    encoder.u64(budget.token_units);
    encoder.u32(budget.tool_calls);
}

fn decode_budget(decoder: &mut Decoder<'_>) -> Result<ResourceBudget, DurableTeamError> {
    Ok(ResourceBudget::new(decoder.u64()?, decoder.u32()?))
}

fn encode_capabilities(
    encoder: &mut Encoder,
    snapshot: &CapabilitySnapshot,
) -> Result<(), DurableTeamError> {
    encoder.count(snapshot.capabilities.len())?;
    for capability in &snapshot.capabilities {
        match capability {
            Capability::WorkspaceRead => encoder.u8(1),
            Capability::WorkspaceWrite => encoder.u8(2),
            Capability::Process => encoder.u8(3),
            Capability::Network => encoder.u8(4),
            Capability::Tool(name) => {
                encoder.u8(5);
                encoder.string(name)?;
            }
        }
    }
    Ok(())
}

fn decode_capabilities(decoder: &mut Decoder<'_>) -> Result<CapabilitySnapshot, DurableTeamError> {
    let count = decoder.count(MAX_CAPABILITIES)?;
    let mut capabilities = BTreeSet::new();
    for _ in 0..count {
        let capability = match decoder.u8()? {
            1 => Capability::WorkspaceRead,
            2 => Capability::WorkspaceWrite,
            3 => Capability::Process,
            4 => Capability::Network,
            5 => Capability::Tool(decoder.string(MAX_TOOL_NAME_BYTES)?),
            _ => {
                return Err(DurableTeamError::CorruptEvent(
                    "invalid Team capability tag",
                ));
            }
        };
        if !capabilities.insert(capability) {
            return Err(DurableTeamError::CorruptEvent("duplicate Team capability"));
        }
    }
    Ok(CapabilitySnapshot { capabilities })
}

fn encode_optional_agent(encoder: &mut Encoder, agent: Option<AgentId>) {
    match agent {
        None => encoder.u8(0),
        Some(agent) => {
            encoder.u8(1);
            encoder.identifier(agent.get());
        }
    }
}

fn decode_optional_agent(decoder: &mut Decoder<'_>) -> Result<Option<AgentId>, DurableTeamError> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_agent(decoder)?)),
        _ => Err(DurableTeamError::CorruptEvent("invalid optional Agent tag")),
    }
}

fn encode_recipient(encoder: &mut Encoder, recipient: MessageRecipient) {
    match recipient {
        MessageRecipient::Agent(agent) => {
            encoder.u8(1);
            encoder.identifier(agent.get());
        }
        MessageRecipient::Team => encoder.u8(2),
    }
}

fn decode_recipient(decoder: &mut Decoder<'_>) -> Result<MessageRecipient, DurableTeamError> {
    match decoder.u8()? {
        1 => Ok(MessageRecipient::Agent(decode_agent(decoder)?)),
        2 => Ok(MessageRecipient::Team),
        _ => Err(DurableTeamError::CorruptEvent(
            "invalid message recipient tag",
        )),
    }
}

fn encode_capsule(
    encoder: &mut Encoder,
    capsule: &CompletionCapsule,
) -> Result<(), DurableTeamError> {
    encoder.string(&capsule.outcome)?;
    for values in [
        &capsule.evidence,
        &capsule.changes,
        &capsule.tests,
        &capsule.decisions,
        &capsule.blockers,
        &capsule.artifacts,
        &capsule.residual_risks,
    ] {
        encoder.string_list(values)?;
    }
    Ok(())
}

fn decode_capsule(decoder: &mut Decoder<'_>) -> Result<CompletionCapsule, DurableTeamError> {
    let mut total_entries = 0_usize;
    let outcome = decoder.string(MAX_CAPSULE_BYTES)?;
    let evidence = decoder.string_list(&mut total_entries)?;
    let changes = decoder.string_list(&mut total_entries)?;
    let tests = decoder.string_list(&mut total_entries)?;
    let decisions = decoder.string_list(&mut total_entries)?;
    let blockers = decoder.string_list(&mut total_entries)?;
    let artifacts = decoder.string_list(&mut total_entries)?;
    let residual_risks = decoder.string_list(&mut total_entries)?;
    Ok(CompletionCapsule {
        outcome,
        evidence,
        changes,
        tests,
        decisions,
        blockers,
        artifacts,
        residual_risks,
    })
}

fn decode_task(decoder: &mut Decoder<'_>) -> Result<TaskId, DurableTeamError> {
    Ok(TaskId(decoder.identifier()?))
}

fn decode_agent(decoder: &mut Decoder<'_>) -> Result<AgentId, DurableTeamError> {
    Ok(AgentId(decoder.identifier()?))
}

fn decode_message(decoder: &mut Decoder<'_>) -> Result<MessageId, DurableTeamError> {
    Ok(MessageId(decoder.identifier()?))
}

fn decode_operation(decoder: &mut Decoder<'_>) -> Result<TeamOperationId, DurableTeamError> {
    Ok(TeamOperationId(decoder.identifier()?))
}

fn decode_transaction(decoder: &mut Decoder<'_>) -> Result<TransactionId, DurableTeamError> {
    Ok(TransactionId(decoder.identifier()?))
}

fn decode_identifier(value: u64) -> Result<u64, DurableTeamError> {
    if value == 0 || value == u64::MAX {
        Err(DurableTeamError::CorruptEvent(
            "invalid Team Event identifier",
        ))
    } else {
        Ok(value)
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

    fn identifier(&mut self, value: u64) {
        self.u64(value);
    }

    fn count(&mut self, value: usize) -> Result<(), DurableTeamError> {
        self.u32(u32::try_from(value).map_err(|_| DurableTeamError::IntegerOverflow)?);
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<(), DurableTeamError> {
        self.count(value.len())?;
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn string_list(&mut self, values: &[String]) -> Result<(), DurableTeamError> {
        self.count(values.len())?;
        for value in values {
            self.string(value)?;
        }
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

    fn bytes(&mut self, length: usize) -> Result<&'a [u8], DurableTeamError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(DurableTeamError::IntegerOverflow)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(DurableTeamError::CorruptEvent("truncated Team Event"))?;
        self.position = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, DurableTeamError> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, DurableTeamError> {
        Ok(u32::from_le_bytes(
            self.bytes(4)?.try_into().expect("fixed integer slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64, DurableTeamError> {
        Ok(u64::from_le_bytes(
            self.bytes(8)?.try_into().expect("fixed integer slice"),
        ))
    }

    fn identifier(&mut self) -> Result<u64, DurableTeamError> {
        decode_identifier(self.u64()?)
    }

    fn count(&mut self, max: usize) -> Result<usize, DurableTeamError> {
        let value = self.u32()? as usize;
        if value > max {
            return Err(DurableTeamError::CorruptEvent(
                "Team Event collection exceeds its bound",
            ));
        }
        Ok(value)
    }

    fn string(&mut self, max_bytes: usize) -> Result<String, DurableTeamError> {
        let length = self.count(max_bytes)?;
        let bytes = self.bytes(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| DurableTeamError::CorruptEvent("Team Event string is not UTF-8"))?;
        Ok(value.to_owned())
    }

    fn string_list(&mut self, total_entries: &mut usize) -> Result<Vec<String>, DurableTeamError> {
        let count = self.count(MAX_CAPSULE_ENTRIES)?;
        *total_entries = total_entries
            .checked_add(count)
            .ok_or(DurableTeamError::IntegerOverflow)?;
        if *total_entries > MAX_CAPSULE_ENTRIES {
            return Err(DurableTeamError::CorruptEvent(
                "Completion Capsule has too many entries",
            ));
        }
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.string(MAX_CAPSULE_BYTES)?);
        }
        Ok(values)
    }

    fn finish(self) -> Result<(), DurableTeamError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(DurableTeamError::CorruptEvent(
                "Team Event has trailing bytes",
            ))
        }
    }
}

#[derive(Debug)]
pub enum DurableTeamError {
    Ledger(LedgerError),
    Team(TeamError),
    Recovery(RecoveryError),
    UnsupportedTeamEventSchema {
        supported: u16,
        actual: u16,
    },
    CorruptEvent(&'static str),
    HeadMismatch {
        ledger: LedgerHead,
        team: LedgerHead,
    },
    IntegerOverflow,
}

impl fmt::Display for DurableTeamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ledger(source) => write!(formatter, "{source}"),
            Self::Team(source) => write!(formatter, "{source}"),
            Self::Recovery(source) => write!(formatter, "Team recovery failed: {source}"),
            Self::UnsupportedTeamEventSchema { supported, actual } => write!(
                formatter,
                "unsupported Team Event schema {actual}; expected {supported}"
            ),
            Self::CorruptEvent(reason) => write!(formatter, "corrupt Team Event: {reason}"),
            Self::HeadMismatch { ledger, team } => write!(
                formatter,
                "Team/Ledger head mismatch: ledger {ledger:?}, Team {team:?}"
            ),
            Self::IntegerOverflow => write!(formatter, "Team Event integer overflow"),
        }
    }
}

impl Error for DurableTeamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ledger(source) => Some(source),
            Self::Team(source) => Some(source),
            Self::Recovery(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::ffi::OsString;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use sha2::{Digest, Sha256};

    use super::*;

    const CRASH_CHILD_ENV: &str = "GREENTYPER_TEAM_CRASH_CHILD_DIR";
    const CRASH_CASE_ENV: &str = "GREENTYPER_TEAM_CRASH_CASE";
    const CRASH_CHILD_TEST: &str = "agent_team::persistence::tests::team_crash_child_entrypoint";
    const SUPERVISOR_FILE: &str = "supervisor";
    const READY_FILE: &str = "crash-ready";
    const READY_PENDING_FILE: &str = "crash-ready.pending";
    const TEAM_LEDGER_FILE: &str = "team.ledger";
    const READY_TIMEOUT: Duration = Duration::from_secs(10);
    const CHILD_TIMEOUT: Duration = Duration::from_secs(30);
    const POLL_INTERVAL: Duration = Duration::from_millis(5);
    const MAX_READY_BYTES: u64 = 256;

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum InjectedWriteFault {
        BeforeWrite,
        AfterByteOne,
        AfterLengthHeader,
        MiddleFrame,
        BeforeCommit,
        AfterFullWrite,
        AfterFlush,
        AfterSync,
    }

    impl InjectedWriteFault {
        const ALL: [Self; 8] = [
            Self::BeforeWrite,
            Self::AfterByteOne,
            Self::AfterLengthHeader,
            Self::MiddleFrame,
            Self::BeforeCommit,
            Self::AfterFullWrite,
            Self::AfterFlush,
            Self::AfterSync,
        ];

        const fn writes_complete_frame(self) -> bool {
            matches!(
                self,
                Self::AfterFullWrite | Self::AfterFlush | Self::AfterSync
            )
        }

        const fn writes_any_bytes(self) -> bool {
            !matches!(self, Self::BeforeWrite)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ProcessCrashPoint {
        BeforeWrite,
        AfterLengthHeader,
        MiddleFrame,
        BeforeCommit,
        AfterFlush,
        AfterSync,
    }

    impl ProcessCrashPoint {
        const ALL: [Self; 6] = [
            Self::BeforeWrite,
            Self::AfterLengthHeader,
            Self::MiddleFrame,
            Self::BeforeCommit,
            Self::AfterFlush,
            Self::AfterSync,
        ];

        const fn as_str(self) -> &'static str {
            match self {
                Self::BeforeWrite => "before-write",
                Self::AfterLengthHeader => "after-length-header",
                Self::MiddleFrame => "middle-frame",
                Self::BeforeCommit => "before-commit",
                Self::AfterFlush => "after-flush",
                Self::AfterSync => "after-sync",
            }
        }

        fn parse(value: &str) -> Result<Self, &'static str> {
            match value {
                "before-write" => Ok(Self::BeforeWrite),
                "after-length-header" => Ok(Self::AfterLengthHeader),
                "middle-frame" => Ok(Self::MiddleFrame),
                "before-commit" => Ok(Self::BeforeCommit),
                "after-flush" => Ok(Self::AfterFlush),
                "after-sync" => Ok(Self::AfterSync),
                _ => Err("unknown Team crash point"),
            }
        }

        const fn writes_complete_frame(self) -> bool {
            matches!(self, Self::AfterFlush | Self::AfterSync)
        }

        const fn writes_any_bytes(self) -> bool {
            !matches!(self, Self::BeforeWrite)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CrashRecovery {
        KnownNotRepeated,
        AmbiguousBlocked,
    }

    fn root_command() -> TeamCommand {
        TeamCommand::AdmitRoot {
            task: TaskSpec::new(
                "coordinate crash recovery",
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

    fn root_session(commit: TeamCommit) -> AgentSession {
        match commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root admission outcome: {other:?}"),
        }
    }

    fn admit_root(team: &mut DurableTeamRuntime) -> AgentSession {
        root_session(
            team.dispatch(root_command())
                .expect("persist crash-test root"),
        )
    }

    fn delegate_command(parent: AgentSession) -> TeamCommand {
        TeamCommand::Delegate {
            parent,
            task: TaskSpec::new(
                "survive a storage crash",
                TaskScope::from_labels(["src", "tests"]),
            ),
            budget: ResourceBudget::new(200, 2),
            capabilities: CapabilitySnapshot::from_capabilities([
                Capability::WorkspaceRead,
                Capability::Process,
            ]),
        }
    }

    fn rebound_session(team: &DurableTeamRuntime, agent: AgentId) -> AgentSession {
        team.trusted_rebind_nonterminal_sessions()
            .into_iter()
            .find(|session| session.agent() == agent)
            .expect("rebind persisted non-terminal owner")
    }

    fn assert_two_transaction_recovery(team: &DurableTeamRuntime) {
        assert_eq!(team.ledger_head().transaction, 2);
        let snapshot = team.snapshot();
        assert_eq!(snapshot.tasks.len(), 2);
        assert_eq!(snapshot.agents.len(), 2);
        assert_ne!(snapshot.tasks[0].id, snapshot.tasks[1].id);
        assert_ne!(snapshot.agents[0].id, snapshot.agents[1].id);

        let events = team.event_log();
        assert!(events.iter().any(|event| event.transaction.get() == 1));
        assert!(events.iter().any(|event| event.transaction.get() == 2));
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.sequence.get(), index as u64 + 1);
        }
    }

    fn injected_io_error() -> io::Error {
        io::Error::other("injected Team Ledger write fault")
    }

    fn write_prefix(file: &mut File, frame: &[u8], bytes: usize) -> io::Result<()> {
        file.seek(SeekFrom::End(0))?;
        file.write_all(
            frame
                .get(..bytes)
                .ok_or_else(|| io::Error::other("invalid injected write offset"))?,
        )?;
        file.flush()
    }

    fn inject_write_fault(
        point: InjectedWriteFault,
        file: &mut File,
        frame: &[u8],
    ) -> io::Result<()> {
        match point {
            InjectedWriteFault::BeforeWrite => {
                file.seek(SeekFrom::End(0))?;
            }
            InjectedWriteFault::AfterByteOne => write_prefix(file, frame, 1)?,
            InjectedWriteFault::AfterLengthHeader => write_prefix(file, frame, 12)?,
            InjectedWriteFault::MiddleFrame => write_prefix(file, frame, frame.len() / 2)?,
            InjectedWriteFault::BeforeCommit => {
                write_prefix(file, frame, frame.len().saturating_sub(1))?;
            }
            InjectedWriteFault::AfterFullWrite => {
                file.seek(SeekFrom::End(0))?;
                file.write_all(frame)?;
            }
            InjectedWriteFault::AfterFlush => {
                file.seek(SeekFrom::End(0))?;
                file.write_all(frame)?;
                file.flush()?;
            }
            InjectedWriteFault::AfterSync => {
                file.seek(SeekFrom::End(0))?;
                file.write_all(frame)?;
                file.flush()?;
                file.sync_data()?;
            }
        }
        Err(injected_io_error())
    }

    fn temp_ledger_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "greentyper-team-fault-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    struct FaultLedgerFile {
        path: Option<PathBuf>,
    }

    impl FaultLedgerFile {
        fn create(name: &str) -> Self {
            Self {
                path: Some(temp_ledger_path(name)),
            }
        }

        fn path(&self) -> &Path {
            self.path.as_deref().expect("fault Ledger path exists")
        }

        fn cleanup(mut self) -> io::Result<()> {
            let path = self.path.take().expect("fault Ledger path exists");
            fs::remove_file(path)
        }
    }

    impl Drop for FaultLedgerFile {
        fn drop(&mut self) {
            if let Some(path) = self.path.take() {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn create_private_file(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.mode(0o600);
        }
        options.open(path)
    }

    fn create_private_run_dir(point: ProcessCrashPoint) -> io::Result<PathBuf> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "greentyper-team-crash-{}-{}-{nonce}-{}",
            point.as_str(),
            std::process::id(),
            NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        path.canonicalize()
    }

    struct CrashRunDirectory {
        path: Option<PathBuf>,
    }

    impl CrashRunDirectory {
        fn create(point: ProcessCrashPoint) -> io::Result<Self> {
            Ok(Self {
                path: Some(create_private_run_dir(point)?),
            })
        }

        fn path(&self) -> &Path {
            self.path.as_deref().expect("crash run directory exists")
        }

        fn cleanup(mut self) -> io::Result<()> {
            let path = self.path.take().expect("crash run directory exists");
            fs::remove_dir_all(path)
        }
    }

    impl Drop for CrashRunDirectory {
        fn drop(&mut self) {
            if let Some(path) = self.path.take() {
                let _ = fs::remove_dir_all(path);
            }
        }
    }

    fn supervisor_token(run_dir: &Path, point: ProcessCrashPoint) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"greentyper-team-crash-v1");
        hasher.update(std::process::id().to_le_bytes());
        hasher.update(NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        hasher.update(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time after epoch")
                .as_nanos()
                .to_le_bytes(),
        );
        hasher.update(run_dir.as_os_str().to_string_lossy().as_bytes());
        hasher.update(point.as_str().as_bytes());
        let mut token = String::with_capacity(64);
        for byte in hasher.finalize() {
            std::fmt::Write::write_fmt(&mut token, format_args!("{byte:02x}"))
                .expect("writing to a String cannot fail");
        }
        token
    }

    fn write_supervisor(run_dir: &Path, token: &str) -> io::Result<()> {
        let mut file = create_private_file(&run_dir.join(SUPERVISOR_FILE))?;
        file.write_all(token.as_bytes())?;
        file.flush()?;
        file.sync_all()
    }

    fn valid_token(token: &str) -> bool {
        token.len() == 64
            && token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn validate_child_directory(run_dir: &Path) -> io::Result<String> {
        let metadata = fs::symlink_metadata(run_dir)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::other("Team crash run directory is not real"));
        }
        if run_dir.canonicalize()? != run_dir {
            return Err(io::Error::other(
                "Team crash run directory is not canonical",
            ));
        }
        let temp_root = env::temp_dir().canonicalize()?;
        if run_dir.parent() != Some(temp_root.as_path()) {
            return Err(io::Error::other(
                "Team crash run directory is outside the temp namespace",
            ));
        }
        let name = run_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("Team crash run directory name is invalid"))?;
        if !name.starts_with("greentyper-team-crash-") {
            return Err(io::Error::other(
                "Team crash run directory is outside the benchmark namespace",
            ));
        }
        let mut entries = fs::read_dir(run_dir)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<OsString>, _>>()?;
        entries.sort();
        if entries != [OsString::from(SUPERVISOR_FILE)] {
            return Err(io::Error::other("Team crash run directory is not fresh"));
        }
        let supervisor_path = run_dir.join(SUPERVISOR_FILE);
        let supervisor_metadata = fs::symlink_metadata(&supervisor_path)?;
        if supervisor_metadata.file_type().is_symlink() || !supervisor_metadata.is_file() {
            return Err(io::Error::other("Team crash supervisor is not a file"));
        }
        let token = fs::read_to_string(supervisor_path)?;
        if !valid_token(&token) {
            return Err(io::Error::other("Team crash supervisor token is invalid"));
        }
        Ok(token)
    }

    #[cfg(unix)]
    fn sync_directory(path: &Path) -> io::Result<()> {
        File::open(path)?.sync_all()
    }

    #[cfg(not(unix))]
    fn sync_directory(_path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn ready_contents(token: &str, point: ProcessCrashPoint, pid: u32) -> String {
        format!(
            "greentyper-team-crash-v1\n{token}\n{pid}\n{}\n",
            point.as_str()
        )
    }

    fn signal_ready_and_wait(
        run_dir: &Path,
        token: &str,
        point: ProcessCrashPoint,
    ) -> io::Result<()> {
        let pending_path = run_dir.join(READY_PENDING_FILE);
        let ready_path = run_dir.join(READY_FILE);
        let mut marker = create_private_file(&pending_path)?;
        marker.write_all(ready_contents(token, point, std::process::id()).as_bytes())?;
        marker.flush()?;
        marker.sync_all()?;
        drop(marker);
        fs::rename(&pending_path, &ready_path)?;
        sync_directory(run_dir)?;

        let deadline = Instant::now() + CHILD_TIMEOUT;
        while Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Team crash child was not terminated by its supervisor",
        ))
    }

    fn crash_write_and_wait(
        run_dir: &Path,
        token: &str,
        point: ProcessCrashPoint,
        file: &mut File,
        frame: &[u8],
    ) -> io::Result<()> {
        file.seek(SeekFrom::End(0))?;
        match point {
            ProcessCrashPoint::BeforeWrite => {}
            ProcessCrashPoint::AfterLengthHeader => {
                file.write_all(&frame[..12])?;
                file.flush()?;
            }
            ProcessCrashPoint::MiddleFrame => {
                file.write_all(&frame[..frame.len() / 2])?;
                file.flush()?;
            }
            ProcessCrashPoint::BeforeCommit => {
                file.write_all(&frame[..frame.len().saturating_sub(1)])?;
                file.flush()?;
            }
            ProcessCrashPoint::AfterFlush => {
                file.write_all(frame)?;
                file.flush()?;
            }
            ProcessCrashPoint::AfterSync => {
                file.write_all(frame)?;
                file.flush()?;
                file.sync_data()?;
            }
        }
        signal_ready_and_wait(run_dir, token, point)
    }

    fn validate_ready_marker(
        run_dir: &Path,
        token: &str,
        point: ProcessCrashPoint,
        pid: u32,
    ) -> io::Result<()> {
        let path = run_dir.join(READY_FILE);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_READY_BYTES
        {
            return Err(io::Error::other("Team crash ready marker is invalid"));
        }
        let actual = fs::read_to_string(path)?;
        let expected = ready_contents(token, point, pid);
        if actual != expected {
            return Err(io::Error::other(
                "Team crash ready marker did not authenticate",
            ));
        }
        Ok(())
    }

    struct CrashChildGuard {
        child: Option<Child>,
    }

    impl CrashChildGuard {
        fn new(child: Child) -> Self {
            Self { child: Some(child) }
        }

        fn id(&self) -> u32 {
            self.child.as_ref().expect("child is present").id()
        }

        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.child.as_mut().expect("child is present").try_wait()
        }

        fn terminate_and_wait(&mut self) -> io::Result<ExitStatus> {
            let mut child = self.child.take().expect("child is present");
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if let Err(kill_error) = child.kill() {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                return Err(kill_error);
            }
            child.wait()
        }
    }

    impl Drop for CrashChildGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn spawn_and_kill_child(
        run_dir: &Path,
        token: &str,
        point: ProcessCrashPoint,
    ) -> io::Result<()> {
        let temp_root = run_dir
            .parent()
            .ok_or_else(|| io::Error::other("Team crash run directory has no temp root"))?;
        let mut command = Command::new(env::current_exe()?);
        // Keep the child environment secret-free while preserving the exact
        // platform temp namespace validated before it writes the test Ledger.
        command
            .arg("--exact")
            .arg(CRASH_CHILD_TEST)
            .arg("--test-threads=1")
            .env_clear()
            .env("TMPDIR", temp_root)
            .env("TMP", temp_root)
            .env("TEMP", temp_root)
            .env(CRASH_CHILD_ENV, run_dir)
            .env(CRASH_CASE_ENV, point.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        let child = command.spawn()?;
        let mut child = CrashChildGuard::new(child);
        let pid = child.id();
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            match fs::symlink_metadata(run_dir.join(READY_FILE)) {
                Ok(_) => {
                    validate_ready_marker(run_dir, token, point, pid)?;
                    let status = child.terminate_and_wait()?;
                    if status.success() {
                        return Err(io::Error::other(
                            "Team crash child exited successfully before termination",
                        ));
                    }
                    return Ok(());
                }
                Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                Err(source) => return Err(source),
            }
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(format!(
                    "Team crash child exited before readiness: {status}"
                )));
            }
            if Instant::now() >= deadline {
                let _ = child.terminate_and_wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Team crash child readiness timed out",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn event_kinds() -> Vec<TeamEventKind> {
        let task = TaskId(2);
        let agent = AgentId(3);
        let other_agent = AgentId(4);
        let blocked_by = TaskId(1);
        let spec = TaskSpec {
            title: "task".into(),
            scope: TaskScope::from_labels(["repo", "src"]),
            dependencies: vec![blocked_by],
        };
        let capabilities = CapabilitySnapshot::from_capabilities([
            Capability::WorkspaceRead,
            Capability::Tool("cargo".into()),
        ]);
        let mut capsule = CompletionCapsule::new("done");
        capsule.evidence.push("commit".into());
        capsule.tests.push("unit".into());

        vec![
            TeamEventKind::TaskCreated { task, spec },
            TeamEventKind::AgentCreated {
                agent,
                task,
                parent: Some(other_agent),
                budget: ResourceBudget::new(100, 2),
                capabilities,
            },
            TeamEventKind::TaskOwnerAssigned { task, agent },
            TeamEventKind::DelegationGranted {
                parent: other_agent,
                child: agent,
            },
            TeamEventKind::TaskReady { task },
            TeamEventKind::AgentActivated { agent },
            TeamEventKind::TaskStarted { task },
            TeamEventKind::MessageSent {
                message: MessageId(5),
                from: agent,
                recipient: MessageRecipient::Team,
                body: "status".into(),
            },
            TeamEventKind::CompletionCapsuleSubmitted {
                task,
                agent,
                capsule,
            },
            TeamEventKind::TaskSucceeded { task },
            TeamEventKind::AgentSucceeded { agent },
            TeamEventKind::TaskFailed {
                task,
                reason: "failed".into(),
            },
            TeamEventKind::AgentFailed { agent },
            TeamEventKind::TaskCancelled {
                task,
                reason: "cancelled".into(),
            },
            TeamEventKind::AgentCancelled { agent },
            TeamEventKind::TaskBlocked { task, blocked_by },
            TeamEventKind::AgentBlocked { agent, blocked_by },
            TeamEventKind::OperationCommitted {
                operation: TeamOperationId(1),
                transaction: TransactionId(6),
            },
            TeamEventKind::OperationAcknowledged {
                operation: TeamOperationId(1),
                committed_transaction: TransactionId(6),
                acknowledgement_transaction: TransactionId(7),
            },
        ]
    }

    #[test]
    fn codec_round_trips_every_team_event_kind() {
        for kind in event_kinds() {
            let encoded = encode_event_data(&kind).expect("encode Team Event");
            assert_eq!(
                decode_event_data(&encoded).expect("decode Team Event"),
                kind
            );
        }
    }

    #[test]
    fn set_encoding_is_canonical() {
        let left = TeamEventKind::AgentCreated {
            agent: AgentId(1),
            task: TaskId(1),
            parent: None,
            budget: ResourceBudget::new(1, 1),
            capabilities: CapabilitySnapshot::from_capabilities([
                Capability::Tool("cargo".into()),
                Capability::WorkspaceRead,
            ]),
        };
        let right = TeamEventKind::AgentCreated {
            agent: AgentId(1),
            task: TaskId(1),
            parent: None,
            budget: ResourceBudget::new(1, 1),
            capabilities: CapabilitySnapshot::from_capabilities([
                Capability::WorkspaceRead,
                Capability::Tool("cargo".into()),
            ]),
        };
        assert_eq!(
            encode_event_data(&left).expect("encode left"),
            encode_event_data(&right).expect("encode right")
        );
    }

    #[test]
    fn codec_rejects_schema_kind_identifier_and_trailing_tampering() {
        let valid =
            encode_event_data(&TeamEventKind::TaskReady { task: TaskId(1) }).expect("encode event");
        let mut unsupported = valid.clone();
        unsupported.schema += 1;
        assert!(matches!(
            decode_event_data(&unsupported),
            Err(DurableTeamError::UnsupportedTeamEventSchema { .. })
        ));

        let mut historical = valid.clone();
        historical.schema = TEAM_EVENT_SCHEMA_V1;
        assert_eq!(
            decode_event_data(&historical).expect("decode historical Team Event"),
            TeamEventKind::TaskReady { task: TaskId(1) }
        );
        historical.payload = [1_u64.to_le_bytes(), 1_u64.to_le_bytes()].concat();
        for kind in [18, 19] {
            historical.kind = kind;
            assert!(matches!(
                decode_event_data(&historical),
                Err(DurableTeamError::CorruptEvent(
                    "Team Event kind is unavailable in schema one"
                ))
            ));
        }

        let mut unknown = valid.clone();
        unknown.kind = 99;
        assert!(matches!(
            decode_event_data(&unknown),
            Err(DurableTeamError::CorruptEvent("unknown Team Event kind"))
        ));

        let mut zero_identifier = valid.clone();
        zero_identifier.payload = 0_u64.to_le_bytes().to_vec();
        assert!(matches!(
            decode_event_data(&zero_identifier),
            Err(DurableTeamError::CorruptEvent(
                "invalid Team Event identifier"
            ))
        ));

        let mut trailing = valid;
        trailing.payload.push(0);
        assert!(matches!(
            decode_event_data(&trailing),
            Err(DurableTeamError::CorruptEvent(
                "Team Event has trailing bytes"
            ))
        ));
    }

    #[test]
    fn schema_one_team_transactions_remain_replayable() {
        let ledger_file = FaultLedgerFile::create("schema-one-replay");
        let mut volatile = TeamRuntime::new(1).expect("create volatile Team");
        volatile
            .dispatch(root_command())
            .expect("build historical root transaction");
        let expected = volatile.snapshot();
        let mut encoded = volatile
            .event_log()
            .iter()
            .map(|event| encode_event_data(&event.kind))
            .collect::<Result<Vec<_>, _>>()
            .expect("encode historical Team transaction");
        for event in &mut encoded {
            event.schema = TEAM_EVENT_SCHEMA_V1;
        }
        let (mut ledger, _) = FileLedger::open(ledger_file.path()).expect("create Team Ledger");
        ledger
            .append(LedgerHead::default(), &encoded)
            .expect("append schema-one Team transaction");
        drop(ledger);

        let recovered = DurableTeamRuntime::open(ledger_file.path(), 1)
            .expect("replay schema-one Team transaction");
        assert_eq!(recovered.snapshot(), expected);
        assert!(recovered.operation_records().is_empty());
        drop(recovered);
        ledger_file
            .cleanup()
            .expect("cleanup schema-one Team Ledger");
    }

    #[test]
    fn operation_journal_replay_rejects_mismatched_and_duplicate_markers() {
        let mismatched = FaultLedgerFile::create("operation-mismatched-transaction");
        let (mut ledger, _) = FileLedger::open(mismatched.path()).expect("create raw Team Ledger");
        ledger
            .append(
                LedgerHead::default(),
                &[encode_event_data(&TeamEventKind::OperationCommitted {
                    operation: TeamOperationId(1),
                    transaction: TransactionId(2),
                })
                .expect("encode mismatched operation marker")],
            )
            .expect("append mismatched operation marker");
        drop(ledger);
        assert!(matches!(
            DurableTeamRuntime::open(mismatched.path(), 1),
            Err(DurableTeamError::Recovery(
                RecoveryError::InvalidEvent { .. }
            ))
        ));
        mismatched
            .cleanup()
            .expect("cleanup mismatched operation Ledger");

        let duplicate = FaultLedgerFile::create("operation-duplicate-acknowledgement");
        let operation = TeamOperationId(1);
        let mut team =
            DurableTeamRuntime::open(duplicate.path(), 1).expect("create operation Team Ledger");
        team.dispatch_operation(operation, root_command())
            .expect("commit operation");
        team.acknowledge_operation(operation)
            .expect("acknowledge operation");
        let head = team.ledger_head();
        drop(team);

        let (mut ledger, _) = FileLedger::open(duplicate.path()).expect("open raw Team Ledger");
        ledger
            .append(
                head,
                &[encode_event_data(&TeamEventKind::OperationAcknowledged {
                    operation,
                    committed_transaction: TransactionId(1),
                    acknowledgement_transaction: TransactionId(3),
                })
                .expect("encode duplicate acknowledgement")],
            )
            .expect("append duplicate acknowledgement");
        drop(ledger);
        assert!(matches!(
            DurableTeamRuntime::open(duplicate.path(), 1),
            Err(DurableTeamError::Recovery(
                RecoveryError::InvalidEvent { .. }
            ))
        ));
        duplicate
            .cleanup()
            .expect("cleanup duplicate acknowledgement Ledger");
    }

    #[test]
    fn write_faults_poison_the_writer_and_require_reopen_reconciliation() {
        for point in InjectedWriteFault::ALL {
            let ledger = FaultLedgerFile::create(&format!("{point:?}"));
            let mut team =
                DurableTeamRuntime::open(ledger.path(), 1).expect("create fault Team Ledger");
            let root = admit_root(&mut team);
            let before = team.snapshot();
            let before_events = team.event_log().to_vec();
            let before_head = team.ledger_head();

            assert!(matches!(
                team.dispatch_with_test_io(delegate_command(root), |file, frame| {
                    inject_write_fault(point, file, frame)
                }),
                Err(DurableTeamError::Ledger(LedgerError::DurabilityAmbiguous(
                    _
                )))
            ));
            assert_eq!(team.snapshot(), before);
            assert_eq!(team.event_log(), before_events);
            assert_eq!(team.ledger_head(), before_head);
            assert!(matches!(
                team.dispatch(delegate_command(root)),
                Err(DurableTeamError::Ledger(LedgerError::WriterPoisoned))
            ));
            drop(team);

            let mut recovered = DurableTeamRuntime::open(ledger.path(), 1)
                .expect("reopen after ambiguous durability");
            let recovery = if point.writes_complete_frame() {
                assert_two_transaction_recovery(&recovered);
                assert_eq!(recovered.recovered_tail_bytes(), 0);
                CrashRecovery::AmbiguousBlocked
            } else {
                assert_eq!(recovered.ledger_head(), before_head);
                assert_eq!(recovered.snapshot(), before);
                assert_eq!(recovered.event_log(), before_events);
                if point.writes_any_bytes() {
                    assert!(recovered.recovered_tail_bytes() > 0);
                } else {
                    assert_eq!(recovered.recovered_tail_bytes(), 0);
                }
                CrashRecovery::KnownNotRepeated
            };

            match recovery {
                CrashRecovery::KnownNotRepeated => {
                    let recovered_root = rebound_session(&recovered, root.agent());
                    recovered
                        .dispatch(delegate_command(recovered_root))
                        .expect("explicit retry after known-not-repeated recovery");
                }
                CrashRecovery::AmbiguousBlocked => {
                    assert_two_transaction_recovery(&recovered);
                }
            }
            assert_two_transaction_recovery(&recovered);
            let expected_snapshot = recovered.snapshot();
            let expected_events = recovered.event_log().to_vec();
            let expected_head = recovered.ledger_head();
            drop(recovered);

            let replayed = DurableTeamRuntime::open(ledger.path(), 1)
                .expect("replay reconciled Team Ledger a second time");
            assert_eq!(replayed.snapshot(), expected_snapshot);
            assert_eq!(replayed.event_log(), expected_events);
            assert_eq!(replayed.ledger_head(), expected_head);
            assert_eq!(replayed.recovered_tail_bytes(), 0);
            assert_two_transaction_recovery(&replayed);
            drop(replayed);
            ledger.cleanup().expect("cleanup fault Team Ledger");
        }
    }

    #[test]
    fn operation_commit_faults_recover_without_automatic_repetition() {
        for point in InjectedWriteFault::ALL {
            let ledger = FaultLedgerFile::create(&format!("operation-{point:?}"));
            let operation = TeamOperationId(1);
            let mut team =
                DurableTeamRuntime::open(ledger.path(), 1).expect("create operation Team Ledger");
            let before = team.snapshot();

            assert!(matches!(
                team.dispatch_operation_with_test_io(operation, root_command(), |file, frame| {
                    inject_write_fault(point, file, frame)
                },),
                Err(DurableTeamError::Ledger(LedgerError::DurabilityAmbiguous(
                    _
                )))
            ));
            assert_eq!(team.snapshot(), before);
            assert!(team.operation_records().is_empty());
            assert_eq!(team.ledger_head(), LedgerHead::default());
            assert!(matches!(
                team.dispatch_operation(operation, root_command()),
                Err(DurableTeamError::Ledger(LedgerError::WriterPoisoned))
            ));
            drop(team);

            let mut recovered =
                DurableTeamRuntime::open(ledger.path(), 1).expect("reopen operation Team Ledger");
            if point.writes_complete_frame() {
                assert_eq!(recovered.snapshot().agents.len(), 1);
                assert_eq!(recovered.operation_records().len(), 1);
                assert_eq!(
                    recovered.operation_records()[0].status,
                    TeamOperationStatus::CommittedAwaitingAcknowledgement
                );
            } else {
                assert_eq!(recovered.snapshot(), before);
                assert!(recovered.operation_records().is_empty());
                assert_eq!(
                    recovered.next_operation_id().expect("next operation"),
                    operation
                );
                recovered
                    .dispatch_operation(operation, root_command())
                    .expect("explicit retry after known-not-repeated operation");
            }
            assert!(matches!(
                recovered
                    .acknowledge_operation(operation)
                    .expect("acknowledge reconciled operation"),
                TeamOperationAcknowledgeOutcome::Durable(_)
            ));
            assert_eq!(
                recovered.operation_records()[0].status,
                TeamOperationStatus::Acknowledged
            );
            drop(recovered);

            let replayed =
                DurableTeamRuntime::open(ledger.path(), 1).expect("replay reconciled operation");
            assert_eq!(replayed.operation_records().len(), 1);
            assert_eq!(
                replayed.operation_records()[0].status,
                TeamOperationStatus::Acknowledged
            );
            drop(replayed);
            ledger.cleanup().expect("cleanup operation Team Ledger");
        }
    }

    #[test]
    fn operation_acknowledgement_faults_remain_explicit_after_reopen() {
        for point in InjectedWriteFault::ALL {
            let ledger = FaultLedgerFile::create(&format!("acknowledgement-{point:?}"));
            let operation = TeamOperationId(1);
            let mut team = DurableTeamRuntime::open(ledger.path(), 1)
                .expect("create acknowledgement Team Ledger");
            team.dispatch_operation(operation, root_command())
                .expect("commit operation before acknowledgement fault");
            let before = team.snapshot();
            let before_head = team.ledger_head();

            assert!(matches!(
                team.acknowledge_operation_with_test_io(operation, |file, frame| {
                    inject_write_fault(point, file, frame)
                }),
                Err(DurableTeamError::Ledger(LedgerError::DurabilityAmbiguous(
                    _
                )))
            ));
            assert_eq!(team.snapshot(), before);
            assert_eq!(team.ledger_head(), before_head);
            assert_eq!(
                team.operation_records()[0].status,
                TeamOperationStatus::CommittedAwaitingAcknowledgement
            );
            assert!(matches!(
                team.acknowledge_operation(operation),
                Err(DurableTeamError::Ledger(LedgerError::WriterPoisoned))
            ));
            drop(team);

            let mut recovered = DurableTeamRuntime::open(ledger.path(), 1)
                .expect("reopen acknowledgement Team Ledger");
            if point.writes_complete_frame() {
                assert_eq!(
                    recovered.operation_records()[0].status,
                    TeamOperationStatus::Acknowledged
                );
                assert_eq!(
                    recovered
                        .acknowledge_operation(operation)
                        .expect("duplicate recovered acknowledgement"),
                    TeamOperationAcknowledgeOutcome::AlreadyAcknowledged
                );
            } else {
                assert_eq!(
                    recovered.operation_records()[0].status,
                    TeamOperationStatus::CommittedAwaitingAcknowledgement
                );
                assert!(matches!(
                    recovered
                        .acknowledge_operation(operation)
                        .expect("retry known-not-repeated acknowledgement"),
                    TeamOperationAcknowledgeOutcome::Durable(_)
                ));
            }
            drop(recovered);

            let replayed =
                DurableTeamRuntime::open(ledger.path(), 1).expect("replay acknowledged operation");
            assert_eq!(
                replayed.operation_records()[0].status,
                TeamOperationStatus::Acknowledged
            );
            drop(replayed);
            ledger
                .cleanup()
                .expect("cleanup acknowledgement Team Ledger");
        }
    }

    #[test]
    fn team_crash_child_entrypoint() {
        let Some(run_dir) = env::var_os(CRASH_CHILD_ENV) else {
            return;
        };
        let run_dir = PathBuf::from(run_dir);
        let point = ProcessCrashPoint::parse(
            &env::var(CRASH_CASE_ENV).expect("Team crash child case is present"),
        )
        .expect("Team crash child case is supported");
        let token = validate_child_directory(&run_dir).expect("validate Team crash directory");
        let ledger_path = run_dir.join(TEAM_LEDGER_FILE);
        let mut team =
            DurableTeamRuntime::open(&ledger_path, 1).expect("open Team crash child Ledger");
        let root = admit_root(&mut team);
        let signal_dir = run_dir.clone();
        let signal_token = token.clone();
        let result = team.dispatch_with_test_io(delegate_command(root), move |file, frame| {
            crash_write_and_wait(&signal_dir, &signal_token, point, file, frame)
        });
        panic!("Team crash child escaped supervisor termination: {result:?}");
    }

    #[test]
    fn process_termination_recovers_known_not_repeated_or_ambiguous_blocked() {
        for point in ProcessCrashPoint::ALL {
            let run =
                CrashRunDirectory::create(point).expect("create private Team crash run directory");
            let run_dir = run.path();
            let token = supervisor_token(run_dir, point);
            assert!(valid_token(&token));
            write_supervisor(run_dir, &token).expect("write Team crash supervisor");

            spawn_and_kill_child(run_dir, &token, point)
                .expect("terminate Team child at authenticated crash boundary");
            fs::remove_file(run_dir.join(SUPERVISOR_FILE))
                .expect("remove Team crash supervisor after child exit");
            fs::remove_file(run_dir.join(READY_FILE))
                .expect("remove authenticated Team crash marker");
            sync_directory(run_dir).expect("sync Team crash marker cleanup");

            let ledger_path = run_dir.join(TEAM_LEDGER_FILE);
            let mut recovered = DurableTeamRuntime::open(&ledger_path, 1)
                .expect("reopen Team Ledger after child termination");
            let recovery = if point.writes_complete_frame() {
                assert_two_transaction_recovery(&recovered);
                assert_eq!(recovered.recovered_tail_bytes(), 0);
                CrashRecovery::AmbiguousBlocked
            } else {
                assert_eq!(recovered.ledger_head().transaction, 1);
                assert_eq!(recovered.snapshot().tasks.len(), 1);
                assert_eq!(recovered.snapshot().agents.len(), 1);
                if point.writes_any_bytes() {
                    assert!(recovered.recovered_tail_bytes() > 0);
                } else {
                    assert_eq!(recovered.recovered_tail_bytes(), 0);
                }
                CrashRecovery::KnownNotRepeated
            };

            match recovery {
                CrashRecovery::KnownNotRepeated => {
                    let root_agent = recovered.snapshot().agents[0].id;
                    let recovered_root = rebound_session(&recovered, root_agent);
                    recovered
                        .dispatch(delegate_command(recovered_root))
                        .expect("operator may explicitly retry a known-not-repeated command");
                }
                CrashRecovery::AmbiguousBlocked => {
                    assert_two_transaction_recovery(&recovered);
                }
            }
            assert_two_transaction_recovery(&recovered);
            let expected_snapshot = recovered.snapshot();
            let expected_events = recovered.event_log().to_vec();
            let expected_head = recovered.ledger_head();
            drop(recovered);

            let replayed = DurableTeamRuntime::open(&ledger_path, 1)
                .expect("replay reconciled child-terminated Team Ledger");
            assert_eq!(replayed.snapshot(), expected_snapshot);
            assert_eq!(replayed.event_log(), expected_events);
            assert_eq!(replayed.ledger_head(), expected_head);
            assert_eq!(replayed.recovered_tail_bytes(), 0);
            assert_two_transaction_recovery(&replayed);
            drop(replayed);
            run.cleanup().expect("cleanup completed Team crash run");
        }
    }
}
