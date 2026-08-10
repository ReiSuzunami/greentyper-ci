//! Layered configuration resolved into immutable per-Turn epochs.

use std::error::Error;
use std::fmt;

use serde::Serialize;

use crate::model::ConfigEpochId;
use crate::pricing::PriceScheduleBook;
use crate::schema::SchemaKind;
use crate::usage::{MAX_USAGE_WINDOWS, UsageTimezoneSource, UsageWeekday, UsageWindow};

mod command_paths;
mod editor;
mod runtime;
pub use command_paths::*;
pub use editor::*;
pub use runtime::*;

pub const DEFAULT_MAX_OUTPUT_BYTES: u32 = 64 * 1024;
pub const MAX_OUTPUT_BYTES: u32 = 512 * 1024;
pub const MAX_OUTPUT_TOKENS: u32 = 1024 * 1024;
pub const MAX_CONFIG_ID_BYTES: usize = 64;
pub const MAX_CONFIG_STRING_BYTES: usize = 512;
pub const CONFIG_SCHEMA_VERSION: u16 = SchemaKind::ConfigEpoch.current().get();

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
    Fast,
}

impl ServiceTier {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Default => "default",
            Self::Flex => "flex",
            Self::Scale => "scale",
            Self::Priority => "priority",
            Self::Fast => "fast",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "default" => Some(Self::Default),
            "flex" => Some(Self::Flex),
            "scale" => Some(Self::Scale),
            "priority" => Some(Self::Priority),
            "fast" => Some(Self::Fast),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigSource {
    BuiltIn,
    User,
    Project,
    Cli,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigLayer {
    pub provider_profile: Option<String>,
    pub provider_model: Option<String>,
    pub max_output_bytes: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub service_tier: Option<ServiceTier>,
}

impl ConfigLayer {
    #[must_use]
    pub fn built_in() -> Self {
        Self {
            provider_profile: Some("simulator".to_owned()),
            provider_model: Some("deterministic-v1".to_owned()),
            max_output_bytes: Some(DEFAULT_MAX_OUTPUT_BYTES),
            max_output_tokens: None,
            reasoning_effort: None,
            service_tier: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigLayers {
    pub built_in: ConfigLayer,
    pub user: ConfigLayer,
    pub project: ConfigLayer,
    pub cli: ConfigLayer,
}

impl Default for ConfigLayers {
    fn default() -> Self {
        Self {
            built_in: ConfigLayer::built_in(),
            user: ConfigLayer::default(),
            project: ConfigLayer::default(),
            cli: ConfigLayer::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sourced<T> {
    value: T,
    source: ConfigSource,
}

impl<T> Sourced<T> {
    #[must_use]
    pub const fn value(&self) -> &T {
        &self.value
    }

    #[must_use]
    pub const fn source(&self) -> ConfigSource {
        self.source
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConfig {
    provider_profile: Sourced<String>,
    provider_model: Sourced<String>,
    max_output_bytes: Sourced<u32>,
    max_output_tokens: Option<Sourced<u32>>,
    reasoning_effort: Option<Sourced<ReasoningEffort>>,
    service_tier: Option<Sourced<ServiceTier>>,
}

impl ResolvedConfig {
    #[must_use]
    pub fn provider_profile(&self) -> &Sourced<String> {
        &self.provider_profile
    }

    #[must_use]
    pub fn provider_model(&self) -> &Sourced<String> {
        &self.provider_model
    }

    #[must_use]
    pub const fn max_output_bytes(&self) -> &Sourced<u32> {
        &self.max_output_bytes
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<&Sourced<u32>> {
        self.max_output_tokens.as_ref()
    }

    #[must_use]
    pub const fn reasoning_effort(&self) -> Option<&Sourced<ReasoningEffort>> {
        self.reasoning_effort.as_ref()
    }

    #[must_use]
    pub const fn service_tier(&self) -> Option<&Sourced<ServiceTier>> {
        self.service_tier.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigEpoch {
    id: ConfigEpochId,
    fingerprint: u64,
    resolved: ResolvedConfig,
    usage_windows: Vec<UsageWindow>,
    price_schedules: PriceScheduleBook,
}

impl ConfigEpoch {
    pub fn freeze(id: ConfigEpochId, layers: &ConfigLayers) -> Result<Self, ConfigError> {
        Self::freeze_with_usage_windows(id, layers, Vec::new())
    }

    pub fn freeze_with_usage_windows(
        id: ConfigEpochId,
        layers: &ConfigLayers,
        usage_windows: Vec<UsageWindow>,
    ) -> Result<Self, ConfigError> {
        Self::freeze_with_observability(id, layers, usage_windows, PriceScheduleBook::default())
    }

    pub fn freeze_with_observability(
        id: ConfigEpochId,
        layers: &ConfigLayers,
        mut usage_windows: Vec<UsageWindow>,
        price_schedules: PriceScheduleBook,
    ) -> Result<Self, ConfigError> {
        let resolved = layers.resolve()?;
        if usage_windows.len() > MAX_USAGE_WINDOWS {
            return Err(ConfigError::TooManyUsageWindows);
        }
        usage_windows.sort_by(|left, right| left.id().cmp(right.id()));
        if usage_windows
            .windows(2)
            .any(|pair| pair[0].id() == pair[1].id())
        {
            return Err(ConfigError::DuplicateUsageWindow);
        }
        let fingerprint = fingerprint(&resolved, &usage_windows, &price_schedules);
        Ok(Self {
            id,
            fingerprint,
            resolved,
            usage_windows,
            price_schedules,
        })
    }

    #[must_use]
    pub const fn id(&self) -> ConfigEpochId {
        self.id
    }

    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    #[must_use]
    pub const fn resolved(&self) -> &ResolvedConfig {
        &self.resolved
    }

    #[must_use]
    pub fn usage_windows(&self) -> &[UsageWindow] {
        &self.usage_windows
    }

    #[must_use]
    pub const fn price_schedules(&self) -> &PriceScheduleBook {
        &self.price_schedules
    }
}

impl ConfigLayers {
    pub fn resolve(&self) -> Result<ResolvedConfig, ConfigError> {
        validate_layer(&self.built_in)?;
        validate_layer(&self.user)?;
        validate_layer(&self.project)?;
        validate_layer(&self.cli)?;

        let provider_profile = resolve_string(
            "provider.profile",
            [
                (&self.built_in.provider_profile, ConfigSource::BuiltIn),
                (&self.user.provider_profile, ConfigSource::User),
                (&self.project.provider_profile, ConfigSource::Project),
                (&self.cli.provider_profile, ConfigSource::Cli),
            ],
        )?;
        let provider_model = resolve_string(
            "provider.model",
            [
                (&self.built_in.provider_model, ConfigSource::BuiltIn),
                (&self.user.provider_model, ConfigSource::User),
                (&self.project.provider_model, ConfigSource::Project),
                (&self.cli.provider_model, ConfigSource::Cli),
            ],
        )?;
        let max_output_bytes = resolve_u32(
            "runtime.max_output_bytes",
            [
                (&self.built_in.max_output_bytes, ConfigSource::BuiltIn),
                (&self.user.max_output_bytes, ConfigSource::User),
                (&self.project.max_output_bytes, ConfigSource::Project),
                (&self.cli.max_output_bytes, ConfigSource::Cli),
            ],
        )?;
        if max_output_bytes.value == 0 {
            return Err(ConfigError::ZeroMaxOutputBytes);
        }
        if max_output_bytes.value > MAX_OUTPUT_BYTES {
            return Err(ConfigError::MaxOutputBytesTooLarge);
        }
        let max_output_tokens = resolve_optional_u32([
            (&self.built_in.max_output_tokens, ConfigSource::BuiltIn),
            (&self.user.max_output_tokens, ConfigSource::User),
            (&self.project.max_output_tokens, ConfigSource::Project),
            (&self.cli.max_output_tokens, ConfigSource::Cli),
        ]);
        if max_output_tokens
            .as_ref()
            .is_some_and(|value| value.value == 0)
        {
            return Err(ConfigError::ZeroMaxOutputTokens);
        }
        if max_output_tokens
            .as_ref()
            .is_some_and(|value| value.value > MAX_OUTPUT_TOKENS)
        {
            return Err(ConfigError::MaxOutputTokensTooLarge);
        }
        let reasoning_effort = resolve_optional([
            (&self.built_in.reasoning_effort, ConfigSource::BuiltIn),
            (&self.user.reasoning_effort, ConfigSource::User),
            (&self.project.reasoning_effort, ConfigSource::Project),
            (&self.cli.reasoning_effort, ConfigSource::Cli),
        ]);
        let service_tier = resolve_optional([
            (&self.built_in.service_tier, ConfigSource::BuiltIn),
            (&self.user.service_tier, ConfigSource::User),
            (&self.project.service_tier, ConfigSource::Project),
            (&self.cli.service_tier, ConfigSource::Cli),
        ]);

        Ok(ResolvedConfig {
            provider_profile,
            provider_model,
            max_output_bytes,
            max_output_tokens,
            reasoning_effort,
            service_tier,
        })
    }
}

fn validate_layer(layer: &ConfigLayer) -> Result<(), ConfigError> {
    for (key, value) in [
        ("provider.profile", layer.provider_profile.as_deref()),
        ("provider.model", layer.provider_model.as_deref()),
    ] {
        if let Some(value) = value {
            if value.trim().is_empty() {
                return Err(ConfigError::EmptyString(key));
            }
            if value.trim() != value {
                return Err(ConfigError::SurroundingWhitespace(key));
            }
            if value.len() > MAX_CONFIG_STRING_BYTES {
                return Err(ConfigError::StringTooLong(key));
            }
        }
    }
    if let Some(max_output_bytes) = layer.max_output_bytes {
        if max_output_bytes == 0 {
            return Err(ConfigError::ZeroMaxOutputBytes);
        }
        if max_output_bytes > MAX_OUTPUT_BYTES {
            return Err(ConfigError::MaxOutputBytesTooLarge);
        }
    }
    if let Some(max_output_tokens) = layer.max_output_tokens {
        if max_output_tokens == 0 {
            return Err(ConfigError::ZeroMaxOutputTokens);
        }
        if max_output_tokens > MAX_OUTPUT_TOKENS {
            return Err(ConfigError::MaxOutputTokensTooLarge);
        }
    }
    Ok(())
}

fn resolve_string<const N: usize>(
    key: &'static str,
    values: [(&Option<String>, ConfigSource); N],
) -> Result<Sourced<String>, ConfigError> {
    values
        .into_iter()
        .filter_map(|(value, source)| value.as_ref().map(|value| (value, source)))
        .next_back()
        .map(|(value, source)| Sourced {
            value: value.clone(),
            source,
        })
        .ok_or(ConfigError::MissingRequired(key))
}

fn resolve_u32<const N: usize>(
    key: &'static str,
    values: [(&Option<u32>, ConfigSource); N],
) -> Result<Sourced<u32>, ConfigError> {
    values
        .into_iter()
        .filter_map(|(value, source)| value.map(|value| (value, source)))
        .next_back()
        .map(|(value, source)| Sourced { value, source })
        .ok_or(ConfigError::MissingRequired(key))
}

fn resolve_optional_u32<const N: usize>(
    values: [(&Option<u32>, ConfigSource); N],
) -> Option<Sourced<u32>> {
    values
        .into_iter()
        .filter_map(|(value, source)| value.map(|value| (value, source)))
        .next_back()
        .map(|(value, source)| Sourced { value, source })
}

fn resolve_optional<T: Clone, const N: usize>(
    values: [(&Option<T>, ConfigSource); N],
) -> Option<Sourced<T>> {
    values
        .into_iter()
        .filter_map(|(value, source)| value.as_ref().map(|value| (value, source)))
        .next_back()
        .map(|(value, source)| Sourced {
            value: value.clone(),
            source,
        })
}

fn fingerprint(
    config: &ResolvedConfig,
    usage_windows: &[UsageWindow],
    price_schedules: &PriceScheduleBook,
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for bytes in [
        CONFIG_SCHEMA_VERSION.to_le_bytes().as_slice(),
        config.provider_profile.value.as_bytes(),
        &[source_tag(config.provider_profile.source)],
        config.provider_model.value.as_bytes(),
        &[source_tag(config.provider_model.source)],
        config.max_output_bytes.value.to_le_bytes().as_slice(),
        &[source_tag(config.max_output_bytes.source)],
    ] {
        hash ^= bytes.len() as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    if let Some(max_output_tokens) = &config.max_output_tokens {
        for bytes in [
            max_output_tokens.value.to_le_bytes().as_slice(),
            &[source_tag(max_output_tokens.source)],
        ] {
            hash ^= bytes.len() as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    if let Some(reasoning_effort) = &config.reasoning_effort {
        for bytes in [
            reasoning_effort.value.as_str().as_bytes(),
            &[source_tag(reasoning_effort.source)],
        ] {
            hash ^= bytes.len() as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    if let Some(service_tier) = &config.service_tier {
        for bytes in [
            service_tier.value.as_str().as_bytes(),
            &[source_tag(service_tier.source)],
        ] {
            hash ^= bytes.len() as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    for window in usage_windows {
        for bytes in [
            window.id().as_bytes(),
            window.start_minute().to_le_bytes().as_slice(),
            window.end_minute().to_le_bytes().as_slice(),
            window.timezone().as_bytes(),
            &[usage_timezone_source_tag(window.timezone_source())],
            window.ruleset_version().as_bytes(),
        ] {
            hash ^= bytes.len() as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        for day in window.days() {
            hash ^= u64::from(usage_weekday_tag(day));
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    for schedule in price_schedules.schedules() {
        let bytes = schedule.fingerprint().to_le_bytes();
        hash ^= bytes.len() as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

const fn usage_timezone_source_tag(source: UsageTimezoneSource) -> u8 {
    match source {
        UsageTimezoneSource::Explicit => 1,
        UsageTimezoneSource::LocalSystem => 2,
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

const fn source_tag(source: ConfigSource) -> u8 {
    match source {
        ConfigSource::BuiltIn => 1,
        ConfigSource::User => 2,
        ConfigSource::Project => 3,
        ConfigSource::Cli => 4,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    MissingRequired(&'static str),
    EmptyString(&'static str),
    SurroundingWhitespace(&'static str),
    StringTooLong(&'static str),
    ZeroMaxOutputBytes,
    ZeroMaxOutputTokens,
    MaxOutputBytesTooLarge,
    MaxOutputTokensTooLarge,
    TooManyUsageWindows,
    DuplicateUsageWindow,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequired(key) => write!(formatter, "missing required config key {key}"),
            Self::EmptyString(key) => write!(formatter, "config key {key} cannot be empty"),
            Self::SurroundingWhitespace(key) => {
                write!(formatter, "config key {key} has surrounding whitespace")
            }
            Self::StringTooLong(key) => write!(formatter, "config key {key} is too long"),
            Self::ZeroMaxOutputBytes => {
                write!(
                    formatter,
                    "runtime.max_output_bytes must be greater than zero"
                )
            }
            Self::ZeroMaxOutputTokens => {
                write!(formatter, "max_output_tokens must be greater than zero")
            }
            Self::MaxOutputBytesTooLarge => {
                write!(
                    formatter,
                    "runtime.max_output_bytes exceeds the supported limit"
                )
            }
            Self::MaxOutputTokensTooLarge => {
                write!(formatter, "max_output_tokens exceeds the supported limit")
            }
            Self::TooManyUsageWindows => {
                write!(formatter, "usage window count exceeds the supported limit")
            }
            Self::DuplicateUsageWindow => write!(formatter, "usage window IDs must be unique"),
        }
    }
}

impl Error for ConfigError {}
