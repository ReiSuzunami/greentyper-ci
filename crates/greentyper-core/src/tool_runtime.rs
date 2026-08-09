//! Durable Tool call identity, approval, effect, and reconciliation policy.
//!
//! The module owns the ordering that callers must not reproduce: a narrow
//! Approval Grant and prepared-effect record are synchronously durable before
//! an executor is invoked. Successful and ambiguous effects are never invoked
//! again automatically after retry or recovery.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::agent_team::{
    AgentExecutionContext, AgentId, AgentSession, Capability, CapabilitySnapshot,
};
use crate::ledger::{
    DurabilityReceipt, EventData, FileLedger, LedgerError, LedgerHead, StoredEvent,
};
use crate::schema::SchemaKind;

const TOOL_EVENT_SCHEMA: u16 = SchemaKind::ToolEvent.current().get();
const MAX_CALL_IDENTITY_BYTES: usize = 256;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_RESOURCE_BYTES: usize = 1024;
const MAX_RESOURCES_PER_AXIS: usize = 64;
const MAX_REASON_BYTES: usize = 8 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const APPROVAL_DENIED_REASON: &str = "Tool approval denied";
const EFFECT_FAILED_REASON: &str = "Tool execution failed";
const EFFECT_AMBIGUOUS_REASON: &str = "Tool outcome is ambiguous";
const RECONCILED_FAILED_REASON: &str = "Tool effect reconciled as failed";

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolCallId(u64);

