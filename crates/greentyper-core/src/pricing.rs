//! Immutable Price Schedules and deterministic pay-as-you-go Cost Estimates.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::provider::{ProviderDialect, UsageAccuracy, UsageRecord};
use crate::usage::UsageTimestamp;

pub const MAX_PRICE_SCHEDULES: usize = 256;
pub const COST_SCALE_DECIMAL_PLACES: u8 = 12;
const MAX_PRICE_ID_BYTES: usize = 64;
const MAX_PRICE_TEXT_BYTES: usize = 512;
const MAX_SERVICE_TIER_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceScheduleSource {
    Template,
    Manual,
    ProviderReported,
}

impl PriceScheduleSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Manual => "manual",
            Self::ProviderReported => "provider_reported",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TokenRates {
    input_micros_per_million: u64,
    cached_input_micros_per_million: u64,
    cache_write_micros_per_million: u64,
    output_micros_per_million: u64,
    reasoning_output_micros_per_million: u64,
}

impl TokenRates {
    #[must_use]
    pub const fn new(
        input_micros_per_million: u64,
        cached_input_micros_per_million: u64,
        cache_write_micros_per_million: u64,
        output_micros_per_million: u64,
        reasoning_output_micros_per_million: u64,
    ) -> Self {
        Self {
            input_micros_per_million,
            cached_input_micros_per_million,
            cache_write_micros_per_million,
            output_micros_per_million,
            reasoning_output_micros_per_million,
        }
    }

    #[must_use]
    pub const fn input_micros_per_million(self) -> u64 {
        self.input_micros_per_million
    }

    #[must_use]
    pub const fn cached_input_micros_per_million(self) -> u64 {
        self.cached_input_micros_per_million
    }

    #[must_use]
    pub const fn cache_write_micros_per_million(self) -> u64 {
        self.cache_write_micros_per_million
    }

    #[must_use]
    pub const fn output_micros_per_million(self) -> u64 {
        self.output_micros_per_million
    }

