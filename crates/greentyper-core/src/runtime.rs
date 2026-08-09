//! Single-Agent Runtime Kernel with durable admission, output preparation, and acknowledgement.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::Path;

use crate::config::{ConfigEpoch, ConfigError, ConfigLayer, ConfigLayers, ConfigSource};
use crate::ledger::{
    DurabilityReceipt, EventData, FileLedger, LedgerError, LedgerHead, StoredEvent,
};
use crate::model::{
    CanonicalItem, ConfigEpochId, DeliveryId, ItemId, ItemRole, ModelError, ProviderEpochId,
    ThreadId, TurnId,
};
use crate::provider::{
    ProviderEpoch, ProviderError, ProviderEvent, ProviderRequest, ProviderRuntime, UsageRecord,
};
use crate::schema::SchemaKind;

pub const RUNTIME_EVENT_SCHEMA: u16 = SchemaKind::RuntimeEvent.current().get();
pub const MAX_INPUT_BYTES: usize = 512 * 1024;
const MAX_BLOCK_REASON_BYTES: usize = 4096;

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
pub struct PreparedOutput {
    delivery: DeliveryId,
    turn: TurnId,
    text: String,
    usage: UsageRecord,
    receipt: DurabilityReceipt,
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
    pub const fn usage(&self) -> UsageRecord {
        self.usage
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

pub struct RuntimeKernel {
    ledger: FileLedger,
    state: RuntimeState,
    recovered_tail_bytes: u64,
}

impl RuntimeKernel {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let (ledger, report) = FileLedger::open(path).map_err(RuntimeError::Ledger)?;
        let state = replay_runtime(&report.events)?;
        Ok(Self {
            ledger,
            state,
            recovered_tail_bytes: report.truncated_tail_bytes,
        })
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

    pub fn execute(
        &mut self,
        layers: &ConfigLayers,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
        self.require_ready()?;
        let input = input.into();
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
        self.drive_pending(provider)
    }

    pub fn resume(
        &mut self,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
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

    fn drive_pending(
        &mut self,
        provider: &mut impl ProviderRuntime,
    ) -> Result<PreparedOutput, RuntimeError> {
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
        let provider_events = match provider.run(&request) {
            Ok(events) => events,
            Err(source) => {
                self.block_pending(pending.turn, &source.to_string())?;
                return Err(RuntimeError::Provider(source));
            }
        };
        let (deltas, text, usage) = match validate_provider_events(
            &provider_events,
            *config.resolved().max_output_bytes().value() as usize,
        ) {
            Ok(output) => output,
            Err(reason) => {
                self.block_pending(pending.turn, reason)?;
                return Err(RuntimeError::InvalidProviderOutput(reason));
            }
        };

        let assistant_item = ItemId::new(self.state.next_item).map_err(RuntimeError::Model)?;
        let delivery = DeliveryId::new(self.state.next_delivery).map_err(RuntimeError::Model)?;
        let mut events = Vec::with_capacity(deltas.len() + 2);
        events.push(RuntimeEvent::AssistantItemStarted {
            turn: pending.turn,
            item: assistant_item,
        });
        for delta in deltas {
            events.push(RuntimeEvent::AssistantTextDelta {
                turn: pending.turn,
                item: assistant_item,
                delta,
            });
        }
        events.push(RuntimeEvent::OutputPrepared {
            turn: pending.turn,
            item: assistant_item,
            delivery,
            text: text.clone(),
            usage,
        });
        let receipt = self.commit(&events)?;
        Ok(PreparedOutput {
            delivery,
            turn: pending.turn,
            text,
            usage,
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

fn validate_input(input: &str) -> Result<(), RuntimeError> {
    if input.trim().is_empty() {
        return Err(RuntimeError::InvalidInput("input cannot be empty"));
    }
    if input.len() > MAX_INPUT_BYTES {
        return Err(RuntimeError::InvalidInput("input is too large"));
    }
    Ok(())
}

fn validate_provider_events(
    events: &[ProviderEvent],
    max_output_bytes: usize,
) -> Result<(Vec<String>, String, UsageRecord), &'static str> {
    if events.is_empty() {
        return Err("provider emitted no events");
    }
    let mut deltas = Vec::new();
    let mut text = String::new();
    let mut usage = None;
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
            ProviderEvent::Completed(record) => {
                if usage.replace(*record).is_some() {
                    return Err("provider emitted completion more than once");
                }
                if index + 1 != events.len() {
                    return Err("provider completion must be the final event");
                }
            }
        }
    }
    let usage = usage.ok_or("provider did not emit completion")?;
    if text.trim().is_empty() {
        return Err("provider output cannot be empty");
    }
    Ok((deltas, text, usage))
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
        usage: UsageRecord,
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
                usage,
            } => {
                payload.u64(turn.get());
                payload.u64(item.get());
                payload.u64(delivery.get());
                payload.string(text)?;
                payload.u32(usage.input_tokens);
                payload.u32(usage.output_tokens);
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
        if event.data.schema != RUNTIME_EVENT_SCHEMA {
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
                usage: UsageRecord {
                    input_tokens: payload.u32()?,
                    output_tokens: payload.u32()?,
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
    usage: UsageRecord,
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
                usage,
            } => {
                if self.acknowledged.contains(&delivery) {
                    return Err(RuntimeError::CorruptState("duplicate Delivery id"));
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
                    usage,
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
    Config(ConfigError),
    Model(ModelError),
    Provider(ProviderError),
    Busy(RecoveryStatus),
    UnknownDelivery(DeliveryId),
    InvalidInput(&'static str),
    InvalidProviderOutput(&'static str),
    UnsupportedRuntimeEventSchema { supported: u16, actual: u16 },
    CorruptEvent(&'static str),
    CorruptState(&'static str),
    IntegerOverflow,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ledger(source) => write!(formatter, "{source}"),
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
                usage: UsageRecord::default(),
            }),
            Err(RuntimeError::CorruptState(
                "prepared output exceeds the frozen Config Epoch limit"
            ))
        ));
    }
}