impl ToolCallId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    fn new(value: u64) -> Result<Self, ToolRuntimeError> {
        if value == 0 || value == u64::MAX {
            Err(ToolRuntimeError::IdentifierExhausted)
        } else {
            Ok(Self(value))
        }
    }

    fn from_stored(value: u64) -> Result<Self, ToolRuntimeError> {
        Self::new(value).map_err(|_| ToolRuntimeError::CorruptEvent("invalid Tool Call ID"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolArgumentsHash([u8; 32]);

impl ToolArgumentsHash {
    #[must_use]
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolArguments {
    canonical_json: String,
}

impl fmt::Debug for ToolArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolArguments")
            .field("bytes", &self.canonical_json.len())
            .field("hash", &self.hash())
            .finish()
    }
}

impl ToolArguments {
    pub fn parse(input: &str) -> Result<Self, ToolRuntimeError> {
        if input.len() > MAX_ARGUMENT_BYTES {
            return Err(ToolRuntimeError::InvalidArguments(
                "Tool arguments exceed the byte limit",
            ));
        }
        let value: Value = serde_json::from_str(input)
            .map_err(|_| ToolRuntimeError::InvalidArguments("Tool arguments are not JSON"))?;
        if !value.is_object() {
            return Err(ToolRuntimeError::InvalidArguments(
                "Tool arguments must be a JSON object",
            ));
        }
        let mut canonical_json = String::new();
        write_canonical_json(&value, &mut canonical_json)?;
        if canonical_json.len() > MAX_ARGUMENT_BYTES {
            return Err(ToolRuntimeError::InvalidArguments(
                "canonical Tool arguments exceed the byte limit",
            ));
        }
        Ok(Self { canonical_json })
    }

    #[must_use]
    pub fn canonical_json(&self) -> &str {
        &self.canonical_json
    }

    fn hash(&self) -> ToolArgumentsHash {
        ToolArgumentsHash(Sha256::digest(self.canonical_json.as_bytes()).into())
    }
}

fn write_canonical_json(value: &Value, output: &mut String) -> Result<(), ToolRuntimeError> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(&serde_json::to_string(value).map_err(|_| {
            ToolRuntimeError::InvalidArguments("Tool argument string cannot be encoded")
        })?),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).map_err(|_| {
                    ToolRuntimeError::InvalidArguments("Tool argument key cannot be encoded")
                })?);
                output.push(':');
                write_canonical_json(value, output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct ToolResources {
    filesystem_reads: BTreeSet<String>,
    filesystem_writes: BTreeSet<String>,
    process: Option<String>,
    network_targets: BTreeSet<String>,
}

impl fmt::Debug for ToolResources {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolResources")
            .field("binding", &self.binding())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolResourceBinding {
    fingerprint: [u8; 32],
    filesystem_read_count: u16,
    filesystem_write_count: u16,
    process: bool,
    network_target_count: u16,
}

impl ToolResourceBinding {
    #[must_use]
    pub const fn fingerprint(self) -> [u8; 32] {
        self.fingerprint
    }

    #[must_use]
    pub const fn filesystem_read_count(self) -> u16 {
        self.filesystem_read_count
    }

    #[must_use]
    pub const fn filesystem_write_count(self) -> u16 {
        self.filesystem_write_count
    }

    #[must_use]
    pub const fn requires_process(self) -> bool {
        self.process
    }

    #[must_use]
    pub const fn network_target_count(self) -> u16 {
        self.network_target_count
    }

    fn validate(self) -> Result<(), ToolRuntimeError> {
        if usize::from(self.filesystem_read_count) > MAX_RESOURCES_PER_AXIS
            || usize::from(self.filesystem_write_count) > MAX_RESOURCES_PER_AXIS
            || usize::from(self.network_target_count) > MAX_RESOURCES_PER_AXIS
        {
            return Err(ToolRuntimeError::CorruptEvent(
                "Tool resource binding exceeds its axis limit",
            ));
        }
        Ok(())
    }

    fn require_capabilities(
        self,
        capabilities: &CapabilitySnapshot,
    ) -> Result<(), ToolRuntimeError> {
        if self.filesystem_read_count > 0 && !capabilities.contains(&Capability::WorkspaceRead) {
            return Err(ToolRuntimeError::CapabilityDenied {
                capability: Capability::WorkspaceRead,
            });
        }
        if self.filesystem_write_count > 0 && !capabilities.contains(&Capability::WorkspaceWrite) {
            return Err(ToolRuntimeError::CapabilityDenied {
                capability: Capability::WorkspaceWrite,
            });
        }
        if self.process && !capabilities.contains(&Capability::Process) {
            return Err(ToolRuntimeError::CapabilityDenied {
                capability: Capability::Process,
            });
        }
        if self.network_target_count > 0 && !capabilities.contains(&Capability::Network) {
            return Err(ToolRuntimeError::CapabilityDenied {
                capability: Capability::Network,
            });
        }
        Ok(())
    }
}

impl ToolResources {
    #[must_use]
    pub fn with_filesystem_read(mut self, resource: impl Into<String>) -> Self {
        self.filesystem_reads.insert(resource.into());
        self
    }

    #[must_use]
    pub fn with_filesystem_write(mut self, resource: impl Into<String>) -> Self {
        self.filesystem_writes.insert(resource.into());
        self
    }

    #[must_use]
    pub fn with_process(mut self, executable: impl Into<String>) -> Self {
        self.process = Some(executable.into());
        self
    }

    #[must_use]
    pub fn with_network_target(mut self, target: impl Into<String>) -> Self {
        self.network_targets.insert(target.into());
        self
    }

    pub fn filesystem_reads(&self) -> impl Iterator<Item = &str> {
        self.filesystem_reads.iter().map(String::as_str)
    }

    pub fn filesystem_writes(&self) -> impl Iterator<Item = &str> {
        self.filesystem_writes.iter().map(String::as_str)
    }

    #[must_use]
    pub fn process(&self) -> Option<&str> {
        self.process.as_deref()
    }

    pub fn network_targets(&self) -> impl Iterator<Item = &str> {
        self.network_targets.iter().map(String::as_str)
    }

    fn validate(&self) -> Result<(), ToolRuntimeError> {
        validate_resource_axis(&self.filesystem_reads)?;
        validate_resource_axis(&self.filesystem_writes)?;
        validate_resource_axis(&self.network_targets)?;
        if let Some(process) = &self.process {
            validate_bounded_text(process, MAX_RESOURCE_BYTES, "process resource")?;
        }
        Ok(())
    }

    fn binding(&self) -> ToolResourceBinding {
        let mut hasher = Sha256::new();
        hasher.update(b"greentyper-tool-resource-binding-v1\0");
        hash_resource_axis(&mut hasher, 1, &self.filesystem_reads);
        hash_resource_axis(&mut hasher, 2, &self.filesystem_writes);
        hasher.update([3]);
        match &self.process {
            Some(process) => {
                hasher.update([1]);
                hash_resource_value(&mut hasher, process);
            }
            None => hasher.update([0]),
        }
        hash_resource_axis(&mut hasher, 4, &self.network_targets);
        ToolResourceBinding {
            fingerprint: hasher.finalize().into(),
            filesystem_read_count: self.filesystem_reads.len() as u16,
            filesystem_write_count: self.filesystem_writes.len() as u16,
            process: self.process.is_some(),
            network_target_count: self.network_targets.len() as u16,
        }
    }

    fn require_capabilities(
        &self,
        capabilities: &CapabilitySnapshot,
    ) -> Result<(), ToolRuntimeError> {
        if !self.filesystem_reads.is_empty() && !capabilities.contains(&Capability::WorkspaceRead) {
            return Err(ToolRuntimeError::CapabilityDenied {
                capability: Capability::WorkspaceRead,
            });
        }
        if !self.filesystem_writes.is_empty() && !capabilities.contains(&Capability::WorkspaceWrite)
        {
            return Err(ToolRuntimeError::CapabilityDenied {
                capability: Capability::WorkspaceWrite,
            });
        }
        if self.process.is_some() && !capabilities.contains(&Capability::Process) {
            return Err(ToolRuntimeError::CapabilityDenied {
                capability: Capability::Process,
            });
        }
        if !self.network_targets.is_empty() && !capabilities.contains(&Capability::Network) {
            return Err(ToolRuntimeError::CapabilityDenied {
                capability: Capability::Network,
            });
        }
        Ok(())
    }
}

fn hash_resource_axis(hasher: &mut Sha256, tag: u8, resources: &BTreeSet<String>) {
    hasher.update([tag]);
    hasher.update((resources.len() as u32).to_le_bytes());
    for resource in resources {
        hash_resource_value(hasher, resource);
    }
}

fn hash_resource_value(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u32).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn validate_resource_axis(resources: &BTreeSet<String>) -> Result<(), ToolRuntimeError> {
    if resources.len() > MAX_RESOURCES_PER_AXIS {
        return Err(ToolRuntimeError::InvalidResource(
            "too many resources on one authority axis",
        ));
    }
    for resource in resources {
        validate_bounded_text(resource, MAX_RESOURCE_BYTES, "resource")?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolIntent {
    identity: String,
    tool: String,
    arguments: ToolArguments,
    resources: ToolResources,
}

impl ToolIntent {
    pub fn new(
        identity: impl Into<String>,
        tool: impl Into<String>,
        arguments: ToolArguments,
        resources: ToolResources,
    ) -> Result<Self, ToolRuntimeError> {
        let intent = Self {
            identity: identity.into(),
            tool: tool.into(),
            arguments,
            resources,
        };
        intent.validate()?;
        Ok(intent)
    }

    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    #[must_use]
    pub const fn arguments(&self) -> &ToolArguments {
        &self.arguments
    }

    #[must_use]
    pub const fn resources(&self) -> &ToolResources {
        &self.resources
    }

    fn validate(&self) -> Result<(), ToolRuntimeError> {
        validate_bounded_text(
            &self.identity,
            MAX_CALL_IDENTITY_BYTES,
            "Tool call identity",
        )?;
        validate_bounded_text(&self.tool, MAX_TOOL_NAME_BYTES, "Tool name")?;
        self.resources.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallStatus {
    AwaitingApproval,
    Denied,
    ReconciliationRequired,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCallRecord {
    pub call: ToolCallId,
    pub identity: String,
    pub agent: AgentId,
    pub tool: String,
    pub arguments_hash: ToolArgumentsHash,
    pub resource_binding: ToolResourceBinding,
    pub status: ToolCallStatus,
    pub approval_expires_at_unix_ms: Option<u64>,
    pub result_digest: Option<[u8; 32]>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSnapshot {
    pub ledger_head: LedgerHead,
    pub recovered_tail_bytes: u64,
    pub calls: Vec<ToolCallRecord>,
}

pub struct ToolApprovalRequest {
    call: ToolCallId,
    session: AgentSession,
    intent: ToolIntent,
    arguments_hash: ToolArgumentsHash,
}

impl ToolApprovalRequest {
    #[must_use]
    pub const fn call(&self) -> ToolCallId {
        self.call
    }

    #[must_use]
    pub fn identity(&self) -> &str {
        self.intent.identity()
    }

    #[must_use]
    pub fn tool(&self) -> &str {
        self.intent.tool()
    }

    #[must_use]
    pub const fn arguments(&self) -> &ToolArguments {
        self.intent.arguments()
    }

    #[must_use]
    pub const fn resources(&self) -> &ToolResources {
        self.intent.resources()
    }

    #[must_use]
    pub const fn arguments_hash(&self) -> ToolArgumentsHash {
        self.arguments_hash
    }

    pub(crate) const fn session(&self) -> AgentSession {
        self.session
    }
}

impl fmt::Debug for ToolApprovalRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolApprovalRequest")
            .field("call", &self.call)
            .field("identity", &self.intent.identity)
            .field("agent", &self.session.agent())
            .field("tool", &self.intent.tool)
            .field("arguments_hash", &self.arguments_hash)
            .field("resources", &self.intent.resources)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum ToolRequestOutcome {
    ApprovalRequired(ToolApprovalRequest),
    Existing(ToolCallRecord),
}

#[derive(Clone, Eq, PartialEq)]
pub enum ApprovalDecision {
    Grant { expires_at_unix_ms: u64 },
    Deny { reason: String },
}

impl fmt::Debug for ApprovalDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grant { expires_at_unix_ms } => formatter
                .debug_struct("Grant")
                .field("expires_at_unix_ms", expires_at_unix_ms)
                .finish(),
            Self::Deny { reason } => formatter
                .debug_struct("Deny")
                .field("reason_bytes", &reason.len())
                .finish(),
        }
    }
}

pub struct AuthorizedToolCall<'a> {
    record: &'a ToolCallRecord,
    arguments: &'a ToolArguments,
    resources: &'a ToolResources,
}

impl AuthorizedToolCall<'_> {
    #[must_use]
    pub const fn call(&self) -> ToolCallId {
        self.record.call
    }

    #[must_use]
    pub const fn agent(&self) -> AgentId {
        self.record.agent
    }

    #[must_use]
    pub fn identity(&self) -> &str {
        &self.record.identity
    }

    #[must_use]
    pub fn tool(&self) -> &str {
        &self.record.tool
    }

    #[must_use]
    pub const fn arguments(&self) -> &ToolArguments {
        self.arguments
    }

    #[must_use]
    pub const fn resources(&self) -> &ToolResources {
        self.resources
    }
}

pub trait ToolEffectExecutor {
    fn execute(&mut self, call: &AuthorizedToolCall<'_>) -> ToolExecution;
}

pub enum ToolExecution {
    Succeeded { output: Vec<u8> },
    Failed { reason: String },
    Ambiguous { reason: String },
}

impl fmt::Debug for ToolExecution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Succeeded { output } => formatter
                .debug_struct("Succeeded")
                .field("output_bytes", &output.len())
                .finish(),
            Self::Failed { reason } => formatter
                .debug_struct("Failed")
                .field("reason_bytes", &reason.len())
                .finish(),
            Self::Ambiguous { reason } => formatter
                .debug_struct("Ambiguous")
                .field("reason_bytes", &reason.len())
                .finish(),
        }
    }
}

pub enum ToolCallOutcome {
    Succeeded {
        record: ToolCallRecord,
        output: Vec<u8>,
    },
    Failed(ToolCallRecord),
    Denied(ToolCallRecord),
    ReconciliationRequired(ToolCallRecord),
}

impl fmt::Debug for ToolCallOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Succeeded { record, output } => formatter
                .debug_struct("Succeeded")
                .field("record", record)
                .field("output_bytes", &output.len())
                .finish(),
            Self::Failed(record) => formatter.debug_tuple("Failed").field(record).finish(),
            Self::Denied(record) => formatter.debug_tuple("Denied").field(record).finish(),
            Self::ReconciliationRequired(record) => formatter
                .debug_tuple("ReconciliationRequired")
                .field(record)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ToolReconciliationDecision {
    ObservedSucceeded { result_digest: [u8; 32] },
    ObservedFailed { reason: String },
}

impl fmt::Debug for ToolReconciliationDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObservedSucceeded { result_digest } => formatter
                .debug_struct("ObservedSucceeded")
                .field("result_digest", result_digest)
                .finish(),
            Self::ObservedFailed { reason } => formatter
                .debug_struct("ObservedFailed")
                .field("reason_bytes", &reason.len())
                .finish(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ToolPrincipal {
    session: AgentSession,
    agent: AgentId,
    capabilities: CapabilitySnapshot,
}

impl ToolPrincipal {
    pub(crate) fn new(session: AgentSession, context: AgentExecutionContext) -> Self {
        Self {
            session,
            agent: context.agent,
            capabilities: context.capabilities,
        }
    }
}

pub(crate) struct DurableToolRuntime {
    ledger: FileLedger,
    state: ToolState,
    recovered_tail_bytes: u64,
}

impl DurableToolRuntime {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, ToolRuntimeError> {
        let (ledger, report) = FileLedger::open(path).map_err(ToolRuntimeError::Ledger)?;
        let state = replay_state(&report.events)?;
        Ok(Self {
            ledger,
            state,
            recovered_tail_bytes: report.truncated_tail_bytes,
        })
    }

    pub(crate) fn snapshot(&self) -> ToolSnapshot {
        ToolSnapshot {
            ledger_head: self.ledger.head(),
            recovered_tail_bytes: self.recovered_tail_bytes,
            calls: self.state.calls.values().cloned().collect(),
        }
    }

    pub(crate) fn pending_reconciliation(&self) -> Option<ToolCallId> {
        self.state.calls.values().find_map(|record| {
            (record.status == ToolCallStatus::ReconciliationRequired).then_some(record.call)
        })
    }

    pub(crate) fn request(
        &mut self,
        principal: ToolPrincipal,
        intent: ToolIntent,
    ) -> Result<ToolRequestOutcome, ToolRuntimeError> {
        intent.validate()?;
        validate_authority(&principal.capabilities, &intent)?;
        let arguments_hash = intent.arguments.hash();
        if let Some(call) = self.state.identities.get(&intent.identity).copied() {
            let record = self
                .state
                .calls
                .get(&call)
                .ok_or(ToolRuntimeError::CorruptState(
                    "Tool identity points to a missing call",
                ))?;
            if !record_matches(record, principal.agent, &intent, arguments_hash) {
                return Err(ToolRuntimeError::IdentityConflict {
                    identity: intent.identity,
                });
            }
            if record.status == ToolCallStatus::AwaitingApproval {
                return Ok(ToolRequestOutcome::ApprovalRequired(ToolApprovalRequest {
                    call,
                    session: principal.session,
                    intent,
                    arguments_hash,
                }));
            }
            return Ok(ToolRequestOutcome::Existing(record.clone()));
        }
        if let Some(call) = self.pending_reconciliation() {
            return Err(ToolRuntimeError::ReconciliationRequired(call));
        }

        let call = ToolCallId::new(self.state.next_call)?;
        let event = ToolEvent::CallRequested {
            call,
            identity: intent.identity.clone(),
            agent: principal.agent,
            tool: intent.tool.clone(),
            arguments_hash,
            resource_binding: intent.resources.binding(),
        };
        self.append_events(&[event])?;
        Ok(ToolRequestOutcome::ApprovalRequired(ToolApprovalRequest {
            call,
            session: principal.session,
            intent,
            arguments_hash,
        }))
    }

    pub(crate) fn resolve(
        &mut self,
        principal: ToolPrincipal,
        request: ToolApprovalRequest,
        decision: ApprovalDecision,
        executor: &mut impl ToolEffectExecutor,
    ) -> Result<ToolCallOutcome, ToolRuntimeError> {
        self.resolve_with(
            principal.clone(),
            request,
            decision,
            executor,
            FileLedger::append,
            FileLedger::append,
        )
    }

    fn resolve_with<PrepareAppend, OutcomeAppend>(
        &mut self,
        principal: ToolPrincipal,
        request: ToolApprovalRequest,
        decision: ApprovalDecision,
        executor: &mut impl ToolEffectExecutor,
        prepare_append: PrepareAppend,
        outcome_append: OutcomeAppend,
    ) -> Result<ToolCallOutcome, ToolRuntimeError>
    where
        PrepareAppend: FnOnce(
            &mut FileLedger,
            LedgerHead,
            &[EventData],
        ) -> Result<DurabilityReceipt, LedgerError>,
        OutcomeAppend: FnOnce(
            &mut FileLedger,
            LedgerHead,
            &[EventData],
        ) -> Result<DurabilityReceipt, LedgerError>,
    {
        self.validate_request(&principal, &request)?;
        match decision {
            ApprovalDecision::Deny { reason } => {
                validate_bounded_text(&reason, MAX_REASON_BYTES, "denial reason")?;
                self.append_events_with(
                    &[ToolEvent::ApprovalDenied {
                        call: request.call,
                        reason: APPROVAL_DENIED_REASON.into(),
                    }],
                    prepare_append,
                )?;
                Ok(ToolCallOutcome::Denied(self.record(request.call)?))
            }
            ApprovalDecision::Grant { expires_at_unix_ms } => {
                if expires_at_unix_ms <= current_unix_ms()? {
                    return Err(ToolRuntimeError::ApprovalExpired);
                }
                let grant = ToolEvent::ApprovalGranted {
                    call: request.call,
                    agent: principal.agent,
                    arguments_hash: request.arguments_hash,
                    resource_binding: request.intent.resources.binding(),
                    expires_at_unix_ms,
                };
                self.append_events_with(
                    &[
                        grant,
                        ToolEvent::EffectPrepared {
                            call: request.call,
                            arguments_hash: request.arguments_hash,
                        },
                    ],
                    prepare_append,
                )?;

                let prepared_record = self.record(request.call)?;
                let authorized = AuthorizedToolCall {
                    record: &prepared_record,
                    arguments: &request.intent.arguments,
                    resources: &request.intent.resources,
                };
                let execution = executor.execute(&authorized);
                match execution {
                    ToolExecution::Succeeded { output } if output.len() <= MAX_OUTPUT_BYTES => {
                        let digest: [u8; 32] = Sha256::digest(&output).into();
                        self.append_events_with(
                            &[ToolEvent::EffectSucceeded {
                                call: request.call,
                                result_digest: digest,
                            }],
                            outcome_append,
                        )?;
                        Ok(ToolCallOutcome::Succeeded {
                            record: self.record(request.call)?,
                            output,
                        })
                    }
                    ToolExecution::Succeeded { .. } => {
                        self.append_events_with(
                            &[ToolEvent::EffectAmbiguous {
                                call: request.call,
                                reason: "Tool output exceeded the durable result boundary".into(),
                            }],
                            outcome_append,
                        )?;
                        Ok(ToolCallOutcome::ReconciliationRequired(
                            self.record(request.call)?,
                        ))
                    }
                    ToolExecution::Failed { .. } => {
                        self.append_events_with(
                            &[ToolEvent::EffectFailed {
                                call: request.call,
                                reason: EFFECT_FAILED_REASON.into(),
                            }],
                            outcome_append,
                        )?;
                        Ok(ToolCallOutcome::Failed(self.record(request.call)?))
                    }
                    ToolExecution::Ambiguous { .. } => {
                        self.append_events_with(
                            &[ToolEvent::EffectAmbiguous {
                                call: request.call,
                                reason: EFFECT_AMBIGUOUS_REASON.into(),
                            }],
                            outcome_append,
                        )?;
                        Ok(ToolCallOutcome::ReconciliationRequired(
                            self.record(request.call)?,
                        ))
                    }
                }
            }
        }
    }

    pub(crate) fn reconcile(
        &mut self,
        principal: ToolPrincipal,
        call: ToolCallId,
        decision: ToolReconciliationDecision,
    ) -> Result<ToolCallRecord, ToolRuntimeError> {
        let record = self.record(call)?;
        if record.agent != principal.agent {
            return Err(ToolRuntimeError::ReconciliationAuthorityDenied(call));
        }
        validate_record_authority(&principal.capabilities, &record)?;
        if matches!(
            record.status,
            ToolCallStatus::Succeeded | ToolCallStatus::Failed
        ) {
            return Ok(record);
        }
        if record.status != ToolCallStatus::ReconciliationRequired {
            return Err(ToolRuntimeError::InvalidTransition {
                call,
                operation: "reconcile",
            });
        }
        let event = match decision {
            ToolReconciliationDecision::ObservedSucceeded { result_digest } => {
                ToolEvent::EffectReconciledSucceeded {
                    call,
                    result_digest,
                }
            }
            ToolReconciliationDecision::ObservedFailed { reason } => {
                validate_bounded_text(&reason, MAX_REASON_BYTES, "reconciliation reason")?;
                ToolEvent::EffectReconciledFailed {
                    call,
                    reason: RECONCILED_FAILED_REASON.into(),
                }
            }
        };
        self.append_events(&[event])?;
        self.record(call)
    }

    fn validate_request(
        &self,
        principal: &ToolPrincipal,
        request: &ToolApprovalRequest,
    ) -> Result<(), ToolRuntimeError> {
        if principal.session != request.session || principal.agent != request.session.agent() {
            return Err(ToolRuntimeError::StaleApprovalRequest);
        }
        request.intent.validate()?;
        validate_authority(&principal.capabilities, &request.intent)?;
        if request.arguments_hash != request.intent.arguments.hash() {
            return Err(ToolRuntimeError::StaleApprovalRequest);
        }
        let record = self.record(request.call)?;
        if record.status != ToolCallStatus::AwaitingApproval
            || !record_matches(
                &record,
                principal.agent,
                &request.intent,
                request.arguments_hash,
            )
        {
            return Err(ToolRuntimeError::StaleApprovalRequest);
        }
        Ok(())
    }

    fn record(&self, call: ToolCallId) -> Result<ToolCallRecord, ToolRuntimeError> {
        self.state
            .calls
            .get(&call)
            .cloned()
            .ok_or(ToolRuntimeError::UnknownCall(call))
    }

    fn append_events(
        &mut self,
        events: &[ToolEvent],
    ) -> Result<DurabilityReceipt, ToolRuntimeError> {
        self.append_events_with(events, FileLedger::append)
    }

    fn append_events_with<Append>(
        &mut self,
        events: &[ToolEvent],
        append: Append,
    ) -> Result<DurabilityReceipt, ToolRuntimeError>
    where
        Append: FnOnce(
            &mut FileLedger,
            LedgerHead,
            &[EventData],
        ) -> Result<DurabilityReceipt, LedgerError>,
    {
        validate_transaction(events)?;
        let mut candidate = self.state.clone();
        candidate.apply_transaction(events)?;
        let encoded = events
            .iter()
            .map(encode_event)
            .collect::<Result<Vec<_>, _>>()?;
        let head = self.ledger.head();
        let receipt = append(&mut self.ledger, head, &encoded).map_err(ToolRuntimeError::Ledger)?;
        self.state = candidate;
        Ok(receipt)
    }
}

fn validate_authority(
    capabilities: &CapabilitySnapshot,
    intent: &ToolIntent,
) -> Result<(), ToolRuntimeError> {
    let tool_capability = Capability::Tool(intent.tool.clone());
    if !capabilities.contains(&tool_capability) {
        return Err(ToolRuntimeError::CapabilityDenied {
            capability: tool_capability,
        });
    }
    intent.resources.require_capabilities(capabilities)
}

fn validate_record_authority(
    capabilities: &CapabilitySnapshot,
    record: &ToolCallRecord,
) -> Result<(), ToolRuntimeError> {
    let tool_capability = Capability::Tool(record.tool.clone());
    if !capabilities.contains(&tool_capability) {
        return Err(ToolRuntimeError::CapabilityDenied {
            capability: tool_capability,
        });
    }
    record.resource_binding.require_capabilities(capabilities)
}

fn record_matches(
    record: &ToolCallRecord,
    agent: AgentId,
    intent: &ToolIntent,
    arguments_hash: ToolArgumentsHash,
) -> bool {
    record.agent == agent
        && record.tool == intent.tool
        && record.arguments_hash == arguments_hash
        && record.resource_binding == intent.resources.binding()
}

fn current_unix_ms() -> Result<u64, ToolRuntimeError> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ToolRuntimeError::Clock)?
        .as_millis();
    u64::try_from(milliseconds).map_err(|_| ToolRuntimeError::IntegerOverflow)
}

