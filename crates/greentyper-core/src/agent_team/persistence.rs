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

    pub fn dispatch(&mut self, command: TeamCommand) -> Result<TeamCommit, DurableTeamError> {
        let prepared = self
            .runtime
            .prepare(command)
            .map_err(DurableTeamError::Team)?;
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
        let receipt = self
            .ledger
            .append(team_head, &encoded)
            .map_err(DurableTeamError::Ledger)?;
        let durability = CommitDurability::Synchronous(team_receipt(receipt));
        Ok(self.runtime.publish(prepared, durability))
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
    };

    Ok(EventData {
        schema: TEAM_EVENT_SCHEMA,
        kind: tag,
        payload: encoder.finish(),
    })
}

fn decode_event_data(data: &EventData) -> Result<TeamEventKind, DurableTeamError> {
    if data.schema != TEAM_EVENT_SCHEMA {
        return Err(DurableTeamError::UnsupportedTeamEventSchema {
            supported: TEAM_EVENT_SCHEMA,
            actual: data.schema,
        });
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
    use super::*;

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
}
