//! Deterministic Context Pressure, bounded Views, and reduction evidence.
//!
//! This module is pure data policy. Runtime owns checkpoint persistence and
//! Safe Barrier validation; neither layer holds Tool, credential, MCP, or
//! Durable Memory authority.

use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::ledger::LedgerHead;
use crate::model::{CanonicalItem, ItemRole};

pub const DEFAULT_SOFT_CONTEXT_PRESSURE_PERCENT: u8 = 65;
pub const DEFAULT_HARD_CONTEXT_PRESSURE_PERCENT: u8 = 90;
pub const MAX_CONTEXT_VIEW_BYTES: usize = 512 * 1024;
pub const MAX_CONTEXT_VIEW_ITEMS: usize = 4096;
pub const DEFAULT_CONTEXT_RAW_TAIL_BYTES: usize = 64 * 1024;
pub const DEFAULT_CONTEXT_RAW_TAIL_ITEMS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPressureAccuracy {
    Exact,
    Estimated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPressureState {
    Normal,
    Soft,
    Hard,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAdmissionDecision {
    Allow,
    Reduce,
    Stop,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPressureUnknownReason {
    MissingContextLimit,
    MissingUsedTokens,
    MissingOutputReserve,
    MissingAccuracy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextPressurePolicy {
    soft_percent: u8,
    hard_percent: u8,
}

impl ContextPressurePolicy {
    pub const fn new(soft_percent: u8, hard_percent: u8) -> Result<Self, ContextPressureError> {
        if soft_percent == 0 || soft_percent >= hard_percent || hard_percent > 100 {
            Err(ContextPressureError::InvalidThresholds)
        } else {
            Ok(Self {
                soft_percent,
                hard_percent,
            })
        }
    }

    #[must_use]
    pub const fn soft_percent(self) -> u8 {
        self.soft_percent
    }

    #[must_use]
    pub const fn hard_percent(self) -> u8 {
        self.hard_percent
    }
}

impl Default for ContextPressurePolicy {
    fn default() -> Self {
        Self {
            soft_percent: DEFAULT_SOFT_CONTEXT_PRESSURE_PERCENT,
            hard_percent: DEFAULT_HARD_CONTEXT_PRESSURE_PERCENT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextPressureInput {
    context_limit_tokens: Option<u64>,
    used_tokens: Option<u64>,
    output_reserve_tokens: Option<u64>,
    accuracy: Option<ContextPressureAccuracy>,
}

impl ContextPressureInput {
    #[must_use]
    pub const fn new(
        context_limit_tokens: Option<u64>,
        used_tokens: Option<u64>,
        output_reserve_tokens: Option<u64>,
        accuracy: Option<ContextPressureAccuracy>,
    ) -> Self {
        Self {
            context_limit_tokens,
            used_tokens,
            output_reserve_tokens,
            accuracy,
        }
    }

    #[must_use]
    pub const fn known(
        context_limit_tokens: u64,
        used_tokens: u64,
        output_reserve_tokens: u64,
        accuracy: ContextPressureAccuracy,
    ) -> Self {
        Self::new(
            Some(context_limit_tokens),
            Some(used_tokens),
            Some(output_reserve_tokens),
            Some(accuracy),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextPressureSnapshot {
    context_limit_tokens: Option<u64>,
    used_tokens: Option<u64>,
    output_reserve_tokens: Option<u64>,
    projected_tokens: Option<u64>,
    occupancy_percent: Option<u8>,
    accuracy: Option<ContextPressureAccuracy>,
    state: ContextPressureState,
    admission: ContextAdmissionDecision,
    unknown_reason: Option<ContextPressureUnknownReason>,
    soft_threshold_percent: u8,
    hard_threshold_percent: u8,
}

impl ContextPressureSnapshot {
    #[must_use]
    pub const fn context_limit_tokens(self) -> Option<u64> {
        self.context_limit_tokens
    }

    #[must_use]
    pub const fn used_tokens(self) -> Option<u64> {
        self.used_tokens
    }

    #[must_use]
    pub const fn output_reserve_tokens(self) -> Option<u64> {
        self.output_reserve_tokens
    }

    #[must_use]
    pub const fn projected_tokens(self) -> Option<u64> {
        self.projected_tokens
    }

    #[must_use]
    pub const fn occupancy_percent(self) -> Option<u8> {
        self.occupancy_percent
    }

    #[must_use]
    pub const fn accuracy(self) -> Option<ContextPressureAccuracy> {
        self.accuracy
    }

    #[must_use]
    pub const fn state(self) -> ContextPressureState {
        self.state
    }

    #[must_use]
    pub const fn admission(self) -> ContextAdmissionDecision {
        self.admission
    }

    #[must_use]
    pub const fn unknown_reason(self) -> Option<ContextPressureUnknownReason> {
        self.unknown_reason
    }

    #[must_use]
    pub const fn soft_threshold_percent(self) -> u8 {
        self.soft_threshold_percent
    }

    #[must_use]
    pub const fn hard_threshold_percent(self) -> u8 {
        self.hard_threshold_percent
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextEventRange {
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    transaction: u64,
}

impl ContextEventRange {
    #[must_use]
    pub const fn from_head(head: LedgerHead) -> Self {
        if head.sequence == 0 {
            Self {
                first_sequence: None,
                last_sequence: None,
                transaction: 0,
            }
        } else {
            Self {
                first_sequence: Some(1),
                last_sequence: Some(head.sequence),
                transaction: head.transaction,
            }
        }
    }

    #[must_use]
    pub const fn first_sequence(self) -> Option<u64> {
        self.first_sequence
    }

    #[must_use]
    pub const fn last_sequence(self) -> Option<u64> {
        self.last_sequence
    }

    #[must_use]
    pub const fn transaction(self) -> u64 {
        self.transaction
    }

    #[must_use]
    pub const fn head(self) -> LedgerHead {
        LedgerHead {
            transaction: self.transaction,
            sequence: match self.last_sequence {
                Some(sequence) => sequence,
                None => 0,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextViewRole {
    User,
    Assistant,
}

impl From<ItemRole> for ContextViewRole {
    fn from(role: ItemRole) -> Self {
        match role {
            ItemRole::User => Self::User,
            ItemRole::Assistant => Self::Assistant,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextViewItem {
    item: u64,
    turn: u64,
    role: ContextViewRole,
    text: String,
    estimated_tokens: u64,
}

impl ContextViewItem {
    fn from_item(item: &CanonicalItem) -> Result<Self, ContextViewError> {
        let characters = u64::try_from(item.text().chars().count())
            .map_err(|_| ContextViewError::ArithmeticOverflow)?;
        let estimated_tokens = characters.div_ceil(4).max(1);
        Ok(Self {
            item: item.id().get(),
            turn: item.turn().get(),
            role: item.role().into(),
            text: item.text().to_owned(),
            estimated_tokens,
        })
    }

    pub(crate) fn from_stored(
        item: u64,
        turn: u64,
        role: ContextViewRole,
        text: String,
    ) -> Result<Self, ContextViewError> {
        let canonical = CanonicalItem::new(
            crate::model::ItemId::new(item).map_err(|_| ContextViewError::InvalidStoredView)?,
            crate::model::TurnId::new(turn).map_err(|_| ContextViewError::InvalidStoredView)?,
            match role {
                ContextViewRole::User => ItemRole::User,
                ContextViewRole::Assistant => ItemRole::Assistant,
            },
            text,
        )
        .map_err(|_| ContextViewError::InvalidStoredView)?;
        Self::from_item(&canonical)
    }

    #[must_use]
    pub const fn item(&self) -> u64 {
        self.item
    }

    #[must_use]
    pub const fn turn(&self) -> u64 {
        self.turn
    }

    #[must_use]
    pub const fn role(&self) -> ContextViewRole {
        self.role
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextView {
    source: ContextEventRange,
    items: Vec<ContextViewItem>,
    raw_bytes: u64,
    estimated_tokens: u64,
}

impl ContextView {
    pub fn from_items(head: LedgerHead, items: &[CanonicalItem]) -> Result<Self, ContextViewError> {
        if items.len() > MAX_CONTEXT_VIEW_ITEMS {
            return Err(ContextViewError::TooManyItems);
        }
        let mut projected = Vec::with_capacity(items.len());
        let mut last_item = 0_u64;
        let mut raw_bytes = 0_u64;
        let mut estimated_tokens = 0_u64;
        for item in items {
            if item.id().get() <= last_item {
                return Err(ContextViewError::InvalidStoredView);
            }
            last_item = item.id().get();
            raw_bytes = raw_bytes
                .checked_add(
                    u64::try_from(item.text().len())
                        .map_err(|_| ContextViewError::ArithmeticOverflow)?,
                )
                .ok_or(ContextViewError::ArithmeticOverflow)?;
            if raw_bytes > MAX_CONTEXT_VIEW_BYTES as u64 {
                return Err(ContextViewError::ViewTooLarge);
            }
            let item = ContextViewItem::from_item(item)?;
            estimated_tokens = estimated_tokens
                .checked_add(item.estimated_tokens())
                .ok_or(ContextViewError::ArithmeticOverflow)?;
            projected.push(item);
        }
        Ok(Self {
            source: ContextEventRange::from_head(head),
            items: projected,
            raw_bytes,
            estimated_tokens,
        })
    }

    #[must_use]
    pub const fn source(&self) -> ContextEventRange {
        self.source
    }

    #[must_use]
    pub fn items(&self) -> &[ContextViewItem] {
        &self.items
    }

    #[must_use]
    pub const fn raw_bytes(&self) -> u64 {
        self.raw_bytes
    }

    #[must_use]
    pub const fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }

    pub fn reduce(
        &self,
        policy: ContextReductionPolicy,
    ) -> Result<ReducedContextView, ContextViewError> {
        let mut recent_start = self.items.len();
        let mut recent_raw_bytes = 0_u64;
        for (index, item) in self.items.iter().enumerate().rev() {
            if self.items.len() - index > policy.max_raw_items {
                break;
            }
            let item_bytes =
                u64::try_from(item.text.len()).map_err(|_| ContextViewError::ArithmeticOverflow)?;
            let Some(next_raw_bytes) = recent_raw_bytes.checked_add(item_bytes) else {
                return Err(ContextViewError::ArithmeticOverflow);
            };
            if next_raw_bytes > policy.max_raw_bytes as u64 {
                break;
            }
            recent_start = index;
            recent_raw_bytes = next_raw_bytes;
        }

        let artifacts = self.items[..recent_start]
            .iter()
            .map(ContextArtifactRef::from_item)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ReducedContextView {
            source: self.source,
            artifacts,
            recent_items: self.items[recent_start..].to_vec(),
            raw_bytes: recent_raw_bytes,
            estimated_tokens: self.estimated_tokens,
        })
    }

    pub fn resolve_artifact(
        &self,
        artifact: &ContextArtifactRef,
    ) -> Result<&str, ContextViewError> {
        let item = self
            .items
            .iter()
            .find(|item| item.item == artifact.item)
            .ok_or(ContextViewError::ArtifactMismatch)?;
        let byte_len =
            u64::try_from(item.text.len()).map_err(|_| ContextViewError::ArithmeticOverflow)?;
        let digest: [u8; 32] = Sha256::digest(item.text.as_bytes()).into();
        if item.turn != artifact.turn
            || item.role != artifact.role
            || item.estimated_tokens != artifact.estimated_tokens
            || byte_len != artifact.byte_len
            || digest != artifact.digest
        {
            return Err(ContextViewError::ArtifactMismatch);
        }
        Ok(&item.text)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextReductionPolicy {
    max_raw_bytes: usize,
    max_raw_items: usize,
}

impl ContextReductionPolicy {
    pub const fn new(max_raw_bytes: usize, max_raw_items: usize) -> Result<Self, ContextViewError> {
        if max_raw_bytes == 0
            || max_raw_bytes > MAX_CONTEXT_VIEW_BYTES
            || max_raw_items == 0
            || max_raw_items > MAX_CONTEXT_VIEW_ITEMS
        {
            Err(ContextViewError::InvalidReductionPolicy)
        } else {
            Ok(Self {
                max_raw_bytes,
                max_raw_items,
            })
        }
    }

    #[must_use]
    pub const fn max_raw_bytes(self) -> usize {
        self.max_raw_bytes
    }

    #[must_use]
    pub const fn max_raw_items(self) -> usize {
        self.max_raw_items
    }
}

impl Default for ContextReductionPolicy {
    fn default() -> Self {
        Self {
            max_raw_bytes: DEFAULT_CONTEXT_RAW_TAIL_BYTES,
            max_raw_items: DEFAULT_CONTEXT_RAW_TAIL_ITEMS,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextArtifactRef {
    item: u64,
    turn: u64,
    role: ContextViewRole,
    byte_len: u64,
    estimated_tokens: u64,
    digest: [u8; 32],
}

impl ContextArtifactRef {
    fn from_item(item: &ContextViewItem) -> Result<Self, ContextViewError> {
        Ok(Self {
            item: item.item,
            turn: item.turn,
            role: item.role,
            byte_len: u64::try_from(item.text.len())
                .map_err(|_| ContextViewError::ArithmeticOverflow)?,
            estimated_tokens: item.estimated_tokens,
            digest: Sha256::digest(item.text.as_bytes()).into(),
        })
    }

    pub(crate) fn from_stored(
        item: u64,
        turn: u64,
        role: ContextViewRole,
        byte_len: u64,
        estimated_tokens: u64,
        digest: [u8; 32],
    ) -> Result<Self, ContextViewError> {
        if item == 0
            || turn == 0
            || byte_len == 0
            || byte_len > crate::model::MAX_ITEM_TEXT_BYTES as u64
            || estimated_tokens == 0
        {
            return Err(ContextViewError::InvalidStoredView);
        }
        Ok(Self {
            item,
            turn,
            role,
            byte_len,
            estimated_tokens,
            digest,
        })
    }

    #[must_use]
    pub const fn item(&self) -> u64 {
        self.item
    }

    #[must_use]
    pub const fn turn(&self) -> u64 {
        self.turn
    }

    #[must_use]
    pub const fn role(&self) -> ContextViewRole {
        self.role
    }

    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    #[must_use]
    pub const fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }

    #[must_use]
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    #[must_use]
    pub fn digest_hex(&self) -> String {
        let mut encoded = String::with_capacity(64);
        for byte in self.digest {
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        encoded
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReducedContextView {
    source: ContextEventRange,
    artifacts: Vec<ContextArtifactRef>,
    recent_items: Vec<ContextViewItem>,
    raw_bytes: u64,
    estimated_tokens: u64,
}

impl ReducedContextView {
    pub fn from_items(
        head: LedgerHead,
        items: &[CanonicalItem],
        policy: ContextReductionPolicy,
    ) -> Result<Self, ContextViewError> {
        if items.len() > MAX_CONTEXT_VIEW_ITEMS {
            return Err(ContextViewError::TooManyItems);
        }
        let mut recent_start = items.len();
        let mut recent_raw_bytes = 0_u64;
        for (index, item) in items.iter().enumerate().rev() {
            if items.len() - index > policy.max_raw_items {
                break;
            }
            let item_bytes = u64::try_from(item.text().len())
                .map_err(|_| ContextViewError::ArithmeticOverflow)?;
            let Some(next_raw_bytes) = recent_raw_bytes.checked_add(item_bytes) else {
                return Err(ContextViewError::ArithmeticOverflow);
            };
            if next_raw_bytes > policy.max_raw_bytes as u64 {
                break;
            }
            recent_start = index;
            recent_raw_bytes = next_raw_bytes;
        }
        let artifacts = items[..recent_start]
            .iter()
            .map(|item| {
                ContextViewItem::from_item(item)
                    .and_then(|item| ContextArtifactRef::from_item(&item))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let recent_items = items[recent_start..]
            .iter()
            .map(ContextViewItem::from_item)
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_stored(ContextEventRange::from_head(head), artifacts, recent_items)
    }

    pub(crate) fn from_stored(
        source: ContextEventRange,
        artifacts: Vec<ContextArtifactRef>,
        recent_items: Vec<ContextViewItem>,
    ) -> Result<Self, ContextViewError> {
        let item_count = artifacts
            .len()
            .checked_add(recent_items.len())
            .ok_or(ContextViewError::ArithmeticOverflow)?;
        if item_count > MAX_CONTEXT_VIEW_ITEMS {
            return Err(ContextViewError::TooManyItems);
        }
        let mut last_item = 0_u64;
        let mut estimated_tokens = 0_u64;
        for artifact in &artifacts {
            if artifact.item <= last_item {
                return Err(ContextViewError::InvalidStoredView);
            }
            last_item = artifact.item;
            estimated_tokens = estimated_tokens
                .checked_add(artifact.estimated_tokens)
                .ok_or(ContextViewError::ArithmeticOverflow)?;
        }
        let mut raw_bytes = 0_u64;
        for item in &recent_items {
            if item.item <= last_item {
                return Err(ContextViewError::InvalidStoredView);
            }
            last_item = item.item;
            raw_bytes = raw_bytes
                .checked_add(
                    u64::try_from(item.text.len())
                        .map_err(|_| ContextViewError::ArithmeticOverflow)?,
                )
                .ok_or(ContextViewError::ArithmeticOverflow)?;
            estimated_tokens = estimated_tokens
                .checked_add(item.estimated_tokens)
                .ok_or(ContextViewError::ArithmeticOverflow)?;
        }
        if raw_bytes > MAX_CONTEXT_VIEW_BYTES as u64 {
            return Err(ContextViewError::ViewTooLarge);
        }
        Ok(Self {
            source,
            artifacts,
            recent_items,
            raw_bytes,
            estimated_tokens,
        })
    }

    pub(crate) fn validate_against_items(
        &self,
        authoritative: &[CanonicalItem],
    ) -> Result<(), ContextViewError> {
        if self.artifacts.len() + self.recent_items.len() != authoritative.len() {
            return Err(ContextViewError::ArtifactMismatch);
        }
        for (artifact, item) in self.artifacts.iter().zip(authoritative) {
            let projected = ContextViewItem::from_item(item)?;
            if *artifact != ContextArtifactRef::from_item(&projected)? {
                return Err(ContextViewError::ArtifactMismatch);
            }
        }
        for (stored, item) in self
            .recent_items
            .iter()
            .zip(&authoritative[self.artifacts.len()..])
        {
            if *stored != ContextViewItem::from_item(item)? {
                return Err(ContextViewError::ArtifactMismatch);
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn source(&self) -> ContextEventRange {
        self.source
    }

    #[must_use]
    pub fn artifacts(&self) -> &[ContextArtifactRef] {
        &self.artifacts
    }

    #[must_use]
    pub fn recent_items(&self) -> &[ContextViewItem] {
        &self.recent_items
    }

    #[must_use]
    pub const fn raw_bytes(&self) -> u64 {
        self.raw_bytes
    }

    #[must_use]
    pub const fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextViewError {
    TooManyItems,
    ViewTooLarge,
    ArithmeticOverflow,
    InvalidReductionPolicy,
    ArtifactMismatch,
    InvalidStoredView,
}

impl fmt::Display for ContextViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyItems => "Context View has too many Items",
            Self::ViewTooLarge => "Context View exceeds its byte boundary",
            Self::ArithmeticOverflow => "Context View arithmetic overflowed",
            Self::InvalidReductionPolicy => "Context reduction policy is outside its boundary",
            Self::ArtifactMismatch => {
                "Context artifact does not match the authoritative Context View"
            }
            Self::InvalidStoredView => "stored Context View is invalid",
        })
    }
}

impl Error for ContextViewError {}

pub struct ContextPressure;

impl ContextPressure {
    pub fn project(
        input: ContextPressureInput,
        policy: ContextPressurePolicy,
    ) -> Result<ContextPressureSnapshot, ContextPressureError> {
        if input.context_limit_tokens == Some(0) {
            return Err(ContextPressureError::InvalidContextLimit);
        }
        let unknown_reason = if input.context_limit_tokens.is_none() {
            Some(ContextPressureUnknownReason::MissingContextLimit)
        } else if input.used_tokens.is_none() {
            Some(ContextPressureUnknownReason::MissingUsedTokens)
        } else if input.output_reserve_tokens.is_none() {
            Some(ContextPressureUnknownReason::MissingOutputReserve)
        } else if input.accuracy.is_none() {
            Some(ContextPressureUnknownReason::MissingAccuracy)
        } else {
            None
        };
        if let Some(unknown_reason) = unknown_reason {
            return Ok(ContextPressureSnapshot {
                context_limit_tokens: input.context_limit_tokens,
                used_tokens: input.used_tokens,
                output_reserve_tokens: input.output_reserve_tokens,
                projected_tokens: None,
                occupancy_percent: None,
                accuracy: None,
                state: ContextPressureState::Unknown,
                admission: ContextAdmissionDecision::Unknown,
                unknown_reason: Some(unknown_reason),
                soft_threshold_percent: policy.soft_percent,
                hard_threshold_percent: policy.hard_percent,
            });
        }

        let context_limit_tokens = input
            .context_limit_tokens
            .expect("known projection requires a context limit");
        let used_tokens = input
            .used_tokens
            .expect("known projection requires used tokens");
        let output_reserve_tokens = input
            .output_reserve_tokens
            .expect("known projection requires an output reserve");
        let accuracy = input
            .accuracy
            .expect("known projection requires an accuracy marker");
        let projected_tokens = used_tokens
            .checked_add(output_reserve_tokens)
            .ok_or(ContextPressureError::ArithmeticOverflow)?;
        let scaled = u128::from(projected_tokens) * 100;
        let limit = u128::from(context_limit_tokens);
        let occupancy_percent = u8::try_from((scaled / limit).min(100))
            .expect("capped Context Pressure percent fits in u8");
        let hard_boundary = limit * u128::from(policy.hard_percent);
        let soft_boundary = limit * u128::from(policy.soft_percent);
        let (state, admission) = if scaled >= hard_boundary {
            (ContextPressureState::Hard, ContextAdmissionDecision::Stop)
        } else if scaled >= soft_boundary {
            (ContextPressureState::Soft, ContextAdmissionDecision::Reduce)
        } else {
            (
                ContextPressureState::Normal,
                ContextAdmissionDecision::Allow,
            )
        };
        Ok(ContextPressureSnapshot {
            context_limit_tokens: Some(context_limit_tokens),
            used_tokens: Some(used_tokens),
            output_reserve_tokens: Some(output_reserve_tokens),
            projected_tokens: Some(projected_tokens),
            occupancy_percent: Some(occupancy_percent),
            accuracy: Some(accuracy),
            state,
            admission,
            unknown_reason: None,
            soft_threshold_percent: policy.soft_percent,
            hard_threshold_percent: policy.hard_percent,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextPressureError {
    InvalidThresholds,
    InvalidContextLimit,
    ArithmeticOverflow,
}

impl fmt::Display for ContextPressureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidThresholds => {
                "Context Pressure thresholds must satisfy 0 < soft < hard <= 100"
            }
            Self::InvalidContextLimit => "Context Pressure limit must be greater than zero",
            Self::ArithmeticOverflow => "Context Pressure token arithmetic overflowed",
        })
    }
}

impl Error for ContextPressureError {}
