//! Deterministic Context Pressure projection and admission policy.
//!
//! This module only projects immutable token facts. It does not compact
//! Context Views, mutate the Event Ledger, or hold Tool, credential, or Memory
//! authority.

use std::error::Error;
use std::fmt;

use serde::Serialize;

pub const DEFAULT_SOFT_CONTEXT_PRESSURE_PERCENT: u8 = 65;
pub const DEFAULT_HARD_CONTEXT_PRESSURE_PERCENT: u8 = 90;

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
