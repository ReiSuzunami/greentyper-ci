//! Normalized inference attempts, pinned usage windows, and cached rollups.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use jiff::civil::Weekday as JiffWeekday;
use jiff::tz::TimeZone;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::pricing::{
    COST_SCALE_DECIMAL_PLACES, CostEstimate, CostEstimateOutcome, CostEstimateUnknownReason,
};
use crate::provider::{ProviderDialect, UsageAccuracy, UsageRecord};

const MAX_USAGE_WINDOW_ID_BYTES: usize = 64;
pub const MAX_USAGE_WINDOWS: usize = 128;
pub const MAX_USAGE_PAGE_SIZE: usize = 1_000;
const MAX_USAGE_CURSOR_BYTES: usize = 160;
const MILLIS_PER_HOUR: i64 = 60 * 60 * 1_000;
const MILLIS_PER_DAY: i64 = 24 * MILLIS_PER_HOUR;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct UsageTimestamp(i64);

impl UsageTimestamp {
    pub fn from_unix_millis(value: i64) -> Result<Self, UsageError> {
        Timestamp::from_millisecond(value).map_err(|_| UsageError::TimestampRange)?;
        Ok(Self(value))
    }

    pub fn now() -> Result<Self, UsageError> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| UsageError::ClockBeforeUnixEpoch)?;
        let millis = i64::try_from(duration.as_millis()).map_err(|_| UsageError::TimestampRange)?;
        Self::from_unix_millis(millis)
    }

    #[must_use]
    pub const fn unix_millis(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageWeekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl UsageWeekday {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mon => "mon",
            Self::Tue => "tue",
            Self::Wed => "wed",
            Self::Thu => "thu",
            Self::Fri => "fri",
            Self::Sat => "sat",
            Self::Sun => "sun",
        }
    }

    const fn from_jiff(value: JiffWeekday) -> Self {
        match value {
            JiffWeekday::Monday => Self::Mon,
            JiffWeekday::Tuesday => Self::Tue,
            JiffWeekday::Wednesday => Self::Wed,
            JiffWeekday::Thursday => Self::Thu,
            JiffWeekday::Friday => Self::Fri,
            JiffWeekday::Saturday => Self::Sat,
            JiffWeekday::Sunday => Self::Sun,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct UsageWindow {
    id: String,
    start_minute: u16,
    end_minute: u16,
    days: BTreeSet<UsageWeekday>,
    timezone: String,
    timezone_source: UsageTimezoneSource,
    ruleset_version: String,
}

impl UsageWindow {
    pub fn resolve(
        id: impl Into<String>,
        start: &str,
        end: &str,
        days: impl IntoIterator<Item = UsageWeekday>,
        timezone: &str,
    ) -> Result<Self, UsageError> {
        let id = id.into();
        validate_window_id(&id)?;
        let start_minute = parse_local_time(start)?;
        let end_minute = parse_local_time(end)?;
        if start_minute == end_minute {
            return Err(UsageError::EmptyWindow);
        }
        let days = days.into_iter().collect::<BTreeSet<_>>();
        if days.is_empty() {
            return Err(UsageError::MissingDays);
        }
        let (timezone, timezone_source) = resolve_timezone(timezone)?;
        let ruleset_version = current_ruleset_version()?.to_owned();
        Ok(Self {
            id,
            start_minute,
            end_minute,
            days,
            timezone,
            timezone_source,
            ruleset_version,
        })
    }

    pub(crate) fn from_resolved_parts(
        id: String,
        start_minute: u16,
        end_minute: u16,
        days: BTreeSet<UsageWeekday>,
        timezone: String,
        timezone_source: UsageTimezoneSource,
        ruleset_version: String,
    ) -> Result<Self, UsageError> {
        validate_window_id(&id)?;
        if start_minute >= 24 * 60 || end_minute >= 24 * 60 {
            return Err(UsageError::InvalidLocalTime);
        }
        if start_minute == end_minute {
            return Err(UsageError::EmptyWindow);
        }
        if days.is_empty() {
            return Err(UsageError::MissingDays);
        }
        let (canonical, _) = bundled_timezone(&timezone)?;
        if canonical != timezone {
            return Err(UsageError::NonCanonicalTimezone);
        }
        if ruleset_version.is_empty() {
            return Err(UsageError::MissingRulesetVersion);
        }
        Ok(Self {
            id,
            start_minute,
            end_minute,
            days,
            timezone,
            timezone_source,
            ruleset_version,
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn start_minute(&self) -> u16 {
        self.start_minute
    }

    #[must_use]
    pub const fn end_minute(&self) -> u16 {
        self.end_minute
    }

    pub fn days(&self) -> impl ExactSizeIterator<Item = UsageWeekday> + '_ {
        self.days.iter().copied()
    }

    #[must_use]
    pub fn timezone(&self) -> &str {
        &self.timezone
    }

    #[must_use]
    pub const fn timezone_source(&self) -> UsageTimezoneSource {
        self.timezone_source
    }

    #[must_use]
    pub fn ruleset_version(&self) -> &str {
        &self.ruleset_version
    }

    pub fn require_current_ruleset(&self) -> Result<(), UsageError> {
        if self.ruleset_version == current_ruleset_version()? {
            Ok(())
        } else {
            Err(UsageError::RulesetVersionMismatch)
        }
    }

    pub fn contains(&self, timestamp: UsageTimestamp) -> Result<bool, UsageError> {
        self.require_current_ruleset()?;
        let (_, data) = bundled_timezone(&self.timezone)?;
        let timezone =
            TimeZone::tzif(&self.timezone, data).map_err(|_| UsageError::InvalidTimezoneData)?;
        let timestamp = Timestamp::from_millisecond(timestamp.unix_millis())
            .map_err(|_| UsageError::TimestampRange)?;
        let local = timestamp.to_zoned(timezone).datetime();
        let minute = u16::try_from(local.hour()).expect("civil hour is nonnegative") * 60
            + u16::try_from(local.minute()).expect("civil minute is nonnegative");
        let (inside, start_day) = if self.start_minute < self.end_minute {
            (
                minute >= self.start_minute && minute < self.end_minute,
                local.weekday(),
            )
        } else if minute >= self.start_minute {
            (true, local.weekday())
        } else if minute < self.end_minute {
            let yesterday = local
                .date()
                .yesterday()
                .map_err(|_| UsageError::TimestampRange)?;
            (true, yesterday.weekday())
        } else {
            (false, local.weekday())
        };
        Ok(inside && self.days.contains(&UsageWeekday::from_jiff(start_day)))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageTimezoneSource {
    Explicit,
    LocalSystem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageAttemptOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCostProvenance {
    Unknown,
    PriceSchedule,
}

impl UsageCostProvenance {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::PriceSchedule => "price_schedule",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageAttempt {
    attempt: u32,
    thread: u64,
    turn: u64,
    agent: Option<u64>,
    provider_profile: String,
    requested_model: String,
    observed_model: Option<String>,
    dialect: Option<ProviderDialect>,
    requested_reasoning_effort: Option<String>,
    observed_reasoning_effort: Option<String>,
    requested_service_tier: Option<String>,
    observed_service_tier: Option<String>,
    started_at: Option<UsageTimestamp>,
    completed_at: Option<UsageTimestamp>,
    outcome: UsageAttemptOutcome,
    usage: Option<UsageRecord>,
    cost_provenance: UsageCostProvenance,
    #[serde(rename = "payg_cost_estimate")]
    cost_estimate: Option<CostEstimate>,
    cost_unknown_reason: Option<CostEstimateUnknownReason>,
    named_windows: Vec<UsageWindow>,
}

impl UsageAttempt {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        attempt: u32,
        thread: u64,
        turn: u64,
        agent: Option<u64>,
        provider_profile: String,
        requested_model: String,
        dialect: Option<ProviderDialect>,
        started_at: Option<UsageTimestamp>,
        completed_at: Option<UsageTimestamp>,
        outcome: UsageAttemptOutcome,
        usage: Option<UsageRecord>,
        named_windows: Vec<UsageWindow>,
    ) -> Result<Self, UsageError> {
        if attempt == 0 || thread == 0 || turn == 0 {
            return Err(UsageError::InvalidAttemptIdentity);
        }
        if started_at
            .zip(completed_at)
            .is_some_and(|(start, end)| end < start)
        {
            return Err(UsageError::CompletionBeforeStart);
        }
        let mut seen = BTreeSet::new();
        if named_windows.iter().any(|window| !seen.insert(window.id())) {
            return Err(UsageError::DuplicateWindow);
        }
        Ok(Self {
            attempt,
            thread,
            turn,
            agent,
            provider_profile,
            requested_model,
            observed_model: None,
            dialect,
            requested_reasoning_effort: None,
            observed_reasoning_effort: None,
            requested_service_tier: None,
            observed_service_tier: usage
                .as_ref()
                .and_then(UsageRecord::service_tier)
                .map(str::to_owned),
            started_at,
            completed_at,
            outcome,
            usage,
            cost_provenance: UsageCostProvenance::Unknown,
            cost_estimate: None,
            cost_unknown_reason: None,
            named_windows,
        })
    }

    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    #[must_use]
    pub const fn thread(&self) -> u64 {
        self.thread
    }

    #[must_use]
    pub const fn turn(&self) -> u64 {
        self.turn
    }

    #[must_use]
    pub const fn agent(&self) -> Option<u64> {
        self.agent
    }

    #[must_use]
    pub fn provider_profile(&self) -> &str {
        &self.provider_profile
    }

    #[must_use]
    pub fn requested_model(&self) -> &str {
        &self.requested_model
    }

    #[must_use]
    pub fn observed_model(&self) -> Option<&str> {
        self.observed_model.as_deref()
    }

    #[must_use]
    pub const fn dialect(&self) -> Option<ProviderDialect> {
        self.dialect
    }

    #[must_use]
    pub fn requested_reasoning_effort(&self) -> Option<&str> {
        self.requested_reasoning_effort.as_deref()
    }

    #[must_use]
    pub fn observed_reasoning_effort(&self) -> Option<&str> {
        self.observed_reasoning_effort.as_deref()
    }

    #[must_use]
    pub fn requested_service_tier(&self) -> Option<&str> {
        self.requested_service_tier.as_deref()
    }

    #[must_use]
    pub fn observed_service_tier(&self) -> Option<&str> {
        self.observed_service_tier.as_deref()
    }

    #[must_use]
    pub const fn started_at(&self) -> Option<UsageTimestamp> {
        self.started_at
    }

    #[must_use]
    pub const fn completed_at(&self) -> Option<UsageTimestamp> {
        self.completed_at
    }

    #[must_use]
    pub const fn outcome(&self) -> UsageAttemptOutcome {
        self.outcome
    }

    #[must_use]
    pub const fn usage(&self) -> Option<&UsageRecord> {
        self.usage.as_ref()
    }

    #[must_use]
    pub const fn cost_provenance(&self) -> UsageCostProvenance {
        self.cost_provenance
    }

    #[must_use]
    pub const fn cost_estimate(&self) -> Option<&CostEstimate> {
        self.cost_estimate.as_ref()
    }

    #[must_use]
    pub const fn cost_unknown_reason(&self) -> Option<CostEstimateUnknownReason> {
        self.cost_unknown_reason
    }

    fn record_cost_evaluation(&mut self, outcome: CostEstimateOutcome) -> Result<(), UsageError> {
        if self.cost_estimate.is_some() || self.cost_unknown_reason.is_some() {
            return Err(UsageError::DuplicateCostEvaluation);
        }
        match outcome {
            CostEstimateOutcome::Known(estimate) => {
                self.cost_provenance = UsageCostProvenance::PriceSchedule;
                self.cost_estimate = Some(*estimate);
            }
            CostEstimateOutcome::Unknown(reason) => {
                self.cost_unknown_reason = Some(reason);
            }
        }
        Ok(())
    }

    pub fn named_windows(&self) -> impl ExactSizeIterator<Item = &UsageWindow> {
        self.named_windows.iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageQuantity {
    exact: Option<u64>,
    estimated: Option<u64>,
    unknown_records: u64,
    overflowed: bool,
}

impl Default for UsageQuantity {
    fn default() -> Self {
        Self {
            exact: Some(0),
            estimated: Some(0),
            unknown_records: 0,
            overflowed: false,
        }
    }
}

impl UsageQuantity {
    #[must_use]
    pub const fn exact(&self) -> Option<u64> {
        self.exact
    }

    #[must_use]
    pub const fn estimated(&self) -> Option<u64> {
        self.estimated
    }

    #[must_use]
    pub const fn unknown_records(&self) -> u64 {
        self.unknown_records
    }

    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn observe(&mut self, value: Option<u64>, accuracy: UsageAccuracy) {
        let Some(value) = value else {
            self.unknown_records = self.unknown_records.saturating_add(1);
            return;
        };
        let target = match accuracy {
            UsageAccuracy::Exact => &mut self.exact,
            UsageAccuracy::Estimated => &mut self.estimated,
        };
        let next = target.and_then(|current| current.checked_add(value));
        if next.is_none() {
            self.overflowed = true;
        }
        *target = next;
    }

    fn merge(&mut self, other: &Self) {
        self.exact = self
            .exact
            .zip(other.exact)
            .and_then(|(a, b)| a.checked_add(b));
        self.estimated = self
            .estimated
            .zip(other.estimated)
            .and_then(|(a, b)| a.checked_add(b));
        self.unknown_records = self.unknown_records.saturating_add(other.unknown_records);
        self.overflowed =
            self.overflowed || other.overflowed || self.exact.is_none() || self.estimated.is_none();
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CostQuantity {
    scale_decimal_places: u8,
    exact_pico_units: Option<u64>,
    estimated_pico_units: Option<u64>,
    records: u64,
    overflowed: bool,
}

impl Default for CostQuantity {
    fn default() -> Self {
        Self {
            scale_decimal_places: COST_SCALE_DECIMAL_PLACES,
            exact_pico_units: Some(0),
            estimated_pico_units: Some(0),
            records: 0,
            overflowed: false,
        }
    }
}

impl CostQuantity {
    #[must_use]
    pub const fn scale_decimal_places(&self) -> u8 {
        self.scale_decimal_places
    }

    #[must_use]
    pub const fn exact_pico_units(&self) -> Option<u64> {
        self.exact_pico_units
    }

    #[must_use]
    pub const fn estimated_pico_units(&self) -> Option<u64> {
        self.estimated_pico_units
    }

    #[must_use]
    pub const fn records(&self) -> u64 {
        self.records
    }

    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn observe(&mut self, amount_pico_units: u64, accuracy: UsageAccuracy) {
        self.records = self.records.saturating_add(1);
        let target = match accuracy {
            UsageAccuracy::Exact => &mut self.exact_pico_units,
            UsageAccuracy::Estimated => &mut self.estimated_pico_units,
        };
        let next = target.and_then(|current| current.checked_add(amount_pico_units));
        if next.is_none() {
            self.overflowed = true;
        }
        *target = next;
    }

    fn merge(&mut self, other: &Self) {
        self.exact_pico_units = self
            .exact_pico_units
            .zip(other.exact_pico_units)
            .and_then(|(left, right)| left.checked_add(right));
        self.estimated_pico_units = self
            .estimated_pico_units
            .zip(other.estimated_pico_units)
            .and_then(|(left, right)| left.checked_add(right));
        self.records = self.records.saturating_add(other.records);
        self.overflowed = self.overflowed
            || other.overflowed
            || self.exact_pico_units.is_none()
            || self.estimated_pico_units.is_none();
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct UsageDistribution {
    values: BTreeMap<String, u64>,
    unknown: u64,
}

impl UsageDistribution {
    #[must_use]
    pub const fn values(&self) -> &BTreeMap<String, u64> {
        &self.values
    }

    #[must_use]
    pub const fn unknown(&self) -> u64 {
        self.unknown
    }

    fn observe(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                let count = self.values.entry(value.to_owned()).or_default();
                *count = count.saturating_add(1);
            }
            None => self.unknown = self.unknown.saturating_add(1),
        }
    }

    fn merge(&mut self, other: &Self) {
        for (value, count) in &other.values {
            let total = self.values.entry(value.clone()).or_default();
            *total = total.saturating_add(*count);
        }
        self.unknown = self.unknown.saturating_add(other.unknown);
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct UsageRollup {
    attempts: u64,
    succeeded: u64,
    failed: u64,
    interrupted: u64,
    usage_records: u64,
    input_tokens: UsageQuantity,
    cached_input_tokens: UsageQuantity,
    cache_write_input_tokens: UsageQuantity,
    output_tokens: UsageQuantity,
    reasoning_output_tokens: UsageQuantity,
    total_tokens: UsageQuantity,
    provider_profiles: UsageDistribution,
    requested_models: UsageDistribution,
    observed_models: UsageDistribution,
    dialects: UsageDistribution,
    requested_reasoning_efforts: UsageDistribution,
    observed_reasoning_efforts: UsageDistribution,
    requested_service_tiers: UsageDistribution,
    service_tiers: UsageDistribution,
    payg_cost_estimates: BTreeMap<String, CostQuantity>,
    cost_unknown_attempts: u64,
}

impl UsageRollup {
    #[must_use]
    pub const fn attempts(&self) -> u64 {
        self.attempts
    }

    #[must_use]
    pub const fn succeeded(&self) -> u64 {
        self.succeeded
    }

    #[must_use]
    pub const fn failed(&self) -> u64 {
        self.failed
    }

    #[must_use]
    pub const fn interrupted(&self) -> u64 {
        self.interrupted
    }

    #[must_use]
    pub const fn usage_records(&self) -> u64 {
        self.usage_records
    }

    #[must_use]
    pub const fn input_tokens(&self) -> &UsageQuantity {
        &self.input_tokens
    }

    #[must_use]
    pub const fn cached_input_tokens(&self) -> &UsageQuantity {
        &self.cached_input_tokens
    }

    #[must_use]
    pub const fn cache_write_input_tokens(&self) -> &UsageQuantity {
        &self.cache_write_input_tokens
    }

    #[must_use]
    pub const fn output_tokens(&self) -> &UsageQuantity {
        &self.output_tokens
    }

    #[must_use]
    pub const fn reasoning_output_tokens(&self) -> &UsageQuantity {
        &self.reasoning_output_tokens
    }

    #[must_use]
    pub const fn total_tokens(&self) -> &UsageQuantity {
        &self.total_tokens
    }

    #[must_use]
    pub const fn provider_profiles(&self) -> &UsageDistribution {
        &self.provider_profiles
    }

    #[must_use]
    pub const fn requested_models(&self) -> &UsageDistribution {
        &self.requested_models
    }

    #[must_use]
    pub const fn observed_models(&self) -> &UsageDistribution {
        &self.observed_models
    }

    #[must_use]
    pub const fn dialects(&self) -> &UsageDistribution {
        &self.dialects
    }

    #[must_use]
    pub const fn requested_reasoning_efforts(&self) -> &UsageDistribution {
        &self.requested_reasoning_efforts
    }

    #[must_use]
    pub const fn observed_reasoning_efforts(&self) -> &UsageDistribution {
        &self.observed_reasoning_efforts
    }

    #[must_use]
    pub const fn requested_service_tiers(&self) -> &UsageDistribution {
        &self.requested_service_tiers
    }

    #[must_use]
    pub const fn service_tiers(&self) -> &UsageDistribution {
        &self.service_tiers
    }

    #[must_use]
    pub const fn cost_unknown_attempts(&self) -> u64 {
        self.cost_unknown_attempts
    }

    #[must_use]
    pub const fn payg_cost_estimates(&self) -> &BTreeMap<String, CostQuantity> {
        &self.payg_cost_estimates
    }

    fn observe(&mut self, attempt: &UsageAttempt) {
        self.attempts = self.attempts.saturating_add(1);
        match attempt.outcome {
            UsageAttemptOutcome::Succeeded => self.succeeded = self.succeeded.saturating_add(1),
            UsageAttemptOutcome::Failed => self.failed = self.failed.saturating_add(1),
            UsageAttemptOutcome::Interrupted => {
                self.interrupted = self.interrupted.saturating_add(1);
            }
        }
        self.provider_profiles
            .observe(Some(&attempt.provider_profile));
        self.requested_models
            .observe(Some(&attempt.requested_model));
        self.observed_models.observe(attempt.observed_model());
        self.dialects
            .observe(attempt.dialect.map(ProviderDialect::as_str));
        self.requested_reasoning_efforts
            .observe(attempt.requested_reasoning_effort());
        self.observed_reasoning_efforts
            .observe(attempt.observed_reasoning_effort());
        self.requested_service_tiers
            .observe(attempt.requested_service_tier());
        self.cost_unknown_attempts = self.cost_unknown_attempts.saturating_add(1);
        let Some(usage) = &attempt.usage else {
            for quantity in [
                &mut self.input_tokens,
                &mut self.cached_input_tokens,
                &mut self.cache_write_input_tokens,
                &mut self.output_tokens,
                &mut self.reasoning_output_tokens,
                &mut self.total_tokens,
            ] {
                quantity.observe(None, UsageAccuracy::Exact);
            }
            self.service_tiers.observe(None);
            return;
        };
        self.usage_records = self.usage_records.saturating_add(1);
        let accuracy = usage.accuracy();
        self.input_tokens.observe(usage.input_tokens(), accuracy);
        self.cached_input_tokens
            .observe(usage.cached_input_tokens(), accuracy);
        self.cache_write_input_tokens
            .observe(usage.cache_write_input_tokens(), accuracy);
        self.output_tokens.observe(usage.output_tokens(), accuracy);
        self.reasoning_output_tokens
            .observe(usage.reasoning_output_tokens(), accuracy);
        self.total_tokens.observe(usage.total_tokens(), accuracy);
        self.service_tiers.observe(usage.service_tier());
    }

    fn observe_cost_evaluation(&mut self, outcome: &CostEstimateOutcome) {
        if let CostEstimateOutcome::Known(estimate) = outcome {
            self.cost_unknown_attempts = self.cost_unknown_attempts.saturating_sub(1);
            self.payg_cost_estimates
                .entry(estimate.currency().to_owned())
                .or_default()
                .observe(estimate.amount_pico_units(), estimate.usage_accuracy());
        }
    }

    fn merge(&mut self, other: &Self) {
        self.attempts = self.attempts.saturating_add(other.attempts);
        self.succeeded = self.succeeded.saturating_add(other.succeeded);
        self.failed = self.failed.saturating_add(other.failed);
        self.interrupted = self.interrupted.saturating_add(other.interrupted);
        self.usage_records = self.usage_records.saturating_add(other.usage_records);
        self.input_tokens.merge(&other.input_tokens);
        self.cached_input_tokens.merge(&other.cached_input_tokens);
        self.cache_write_input_tokens
            .merge(&other.cache_write_input_tokens);
        self.output_tokens.merge(&other.output_tokens);
        self.reasoning_output_tokens
            .merge(&other.reasoning_output_tokens);
        self.total_tokens.merge(&other.total_tokens);
        self.provider_profiles.merge(&other.provider_profiles);
        self.requested_models.merge(&other.requested_models);
        self.observed_models.merge(&other.observed_models);
        self.dialects.merge(&other.dialects);
        self.requested_reasoning_efforts
            .merge(&other.requested_reasoning_efforts);
        self.observed_reasoning_efforts
            .merge(&other.observed_reasoning_efforts);
        self.requested_service_tiers
            .merge(&other.requested_service_tiers);
        self.service_tiers.merge(&other.service_tiers);
        for (currency, quantity) in &other.payg_cost_estimates {
            self.payg_cost_estimates
                .entry(currency.clone())
                .or_default()
                .merge(quantity);
        }
        self.cost_unknown_attempts = self
            .cost_unknown_attempts
            .saturating_add(other.cost_unknown_attempts);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScopedUsageRollup {
    id: u64,
    usage: UsageRollup,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NamedUsageRollup {
    window: UsageWindow,
    usage: UsageRollup,
}

impl NamedUsageRollup {
    #[must_use]
    pub const fn window(&self) -> &UsageWindow {
        &self.window
    }

    #[must_use]
    pub const fn usage(&self) -> &UsageRollup {
        &self.usage
    }
}

impl ScopedUsageRollup {
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub const fn usage(&self) -> &UsageRollup {
        &self.usage
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RollingUsageRollups {
    one_hour: UsageRollup,
    one_day: UsageRollup,
    seven_days: UsageRollup,
}

impl RollingUsageRollups {
    #[must_use]
    pub const fn one_hour(&self) -> &UsageRollup {
        &self.one_hour
    }

    #[must_use]
    pub const fn one_day(&self) -> &UsageRollup {
        &self.one_day
    }

    #[must_use]
    pub const fn seven_days(&self) -> &UsageRollup {
        &self.seven_days
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeUsageSnapshot {
    as_of: UsageTimestamp,
    attempts: Vec<UsageAttempt>,
    thread: Option<ScopedUsageRollup>,
    turns: Vec<ScopedUsageRollup>,
    agents: Vec<ScopedUsageRollup>,
    team: Option<UsageRollup>,
    rolling: RollingUsageRollups,
    named_windows: Vec<NamedUsageRollup>,
}

impl RuntimeUsageSnapshot {
    #[must_use]
    pub const fn as_of(&self) -> UsageTimestamp {
        self.as_of
    }

    #[must_use]
    pub fn attempts(&self) -> &[UsageAttempt] {
        &self.attempts
    }

    #[must_use]
    pub const fn thread(&self) -> Option<&ScopedUsageRollup> {
        self.thread.as_ref()
    }

    #[must_use]
    pub fn turns(&self) -> &[ScopedUsageRollup] {
        &self.turns
    }

    #[must_use]
    pub fn agents(&self) -> &[ScopedUsageRollup] {
        &self.agents
    }

    #[must_use]
    pub fn named_windows(&self) -> &[NamedUsageRollup] {
        &self.named_windows
    }

    #[must_use]
    pub const fn team(&self) -> Option<&UsageRollup> {
        self.team.as_ref()
    }

    #[must_use]
    pub const fn rolling(&self) -> &RollingUsageRollups {
        &self.rolling
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct UsageRevision {
    transaction: u64,
    sequence: u64,
}

impl UsageRevision {
    pub(crate) const fn new(transaction: u64, sequence: u64) -> Self {
        Self {
            transaction,
            sequence,
        }
    }

    #[must_use]
    pub const fn transaction(self) -> u64 {
        self.transaction
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct UsageCursor(String);

impl UsageCursor {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn encode(revision: UsageRevision, as_of: UsageTimestamp, next_index: usize) -> Self {
        let payload = format!(
            "v1:{}:{}:{}:{next_index}",
            revision.transaction,
            revision.sequence,
            as_of.unix_millis()
        );
        let digest = Sha256::digest(payload.as_bytes());
        let mut checksum = String::with_capacity(digest.len() * 2);
        for byte in digest {
            write!(&mut checksum, "{byte:02x}").expect("write cursor checksum");
        }
        Self(format!("{payload}:{checksum}"))
    }

    fn decode(&self) -> Result<UsageCursorState, UsageError> {
        if self.0.is_empty() || self.0.len() > MAX_USAGE_CURSOR_BYTES || !self.0.is_ascii() {
            return Err(UsageError::InvalidCursor);
        }
        let mut parts = self.0.split(':');
        let version = parts.next().ok_or(UsageError::InvalidCursor)?;
        let transaction = parse_cursor_part(parts.next())?;
        let sequence = parse_cursor_part(parts.next())?;
        let as_of = parts
            .next()
            .ok_or(UsageError::InvalidCursor)?
            .parse::<i64>()
            .map_err(|_| UsageError::InvalidCursor)?;
        let next_index = parts
            .next()
            .ok_or(UsageError::InvalidCursor)?
            .parse::<usize>()
            .map_err(|_| UsageError::InvalidCursor)?;
        let checksum = parts.next().ok_or(UsageError::InvalidCursor)?;
        if version != "v1" || parts.next().is_some() || checksum.len() != 64 {
            return Err(UsageError::InvalidCursor);
        }
        let payload = format!("v1:{transaction}:{sequence}:{as_of}:{next_index}");
        let digest = Sha256::digest(payload.as_bytes());
        let mut expected = String::with_capacity(digest.len() * 2);
        for byte in digest {
            write!(&mut expected, "{byte:02x}").expect("write cursor checksum");
        }
        if checksum != expected {
            return Err(UsageError::InvalidCursor);
        }
        Ok(UsageCursorState {
            revision: UsageRevision::new(transaction, sequence),
            as_of,
            next_index,
        })
    }
}

impl fmt::Display for UsageCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for UsageCursor {
    type Err = UsageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let cursor = Self(value.to_owned());
        cursor.decode()?;
        Ok(cursor)
    }
}

fn parse_cursor_part(value: Option<&str>) -> Result<u64, UsageError> {
    value
        .ok_or(UsageError::InvalidCursor)?
        .parse::<u64>()
        .map_err(|_| UsageError::InvalidCursor)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UsageCursorState {
    revision: UsageRevision,
    as_of: i64,
    next_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeUsageQuery {
    page_size: Option<usize>,
    cursor: Option<UsageCursor>,
}

impl RuntimeUsageQuery {
    #[must_use]
    pub const fn summary_only() -> Self {
        Self {
            page_size: None,
            cursor: None,
        }
    }

    pub fn page(page_size: usize, cursor: Option<UsageCursor>) -> Result<Self, UsageError> {
        if page_size == 0 || page_size > MAX_USAGE_PAGE_SIZE {
            return Err(UsageError::InvalidPageSize);
        }
        Ok(Self {
            page_size: Some(page_size),
            cursor,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeUsageSummary {
    as_of: UsageTimestamp,
    total: UsageRollup,
    thread: Option<ScopedUsageRollup>,
    team: Option<UsageRollup>,
    rolling: RollingUsageRollups,
    named_windows: Vec<NamedUsageRollup>,
}

impl RuntimeUsageSummary {
    #[must_use]
    pub const fn as_of(&self) -> UsageTimestamp {
        self.as_of
    }

    #[must_use]
    pub const fn total(&self) -> &UsageRollup {
        &self.total
    }

    #[must_use]
    pub const fn thread(&self) -> Option<&ScopedUsageRollup> {
        self.thread.as_ref()
    }

    #[must_use]
    pub const fn team(&self) -> Option<&UsageRollup> {
        self.team.as_ref()
    }

    #[must_use]
    pub const fn rolling(&self) -> &RollingUsageRollups {
        &self.rolling
    }

    #[must_use]
    pub fn named_windows(&self) -> &[NamedUsageRollup] {
        &self.named_windows
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageAttemptPage {
    attempts: Vec<UsageAttempt>,
    next_cursor: Option<UsageCursor>,
}

impl UsageAttemptPage {
    #[must_use]
    pub fn attempts(&self) -> &[UsageAttempt] {
        &self.attempts
    }

    #[must_use]
    pub const fn next_cursor(&self) -> Option<&UsageCursor> {
        self.next_cursor.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeUsageReport {
    revision: UsageRevision,
    summary: RuntimeUsageSummary,
    page: Option<UsageAttemptPage>,
}

impl RuntimeUsageReport {
    #[must_use]
    pub const fn revision(&self) -> UsageRevision {
        self.revision
    }

    #[must_use]
    pub const fn summary(&self) -> &RuntimeUsageSummary {
        &self.summary
    }

    #[must_use]
    pub const fn page(&self) -> Option<&UsageAttemptPage> {
        self.page.as_ref()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct UsageProjection {
    attempts: Vec<UsageAttempt>,
    total: UsageRollup,
    threads: BTreeMap<u64, UsageRollup>,
    turns: BTreeMap<u64, UsageRollup>,
    agents: BTreeMap<u64, UsageRollup>,
    named_windows: BTreeMap<UsageWindow, UsageRollup>,
    instants: BTreeMap<i64, UsageRollup>,
}

impl UsageProjection {
    pub(crate) fn attempt(&self, turn: u64, attempt: u32) -> Option<&UsageAttempt> {
        self.attempts
            .iter()
            .find(|candidate| candidate.turn == turn && candidate.attempt == attempt)
    }

    pub(crate) fn record(&mut self, attempt: UsageAttempt) -> Result<(), UsageError> {
        if self
            .attempts
            .iter()
            .any(|candidate| candidate.turn == attempt.turn && candidate.attempt == attempt.attempt)
        {
            return Err(UsageError::DuplicateAttempt);
        }
        self.total.observe(&attempt);
        self.threads
            .entry(attempt.thread)
            .or_default()
            .observe(&attempt);
        self.turns
            .entry(attempt.turn)
            .or_default()
            .observe(&attempt);
        if let Some(agent) = attempt.agent {
            self.agents.entry(agent).or_default().observe(&attempt);
        }
        for window in &attempt.named_windows {
            self.named_windows
                .entry(window.clone())
                .or_default()
                .observe(&attempt);
        }
        if let Some(started_at) = attempt.started_at {
            self.instants
                .entry(started_at.unix_millis())
                .or_default()
                .observe(&attempt);
        }
        self.attempts.push(attempt);
        Ok(())
    }

    pub(crate) fn record_cost_evaluation(
        &mut self,
        turn: u64,
        attempt: u32,
        outcome: CostEstimateOutcome,
    ) -> Result<(), UsageError> {
        let usage_attempt = self
            .attempts
            .iter_mut()
            .find(|candidate| candidate.turn == turn && candidate.attempt == attempt)
            .ok_or(UsageError::UnknownAttempt)?;
        usage_attempt.record_cost_evaluation(outcome.clone())?;
        let usage_attempt = usage_attempt.clone();
        self.total.observe_cost_evaluation(&outcome);
        self.threads
            .get_mut(&usage_attempt.thread)
            .ok_or(UsageError::UnknownAttempt)?
            .observe_cost_evaluation(&outcome);
        self.turns
            .get_mut(&usage_attempt.turn)
            .ok_or(UsageError::UnknownAttempt)?
            .observe_cost_evaluation(&outcome);
        if let Some(agent) = usage_attempt.agent {
            self.agents
                .get_mut(&agent)
                .ok_or(UsageError::UnknownAttempt)?
                .observe_cost_evaluation(&outcome);
        }
        for window in &usage_attempt.named_windows {
            self.named_windows
                .get_mut(window)
                .ok_or(UsageError::UnknownAttempt)?
                .observe_cost_evaluation(&outcome);
        }
        if let Some(started_at) = usage_attempt.started_at {
            self.instants
                .get_mut(&started_at.unix_millis())
                .ok_or(UsageError::UnknownAttempt)?
                .observe_cost_evaluation(&outcome);
        }
        Ok(())
    }

    pub(crate) fn snapshot(
        &self,
        thread: Option<u64>,
        as_of: UsageTimestamp,
    ) -> RuntimeUsageSnapshot {
        let RuntimeUsageSummary {
            as_of,
            total: _,
            thread,
            team,
            rolling,
            named_windows,
        } = self.summary(thread, as_of);
        RuntimeUsageSnapshot {
            as_of,
            attempts: self.attempts.clone(),
            thread,
            turns: scoped(&self.turns),
            agents: scoped(&self.agents),
            team,
            rolling,
            named_windows,
        }
    }

    pub(crate) fn report(
        &self,
        thread: Option<u64>,
        as_of: UsageTimestamp,
        revision: UsageRevision,
        query: RuntimeUsageQuery,
    ) -> Result<RuntimeUsageReport, UsageError> {
        let summary = self.summary(thread, as_of);
        let page = match query.page_size {
            None => None,
            Some(page_size) => {
                let start = match query.cursor {
                    None => 0,
                    Some(cursor) => {
                        let state = cursor.decode()?;
                        if state.revision != revision {
                            return Err(UsageError::StaleCursor);
                        }
                        if state.as_of != as_of.unix_millis() {
                            return Err(UsageError::CursorQueryMismatch);
                        }
                        if state.next_index > self.attempts.len() {
                            return Err(UsageError::InvalidCursor);
                        }
                        state.next_index
                    }
                };
                let end = start.saturating_add(page_size).min(self.attempts.len());
                let next_cursor =
                    (end < self.attempts.len()).then(|| UsageCursor::encode(revision, as_of, end));
                Some(UsageAttemptPage {
                    attempts: self.attempts[start..end].to_vec(),
                    next_cursor,
                })
            }
        };
        Ok(RuntimeUsageReport {
            revision,
            summary,
            page,
        })
    }

    fn summary(&self, thread: Option<u64>, as_of: UsageTimestamp) -> RuntimeUsageSummary {
        RuntimeUsageSummary {
            as_of,
            total: self.total.clone(),
            thread: thread.and_then(|id| {
                self.threads
                    .get(&id)
                    .cloned()
                    .map(|usage| ScopedUsageRollup { id, usage })
            }),
            team: (!self.agents.is_empty()).then(|| self.total.clone()),
            rolling: RollingUsageRollups {
                one_hour: self.rolling(as_of, MILLIS_PER_HOUR),
                one_day: self.rolling(as_of, MILLIS_PER_DAY),
                seven_days: self.rolling(as_of, 7 * MILLIS_PER_DAY),
            },
            named_windows: self
                .named_windows
                .iter()
                .map(|(window, usage)| NamedUsageRollup {
                    window: window.clone(),
                    usage: usage.clone(),
                })
                .collect(),
        }
    }

    fn rolling(&self, as_of: UsageTimestamp, duration: i64) -> UsageRollup {
        let start = as_of.unix_millis().saturating_sub(duration);
        let mut total = UsageRollup::default();
        for rollup in self
            .instants
            .range(start..as_of.unix_millis())
            .map(|(_, value)| value)
        {
            total.merge(rollup);
        }
        total
    }
}

fn scoped(values: &BTreeMap<u64, UsageRollup>) -> Vec<ScopedUsageRollup> {
    values
        .iter()
        .map(|(id, usage)| ScopedUsageRollup {
            id: *id,
            usage: usage.clone(),
        })
        .collect()
}

fn validate_window_id(id: &str) -> Result<(), UsageError> {
    if id.is_empty()
        || id.len() > MAX_USAGE_WINDOW_ID_BYTES
        || !id.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
        })
    {
        Err(UsageError::InvalidWindowId)
    } else {
        Ok(())
    }
}

fn parse_local_time(value: &str) -> Result<u16, UsageError> {
    let Some((hour, minute)) = value.split_once(':') else {
        return Err(UsageError::InvalidLocalTime);
    };
    if hour.len() != 2 || minute.len() != 2 {
        return Err(UsageError::InvalidLocalTime);
    }
    let hour = hour
        .parse::<u16>()
        .map_err(|_| UsageError::InvalidLocalTime)?;
    let minute = minute
        .parse::<u16>()
        .map_err(|_| UsageError::InvalidLocalTime)?;
    if hour > 23 || minute > 59 {
        return Err(UsageError::InvalidLocalTime);
    }
    Ok(hour * 60 + minute)
}

fn resolve_timezone(value: &str) -> Result<(String, UsageTimezoneSource), UsageError> {
    if value == "local" {
        let timezone = TimeZone::try_system().map_err(|_| UsageError::LocalTimezoneUnavailable)?;
        let name = timezone
            .iana_name()
            .ok_or(UsageError::LocalTimezoneUnavailable)?;
        let (canonical, _) = bundled_timezone(name)?;
        Ok((canonical.to_owned(), UsageTimezoneSource::LocalSystem))
    } else {
        let (canonical, _) = bundled_timezone(value)?;
        Ok((canonical.to_owned(), UsageTimezoneSource::Explicit))
    }
}

fn bundled_timezone(name: &str) -> Result<(&'static str, &'static [u8]), UsageError> {
    jiff_tzdb::get(name).ok_or(UsageError::UnknownTimezone)
}

fn current_ruleset_version() -> Result<&'static str, UsageError> {
    jiff_tzdb::VERSION.ok_or(UsageError::MissingRulesetVersion)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageError {
    InvalidWindowId,
    InvalidLocalTime,
    EmptyWindow,
    MissingDays,
    UnknownTimezone,
    NonCanonicalTimezone,
    LocalTimezoneUnavailable,
    MissingRulesetVersion,
    RulesetVersionMismatch,
    InvalidTimezoneData,
    ClockBeforeUnixEpoch,
    TimestampRange,
    InvalidAttemptIdentity,
    CompletionBeforeStart,
    DuplicateWindow,
    DuplicateAttempt,
    UnknownAttempt,
    DuplicateCostEvaluation,
    InvalidPageSize,
    InvalidCursor,
    StaleCursor,
    CursorQueryMismatch,
}

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidWindowId => "usage window ID is invalid",
            Self::InvalidLocalTime => "usage window time must use valid HH:MM",
            Self::EmptyWindow => "usage window cannot be empty",
            Self::MissingDays => "usage window requires at least one day",
            Self::UnknownTimezone => "usage window time zone is not in the pinned IANA database",
            Self::NonCanonicalTimezone => "usage window time zone is not canonical",
            Self::LocalTimezoneUnavailable => "local time zone could not be resolved to IANA",
            Self::MissingRulesetVersion => "pinned IANA ruleset has no version",
            Self::RulesetVersionMismatch => "usage window ruleset version is unavailable",
            Self::InvalidTimezoneData => "pinned IANA time-zone data is invalid",
            Self::ClockBeforeUnixEpoch => "system clock is before the Unix epoch",
            Self::TimestampRange => "usage timestamp is outside the supported range",
            Self::InvalidAttemptIdentity => "usage attempt identity is invalid",
            Self::CompletionBeforeStart => "usage attempt completed before it started",
            Self::DuplicateWindow => "usage attempt contains a duplicate named window",
            Self::DuplicateAttempt => "usage attempt identity was recorded more than once",
            Self::UnknownAttempt => "usage attempt identity is unknown",
            Self::DuplicateCostEvaluation => {
                "usage attempt Cost Estimate was recorded more than once"
            }
            Self::InvalidPageSize => "usage page size is outside the supported range",
            Self::InvalidCursor => "usage cursor is invalid",
            Self::StaleCursor => "usage cursor refers to a stale Ledger revision",
            Self::CursorQueryMismatch => "usage cursor does not match the requested instant",
        };
        formatter.write_str(message)
    }
}

impl Error for UsageError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(
        id: u32,
        started_at: i64,
        outcome: UsageAttemptOutcome,
        usage: Option<UsageRecord>,
        windows: &[&str],
    ) -> UsageAttempt {
        UsageAttempt::new(
            id,
            1,
            1,
            Some(7),
            "fixture".to_owned(),
            "model".to_owned(),
            Some(ProviderDialect::Responses),
            Some(UsageTimestamp::from_unix_millis(started_at).unwrap()),
            Some(UsageTimestamp::from_unix_millis(started_at + 1).unwrap()),
            outcome,
            usage,
            windows
                .iter()
                .map(|value| {
                    UsageWindow::resolve(
                        *value,
                        "00:00",
                        "23:59",
                        [
                            UsageWeekday::Mon,
                            UsageWeekday::Tue,
                            UsageWeekday::Wed,
                            UsageWeekday::Thu,
                            UsageWeekday::Fri,
                            UsageWeekday::Sat,
                            UsageWeekday::Sun,
                        ],
                        "Etc/UTC",
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn projection_separates_exact_estimated_unknown_and_overflow() {
        let exact_max = UsageRecord::new(
            Some(u64::MAX),
            None,
            None,
            Some(1),
            None,
            None,
            Some("standard".to_owned()),
        )
        .unwrap();
        let exact_one = UsageRecord::new(Some(1), None, None, None, None, None, None).unwrap();
        let estimated = UsageRecord::estimated(3, 2);
        let mut projection = UsageProjection::default();
        projection
            .record(attempt(
                1,
                1_000,
                UsageAttemptOutcome::Succeeded,
                Some(exact_max),
                &["workday"],
            ))
            .unwrap();
        projection
            .record(attempt(
                2,
                2_000,
                UsageAttemptOutcome::Succeeded,
                Some(exact_one),
                &["workday"],
            ))
            .unwrap();
        projection
            .record(attempt(
                3,
                3_000,
                UsageAttemptOutcome::Succeeded,
                Some(estimated),
                &[],
            ))
            .unwrap();
        projection
            .record(attempt(4, 4_000, UsageAttemptOutcome::Failed, None, &[]))
            .unwrap();

        let snapshot =
            projection.snapshot(Some(1), UsageTimestamp::from_unix_millis(5_000).unwrap());
        let total = snapshot.thread().unwrap().usage();
        assert_eq!(total.attempts, 4);
        assert_eq!(total.input_tokens.exact(), None);
        assert_eq!(total.input_tokens.estimated(), Some(3));
        assert_eq!(total.input_tokens.unknown_records(), 1);
        assert_eq!(snapshot.named_windows[0].usage.attempts, 2);
        assert_eq!(snapshot.agents[0].id, 7);
        assert_eq!(snapshot.team().unwrap().attempts, 4);
    }

    #[test]
    fn rolling_projection_uses_attempt_start_and_half_open_cutoffs() {
        let mut projection = UsageProjection::default();
        let now = 10 * MILLIS_PER_HOUR;
        for (id, started_at) in [
            (1, now - MILLIS_PER_HOUR),
            (2, now - MILLIS_PER_HOUR + 1),
            (3, now - 1),
            (4, now),
        ] {
            projection
                .record(attempt(
                    id,
                    started_at,
                    UsageAttemptOutcome::Succeeded,
                    Some(UsageRecord::estimated(1, 1)),
                    &[],
                ))
                .unwrap();
        }
        let snapshot = projection.snapshot(Some(1), UsageTimestamp::from_unix_millis(now).unwrap());
        assert_eq!(snapshot.rolling.one_hour.attempts, 3);
        assert_eq!(snapshot.rolling.one_day.attempts, 3);
    }

    #[test]
    fn thread_rollup_excludes_other_threads() {
        let mut projection = UsageProjection::default();
        projection
            .record(attempt(
                1,
                1_000,
                UsageAttemptOutcome::Succeeded,
                Some(UsageRecord::estimated(1, 1)),
                &[],
            ))
            .unwrap();
        let mut other_thread = attempt(
            2,
            2_000,
            UsageAttemptOutcome::Succeeded,
            Some(UsageRecord::estimated(2, 2)),
            &[],
        );
        other_thread.thread = 2;
        projection.record(other_thread).unwrap();

        let as_of = UsageTimestamp::from_unix_millis(3_000).unwrap();
        assert_eq!(
            projection
                .snapshot(Some(1), as_of)
                .thread()
                .unwrap()
                .usage()
                .attempts(),
            1
        );
        assert_eq!(
            projection
                .snapshot(Some(2), as_of)
                .thread()
                .unwrap()
                .usage()
                .attempts(),
            1
        );
        assert_eq!(projection.snapshot(None, as_of).attempts().len(), 2);
    }

    #[test]
    fn same_named_window_with_changed_rules_keeps_separate_provenance() {
        let first = UsageWindow::resolve(
            "workday",
            "09:00",
            "17:00",
            [UsageWeekday::Mon],
            "Asia/Hong_Kong",
        )
        .unwrap();
        let second = UsageWindow::resolve(
            "workday",
            "10:00",
            "18:00",
            [UsageWeekday::Mon],
            "Asia/Hong_Kong",
        )
        .unwrap();
        assert_eq!(
            UsageAttempt::new(
                1,
                1,
                1,
                Some(7),
                "fixture".to_owned(),
                "model".to_owned(),
                Some(ProviderDialect::Responses),
                Some(UsageTimestamp::from_unix_millis(1).unwrap()),
                Some(UsageTimestamp::from_unix_millis(2).unwrap()),
                UsageAttemptOutcome::Succeeded,
                Some(UsageRecord::estimated(1, 1)),
                vec![first.clone(), second.clone()],
            ),
            Err(UsageError::DuplicateWindow)
        );
        let mut projection = UsageProjection::default();
        for (id, window) in [(1, first), (2, second)] {
            projection
                .record(
                    UsageAttempt::new(
                        id,
                        1,
                        1,
                        Some(7),
                        "fixture".to_owned(),
                        "model".to_owned(),
                        Some(ProviderDialect::Responses),
                        Some(UsageTimestamp::from_unix_millis(i64::from(id)).unwrap()),
                        Some(UsageTimestamp::from_unix_millis(i64::from(id) + 1).unwrap()),
                        UsageAttemptOutcome::Succeeded,
                        Some(UsageRecord::estimated(1, 1)),
                        vec![window],
                    )
                    .unwrap(),
                )
                .unwrap();
        }

        let snapshot = projection.snapshot(Some(1), UsageTimestamp::from_unix_millis(10).unwrap());
        assert_eq!(snapshot.named_windows().len(), 2);
        assert_eq!(snapshot.named_windows()[0].window().id(), "workday");
        assert_ne!(
            snapshot.named_windows()[0].window().start_minute(),
            snapshot.named_windows()[1].window().start_minute()
        );
    }

    #[test]
    fn cached_rollups_match_source_attempts_for_all_small_combinations() {
        for pattern in 0_u16..256 {
            let mut projection = UsageProjection::default();
            let mut exact = 0_u64;
            let mut estimated = 0_u64;
            let mut unknown = 0_u64;
            let mut succeeded = 0_u64;
            let mut failed = 0_u64;
            let mut usage_records = 0_u64;
            for index in 0_u32..4 {
                let value = u64::from(index + 1);
                let kind = (pattern >> (index * 2)) & 0b11;
                let (outcome, usage) = match kind {
                    0 => {
                        exact += value;
                        usage_records += 1;
                        (
                            UsageAttemptOutcome::Succeeded,
                            Some(
                                UsageRecord::new(Some(value), None, None, None, None, None, None)
                                    .unwrap(),
                            ),
                        )
                    }
                    1 => {
                        estimated += value;
                        usage_records += 1;
                        (
                            UsageAttemptOutcome::Succeeded,
                            Some(UsageRecord::estimated(u32::try_from(value).unwrap(), 0)),
                        )
                    }
                    2 => {
                        unknown += 1;
                        usage_records += 1;
                        (
                            UsageAttemptOutcome::Succeeded,
                            Some(
                                UsageRecord::new(None, None, None, None, None, None, None).unwrap(),
                            ),
                        )
                    }
                    _ => {
                        unknown += 1;
                        (UsageAttemptOutcome::Failed, None)
                    }
                };
                if outcome == UsageAttemptOutcome::Succeeded {
                    succeeded += 1;
                } else {
                    failed += 1;
                }
                projection
                    .record(attempt(index + 1, i64::from(index), outcome, usage, &[]))
                    .unwrap();
            }

            let snapshot =
                projection.snapshot(Some(1), UsageTimestamp::from_unix_millis(10).unwrap());
            let total = snapshot.thread().unwrap().usage();
            assert_eq!(total.attempts(), 4, "pattern {pattern}");
            assert_eq!(total.succeeded(), succeeded, "pattern {pattern}");
            assert_eq!(total.failed(), failed, "pattern {pattern}");
            assert_eq!(total.usage_records(), usage_records, "pattern {pattern}");
            assert_eq!(
                total.input_tokens().exact(),
                Some(exact),
                "pattern {pattern}"
            );
            assert_eq!(
                total.input_tokens().estimated(),
                Some(estimated),
                "pattern {pattern}"
            );
            assert_eq!(
                total.input_tokens().unknown_records(),
                unknown,
                "pattern {pattern}"
            );
        }
    }
}