    #[must_use]
    pub const fn reasoning_output_micros_per_million(self) -> u64 {
        self.reasoning_output_micros_per_million
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceScheduleDefinition {
    pub id: String,
    pub version: String,
    pub currency: String,
    pub provider_profile: String,
    pub model: String,
    pub dialect: Option<ProviderDialect>,
    pub service_tier: Option<String>,
    pub minimum_context_tokens: u64,
    pub maximum_context_tokens: Option<u64>,
    pub effective_from: UsageTimestamp,
    pub effective_until: Option<UsageTimestamp>,
    pub source: PriceScheduleSource,
    pub source_ref: String,
    pub rates: TokenRates,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PriceSchedule {
    id: String,
    version: String,
    currency: String,
    provider_profile: String,
    model: String,
    dialect: Option<ProviderDialect>,
    service_tier: Option<String>,
    minimum_context_tokens: u64,
    maximum_context_tokens: Option<u64>,
    effective_from: UsageTimestamp,
    effective_until: Option<UsageTimestamp>,
    source: PriceScheduleSource,
    source_ref: String,
    rates: TokenRates,
    fingerprint: u64,
}

impl PriceSchedule {
    pub fn new(definition: PriceScheduleDefinition) -> Result<Self, PricingError> {
        if definition.source != PriceScheduleSource::Manual {
            return Err(PricingError::UntrustedSource);
        }
        Self::new_trusted(definition)
    }

    pub(crate) fn new_trusted(definition: PriceScheduleDefinition) -> Result<Self, PricingError> {
        validate_id(&definition.id)?;
        validate_text(&definition.version, PricingError::InvalidVersion)?;
        validate_currency(&definition.currency)?;
        validate_provider_profile(&definition.provider_profile)?;
        validate_text(&definition.model, PricingError::InvalidModel)?;
        if let Some(service_tier) = &definition.service_tier {
            validate_service_tier(service_tier)?;
        }
        if definition
            .maximum_context_tokens
            .is_some_and(|maximum| maximum <= definition.minimum_context_tokens)
        {
            return Err(PricingError::InvalidContextRange);
        }
        if definition
            .effective_until
            .is_some_and(|until| until <= definition.effective_from)
        {
            return Err(PricingError::InvalidEffectiveInterval);
        }
        validate_text(&definition.source_ref, PricingError::InvalidSourceReference)?;
        let fingerprint = fingerprint(&definition);
        Ok(Self {
            id: definition.id,
            version: definition.version,
            currency: definition.currency,
            provider_profile: definition.provider_profile,
            model: definition.model,
            dialect: definition.dialect,
            service_tier: definition.service_tier,
            minimum_context_tokens: definition.minimum_context_tokens,
            maximum_context_tokens: definition.maximum_context_tokens,
            effective_from: definition.effective_from,
            effective_until: definition.effective_until,
            source: definition.source,
            source_ref: definition.source_ref,
            rates: definition.rates,
            fingerprint,
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn currency(&self) -> &str {
        &self.currency
    }

    #[must_use]
    pub fn provider_profile(&self) -> &str {
        &self.provider_profile
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub const fn dialect(&self) -> Option<ProviderDialect> {
        self.dialect
    }

    #[must_use]
    pub fn service_tier(&self) -> Option<&str> {
        self.service_tier.as_deref()
    }

    #[must_use]
    pub const fn minimum_context_tokens(&self) -> u64 {
        self.minimum_context_tokens
    }

    #[must_use]
    pub const fn maximum_context_tokens(&self) -> Option<u64> {
        self.maximum_context_tokens
    }

    #[must_use]
    pub const fn effective_from(&self) -> UsageTimestamp {
        self.effective_from
    }

    #[must_use]
    pub const fn effective_until(&self) -> Option<UsageTimestamp> {
        self.effective_until
    }

    #[must_use]
    pub const fn source(&self) -> PriceScheduleSource {
        self.source
    }

    #[must_use]
    pub fn source_ref(&self) -> &str {
        &self.source_ref
    }

    #[must_use]
    pub const fn rates(&self) -> TokenRates {
        self.rates
    }

    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    fn matches_base(
        &self,
        provider_profile: &str,
        model: &str,
        dialect: Option<ProviderDialect>,
        started_at: UsageTimestamp,
    ) -> bool {
        self.provider_profile == provider_profile
            && self.model == model
            && self
                .dialect
                .is_none_or(|expected| Some(expected) == dialect)
            && started_at >= self.effective_from
            && self.effective_until.is_none_or(|until| started_at < until)
    }

    fn matches_service_tier(&self, service_tier: Option<&str>) -> bool {
        self.service_tier
            .as_deref()
            .is_none_or(|expected| Some(expected) == service_tier)
    }

    fn matches_context(&self, input_tokens: u64) -> bool {
        input_tokens >= self.minimum_context_tokens
            && self
                .maximum_context_tokens
                .is_none_or(|maximum| input_tokens < maximum)
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.provider_profile == other.provider_profile
            && self.model == other.model
            && selectors_overlap(self.dialect, other.dialect)
            && optional_text_selectors_overlap(
                self.service_tier.as_deref(),
                other.service_tier.as_deref(),
            )
            && ranges_overlap(
                self.minimum_context_tokens,
                self.maximum_context_tokens,
                other.minimum_context_tokens,
                other.maximum_context_tokens,
            )
            && ranges_overlap(
                self.effective_from.unix_millis(),
                self.effective_until.map(UsageTimestamp::unix_millis),
                other.effective_from.unix_millis(),
                other.effective_until.map(UsageTimestamp::unix_millis),
            )
    }

    fn estimate(&self, usage: &UsageRecord) -> CostEstimateOutcome {
        let Some(input) = usage.input_tokens() else {
            return CostEstimateOutcome::Unknown(CostEstimateUnknownReason::MissingInputTokens);
        };
        let Some(cached_input) = usage.cached_input_tokens() else {
            return CostEstimateOutcome::Unknown(
                CostEstimateUnknownReason::MissingCachedInputTokens,
            );
        };
        let Some(cache_write) = usage.cache_write_input_tokens() else {
            return CostEstimateOutcome::Unknown(
                CostEstimateUnknownReason::MissingCacheWriteInputTokens,
            );
        };
        let Some(output) = usage.output_tokens() else {
            return CostEstimateOutcome::Unknown(CostEstimateUnknownReason::MissingOutputTokens);
        };
        let Some(reasoning_output) = usage.reasoning_output_tokens() else {
            return CostEstimateOutcome::Unknown(
                CostEstimateUnknownReason::MissingReasoningOutputTokens,
            );
        };
        let Some(uncached_input) = input
            .checked_sub(cached_input)
            .and_then(|value| value.checked_sub(cache_write))
        else {
            return CostEstimateOutcome::Unknown(
                CostEstimateUnknownReason::InconsistentInputAccounting,
            );
        };
        let Some(visible_output) = output.checked_sub(reasoning_output) else {
            return CostEstimateOutcome::Unknown(
                CostEstimateUnknownReason::InconsistentOutputAccounting,
            );
        };
        let values = [
            uncached_input.checked_mul(self.rates.input_micros_per_million),
            cached_input.checked_mul(self.rates.cached_input_micros_per_million),
            cache_write.checked_mul(self.rates.cache_write_micros_per_million),
            visible_output.checked_mul(self.rates.output_micros_per_million),
            reasoning_output.checked_mul(self.rates.reasoning_output_micros_per_million),
        ];
        let [
            Some(uncached_input_pico_units),
            Some(cached_input_pico_units),
            Some(cache_write_pico_units),
            Some(visible_output_pico_units),
            Some(reasoning_output_pico_units),
        ] = values
        else {
            return CostEstimateOutcome::Unknown(CostEstimateUnknownReason::ArithmeticOverflow);
        };
        let breakdown = CostBreakdown {
            uncached_input_pico_units,
            cached_input_pico_units,
            cache_write_pico_units,
            visible_output_pico_units,
            reasoning_output_pico_units,
        };
        let Some(amount_pico_units) = breakdown.checked_total() else {
            return CostEstimateOutcome::Unknown(CostEstimateUnknownReason::ArithmeticOverflow);
        };
        CostEstimateOutcome::Known(Box::new(CostEstimate {
            schedule: self.clone(),
            amount_pico_units,
            breakdown,
            usage_accuracy: usage.accuracy(),
        }))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PriceScheduleBook {
    schedules: Vec<PriceSchedule>,
}

impl PriceScheduleBook {
    pub fn new(mut schedules: Vec<PriceSchedule>) -> Result<Self, PricingError> {
        if schedules.len() > MAX_PRICE_SCHEDULES {
            return Err(PricingError::TooManySchedules);
        }
        schedules.sort_by(|left, right| left.id.cmp(&right.id));
        if schedules.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(PricingError::DuplicateSchedule);
        }
        for (index, schedule) in schedules.iter().enumerate() {
            if schedules[index + 1..]
                .iter()
                .any(|candidate| schedule.overlaps(candidate))
            {
                return Err(PricingError::OverlappingSchedules);
            }
        }
        Ok(Self { schedules })
    }

    #[must_use]
    pub fn schedules(&self) -> &[PriceSchedule] {
        &self.schedules
    }

    #[must_use]
    pub fn estimate_attempt(
        &self,
        provider_profile: &str,
        model: &str,
        dialect: Option<ProviderDialect>,
        started_at: UsageTimestamp,
        usage: &UsageRecord,
    ) -> CostEstimateOutcome {
        let base = self
            .schedules
            .iter()
            .filter(|schedule| schedule.matches_base(provider_profile, model, dialect, started_at))
            .collect::<Vec<_>>();
        if base.is_empty() {
            return CostEstimateOutcome::Unknown(CostEstimateUnknownReason::NoMatchingSchedule);
        }
        let tiers = base
            .into_iter()
            .filter(|schedule| schedule.matches_service_tier(usage.service_tier()))
            .collect::<Vec<_>>();
        if tiers.is_empty() {
            return CostEstimateOutcome::Unknown(if usage.service_tier().is_none() {
                CostEstimateUnknownReason::MissingServiceTier
            } else {
                CostEstimateUnknownReason::NoMatchingSchedule
            });
        }
        let Some(input_tokens) = usage.input_tokens() else {
            return CostEstimateOutcome::Unknown(CostEstimateUnknownReason::MissingInputTokens);
        };
        let Some(schedule) = tiers
            .into_iter()
            .find(|schedule| schedule.matches_context(input_tokens))
        else {
            return CostEstimateOutcome::Unknown(CostEstimateUnknownReason::NoMatchingSchedule);
        };
        schedule.estimate(usage)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CostBreakdown {
    uncached_input_pico_units: u64,
    cached_input_pico_units: u64,
    cache_write_pico_units: u64,
    visible_output_pico_units: u64,
    reasoning_output_pico_units: u64,
}

impl CostBreakdown {
    #[must_use]
    pub const fn uncached_input_pico_units(&self) -> u64 {
        self.uncached_input_pico_units
    }

    #[must_use]
    pub const fn cached_input_pico_units(&self) -> u64 {
        self.cached_input_pico_units
    }

    #[must_use]
    pub const fn cache_write_pico_units(&self) -> u64 {
        self.cache_write_pico_units
    }

    #[must_use]
    pub const fn visible_output_pico_units(&self) -> u64 {
        self.visible_output_pico_units
    }

    #[must_use]
    pub const fn reasoning_output_pico_units(&self) -> u64 {
        self.reasoning_output_pico_units
    }

    fn checked_total(&self) -> Option<u64> {
        [
            self.uncached_input_pico_units,
            self.cached_input_pico_units,
            self.cache_write_pico_units,
            self.visible_output_pico_units,
            self.reasoning_output_pico_units,
        ]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CostEstimate {
    schedule: PriceSchedule,
    amount_pico_units: u64,
    breakdown: CostBreakdown,
    usage_accuracy: UsageAccuracy,
}

impl CostEstimate {
    #[must_use]
    pub const fn schedule(&self) -> &PriceSchedule {
        &self.schedule
    }

    #[must_use]
    pub fn currency(&self) -> &str {
        self.schedule.currency()
    }

    #[must_use]
    pub const fn amount_pico_units(&self) -> u64 {
        self.amount_pico_units
    }

    #[must_use]
    pub const fn scale_decimal_places(&self) -> u8 {
        COST_SCALE_DECIMAL_PLACES
    }

    #[must_use]
    pub const fn breakdown(&self) -> &CostBreakdown {
        &self.breakdown
    }

    #[must_use]
    pub const fn usage_accuracy(&self) -> UsageAccuracy {
        self.usage_accuracy
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostEstimateUnknownReason {
    MissingUsageRecord,
    NoMatchingSchedule,
    MissingInputTokens,
    MissingCachedInputTokens,
    MissingCacheWriteInputTokens,
    MissingOutputTokens,
    MissingReasoningOutputTokens,
    MissingServiceTier,
    InconsistentInputAccounting,
    InconsistentOutputAccounting,
    ArithmeticOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum CostEstimateOutcome {
    Known(Box<CostEstimate>),
    Unknown(CostEstimateUnknownReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PricingError {
    InvalidId,
    InvalidVersion,
    InvalidCurrency,
    InvalidProviderProfile,
    InvalidModel,
    InvalidServiceTier,
    InvalidContextRange,
    InvalidEffectiveInterval,
    InvalidSourceReference,
    UntrustedSource,
    TooManySchedules,
    DuplicateSchedule,
    OverlappingSchedules,
}

impl fmt::Display for PricingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidId => "price schedule ID is invalid",
            Self::InvalidVersion => "price schedule version is invalid",
            Self::InvalidCurrency => {
                "price schedule currency must be three uppercase ASCII letters"
            }
            Self::InvalidProviderProfile => "price schedule Provider Profile is invalid",
            Self::InvalidModel => "price schedule model is invalid",
            Self::InvalidServiceTier => "price schedule service tier is invalid",
            Self::InvalidContextRange => "price schedule context range is invalid",
            Self::InvalidEffectiveInterval => "price schedule effective interval is invalid",
            Self::InvalidSourceReference => "price schedule source reference is invalid",
            Self::UntrustedSource => {
                "public Price Schedule construction accepts manual provenance only"
            }
            Self::TooManySchedules => "price schedule count exceeds the supported limit",
            Self::DuplicateSchedule => "price schedule IDs must be unique",
            Self::OverlappingSchedules => {
                "price schedules have ambiguous overlapping applicability"
            }
        })
    }
}

impl Error for PricingError {}

fn validate_id(value: &str) -> Result<(), PricingError> {
    if !valid_id(value) {
        Err(PricingError::InvalidId)
    } else {
        Ok(())
    }
}

fn validate_provider_profile(value: &str) -> Result<(), PricingError> {
    if valid_id(value) {
        Ok(())
    } else {
        Err(PricingError::InvalidProviderProfile)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PRICE_ID_BYTES
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
        })
}

fn validate_text(value: &str, error: PricingError) -> Result<(), PricingError> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > MAX_PRICE_TEXT_BYTES
        || value.chars().any(char::is_control)
    {
        Err(error)
    } else {
        Ok(())
    }
}

fn validate_currency(value: &str) -> Result<(), PricingError> {
    if value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(PricingError::InvalidCurrency)
    }
}

fn validate_service_tier(value: &str) -> Result<(), PricingError> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > MAX_SERVICE_TIER_BYTES
        || value.chars().any(char::is_control)
    {
        Err(PricingError::InvalidServiceTier)
    } else {
        Ok(())
    }
}

fn selectors_overlap<T: Copy + Eq>(left: Option<T>, right: Option<T>) -> bool {
    left.is_none() || right.is_none() || left == right
}

fn optional_text_selectors_overlap(left: Option<&str>, right: Option<&str>) -> bool {
    left.is_none() || right.is_none() || left == right
}

fn ranges_overlap<T: Copy + Ord>(
    left_start: T,
    left_end: Option<T>,
    right_start: T,
    right_end: Option<T>,
) -> bool {
    left_end.is_none_or(|end| right_start < end) && right_end.is_none_or(|end| left_start < end)
}

fn fingerprint(definition: &PriceScheduleDefinition) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    hash_bytes(&mut hash, definition.id.as_bytes());
    hash_bytes(&mut hash, definition.version.as_bytes());
    hash_bytes(&mut hash, definition.currency.as_bytes());
    hash_bytes(&mut hash, definition.provider_profile.as_bytes());
    hash_bytes(&mut hash, definition.model.as_bytes());
    hash_bytes(
        &mut hash,
        &[definition.dialect.map_or(0, provider_dialect_tag)],
    );
    hash_bytes(
        &mut hash,
        definition.service_tier.as_deref().unwrap_or("").as_bytes(),
    );
    hash_bytes(&mut hash, &definition.minimum_context_tokens.to_le_bytes());
    match definition.maximum_context_tokens {
        Some(maximum) => {
            hash_bytes(&mut hash, &[1]);
            hash_bytes(&mut hash, &maximum.to_le_bytes());
        }
        None => hash_bytes(&mut hash, &[0]),
    }
    hash_bytes(
        &mut hash,
        &definition.effective_from.unix_millis().to_le_bytes(),
    );
    match definition.effective_until {
        Some(until) => {
            hash_bytes(&mut hash, &[1]);
            hash_bytes(&mut hash, &until.unix_millis().to_le_bytes());
        }
        None => hash_bytes(&mut hash, &[0]),
    }
    hash_bytes(&mut hash, &[source_tag(definition.source)]);
    hash_bytes(&mut hash, definition.source_ref.as_bytes());
    for rate in [
        definition.rates.input_micros_per_million,
        definition.rates.cached_input_micros_per_million,
        definition.rates.cache_write_micros_per_million,
        definition.rates.output_micros_per_million,
        definition.rates.reasoning_output_micros_per_million,
    ] {
        hash_bytes(&mut hash, &rate.to_le_bytes());
    }
    hash
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    *hash ^= bytes.len() as u64;
    *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

const fn provider_dialect_tag(dialect: ProviderDialect) -> u8 {
    match dialect {
        ProviderDialect::Responses => 1,
        ProviderDialect::ChatCompletions => 2,
        ProviderDialect::Messages => 3,
    }
}

const fn source_tag(source: PriceScheduleSource) -> u8 {
    match source {
        PriceScheduleSource::Template => 1,
        PriceScheduleSource::Manual => 2,
        PriceScheduleSource::ProviderReported => 3,
    }
}