fn validate_bounded_text(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), ToolRuntimeError> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(ToolRuntimeError::InvalidText(field));
    }
    if value.len() > max_bytes {
        return Err(ToolRuntimeError::TextTooLarge(field));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolEvent {
    CallRequested {
        call: ToolCallId,
        identity: String,
        agent: AgentId,
        tool: String,
        arguments_hash: ToolArgumentsHash,
        resource_binding: ToolResourceBinding,
    },
    ApprovalGranted {
        call: ToolCallId,
        agent: AgentId,
        arguments_hash: ToolArgumentsHash,
        resource_binding: ToolResourceBinding,
        expires_at_unix_ms: u64,
    },
    EffectPrepared {
        call: ToolCallId,
        arguments_hash: ToolArgumentsHash,
    },
    ApprovalDenied {
        call: ToolCallId,
        reason: String,
    },
    EffectSucceeded {
        call: ToolCallId,
        result_digest: [u8; 32],
    },
    EffectFailed {
        call: ToolCallId,
        reason: String,
    },
    EffectAmbiguous {
        call: ToolCallId,
        reason: String,
    },
    EffectReconciledSucceeded {
        call: ToolCallId,
        result_digest: [u8; 32],
    },
    EffectReconciledFailed {
        call: ToolCallId,
        reason: String,
    },
}

impl ToolEvent {
    const fn call(&self) -> ToolCallId {
        match self {
            Self::CallRequested { call, .. }
            | Self::ApprovalGranted { call, .. }
            | Self::EffectPrepared { call, .. }
            | Self::ApprovalDenied { call, .. }
            | Self::EffectSucceeded { call, .. }
            | Self::EffectFailed { call, .. }
            | Self::EffectAmbiguous { call, .. }
            | Self::EffectReconciledSucceeded { call, .. }
            | Self::EffectReconciledFailed { call, .. } => *call,
        }
    }
}

#[derive(Clone, Debug)]
struct ToolState {
    calls: BTreeMap<ToolCallId, ToolCallRecord>,
    identities: BTreeMap<String, ToolCallId>,
    next_call: u64,
}

impl Default for ToolState {
    fn default() -> Self {
        Self {
            calls: BTreeMap::new(),
            identities: BTreeMap::new(),
            next_call: 1,
        }
    }
}

impl ToolState {
    fn apply_transaction(&mut self, events: &[ToolEvent]) -> Result<(), ToolRuntimeError> {
        validate_transaction(events)?;
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }

    fn apply(&mut self, event: &ToolEvent) -> Result<(), ToolRuntimeError> {
        match event {
            ToolEvent::CallRequested {
                call,
                identity,
                agent,
                tool,
                arguments_hash,
                resource_binding,
            } => {
                if call.get() != self.next_call {
                    return Err(ToolRuntimeError::CorruptState(
                        "Tool Call IDs are not contiguous",
                    ));
                }
                validate_bounded_text(identity, MAX_CALL_IDENTITY_BYTES, "Tool call identity")?;
                validate_bounded_text(tool, MAX_TOOL_NAME_BYTES, "Tool name")?;
                resource_binding.validate()?;
                if self.identities.contains_key(identity) || self.calls.contains_key(call) {
                    return Err(ToolRuntimeError::CorruptState(
                        "duplicate Tool call identity",
                    ));
                }
                self.identities.insert(identity.clone(), *call);
                self.calls.insert(
                    *call,
                    ToolCallRecord {
                        call: *call,
                        identity: identity.clone(),
                        agent: *agent,
                        tool: tool.clone(),
                        arguments_hash: *arguments_hash,
                        resource_binding: *resource_binding,
                        status: ToolCallStatus::AwaitingApproval,
                        approval_expires_at_unix_ms: None,
                        result_digest: None,
                        reason: None,
                    },
                );
                self.next_call = self
                    .next_call
                    .checked_add(1)
                    .ok_or(ToolRuntimeError::IdentifierExhausted)?;
            }
            ToolEvent::ApprovalGranted {
                call,
                agent,
                arguments_hash,
                resource_binding,
                expires_at_unix_ms,
            } => {
                let record = self.awaiting_approval_mut(*call)?;
                if record.agent != *agent
                    || record.arguments_hash != *arguments_hash
                    || record.resource_binding != *resource_binding
                    || *expires_at_unix_ms == 0
                {
                    return Err(ToolRuntimeError::CorruptState(
                        "Approval Grant does not match its Tool call",
                    ));
                }
                record.approval_expires_at_unix_ms = Some(*expires_at_unix_ms);
            }
            ToolEvent::EffectPrepared {
                call,
                arguments_hash,
            } => {
                let record = self.awaiting_approval_mut(*call)?;
                if record.arguments_hash != *arguments_hash
                    || record.approval_expires_at_unix_ms.is_none()
                {
                    return Err(ToolRuntimeError::CorruptState(
                        "prepared Tool effect lacks a matching Approval Grant",
                    ));
                }
                record.status = ToolCallStatus::ReconciliationRequired;
                record.reason = Some("prepared Tool effect has no durable outcome".into());
            }
            ToolEvent::ApprovalDenied { call, reason } => {
                validate_bounded_text(reason, MAX_REASON_BYTES, "denial reason")?;
                let record = self.awaiting_approval_mut(*call)?;
                record.status = ToolCallStatus::Denied;
                record.reason = Some(reason.clone());
            }
            ToolEvent::EffectSucceeded {
                call,
                result_digest,
            }
            | ToolEvent::EffectReconciledSucceeded {
                call,
                result_digest,
            } => {
                let record = self.reconciliation_required_mut(*call)?;
                record.status = ToolCallStatus::Succeeded;
                record.result_digest = Some(*result_digest);
                record.reason = None;
            }
            ToolEvent::EffectFailed { call, reason }
            | ToolEvent::EffectReconciledFailed { call, reason } => {
                validate_bounded_text(reason, MAX_REASON_BYTES, "Tool failure reason")?;
                let record = self.reconciliation_required_mut(*call)?;
                record.status = ToolCallStatus::Failed;
                record.reason = Some(reason.clone());
            }
            ToolEvent::EffectAmbiguous { call, reason } => {
                validate_bounded_text(reason, MAX_REASON_BYTES, "ambiguous Tool effect reason")?;
                let record = self.reconciliation_required_mut(*call)?;
                record.reason = Some(reason.clone());
            }
        }
        Ok(())
    }

    fn awaiting_approval_mut(
        &mut self,
        call: ToolCallId,
    ) -> Result<&mut ToolCallRecord, ToolRuntimeError> {
        let record = self
            .calls
            .get_mut(&call)
            .ok_or(ToolRuntimeError::CorruptState("unknown Tool call"))?;
        if record.status != ToolCallStatus::AwaitingApproval {
            return Err(ToolRuntimeError::CorruptState(
                "Tool call is not awaiting approval",
            ));
        }
        Ok(record)
    }

    fn reconciliation_required_mut(
        &mut self,
        call: ToolCallId,
    ) -> Result<&mut ToolCallRecord, ToolRuntimeError> {
        let record = self
            .calls
            .get_mut(&call)
            .ok_or(ToolRuntimeError::CorruptState("unknown Tool call"))?;
        if record.status != ToolCallStatus::ReconciliationRequired {
            return Err(ToolRuntimeError::CorruptState(
                "Tool call is not awaiting reconciliation",
            ));
        }
        Ok(record)
    }
}

fn validate_transaction(events: &[ToolEvent]) -> Result<(), ToolRuntimeError> {
    let valid = match events {
        [ToolEvent::CallRequested { .. }]
        | [ToolEvent::ApprovalDenied { .. }]
        | [ToolEvent::EffectSucceeded { .. }]
        | [ToolEvent::EffectFailed { .. }]
        | [ToolEvent::EffectAmbiguous { .. }]
        | [ToolEvent::EffectReconciledSucceeded { .. }]
        | [ToolEvent::EffectReconciledFailed { .. }] => true,
        [
            ToolEvent::ApprovalGranted { call, .. },
            ToolEvent::EffectPrepared { call: prepared, .. },
        ] => call == prepared,
        _ => false,
    };
    if !valid {
        return Err(ToolRuntimeError::CorruptState(
            "invalid Tool Event transaction",
        ));
    }
    let call = events
        .first()
        .map(ToolEvent::call)
        .ok_or(ToolRuntimeError::CorruptState("empty Tool transaction"))?;
    if events.iter().any(|event| event.call() != call) {
        return Err(ToolRuntimeError::CorruptState(
            "mixed Tool Call IDs in one transaction",
        ));
    }
    Ok(())
}

fn replay_state(events: &[StoredEvent]) -> Result<ToolState, ToolRuntimeError> {
    let mut state = ToolState::default();
    let mut position = 0;
    while position < events.len() {
        let transaction = events[position].transaction;
        let event_count = events[position].events_in_transaction as usize;
        let end = position
            .checked_add(event_count)
            .ok_or(ToolRuntimeError::IntegerOverflow)?;
        if event_count == 0 || end > events.len() {
            return Err(ToolRuntimeError::CorruptEvent(
                "incomplete Tool Event transaction",
            ));
        }
        let stored = &events[position..end];
        if stored.iter().any(|event| event.transaction != transaction) {
            return Err(ToolRuntimeError::CorruptEvent(
                "mixed Tool Ledger transactions",
            ));
        }
        let decoded = stored
            .iter()
            .map(decode_event)
            .collect::<Result<Vec<_>, _>>()?;
        state.apply_transaction(&decoded)?;
        position = end;
    }
    Ok(state)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestedPayload {
    call: u64,
    identity: String,
    agent: u64,
    tool: String,
    arguments_hash: String,
    resource_binding: ResourceBindingPayload,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantPayload {
    call: u64,
    agent: u64,
    arguments_hash: String,
    resource_binding: ResourceBindingPayload,
    expires_at_unix_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedPayload {
    call: u64,
    arguments_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonPayload {
    call: u64,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DigestPayload {
    call: u64,
    result_digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceBindingPayload {
    fingerprint: String,
    filesystem_read_count: u16,
    filesystem_write_count: u16,
    process: bool,
    network_target_count: u16,
}

impl From<ToolResourceBinding> for ResourceBindingPayload {
    fn from(binding: ToolResourceBinding) -> Self {
        Self {
            fingerprint: encode_hash(binding.fingerprint),
            filesystem_read_count: binding.filesystem_read_count,
            filesystem_write_count: binding.filesystem_write_count,
            process: binding.process,
            network_target_count: binding.network_target_count,
        }
    }
}

impl TryFrom<ResourceBindingPayload> for ToolResourceBinding {
    type Error = ToolRuntimeError;

    fn try_from(binding: ResourceBindingPayload) -> Result<Self, Self::Error> {
        let binding = Self {
            fingerprint: decode_hash(&binding.fingerprint)?,
            filesystem_read_count: binding.filesystem_read_count,
            filesystem_write_count: binding.filesystem_write_count,
            process: binding.process,
            network_target_count: binding.network_target_count,
        };
        binding.validate()?;
        Ok(binding)
    }
}

fn encode_event(event: &ToolEvent) -> Result<EventData, ToolRuntimeError> {
    let (kind, payload) = match event {
        ToolEvent::CallRequested {
            call,
            identity,
            agent,
            tool,
            arguments_hash,
            resource_binding,
        } => (
            1,
            serialize_payload(&RequestedPayload {
                call: call.get(),
                identity: identity.clone(),
                agent: agent.get(),
                tool: tool.clone(),
                arguments_hash: encode_hash(arguments_hash.0),
                resource_binding: (*resource_binding).into(),
            })?,
        ),
        ToolEvent::ApprovalGranted {
            call,
            agent,
            arguments_hash,
            resource_binding,
            expires_at_unix_ms,
        } => (
            2,
            serialize_payload(&GrantPayload {
                call: call.get(),
                agent: agent.get(),
                arguments_hash: encode_hash(arguments_hash.0),
                resource_binding: (*resource_binding).into(),
                expires_at_unix_ms: *expires_at_unix_ms,
            })?,
        ),
        ToolEvent::EffectPrepared {
            call,
            arguments_hash,
        } => (
            3,
            serialize_payload(&PreparedPayload {
                call: call.get(),
                arguments_hash: encode_hash(arguments_hash.0),
            })?,
        ),
        ToolEvent::ApprovalDenied { call, reason } => (
            4,
            serialize_payload(&ReasonPayload {
                call: call.get(),
                reason: reason.clone(),
            })?,
        ),
        ToolEvent::EffectSucceeded {
            call,
            result_digest,
        } => (
            5,
            serialize_payload(&DigestPayload {
                call: call.get(),
                result_digest: encode_hash(*result_digest),
            })?,
        ),
        ToolEvent::EffectFailed { call, reason } => (
            6,
            serialize_payload(&ReasonPayload {
                call: call.get(),
                reason: reason.clone(),
            })?,
        ),
        ToolEvent::EffectAmbiguous { call, reason } => (
            7,
            serialize_payload(&ReasonPayload {
                call: call.get(),
                reason: reason.clone(),
            })?,
        ),
        ToolEvent::EffectReconciledSucceeded {
            call,
            result_digest,
        } => (
            8,
            serialize_payload(&DigestPayload {
                call: call.get(),
                result_digest: encode_hash(*result_digest),
            })?,
        ),
        ToolEvent::EffectReconciledFailed { call, reason } => (
            9,
            serialize_payload(&ReasonPayload {
                call: call.get(),
                reason: reason.clone(),
            })?,
        ),
    };
    Ok(EventData {
        schema: TOOL_EVENT_SCHEMA,
        kind,
        payload,
    })
}

fn serialize_payload(value: &impl Serialize) -> Result<Vec<u8>, ToolRuntimeError> {
    serde_json::to_vec(value)
        .map_err(|_| ToolRuntimeError::CorruptState("Tool Event cannot be encoded"))
}

fn decode_event(stored: &StoredEvent) -> Result<ToolEvent, ToolRuntimeError> {
    if stored.data.schema != TOOL_EVENT_SCHEMA {
        return Err(ToolRuntimeError::UnsupportedToolEventSchema {
            supported: TOOL_EVENT_SCHEMA,
            actual: stored.data.schema,
        });
    }
    match stored.data.kind {
        1 => {
            let payload: RequestedPayload = deserialize_payload(&stored.data.payload)?;
            Ok(ToolEvent::CallRequested {
                call: ToolCallId::from_stored(payload.call)?,
                identity: payload.identity,
                agent: AgentId::from_stored(payload.agent)
                    .ok_or(ToolRuntimeError::CorruptEvent("invalid Agent ID"))?,
                tool: payload.tool,
                arguments_hash: ToolArgumentsHash(decode_hash(&payload.arguments_hash)?),
                resource_binding: payload.resource_binding.try_into()?,
            })
        }
        2 => {
            let payload: GrantPayload = deserialize_payload(&stored.data.payload)?;
            Ok(ToolEvent::ApprovalGranted {
                call: ToolCallId::from_stored(payload.call)?,
                agent: AgentId::from_stored(payload.agent)
                    .ok_or(ToolRuntimeError::CorruptEvent("invalid Agent ID"))?,
                arguments_hash: ToolArgumentsHash(decode_hash(&payload.arguments_hash)?),
                resource_binding: payload.resource_binding.try_into()?,
                expires_at_unix_ms: payload.expires_at_unix_ms,
            })
        }
        3 => {
            let payload: PreparedPayload = deserialize_payload(&stored.data.payload)?;
            Ok(ToolEvent::EffectPrepared {
                call: ToolCallId::from_stored(payload.call)?,
                arguments_hash: ToolArgumentsHash(decode_hash(&payload.arguments_hash)?),
            })
        }
        4 => decode_reason(&stored.data.payload, |call, reason| {
            ToolEvent::ApprovalDenied { call, reason }
        }),
        5 => decode_digest(&stored.data.payload, |call, result_digest| {
            ToolEvent::EffectSucceeded {
                call,
                result_digest,
            }
        }),
        6 => decode_reason(&stored.data.payload, |call, reason| {
            ToolEvent::EffectFailed { call, reason }
        }),
        7 => decode_reason(&stored.data.payload, |call, reason| {
            ToolEvent::EffectAmbiguous { call, reason }
        }),
        8 => decode_digest(&stored.data.payload, |call, result_digest| {
            ToolEvent::EffectReconciledSucceeded {
                call,
                result_digest,
            }
        }),
        9 => decode_reason(&stored.data.payload, |call, reason| {
            ToolEvent::EffectReconciledFailed { call, reason }
        }),
        _ => Err(ToolRuntimeError::CorruptEvent("unknown Tool Event kind")),
    }
}

fn deserialize_payload<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, ToolRuntimeError> {
    serde_json::from_slice(bytes)
        .map_err(|_| ToolRuntimeError::CorruptEvent("invalid Tool Event payload"))
}

fn decode_reason(
    bytes: &[u8],
    build: impl FnOnce(ToolCallId, String) -> ToolEvent,
) -> Result<ToolEvent, ToolRuntimeError> {
    let payload: ReasonPayload = deserialize_payload(bytes)?;
    Ok(build(
        ToolCallId::from_stored(payload.call)?,
        payload.reason,
    ))
}

fn decode_digest(
    bytes: &[u8],
    build: impl FnOnce(ToolCallId, [u8; 32]) -> ToolEvent,
) -> Result<ToolEvent, ToolRuntimeError> {
    let payload: DigestPayload = deserialize_payload(bytes)?;
    Ok(build(
        ToolCallId::from_stored(payload.call)?,
        decode_hash(&payload.result_digest)?,
    ))
}

fn encode_hash(hash: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in hash {
        encoded.push(char::from(HEX[(byte >> 4) as usize]));
        encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    encoded
}

fn decode_hash(value: &str) -> Result<[u8; 32], ToolRuntimeError> {
    if value.len() != 64 {
        return Err(ToolRuntimeError::CorruptEvent("invalid Tool hash length"));
    }
    let mut decoded = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (decode_hex_digit(pair[0])? << 4) | decode_hex_digit(pair[1])?;
    }
    Ok(decoded)
}

fn decode_hex_digit(value: u8) -> Result<u8, ToolRuntimeError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(ToolRuntimeError::CorruptEvent(
            "Tool hash is not lowercase hexadecimal",
        )),
    }
}

#[derive(Debug)]
pub enum ToolRuntimeError {
    Ledger(LedgerError),
    InvalidArguments(&'static str),
    InvalidResource(&'static str),
    InvalidText(&'static str),
    TextTooLarge(&'static str),
    CapabilityDenied {
        capability: Capability,
    },
    IdentityConflict {
        identity: String,
    },
    UnknownCall(ToolCallId),
    StaleApprovalRequest,
    ApprovalExpired,
    ReconciliationRequired(ToolCallId),
    ReconciliationAuthorityDenied(ToolCallId),
    InvalidTransition {
        call: ToolCallId,
        operation: &'static str,
    },
    UnsupportedToolEventSchema {
        supported: u16,
        actual: u16,
    },
    CorruptEvent(&'static str),
    CorruptState(&'static str),
    IdentifierExhausted,
    IntegerOverflow,
    Clock,
}

impl fmt::Display for ToolRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ledger(source) => write!(formatter, "{source}"),
            Self::InvalidArguments(reason) => write!(formatter, "invalid Tool arguments: {reason}"),
            Self::InvalidResource(reason) => write!(formatter, "invalid Tool resource: {reason}"),
            Self::InvalidText(field) => write!(formatter, "invalid {field}"),
            Self::TextTooLarge(field) => write!(formatter, "{field} exceeds its byte limit"),
            Self::CapabilityDenied { capability } => {
                write!(formatter, "Tool capability denied: {capability:?}")
            }
            Self::IdentityConflict { identity } => {
                write!(formatter, "Tool call identity {identity:?} changed meaning")
            }
            Self::UnknownCall(call) => write!(formatter, "unknown Tool call {}", call.get()),
            Self::StaleApprovalRequest => write!(formatter, "stale Tool Approval Request"),
            Self::ApprovalExpired => write!(formatter, "Tool Approval Grant is expired"),
            Self::ReconciliationRequired(call) => {
                write!(
                    formatter,
                    "Tool call {} requires reconciliation",
                    call.get()
                )
            }
            Self::ReconciliationAuthorityDenied(call) => write!(
                formatter,
                "Tool call {} reconciliation authority denied",
                call.get()
            ),
            Self::InvalidTransition { call, operation } => write!(
                formatter,
                "Tool call {} cannot {operation} from its current state",
                call.get()
            ),
            Self::UnsupportedToolEventSchema { supported, actual } => write!(
                formatter,
                "unsupported Tool Event schema {actual}; expected {supported}"
            ),
            Self::CorruptEvent(reason) => write!(formatter, "corrupt Tool Event: {reason}"),
            Self::CorruptState(reason) => write!(formatter, "corrupt Tool state: {reason}"),
            Self::IdentifierExhausted => write!(formatter, "Tool identifier space is exhausted"),
            Self::IntegerOverflow => write!(formatter, "Tool integer overflow"),
            Self::Clock => write!(formatter, "system clock is before the Unix epoch"),
        }
    }
}

impl Error for ToolRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ledger(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::agent_team::{
        CommandOutcome, ResourceBudget, TaskScope, TaskSpec, TeamCommand, TeamRuntime,
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn temp_path(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "greentyper-tool-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn principal() -> ToolPrincipal {
        let mut team = TeamRuntime::new(1).expect("Team Runtime");
        let commit = team
            .dispatch(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "test Tool durability",
                    TaskScope::from_labels(["repo", "tests"]),
                ),
                budget: ResourceBudget::new(100, 2),
                capabilities: CapabilitySnapshot::from_capabilities([
                    Capability::Tool("local.echo".into()),
                    Capability::Process,
                ]),
            })
            .expect("root admission");
        let session = match commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected outcome: {other:?}"),
        };
        let context = team
            .trusted_active_agent_context(session)
            .expect("active Agent context");
        ToolPrincipal::new(session, context)
    }

    fn two_principals() -> (ToolPrincipal, ToolPrincipal) {
        let mut team = TeamRuntime::new(2).expect("Team Runtime");
        let capabilities = CapabilitySnapshot::from_capabilities([
            Capability::Tool("local.echo".into()),
            Capability::Process,
        ]);
        let root_commit = team
            .dispatch(TeamCommand::AdmitRoot {
                task: TaskSpec::new("root Tool owner", TaskScope::from_labels(["repo", "tests"])),
                budget: ResourceBudget::new(100, 2),
                capabilities: capabilities.clone(),
            })
            .expect("root admission");
        let root = match root_commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected outcome: {other:?}"),
        };
        let child_commit = team
            .dispatch(TeamCommand::Delegate {
                parent: root,
                task: TaskSpec::new("child Tool owner", TaskScope::from_labels(["repo"])),
                budget: ResourceBudget::new(50, 1),
                capabilities,
            })
            .expect("child delegation");
        let child = match child_commit.outcome {
            CommandOutcome::Delegated { session, .. } => session,
            other => panic!("unexpected outcome: {other:?}"),
        };
        let root_context = team
            .trusted_active_agent_context(root)
            .expect("active root context");
        let child_context = team
            .trusted_active_agent_context(child)
            .expect("active child context");
        (
            ToolPrincipal::new(root, root_context),
            ToolPrincipal::new(child, child_context),
        )
    }

    fn intent(identity: &str, message: &str) -> ToolIntent {
        ToolIntent::new(
            identity,
            "local.echo",
            ToolArguments::parse(&format!(r#"{{"message":"{message}"}}"#)).expect("Tool arguments"),
            ToolResources::default().with_process("local.echo"),
        )
        .expect("Tool intent")
    }

    fn approval_request(
        runtime: &mut DurableToolRuntime,
        principal: ToolPrincipal,
        intent: ToolIntent,
    ) -> ToolApprovalRequest {
        match runtime.request(principal, intent).expect("Tool request") {
            ToolRequestOutcome::ApprovalRequired(request) => request,
            ToolRequestOutcome::Existing(record) => {
                panic!("unexpected existing Tool call: {record:?}")
            }
        }
    }

    #[derive(Default)]
    struct CountingExecutor {
        calls: usize,
    }

    struct AmbiguousExecutor;

    impl ToolEffectExecutor for AmbiguousExecutor {
        fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
            ToolExecution::Ambiguous {
                reason: "executor-secret-marker".into(),
            }
        }
    }

    impl ToolEffectExecutor for CountingExecutor {
        fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
            self.calls += 1;
            ToolExecution::Succeeded {
                output: b"test output".to_vec(),
            }
        }
    }

    #[test]
    fn canonical_arguments_sort_object_keys_recursively() {
        let arguments = ToolArguments::parse(r#"{"z":1,"nested":{"b":2,"a":1},"a":0}"#)
            .expect("canonical arguments");
        assert_eq!(
            arguments.canonical_json(),
            r#"{"a":0,"nested":{"a":1,"b":2},"z":1}"#
        );
        assert_eq!(
            arguments.hash(),
            ToolArguments::parse(r#"{ "a": 0, "nested": {"a":1,"b":2}, "z":1 }"#)
                .expect("equivalent arguments")
                .hash()
        );
    }

    #[test]
    fn tool_event_codec_rejects_unknown_schema_kind_and_trailing_fields() {
        let event = ToolEvent::CallRequested {
            call: ToolCallId::new(1).expect("Call id"),
            identity: "fixture-call".into(),
            agent: AgentId::from_stored(1).expect("Agent id"),
            tool: "local.echo".into(),
            arguments_hash: ToolArgumentsHash([1; 32]),
            resource_binding: ToolResources::default().binding(),
        };
        let data = encode_event(&event).expect("encode Tool Event");
        let stored = StoredEvent {
            sequence: 1,
            transaction: 1,
            index_in_transaction: 0,
            events_in_transaction: 1,
            data: data.clone(),
        };
        assert_eq!(decode_event(&stored).expect("decode Tool Event"), event);

        let mut wrong_schema = stored.clone();
        wrong_schema.data.schema += 1;
        assert!(matches!(
            decode_event(&wrong_schema),
            Err(ToolRuntimeError::UnsupportedToolEventSchema { .. })
        ));
        let mut wrong_kind = stored.clone();
        wrong_kind.data.kind = 99;
        assert!(matches!(
            decode_event(&wrong_kind),
            Err(ToolRuntimeError::CorruptEvent("unknown Tool Event kind"))
        ));
        let mut extra_field = stored;
        extra_field.data.payload.pop();
        extra_field
            .data
            .payload
            .extend_from_slice(br#","unexpected":true}"#);
        assert!(matches!(
            decode_event(&extra_field),
            Err(ToolRuntimeError::CorruptEvent("invalid Tool Event payload"))
        ));

        let oversized_resource_axis = StoredEvent {
            sequence: 1,
            transaction: 1,
            index_in_transaction: 0,
            events_in_transaction: 1,
            data: EventData {
                schema: TOOL_EVENT_SCHEMA,
                kind: 1,
                payload: serialize_payload(&RequestedPayload {
                    call: 1,
                    identity: "fixture-call".into(),
                    agent: 1,
                    tool: "local.echo".into(),
                    arguments_hash: encode_hash([1; 32]),
                    resource_binding: ResourceBindingPayload {
                        fingerprint: encode_hash([2; 32]),
                        filesystem_read_count: (MAX_RESOURCES_PER_AXIS + 1) as u16,
                        filesystem_write_count: 0,
                        process: false,
                        network_target_count: 0,
                    },
                })
                .expect("encode invalid resource binding"),
            },
        };
        assert!(matches!(
            decode_event(&oversized_resource_axis),
            Err(ToolRuntimeError::CorruptEvent(
                "Tool resource binding exceeds its axis limit"
            ))
        ));
    }

    #[test]
    fn hash_decoder_requires_lowercase_exact_width() {
        assert_eq!(decode_hash(&"00".repeat(32)).expect("zero hash"), [0; 32]);
        assert!(decode_hash("00").is_err());
        assert!(decode_hash(&"AA".repeat(32)).is_err());
    }

    #[test]
    fn prepare_durability_failure_never_invokes_the_effect() {
        let path = temp_path("prepare-fault");
        let mut runtime = DurableToolRuntime::open(&path).expect("open Tool Runtime");
        let principal = principal();
        let request = approval_request(
            &mut runtime,
            principal.clone(),
            intent("prepare-fault", "confidential-input-marker"),
        );
        let mut executor = CountingExecutor::default();
        let result = runtime.resolve_with(
            principal,
            request,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut executor,
            |ledger, head, events| {
                ledger.append_with_test_io(head, events, |_file, _frame| {
                    Err(io::Error::other("injected prepare fault"))
                })
            },
            FileLedger::append,
        );
        assert!(matches!(
            result,
            Err(ToolRuntimeError::Ledger(LedgerError::DurabilityAmbiguous(
                _
            )))
        ));
        assert_eq!(executor.calls, 0);
        assert!(matches!(
            runtime.append_events(&[ToolEvent::ApprovalDenied {
                call: ToolCallId::new(1).expect("Call id"),
                reason: "writer cannot continue".into(),
            }]),
            Err(ToolRuntimeError::Ledger(LedgerError::WriterPoisoned))
        ));
        drop(runtime);

        let recovered = DurableToolRuntime::open(&path).expect("recover Tool Runtime");
        assert_eq!(
            recovered.snapshot().calls[0].status,
            ToolCallStatus::AwaitingApproval
        );
        let bytes = fs::read(&path).expect("read Tool Ledger");
        assert!(
            !bytes
                .windows(b"confidential-input-marker".len())
                .any(|window| window == b"confidential-input-marker")
        );
        drop(recovered);
        fs::remove_file(path).expect("cleanup Tool Ledger");
    }

    #[test]
    fn outcome_durability_failure_requires_reopen_and_explicit_reconciliation() {
        let path = temp_path("outcome-fault");
        let mut runtime = DurableToolRuntime::open(&path).expect("open Tool Runtime");
        let principal = principal();
        let request = approval_request(
            &mut runtime,
            principal.clone(),
            intent("outcome-fault", "execute-once"),
        );
        let call = request.call();
        let mut executor = CountingExecutor::default();
        let result = runtime.resolve_with(
            principal.clone(),
            request,
            ApprovalDecision::Grant {
                expires_at_unix_ms: u64::MAX,
            },
            &mut executor,
            FileLedger::append,
            |ledger, head, events| {
                ledger.append_with_test_io(head, events, |_file, _frame| {
                    Err(io::Error::other("injected outcome fault"))
                })
            },
        );
        assert!(matches!(
            result,
            Err(ToolRuntimeError::Ledger(LedgerError::DurabilityAmbiguous(
                _
            )))
        ));
        assert_eq!(executor.calls, 1);
        assert_eq!(
            runtime.snapshot().calls[0].status,
            ToolCallStatus::ReconciliationRequired
        );
        assert!(matches!(
            runtime.reconcile(
                principal.clone(),
                call,
                ToolReconciliationDecision::ObservedSucceeded {
                    result_digest: [3; 32],
                },
            ),
            Err(ToolRuntimeError::Ledger(LedgerError::WriterPoisoned))
        ));
        drop(runtime);

        let mut recovered = DurableToolRuntime::open(&path).expect("recover Tool Runtime");
        assert_eq!(
            recovered.snapshot().calls[0].status,
            ToolCallStatus::ReconciliationRequired
        );
        let record = recovered
            .reconcile(
                principal,
                call,
                ToolReconciliationDecision::ObservedSucceeded {
                    result_digest: [3; 32],
                },
            )
            .expect("explicit reconciliation");
        assert_eq!(record.status, ToolCallStatus::Succeeded);
        drop(recovered);
        let replayed = DurableToolRuntime::open(&path).expect("replay reconciliation");
        assert_eq!(replayed.snapshot().calls[0], record);
        drop(replayed);
        fs::remove_file(path).expect("cleanup Tool Ledger");
    }

    #[test]
    fn reconciliation_requires_the_original_agent_and_persists_no_external_reason() {
        let path = temp_path("reconcile-authority");
        let mut runtime = DurableToolRuntime::open(&path).expect("open Tool Runtime");
        let (owner, other) = two_principals();
        let private_resource = "process-private-resource-marker";
        let request = approval_request(
            &mut runtime,
            owner.clone(),
            ToolIntent::new(
                "reconcile-authority",
                "local.echo",
                ToolArguments::parse(r#"{"message":"private-argument-marker"}"#)
                    .expect("Tool arguments"),
                ToolResources::default().with_process(private_resource),
            )
            .expect("Tool intent"),
        );
        let call = request.call();
        let outcome = runtime
            .resolve(
                owner.clone(),
                request,
                ApprovalDecision::Grant {
                    expires_at_unix_ms: u64::MAX,
                },
                &mut AmbiguousExecutor,
            )
            .expect("record ambiguous outcome");
        assert!(matches!(
            outcome,
            ToolCallOutcome::ReconciliationRequired(_)
        ));
        assert!(matches!(
            runtime.reconcile(
                other,
                call,
                ToolReconciliationDecision::ObservedSucceeded {
                    result_digest: [4; 32],
                },
            ),
            Err(ToolRuntimeError::ReconciliationAuthorityDenied(rejected))
                if rejected == call
        ));
        let record = runtime
            .reconcile(
                owner.clone(),
                call,
                ToolReconciliationDecision::ObservedFailed {
                    reason: "reconciler-secret-marker".into(),
                },
            )
            .expect("owner reconciliation");
        assert_eq!(record.status, ToolCallStatus::Failed);
        assert_eq!(record.reason.as_deref(), Some(RECONCILED_FAILED_REASON));
        let denied_request = approval_request(
            &mut runtime,
            owner.clone(),
            intent("denied-reason", "denied-argument"),
        );
        let denied = runtime
            .resolve(
                owner,
                denied_request,
                ApprovalDecision::Deny {
                    reason: "approval-secret-marker".into(),
                },
                &mut CountingExecutor::default(),
            )
            .expect("durable denial");
        let ToolCallOutcome::Denied(denied_record) = denied else {
            panic!("unexpected denial outcome: {denied:?}");
        };
        assert_eq!(
            denied_record.reason.as_deref(),
            Some(APPROVAL_DENIED_REASON)
        );
        drop(runtime);

        let bytes = fs::read(&path).expect("read Tool Ledger");
        for marker in [
            "private-argument-marker",
            "executor-secret-marker",
            "reconciler-secret-marker",
            "approval-secret-marker",
            private_resource,
        ] {
            assert!(
                !bytes
                    .windows(marker.len())
                    .any(|window| window == marker.as_bytes())
            );
        }
        let debug_marker = "debug-secret-marker";
        let arguments = ToolArguments::parse(&format!(r#"{{"value":"{debug_marker}"}}"#))
            .expect("debug arguments");
        assert!(!format!("{arguments:?}").contains(debug_marker));
        assert!(
            !format!("{:?}", ToolResources::default().with_process(debug_marker))
                .contains(debug_marker)
        );
        assert!(
            !format!(
                "{:?}",
                ApprovalDecision::Deny {
                    reason: debug_marker.into()
                }
            )
            .contains(debug_marker)
        );
        assert!(
            !format!(
                "{:?}",
                ToolExecution::Failed {
                    reason: debug_marker.into()
                }
            )
            .contains(debug_marker)
        );
        assert!(
            !format!(
                "{:?}",
                ToolReconciliationDecision::ObservedFailed {
                    reason: debug_marker.into()
                }
            )
            .contains(debug_marker)
        );
        fs::remove_file(path).expect("cleanup Tool Ledger");
    }
}
