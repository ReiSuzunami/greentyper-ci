//! Versioned schema, TOML layers, drafts, and atomic Config storage.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::{Host, Url};

use super::{
    ConfigLayer, ConfigLayers, MAX_CONFIG_ID_BYTES, MAX_CONFIG_STRING_BYTES, ReasoningEffort,
    ServiceTier,
};
use crate::pricing::{
    PriceSchedule, PriceScheduleBook, PriceScheduleDefinition, PriceScheduleSource, TokenRates,
};
use crate::provider::{ProviderDialect, ProviderPricingSource, ProviderProfileSnapshot};
use crate::provider_catalog::{
    ModelCatalogRecord, ProviderCatalog, ProviderCatalogMode, has_release_price_schedules,
    release_price_schedules_for_profile,
};
use crate::schema::SchemaKind;
use crate::usage::{MAX_USAGE_WINDOWS, UsageWeekday, UsageWindow};

pub const CONFIG_FILE_SCHEMA_VERSION: u16 = SchemaKind::ConfigFile.current().get();
pub const MAX_CONFIG_FILE_BYTES: usize = 1024 * 1024;
const MAX_CONFIG_LIST_ITEMS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigScope {
    BuiltIn,
    User,
    Project,
    Cli,
}

impl ConfigScope {
    #[must_use]
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::User | Self::Project)
    }
}

impl fmt::Display for ConfigScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BuiltIn => "built_in",
            Self::User => "user",
            Self::Project => "project",
            Self::Cli => "cli",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigApplicationTiming {
    Immediate,
    NextConfigEpoch,
    NextTurn,
    NextProviderEpoch,
    NextTurnAndProviderEpoch,
    Restart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigValueKind {
    String,
    PositiveInteger,
    NonNegativeInteger,
    Boolean,
    StringList,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigFieldInteraction {
    ReadOnly,
    Choice { choices: &'static [&'static str] },
    Text { max_bytes: usize },
    CredentialReference { max_bytes: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigSchemaEntry {
    pub path_pattern: &'static str,
    pub command_path: &'static str,
    pub value_kind: ConfigValueKind,
    pub scopes: &'static [ConfigScope],
    pub timing: ConfigApplicationTiming,
    pub credential_reference: bool,
    pub editor: &'static str,
}

impl ConfigSchemaEntry {
    #[must_use]
    pub fn interaction(&self) -> ConfigFieldInteraction {
        config_field_interaction(self)
    }
}

const ALL_SCOPES: &[ConfigScope] = &[
    ConfigScope::BuiltIn,
    ConfigScope::User,
    ConfigScope::Project,
    ConfigScope::Cli,
];
const FILE_SCOPES: &[ConfigScope] = &[ConfigScope::User, ConfigScope::Project];
const PROVIDER_TEMPLATE_CHOICES: &[&str] = &["deepseek", "openai", "opencode-go"];
const PROVIDER_DIALECT_CHOICES: &[&str] = &[
    ProviderDialect::Responses.as_str(),
    ProviderDialect::ChatCompletions.as_str(),
    ProviderDialect::Messages.as_str(),
];
const PROVIDER_CATALOG_MODE_CHOICES: &[&str] = &[
    ProviderCatalogMode::Template.as_str(),
    ProviderCatalogMode::Discovery.as_str(),
    ProviderCatalogMode::TemplateAndDiscovery.as_str(),
    ProviderCatalogMode::Manual.as_str(),
];
const PROVIDER_PRICING_SOURCE_CHOICES: &[&str] = &[
    ProviderPricingSource::Unknown.as_str(),
    ProviderPricingSource::Template.as_str(),
    ProviderPricingSource::TemplateMirror.as_str(),
    ProviderPricingSource::Manual.as_str(),
    ProviderPricingSource::ProviderReported.as_str(),
];
const REASONING_EFFORT_CHOICES: &[&str] = &[
    ReasoningEffort::None.as_str(),
    ReasoningEffort::Minimal.as_str(),
    ReasoningEffort::Low.as_str(),
    ReasoningEffort::Medium.as_str(),
    ReasoningEffort::High.as_str(),
    ReasoningEffort::XHigh.as_str(),
    ReasoningEffort::Max.as_str(),
];
const SERVICE_TIER_CHOICES: &[&str] = &[
    ServiceTier::Auto.as_str(),
    ServiceTier::Default.as_str(),
    ServiceTier::Flex.as_str(),
    ServiceTier::Scale.as_str(),
    ServiceTier::Priority.as_str(),
    ServiceTier::Fast.as_str(),
];
const BOOLEAN_CHOICES: &[&str] = &["false", "true"];
const STATUSLINE_PRESET_CHOICES: &[&str] = &["minimal", "balanced", "diagnostic", "custom"];
const STATUSLINE_EXPANSION_CHOICES: &[&str] = &["auto", "compact", "expanded"];
const PRICE_SCHEDULE_SOURCE_CHOICES: &[&str] = &[PriceScheduleSource::Manual.as_str()];

const CONFIG_SCHEMA: &[ConfigSchemaEntry] = &[
    schema_entry(
        "provider.profile",
        "/config provider selected",
        ConfigValueKind::String,
        ALL_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "provider_selector",
    ),
    schema_entry(
        "provider.model",
        "/config model selected",
        ConfigValueKind::String,
        ALL_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "model_selector",
    ),
    schema_entry(
        "runtime.max_output_bytes",
        "/config runtime max-output",
        ConfigValueKind::PositiveInteger,
        ALL_SCOPES,
        ConfigApplicationTiming::NextTurn,
        false,
        "integer",
    ),
    schema_entry(
        "providers.<id>.template",
        "/config provider template",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "provider_template",
    ),
    schema_entry(
        "providers.<id>.credential",
        "/config provider credential",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        true,
        "credential_binding",
    ),
    schema_entry(
        "providers.<id>.base_url",
        "/config provider url",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "url",
    ),
    schema_entry(
        "providers.<id>.routes.responses",
        "/config provider route responses",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "route",
    ),
    schema_entry(
        "providers.<id>.routes.chat_completions",
        "/config provider route chat-completions",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "route",
    ),
    schema_entry(
        "providers.<id>.routes.messages",
        "/config provider route messages",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "route",
    ),
    schema_entry(
        "providers.<id>.routes.models",
        "/config provider route models",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "route",
    ),
    schema_entry(
        "providers.<id>.dialects",
        "/config provider dialects",
        ConfigValueKind::StringList,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "dialect_list",
    ),
    schema_entry(
        "providers.<id>.catalog.mode",
        "/config provider catalog",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "catalog_mode",
    ),
    schema_entry(
        "providers.<id>.pricing.source",
        "/config provider pricing",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "pricing_source",
    ),
    schema_entry(
        "providers.<id>.allow_insecure_loopback",
        "/config provider insecure-loopback",
        ConfigValueKind::Boolean,
        FILE_SCOPES,
        ConfigApplicationTiming::NextProviderEpoch,
        false,
        "toggle",
    ),
    schema_entry(
        "model_presets.<id>.provider",
        "/config model provider",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextTurnAndProviderEpoch,
        false,
        "provider_selector",
    ),
    schema_entry(
        "model_presets.<id>.model",
        "/config model model",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextTurnAndProviderEpoch,
        false,
        "model_selector",
    ),
    schema_entry(
        "model_presets.<id>.dialect",
        "/config model dialect",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextTurnAndProviderEpoch,
        false,
        "dialect",
    ),
    schema_entry(
        "model_presets.<id>.reasoning_effort",
        "/config model reasoning",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextTurn,
        false,
        "reasoning_effort",
    ),
    schema_entry(
        "model_presets.<id>.service_tier",
        "/config model service-tier",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextTurn,
        false,
        "service_tier",
    ),
    schema_entry(
        "model_presets.<id>.max_output_tokens",
        "/config model max-output",
        ConfigValueKind::PositiveInteger,
        FILE_SCOPES,
        ConfigApplicationTiming::NextTurn,
        false,
        "integer",
    ),
    schema_entry(
        "model_presets.<id>.context_mode",
        "/config model context",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextTurn,
        false,
        "context_mode",
    ),
    schema_entry(
        "model_presets.<id>.favorite",
        "/config model favorite",
        ConfigValueKind::Boolean,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "toggle",
    ),
    schema_entry(
        "model_presets.<id>.fallback",
        "/config model fallback",
        ConfigValueKind::StringList,
        FILE_SCOPES,
        ConfigApplicationTiming::NextTurn,
        false,
        "preset_list",
    ),
    schema_entry(
        "price_schedules.<id>.version",
        "/config pricing version",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "price_schedule_version",
    ),
    schema_entry(
        "price_schedules.<id>.currency",
        "/config pricing currency",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "currency",
    ),
    schema_entry(
        "price_schedules.<id>.provider",
        "/config pricing provider",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "provider_selector",
    ),
    schema_entry(
        "price_schedules.<id>.model",
        "/config pricing model",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "model_selector",
    ),
    schema_entry(
        "price_schedules.<id>.dialect",
        "/config pricing dialect",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "dialect",
    ),
    schema_entry(
        "price_schedules.<id>.service_tier",
        "/config pricing service-tier",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "service_tier",
    ),
    schema_entry(
        "price_schedules.<id>.minimum_context_tokens",
        "/config pricing context-min",
        ConfigValueKind::NonNegativeInteger,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "non_negative_integer",
    ),
    schema_entry(
        "price_schedules.<id>.maximum_context_tokens",
        "/config pricing context-max",
        ConfigValueKind::NonNegativeInteger,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "non_negative_integer",
    ),
    schema_entry(
        "price_schedules.<id>.effective_from",
        "/config pricing effective-from",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "utc_timestamp",
    ),
    schema_entry(
        "price_schedules.<id>.effective_until",
        "/config pricing effective-until",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "utc_timestamp",
    ),
    schema_entry(
        "price_schedules.<id>.source",
        "/config pricing source",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "pricing_source",
    ),
    schema_entry(
        "price_schedules.<id>.source_ref",
        "/config pricing source-ref",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "source_reference",
    ),
    schema_entry(
        "price_schedules.<id>.rates.input_micros_per_million",
        "/config pricing rate-input",
        ConfigValueKind::NonNegativeInteger,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "price_rate",
    ),
    schema_entry(
        "price_schedules.<id>.rates.cached_input_micros_per_million",
        "/config pricing rate-cached-input",
        ConfigValueKind::NonNegativeInteger,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "price_rate",
    ),
    schema_entry(
        "price_schedules.<id>.rates.cache_write_micros_per_million",
        "/config pricing rate-cache-write",
        ConfigValueKind::NonNegativeInteger,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "price_rate",
    ),
    schema_entry(
        "price_schedules.<id>.rates.output_micros_per_million",
        "/config pricing rate-output",
        ConfigValueKind::NonNegativeInteger,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "price_rate",
    ),
    schema_entry(
        "price_schedules.<id>.rates.reasoning_output_micros_per_million",
        "/config pricing rate-reasoning",
        ConfigValueKind::NonNegativeInteger,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "price_rate",
    ),
    schema_entry(
        "ui.statusline.preset",
        "/config statusline preset",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::Immediate,
        false,
        "statusline_preset",
    ),
    schema_entry(
        "ui.statusline.expand",
        "/config statusline expansion",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::Immediate,
        false,
        "expansion_policy",
    ),
    schema_entry(
        "ui.statusline.primary_usage_window",
        "/config statusline usage-window",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::Immediate,
        false,
        "usage_window_selector",
    ),
    schema_entry(
        "ui.statusline.custom.left",
        "/config statusline left",
        ConfigValueKind::StringList,
        FILE_SCOPES,
        ConfigApplicationTiming::Immediate,
        false,
        "segment_list",
    ),
    schema_entry(
        "ui.statusline.custom.right",
        "/config statusline right",
        ConfigValueKind::StringList,
        FILE_SCOPES,
        ConfigApplicationTiming::Immediate,
        false,
        "segment_list",
    ),
    schema_entry(
        "stats.windows.<id>.start",
        "/config stats-window start",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "local_time",
    ),
    schema_entry(
        "stats.windows.<id>.end",
        "/config stats-window end",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "local_time",
    ),
    schema_entry(
        "stats.windows.<id>.days",
        "/config stats-window days",
        ConfigValueKind::StringList,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "weekday_list",
    ),
    schema_entry(
        "stats.windows.<id>.timezone",
        "/config stats-window timezone",
        ConfigValueKind::String,
        FILE_SCOPES,
        ConfigApplicationTiming::NextConfigEpoch,
        false,
        "timezone",
    ),
];

const fn schema_entry(
    path_pattern: &'static str,
    command_path: &'static str,
    value_kind: ConfigValueKind,
    scopes: &'static [ConfigScope],
    timing: ConfigApplicationTiming,
    credential_reference: bool,
    editor: &'static str,
) -> ConfigSchemaEntry {
    ConfigSchemaEntry {
        path_pattern,
        command_path,
        value_kind,
        scopes,
        timing,
        credential_reference,
        editor,
    }
}

#[must_use]
pub const fn config_schema() -> &'static [ConfigSchemaEntry] {
    CONFIG_SCHEMA
}

fn config_field_interaction(descriptor: &ConfigSchemaEntry) -> ConfigFieldInteraction {
    match (
        descriptor.path_pattern,
        descriptor.value_kind,
        descriptor.credential_reference,
        descriptor.editor,
    ) {
        ("provider.profile", ConfigValueKind::String, false, "provider_selector") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_ID_BYTES,
            }
        }
        ("provider.model", ConfigValueKind::String, false, "model_selector") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_STRING_BYTES,
            }
        }
        ("runtime.max_output_bytes", ConfigValueKind::PositiveInteger, false, "integer") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_STRING_BYTES,
            }
        }
        ("providers.<id>.template", ConfigValueKind::String, false, "provider_template") => {
            ConfigFieldInteraction::Choice {
                choices: PROVIDER_TEMPLATE_CHOICES,
            }
        }
        ("providers.<id>.credential", ConfigValueKind::String, true, "credential_binding") => {
            ConfigFieldInteraction::CredentialReference {
                max_bytes: MAX_CONFIG_ID_BYTES,
            }
        }
        ("ui.statusline.preset", ConfigValueKind::String, false, "statusline_preset") => {
            ConfigFieldInteraction::Choice {
                choices: STATUSLINE_PRESET_CHOICES,
            }
        }
        ("ui.statusline.expand", ConfigValueKind::String, false, "expansion_policy") => {
            ConfigFieldInteraction::Choice {
                choices: STATUSLINE_EXPANSION_CHOICES,
            }
        }
        (
            "ui.statusline.primary_usage_window",
            ConfigValueKind::String,
            false,
            "usage_window_selector",
        ) => ConfigFieldInteraction::Text {
            max_bytes: MAX_CONFIG_ID_BYTES,
        },
        (
            "ui.statusline.custom.left" | "ui.statusline.custom.right",
            ConfigValueKind::StringList,
            false,
            "segment_list",
        ) => ConfigFieldInteraction::Text {
            max_bytes: MAX_CONFIG_STRING_BYTES,
        },
        ("providers.<id>.base_url", ConfigValueKind::String, false, "url") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_STRING_BYTES,
            }
        }
        (
            "providers.<id>.routes.responses"
            | "providers.<id>.routes.chat_completions"
            | "providers.<id>.routes.messages"
            | "providers.<id>.routes.models",
            ConfigValueKind::String,
            false,
            "route",
        )
        | ("providers.<id>.dialects", ConfigValueKind::StringList, false, "dialect_list") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_STRING_BYTES,
            }
        }
        ("providers.<id>.catalog.mode", ConfigValueKind::String, false, "catalog_mode") => {
            ConfigFieldInteraction::Choice {
                choices: PROVIDER_CATALOG_MODE_CHOICES,
            }
        }
        ("providers.<id>.pricing.source", ConfigValueKind::String, false, "pricing_source") => {
            ConfigFieldInteraction::Choice {
                choices: PROVIDER_PRICING_SOURCE_CHOICES,
            }
        }
        ("providers.<id>.allow_insecure_loopback", ConfigValueKind::Boolean, false, "toggle") => {
            ConfigFieldInteraction::Choice {
                choices: BOOLEAN_CHOICES,
            }
        }
        ("model_presets.<id>.provider", ConfigValueKind::String, false, "provider_selector") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_ID_BYTES,
            }
        }
        ("model_presets.<id>.model", ConfigValueKind::String, false, "model_selector") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_STRING_BYTES,
            }
        }
        ("model_presets.<id>.dialect", ConfigValueKind::String, false, "dialect") => {
            ConfigFieldInteraction::Choice {
                choices: PROVIDER_DIALECT_CHOICES,
            }
        }
        (
            "model_presets.<id>.reasoning_effort",
            ConfigValueKind::String,
            false,
            "reasoning_effort",
        ) => ConfigFieldInteraction::Choice {
            choices: REASONING_EFFORT_CHOICES,
        },
        ("model_presets.<id>.service_tier", ConfigValueKind::String, false, "service_tier") => {
            ConfigFieldInteraction::Choice {
                choices: SERVICE_TIER_CHOICES,
            }
        }
        (
            "model_presets.<id>.max_output_tokens",
            ConfigValueKind::PositiveInteger,
            false,
            "integer",
        )
        | ("model_presets.<id>.context_mode", ConfigValueKind::String, false, "context_mode")
        | ("model_presets.<id>.fallback", ConfigValueKind::StringList, false, "preset_list") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_STRING_BYTES,
            }
        }
        ("model_presets.<id>.favorite", ConfigValueKind::Boolean, false, "toggle") => {
            ConfigFieldInteraction::Choice {
                choices: BOOLEAN_CHOICES,
            }
        }
        ("price_schedules.<id>.provider", ConfigValueKind::String, false, "provider_selector") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_ID_BYTES,
            }
        }
        ("price_schedules.<id>.dialect", ConfigValueKind::String, false, "dialect") => {
            ConfigFieldInteraction::Choice {
                choices: PROVIDER_DIALECT_CHOICES,
            }
        }
        ("price_schedules.<id>.source", ConfigValueKind::String, false, "pricing_source") => {
            ConfigFieldInteraction::Choice {
                choices: PRICE_SCHEDULE_SOURCE_CHOICES,
            }
        }
        (
            "price_schedules.<id>.version",
            ConfigValueKind::String,
            false,
            "price_schedule_version",
        )
        | ("price_schedules.<id>.currency", ConfigValueKind::String, false, "currency")
        | ("price_schedules.<id>.model", ConfigValueKind::String, false, "model_selector")
        | ("price_schedules.<id>.service_tier", ConfigValueKind::String, false, "service_tier")
        | (
            "price_schedules.<id>.effective_from" | "price_schedules.<id>.effective_until",
            ConfigValueKind::String,
            false,
            "utc_timestamp",
        )
        | ("price_schedules.<id>.source_ref", ConfigValueKind::String, false, "source_reference")
        | (
            "price_schedules.<id>.minimum_context_tokens"
            | "price_schedules.<id>.maximum_context_tokens",
            ConfigValueKind::NonNegativeInteger,
            false,
            "non_negative_integer",
        )
        | (
            "price_schedules.<id>.rates.input_micros_per_million"
            | "price_schedules.<id>.rates.cached_input_micros_per_million"
            | "price_schedules.<id>.rates.cache_write_micros_per_million"
            | "price_schedules.<id>.rates.output_micros_per_million"
            | "price_schedules.<id>.rates.reasoning_output_micros_per_million",
            ConfigValueKind::NonNegativeInteger,
            false,
            "price_rate",
        ) => ConfigFieldInteraction::Text {
            max_bytes: MAX_CONFIG_STRING_BYTES,
        },
        (
            "stats.windows.<id>.start" | "stats.windows.<id>.end",
            ConfigValueKind::String,
            false,
            "local_time",
        )
        | ("stats.windows.<id>.timezone", ConfigValueKind::String, false, "timezone")
        | ("stats.windows.<id>.days", ConfigValueKind::StringList, false, "weekday_list") => {
            ConfigFieldInteraction::Text {
                max_bytes: MAX_CONFIG_STRING_BYTES,
            }
        }
        _ => ConfigFieldInteraction::ReadOnly,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ConfigValue {
    String(String),
    PositiveInteger(u32),
    NonNegativeInteger(u64),
    Boolean(bool),
    StringList(Vec<String>),
}

impl ConfigValue {
    fn kind(&self) -> ConfigValueKind {
        match self {
            Self::String(_) => ConfigValueKind::String,
            Self::PositiveInteger(_) => ConfigValueKind::PositiveInteger,
            Self::NonNegativeInteger(_) => ConfigValueKind::NonNegativeInteger,
            Self::Boolean(_) => ConfigValueKind::Boolean,
            Self::StringList(_) => ConfigValueKind::StringList,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveConfigEntry {
    pub path: String,
    pub value: ConfigValue,
    pub source: ConfigScope,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigObjectKind {
    ProviderProfile,
    ModelPreset,
    UsageWindow,
    PriceSchedule,
}

impl ConfigObjectKind {
    const fn path_prefix(self) -> &'static str {
        match self {
            Self::ProviderProfile => "providers.<id>.",
            Self::ModelPreset => "model_presets.<id>.",
            Self::UsageWindow => "stats.windows.<id>.",
            Self::PriceSchedule => "price_schedules.<id>.",
        }
    }

    fn object_path(self, id: &str) -> String {
        self.path_prefix().replace("<id>.", id)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ConfigObjectRef {
    kind: ConfigObjectKind,
    id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelPresetView {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub dialect: ProviderDialect,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub service_tier: Option<ServiceTier>,
    pub max_output_tokens: Option<u32>,
    pub context_mode: Option<String>,
    pub favorite: bool,
    pub fallback: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelCatalogView {
    provider: String,
    record: &'static ModelCatalogRecord,
    profile_compatible: bool,
}

impl ModelCatalogView {
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub const fn record(&self) -> &'static ModelCatalogRecord {
        self.record
    }

    #[must_use]
    pub const fn profile_compatible(&self) -> bool {
        self.profile_compatible
    }
}

impl ConfigObjectRef {
    #[must_use]
    pub fn new(kind: ConfigObjectKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ConfigObjectKind {
        self.kind
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConfigFieldContents {
    Value {
        effective: Option<ConfigValue>,
        source: Option<ConfigScope>,
        target: Option<ConfigValue>,
    },
    CredentialBinding {
        effective_bound: bool,
        source: Option<ConfigScope>,
        target_bound: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigFieldView {
    pub path: String,
    pub path_pattern: &'static str,
    pub command_path: &'static str,
    pub value_kind: ConfigValueKind,
    pub target_scope: ConfigScope,
    pub timing: ConfigApplicationTiming,
    pub editor: &'static str,
    pub interaction: ConfigFieldInteraction,
    pub contents: ConfigFieldContents,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigChange {
    pub path: String,
    pub before: Option<ConfigValue>,
    pub after: Option<ConfigValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_binding: Option<ConfigCredentialBindingChange>,
    pub timing: ConfigApplicationTiming,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigCredentialBindingChange {
    pub before_bound: bool,
    pub after_bound: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ConfigRevision([u8; 32]);

impl ConfigRevision {
    #[must_use]
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Display for ConfigRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigCommit {
    pub scope: ConfigScope,
    pub base_revision: ConfigRevision,
    pub revision: ConfigRevision,
    pub changes: Vec<ConfigChange>,
    pub written: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigRepairIssue {
    pub scope: ConfigScope,
    pub path: PathBuf,
    pub category: ConfigErrorCategory,
    pub detail: String,
    pub backup_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigRuntimeStatus {
    pub ready: bool,
    pub issues: Vec<ConfigRepairIssue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigPaths {
    user: PathBuf,
    project: PathBuf,
}

impl ConfigPaths {
    #[must_use]
    pub fn new(user: impl Into<PathBuf>, project: impl Into<PathBuf>) -> Self {
        Self {
            user: user.into(),
            project: project.into(),
        }
    }

    #[must_use]
    pub fn user(&self) -> &Path {
        &self.user
    }

    #[must_use]
    pub fn project(&self) -> &Path {
        &self.project
    }

    fn for_scope(&self, scope: ConfigScope) -> Result<&Path, ConfigRuntimeError> {
        match scope {
            ConfigScope::User => Ok(&self.user),
            ConfigScope::Project => Ok(&self.project),
            ConfigScope::BuiltIn | ConfigScope::Cli => {
                Err(ConfigRuntimeError::ReadOnlyScope(scope))
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDocument {
    schema_version: u16,
    #[serde(default, skip_serializing_if = "BootstrapProviderLayer::is_empty")]
    provider: BootstrapProviderLayer,
    #[serde(default, skip_serializing_if = "RuntimeLayer::is_empty")]
    runtime: RuntimeLayer,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    providers: BTreeMap<String, ProviderProfileLayer>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    model_presets: BTreeMap<String, ModelPresetLayer>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    price_schedules: BTreeMap<String, PriceScheduleLayer>,
    #[serde(default, skip_serializing_if = "UiLayer::is_empty")]
    ui: UiLayer,
    #[serde(default, skip_serializing_if = "StatsLayer::is_empty")]
    stats: StatsLayer,
}

impl Default for ConfigDocument {
    fn default() -> Self {
        Self::empty()
    }
}

impl ConfigDocument {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: CONFIG_FILE_SCHEMA_VERSION,
            provider: BootstrapProviderLayer::default(),
            runtime: RuntimeLayer::default(),
            providers: BTreeMap::new(),
            model_presets: BTreeMap::new(),
            price_schedules: BTreeMap::new(),
            ui: UiLayer::default(),
            stats: StatsLayer::default(),
        }
    }

    #[must_use]
    pub fn built_in() -> Self {
        let mut document = Self::empty();
        document.provider.profile = Some("simulator".to_owned());
        document.provider.model = Some("deterministic-v1".to_owned());
        document.runtime.max_output_bytes = Some(super::DEFAULT_MAX_OUTPUT_BYTES);
        document.ui.statusline.preset = Some(StatuslinePreset::Balanced);
        document.ui.statusline.expand = Some(StatuslineExpansion::Auto);
        document
    }

    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct BootstrapProviderLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

impl BootstrapProviderLayer {
    fn is_empty(&self) -> bool {
        self.profile.is_none() && self.model.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct RuntimeLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_bytes: Option<u32>,
}

impl RuntimeLayer {
    fn is_empty(&self) -> bool {
        self.max_output_bytes.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderProfileLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "ProviderRoutesLayer::is_empty")]
    routes: ProviderRoutesLayer,
    #[serde(skip_serializing_if = "Option::is_none")]
    dialects: Option<Vec<ProviderDialect>>,
    #[serde(skip_serializing_if = "CatalogLayer::is_empty")]
    catalog: CatalogLayer,
    #[serde(skip_serializing_if = "PricingLayer::is_empty")]
    pricing: PricingLayer,
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_insecure_loopback: Option<bool>,
}

impl ProviderProfileLayer {
    fn is_empty(&self) -> bool {
        self.template.is_none()
            && self.credential.is_none()
            && self.base_url.is_none()
            && self.routes.is_empty()
            && self.dialects.is_none()
            && self.catalog.is_empty()
            && self.pricing.is_empty()
            && self.allow_insecure_loopback.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderRoutesLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    responses: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_completions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<String>,
}

impl ProviderRoutesLayer {
    fn is_empty(&self) -> bool {
        self.responses.is_none()
            && self.chat_completions.is_none()
            && self.messages.is_none()
            && self.models.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct CatalogLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<ProviderCatalogMode>,
}

impl CatalogLayer {
    fn is_empty(&self) -> bool {
        self.mode.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct PricingLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<ProviderPricingSource>,
}

impl PricingLayer {
    fn is_empty(&self) -> bool {
        self.source.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ModelPresetLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dialect: Option<ProviderDialect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    favorite: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback: Option<Vec<String>>,
}

impl ModelPresetLayer {
    fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && self.dialect.is_none()
            && self.reasoning_effort.is_none()
            && self.service_tier.is_none()
            && self.max_output_tokens.is_none()
            && self.context_mode.is_none()
            && self.favorite.is_none()
            && self.fallback.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct PriceScheduleLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dialect: Option<ProviderDialect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum_context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<PriceScheduleSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ref: Option<String>,
    #[serde(skip_serializing_if = "PriceRatesLayer::is_empty")]
    rates: PriceRatesLayer,
}

impl PriceScheduleLayer {
    fn is_empty(&self) -> bool {
        self.version.is_none()
            && self.currency.is_none()
            && self.provider.is_none()
            && self.model.is_none()
            && self.dialect.is_none()
            && self.service_tier.is_none()
            && self.minimum_context_tokens.is_none()
            && self.maximum_context_tokens.is_none()
            && self.effective_from.is_none()
            && self.effective_until.is_none()
            && self.source.is_none()
            && self.source_ref.is_none()
            && self.rates.is_empty()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct PriceRatesLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    input_micros_per_million: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_input_micros_per_million: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write_micros_per_million: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_micros_per_million: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_output_micros_per_million: Option<u64>,
}

impl PriceRatesLayer {
    fn is_empty(&self) -> bool {
        self.input_micros_per_million.is_none()
            && self.cached_input_micros_per_million.is_none()
            && self.cache_write_micros_per_million.is_none()
            && self.output_micros_per_million.is_none()
            && self.reasoning_output_micros_per_million.is_none()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct UiLayer {
    #[serde(skip_serializing_if = "StatuslineLayer::is_empty")]
    statusline: StatuslineLayer,
}

impl UiLayer {
    fn is_empty(&self) -> bool {
        self.statusline.is_empty()
    }
}

#[derive(Clone)]
pub struct ConfigDraft {
    scope: ConfigScope,
    base_revision: ConfigRevision,
    document: ConfigDocument,
}

impl ConfigDraft {
    #[must_use]
    pub const fn scope(&self) -> ConfigScope {
        self.scope
    }

    #[must_use]
    pub const fn base_revision(&self) -> ConfigRevision {
        self.base_revision
    }

    pub fn get(&self, path: &str) -> Result<Option<ConfigValue>, ConfigRuntimeError> {
        if require_schema_entry(path)?.credential_reference {
            return Err(ConfigRuntimeError::SecretReadForbidden(path.to_owned()));
        }
        self.document.get(path)
    }

    pub fn set(&mut self, path: &str, value: ConfigValue) -> Result<(), ConfigRuntimeError> {
        self.document.set(self.scope, path, value)
    }

    pub fn set_raw(&mut self, path: &str, raw: &str) -> Result<(), ConfigRuntimeError> {
        self.set(path, parse_config_value(path, raw)?)
    }

    pub fn reset(&mut self, path: &str) -> Result<(), ConfigRuntimeError> {
        self.document.reset(self.scope, path)
    }

    pub fn contains_object(&self, object: &ConfigObjectRef) -> Result<bool, ConfigRuntimeError> {
        self.document.contains_object(object)
    }

    pub fn delete_object(&mut self, object: &ConfigObjectRef) -> Result<(), ConfigRuntimeError> {
        self.document.delete_object(object)
    }
}

impl fmt::Debug for ConfigDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigDraft")
            .field("scope", &self.scope)
            .field("base_revision", &self.base_revision)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
struct LoadedLayer {
    document: ConfigDocument,
    revision: ConfigRevision,
    bytes: Vec<u8>,
    exists: bool,
}

#[derive(Debug)]
struct LayerState {
    current: Option<LoadedLayer>,
    issue: Option<ConfigRepairIssue>,
}

impl LayerState {
    fn valid(layer: LoadedLayer) -> Self {
        Self {
            current: Some(layer),
            issue: None,
        }
    }
}

#[derive(Clone, Debug)]
struct ResolvedState {
    document: ConfigDocument,
    entries: Vec<EffectiveConfigEntry>,
    internal_entries: Vec<EffectiveConfigEntry>,
    layers: ConfigLayers,
}

#[derive(Debug)]
pub struct ConfigRuntime {
    paths: ConfigPaths,
    built_in: ConfigDocument,
    cli: ConfigDocument,
    user: LayerState,
    project: LayerState,
    last_valid: Option<ResolvedState>,
}

impl ConfigRuntime {
    pub fn open(paths: ConfigPaths, cli: ConfigDocument) -> Result<Self, ConfigRuntimeError> {
        let built_in = ConfigDocument::built_in();
        built_in.validate_layer()?;
        cli.validate_layer()?;
        let mut user = load_layer_state(ConfigScope::User, paths.user())?;
        let mut project = load_layer_state(ConfigScope::Project, paths.project())?;
        let last_valid = resolve_if_valid(&built_in, &paths, &mut user, &mut project, &cli)?;
        Ok(Self {
            paths,
            built_in,
            cli,
            user,
            project,
            last_valid,
        })
    }

    #[must_use]
    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    #[must_use]
    pub fn status(&self) -> ConfigRuntimeStatus {
        let issues = [self.user.issue.as_ref(), self.project.issue.as_ref()]
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        ConfigRuntimeStatus {
            ready: issues.is_empty() && self.last_valid.is_some(),
            issues,
        }
    }

    pub fn reload(&mut self) -> Result<ConfigRuntimeStatus, ConfigRuntimeError> {
        let mut user = load_layer_state(ConfigScope::User, self.paths.user())?;
        let mut project = load_layer_state(ConfigScope::Project, self.paths.project())?;
        if let Some(resolved) = resolve_if_valid(
            &self.built_in,
            &self.paths,
            &mut user,
            &mut project,
            &self.cli,
        )? {
            self.last_valid = Some(resolved);
        }
        self.user = user;
        self.project = project;
        Ok(self.status())
    }

    pub fn effective_entries(&self) -> Result<&[EffectiveConfigEntry], ConfigRuntimeError> {
        Ok(self.resolved_state()?.entries.as_slice())
    }

    pub fn get_effective(
        &self,
        path: &str,
    ) -> Result<Option<&EffectiveConfigEntry>, ConfigRuntimeError> {
        let descriptor = require_schema_entry(path)?;
        if descriptor.credential_reference {
            return Err(ConfigRuntimeError::SecretReadForbidden(path.to_owned()));
        }
        self.effective_entry(path)
    }

    pub fn addressable_objects(&self) -> Result<Vec<ConfigObjectRef>, ConfigRuntimeError> {
        let resolved = self
            .last_valid
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        let mut objects =
            resolved
                .document
                .providers
                .keys()
                .map(|id| ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, id.clone()))
                .chain(
                    resolved
                        .document
                        .model_presets
                        .keys()
                        .map(|id| ConfigObjectRef::new(ConfigObjectKind::ModelPreset, id.clone())),
                )
                .chain(
                    resolved.document.price_schedules.keys().map(|id| {
                        ConfigObjectRef::new(ConfigObjectKind::PriceSchedule, id.clone())
                    }),
                )
                .collect::<Vec<_>>();
        if let Some(windows) = &resolved.document.stats.windows {
            objects.extend(
                windows
                    .iter()
                    .map(|window| ConfigObjectRef::new(ConfigObjectKind::UsageWindow, &window.id)),
            );
        }
        objects.sort();
        Ok(objects)
    }

    pub fn model_presets(&self) -> Result<Vec<ModelPresetView>, ConfigRuntimeError> {
        let resolved = self
            .last_valid
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        resolved
            .document
            .model_presets
            .iter()
            .map(|(id, preset)| {
                let provider = preset.provider.clone().ok_or_else(|| {
                    invalid(
                        format!("model_presets.{id}.provider"),
                        "model preset requires a provider",
                    )
                })?;
                let model = preset.model.clone().ok_or_else(|| {
                    invalid(
                        format!("model_presets.{id}.model"),
                        "model preset requires a model",
                    )
                })?;
                let dialect = preset.dialect.ok_or_else(|| {
                    invalid(
                        format!("model_presets.{id}.dialect"),
                        "model preset requires a dialect",
                    )
                })?;
                let reasoning_effort = preset
                    .reasoning_effort
                    .as_deref()
                    .map(|value| {
                        ReasoningEffort::parse(value).ok_or_else(|| {
                            invalid(
                                format!("model_presets.{id}.reasoning_effort"),
                                "unknown reasoning effort",
                            )
                        })
                    })
                    .transpose()?;
                let service_tier = preset
                    .service_tier
                    .as_deref()
                    .map(|value| {
                        ServiceTier::parse(value).ok_or_else(|| {
                            invalid(
                                format!("model_presets.{id}.service_tier"),
                                "unknown service tier",
                            )
                        })
                    })
                    .transpose()?;
                Ok(ModelPresetView {
                    id: id.clone(),
                    provider,
                    model,
                    dialect,
                    reasoning_effort,
                    service_tier,
                    max_output_tokens: preset.max_output_tokens,
                    context_mode: preset.context_mode.clone(),
                    favorite: preset.favorite.unwrap_or(false),
                    fallback: preset.fallback.clone().unwrap_or_default(),
                })
            })
            .collect()
    }

    pub fn model_preset(&self, id: &str) -> Result<ModelPresetView, ConfigRuntimeError> {
        validate_id("model preset", id)?;
        self.model_presets()?
            .into_iter()
            .find(|preset| preset.id == id)
            .ok_or_else(|| ConfigRuntimeError::UnknownObject(format!("model_presets.{id}")))
    }

    pub fn catalog_models(&self) -> Result<Vec<ModelCatalogView>, ConfigRuntimeError> {
        let resolved = self
            .last_valid
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        let catalog = ProviderCatalog::release();
        let mut models = Vec::new();
        for (profile, definition) in &resolved.document.providers {
            let Some(template_id) = definition.template.as_deref() else {
                continue;
            };
            let Some(template) = catalog.template(template_id) else {
                continue;
            };
            let catalog_mode = definition
                .catalog
                .mode
                .unwrap_or(template.catalog_mode().value());
            if !catalog_mode.includes_release_seed() {
                continue;
            }
            let snapshot = provider_profile_snapshot(&resolved.document, profile)?;
            models.extend(
                catalog
                    .models()
                    .iter()
                    .filter(|record| record.provider_template() == template_id)
                    .map(|record| ModelCatalogView {
                        provider: profile.clone(),
                        record,
                        profile_compatible: snapshot.supports(record.primary_dialect().value()),
                    }),
            );
        }
        Ok(models)
    }

    pub fn inspect_field(
        &self,
        target_scope: ConfigScope,
        path: &str,
    ) -> Result<ConfigFieldView, ConfigRuntimeError> {
        let target = self.target_document(target_scope)?;
        self.inspect_document_field(target_scope, path, target)
    }

    pub fn inspect_draft_field(
        &self,
        draft: &ConfigDraft,
        path: &str,
    ) -> Result<ConfigFieldView, ConfigRuntimeError> {
        self.inspect_document_field(draft.scope, path, &draft.document)
    }

    fn inspect_document_field(
        &self,
        target_scope: ConfigScope,
        path: &str,
        target_document: &ConfigDocument,
    ) -> Result<ConfigFieldView, ConfigRuntimeError> {
        if !target_scope.is_writable() {
            return Err(ConfigRuntimeError::ReadOnlyScope(target_scope));
        }
        let descriptor = require_schema_entry(path)?;
        if !descriptor.scopes.contains(&target_scope) {
            return Err(ConfigRuntimeError::ReadOnlyScope(target_scope));
        }
        let target = target_document.get(path)?;
        let effective = self.effective_entry(path)?.cloned();
        let contents = if descriptor.credential_reference {
            ConfigFieldContents::CredentialBinding {
                effective_bound: effective.is_some(),
                source: effective.as_ref().map(|entry| entry.source),
                target_bound: target.is_some(),
            }
        } else {
            ConfigFieldContents::Value {
                effective: effective.as_ref().map(|entry| entry.value.clone()),
                source: effective.as_ref().map(|entry| entry.source),
                target,
            }
        };
        Ok(ConfigFieldView {
            path: path.to_owned(),
            path_pattern: descriptor.path_pattern,
            command_path: descriptor.command_path,
            value_kind: descriptor.value_kind,
            target_scope,
            timing: descriptor.timing,
            editor: descriptor.editor,
            interaction: descriptor.interaction(),
            contents,
        })
    }

    fn resolved_state(&self) -> Result<&ResolvedState, ConfigRuntimeError> {
        self.last_valid
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))
    }

    fn effective_entry(
        &self,
        path: &str,
    ) -> Result<Option<&EffectiveConfigEntry>, ConfigRuntimeError> {
        Ok(self
            .resolved_state()?
            .internal_entries
            .iter()
            .find(|entry| entry.path == path))
    }

    pub fn object_fields(
        &self,
        target_scope: ConfigScope,
        kind: ConfigObjectKind,
        id: &str,
    ) -> Result<Vec<ConfigFieldView>, ConfigRuntimeError> {
        validate_id("<id>", id)?;
        let object = ConfigObjectRef::new(kind, id);
        if !self.addressable_objects()?.contains(&object) {
            let path = kind.path_prefix().replace("<id>.", id);
            return Err(ConfigRuntimeError::UnknownObject(path));
        }
        config_schema()
            .iter()
            .filter(|entry| entry.path_pattern.starts_with(kind.path_prefix()))
            .map(|entry| {
                let path = entry.path_pattern.replacen("<id>", id, 1);
                self.inspect_field(target_scope, &path)
            })
            .collect()
    }

    pub fn draft_object_fields(
        &self,
        draft: &ConfigDraft,
        kind: ConfigObjectKind,
        id: &str,
    ) -> Result<Vec<ConfigFieldView>, ConfigRuntimeError> {
        validate_id("<id>", id)?;
        config_schema()
            .iter()
            .filter(|entry| entry.path_pattern.starts_with(kind.path_prefix()))
            .map(|entry| {
                let path = entry.path_pattern.replacen("<id>", id, 1);
                self.inspect_draft_field(draft, &path)
            })
            .collect()
    }

    pub fn config_layers(&self) -> Result<&ConfigLayers, ConfigRuntimeError> {
        self.last_valid
            .as_ref()
            .map(|resolved| &resolved.layers)
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))
    }

    fn target_document(&self, scope: ConfigScope) -> Result<&ConfigDocument, ConfigRuntimeError> {
        let document = match scope {
            ConfigScope::User => self.user.current.as_ref().map(|layer| &layer.document),
            ConfigScope::Project => self.project.current.as_ref().map(|layer| &layer.document),
            ConfigScope::BuiltIn | ConfigScope::Cli => {
                return Err(ConfigRuntimeError::ReadOnlyScope(scope));
            }
        };
        document.ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))
    }

    pub fn selected_provider_profile(
        &self,
    ) -> Result<Option<ProviderProfileSnapshot>, ConfigRuntimeError> {
        let resolved = self
            .last_valid
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        let profile = resolved
            .layers
            .resolve()
            .map_err(|source| invalid("provider.profile", source.to_string()))?
            .provider_profile()
            .value()
            .clone();
        self.provider_profile(&profile)
    }

    pub fn provider_profile(
        &self,
        profile: &str,
    ) -> Result<Option<ProviderProfileSnapshot>, ConfigRuntimeError> {
        validate_id("provider profile", profile)?;
        if profile == "simulator" {
            return Ok(None);
        }
        let resolved = self
            .last_valid
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        provider_profile_snapshot(&resolved.document, profile).map(Some)
    }

    pub fn provider_profile_for_draft(
        &self,
        draft: &ConfigDraft,
        profile: &str,
    ) -> Result<ProviderProfileSnapshot, ConfigRuntimeError> {
        validate_id("provider profile", profile)?;
        let resolved = self.resolve_draft(draft)?;
        provider_profile_snapshot(&resolved.document, profile)
    }

    pub fn resolved_usage_windows(&self) -> Result<Vec<UsageWindow>, ConfigRuntimeError> {
        let resolved = self
            .last_valid
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        resolved
            .document
            .stats
            .windows
            .iter()
            .flatten()
            .map(resolve_usage_window)
            .collect()
    }

    pub fn resolved_price_schedules(&self) -> Result<PriceScheduleBook, ConfigRuntimeError> {
        let resolved = self
            .last_valid
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        resolve_price_schedule_book(&resolved.document)
    }

    pub fn begin_draft(&self, scope: ConfigScope) -> Result<ConfigDraft, ConfigRuntimeError> {
        if !scope.is_writable() {
            return Err(ConfigRuntimeError::ReadOnlyScope(scope));
        }
        let state = self.state(scope)?;
        let layer = state
            .current
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        Ok(ConfigDraft {
            scope,
            base_revision: layer.revision,
            document: layer.document.clone(),
        })
    }

    pub fn validate_draft(
        &self,
        draft: &ConfigDraft,
    ) -> Result<Vec<ConfigChange>, ConfigRuntimeError> {
        self.resolve_draft(draft)?;
        let current = self
            .state(draft.scope)?
            .current
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        Ok(diff_documents(&current.document, &draft.document))
    }

    fn resolve_draft(&self, draft: &ConfigDraft) -> Result<ResolvedState, ConfigRuntimeError> {
        let current = self
            .state(draft.scope)?
            .current
            .as_ref()
            .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?;
        if current.revision != draft.base_revision {
            return Err(ConfigRuntimeError::RevisionConflict {
                expected: draft.base_revision,
                actual: current.revision,
            });
        }
        draft.document.validate_layer()?;
        let (user, project) = match draft.scope {
            ConfigScope::User => (
                &draft.document,
                &self
                    .project
                    .current
                    .as_ref()
                    .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?
                    .document,
            ),
            ConfigScope::Project => (
                &self
                    .user
                    .current
                    .as_ref()
                    .ok_or_else(|| ConfigRuntimeError::RepairRequired(self.status().issues))?
                    .document,
                &draft.document,
            ),
            ConfigScope::BuiltIn | ConfigScope::Cli => {
                return Err(ConfigRuntimeError::ReadOnlyScope(draft.scope));
            }
        };
        resolve_documents(&self.built_in, user, project, &self.cli)
    }

    pub fn commit(
        &mut self,
        draft: ConfigDraft,
        dry_run: bool,
    ) -> Result<ConfigCommit, ConfigRuntimeError> {
        if !draft.scope.is_writable() {
            return Err(ConfigRuntimeError::ReadOnlyScope(draft.scope));
        }
        let _locks = lock_config_paths(&self.paths)?;
        let user = read_layer(self.paths.user())?;
        let project = read_layer(self.paths.project())?;
        let current = match draft.scope {
            ConfigScope::User => &user,
            ConfigScope::Project => &project,
            ConfigScope::BuiltIn | ConfigScope::Cli => {
                return Err(ConfigRuntimeError::ReadOnlyScope(draft.scope));
            }
        };
        if current.revision != draft.base_revision {
            return Err(ConfigRuntimeError::RevisionConflict {
                expected: draft.base_revision,
                actual: current.revision,
            });
        }
        draft.document.validate_layer()?;
        let (resolved_user, resolved_project) = match draft.scope {
            ConfigScope::User => (&draft.document, &project.document),
            ConfigScope::Project => (&user.document, &draft.document),
            ConfigScope::BuiltIn | ConfigScope::Cli => unreachable!("checked above"),
        };
        resolve_documents(&self.built_in, resolved_user, resolved_project, &self.cli)?;
        let changes = diff_documents(&current.document, &draft.document);
        let bytes = draft.document.to_toml()?.into_bytes();
        let revision = revision(&bytes);
        if !dry_run {
            let path = self.paths.for_scope(draft.scope)?;
            if current.exists {
                atomic_write(&backup_path(path), &current.bytes)?;
            }
            atomic_write(path, &bytes)?;
            self.reload()?;
        }
        Ok(ConfigCommit {
            scope: draft.scope,
            base_revision: draft.base_revision,
            revision,
            changes,
            written: !dry_run,
        })
    }

    pub fn restore_backup(
        &mut self,
        scope: ConfigScope,
    ) -> Result<ConfigCommit, ConfigRuntimeError> {
        if !scope.is_writable() {
            return Err(ConfigRuntimeError::ReadOnlyScope(scope));
        }
        let _locks = lock_config_paths(&self.paths)?;
        let path = self.paths.for_scope(scope)?;
        let current = read_layer_unvalidated(path)?;
        let backup = read_layer(&backup_path(path)).map_err(|source| {
            ConfigRuntimeError::BackupUnavailable {
                scope,
                detail: source.to_string(),
            }
        })?;
        if !backup.exists {
            return Err(ConfigRuntimeError::BackupUnavailable {
                scope,
                detail: "backup file does not exist".to_owned(),
            });
        }
        let user = if scope == ConfigScope::User {
            backup.clone()
        } else {
            read_layer(self.paths.user())?
        };
        let project = if scope == ConfigScope::Project {
            backup.clone()
        } else {
            read_layer(self.paths.project())?
        };
        resolve_documents(&self.built_in, &user.document, &project.document, &self.cli)?;
        atomic_write(path, &backup.bytes)?;
        self.reload()?;
        Ok(ConfigCommit {
            scope,
            base_revision: current.revision,
            revision: backup.revision,
            changes: diff_documents(&current.document, &backup.document),
            written: true,
        })
    }

    fn state(&self, scope: ConfigScope) -> Result<&LayerState, ConfigRuntimeError> {
        match scope {
            ConfigScope::User => Ok(&self.user),
            ConfigScope::Project => Ok(&self.project),
            ConfigScope::BuiltIn | ConfigScope::Cli => {
                Err(ConfigRuntimeError::ReadOnlyScope(scope))
            }
        }
    }

    #[must_use]
    pub fn effective_document(&self) -> Option<&ConfigDocument> {
        self.last_valid.as_ref().map(|resolved| &resolved.document)
    }
}

fn resolve_if_valid(
    built_in: &ConfigDocument,
    paths: &ConfigPaths,
    user: &mut LayerState,
    project: &mut LayerState,
    cli: &ConfigDocument,
) -> Result<Option<ResolvedState>, ConfigRuntimeError> {
    let (Some(user_layer), Some(project_layer)) = (&user.current, &project.current) else {
        return Ok(None);
    };
    let user_document = user_layer.document.clone();
    let project_document = project_layer.document.clone();
    match resolve_documents(built_in, &user_document, &project_document, cli) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(source) => {
            let empty = ConfigDocument::empty();
            let user_is_invalid = resolve_documents(built_in, &user_document, &empty, cli).is_err();
            let (scope, path, state) = if user_is_invalid {
                (ConfigScope::User, paths.user(), user)
            } else {
                (ConfigScope::Project, paths.project(), project)
            };
            state.issue = Some(ConfigRepairIssue {
                scope,
                path: path.to_owned(),
                category: source.category(),
                detail: source.to_string(),
                backup_available: read_layer(&backup_path(path)).is_ok_and(|layer| layer.exists),
            });
            Ok(None)
        }
    }
}

fn provider_profile_snapshot(
    document: &ConfigDocument,
    profile: &str,
) -> Result<ProviderProfileSnapshot, ConfigRuntimeError> {
    let definition = document.providers.get(profile).ok_or_else(|| {
        invalid(
            format!("providers.{profile}"),
            "provider profile does not exist",
        )
    })?;
    let template = definition.template.clone().ok_or_else(|| {
        invalid(
            format!("providers.{profile}.template"),
            "provider profile requires a template",
        )
    })?;
    let defaults = ProviderCatalog::release().template(&template);
    let custom_origin = definition.base_url.is_some();
    let base_url = definition
        .base_url
        .clone()
        .or_else(|| defaults.map(|template| template.base_url().value().to_owned()));
    let responses_route = definition.routes.responses.clone().or_else(|| {
        defaults.and_then(|template| template.responses_route().value().map(str::to_owned))
    });
    let chat_completions_route = definition.routes.chat_completions.clone().or_else(|| {
        defaults.and_then(|template| template.chat_completions_route().value().map(str::to_owned))
    });
    let messages_route = definition.routes.messages.clone().or_else(|| {
        defaults.and_then(|template| template.messages_route().value().map(str::to_owned))
    });
    let models_route = definition.routes.models.clone().or_else(|| {
        defaults.and_then(|template| template.models_route().value().map(str::to_owned))
    });
    let dialects = definition.dialects.clone().unwrap_or_else(|| {
        defaults.map_or_else(Vec::new, |template| template.dialects().value().to_vec())
    });
    let pricing_source = resolve_provider_pricing_source(
        profile,
        &template,
        definition.pricing.source,
        custom_origin,
        defaults.map(|template| template.pricing_source().value()),
    )?;
    ProviderProfileSnapshot::from_parts(
        profile,
        template,
        definition.credential.clone(),
        base_url,
        responses_route,
        chat_completions_route,
        messages_route,
        models_route,
        dialects,
        pricing_source,
        definition.allow_insecure_loopback.unwrap_or(false),
    )
    .map_err(|source| invalid(format!("providers.{profile}"), source.to_string()))
}

fn resolve_provider_pricing_source(
    profile: &str,
    template: &str,
    explicit: Option<ProviderPricingSource>,
    custom_origin: bool,
    template_default: Option<ProviderPricingSource>,
) -> Result<Option<ProviderPricingSource>, ConfigRuntimeError> {
    let has_release_rate_card = has_release_price_schedules(template);
    match (explicit, custom_origin) {
        (Some(ProviderPricingSource::Template), true) if has_release_rate_card => {
            Ok(Some(ProviderPricingSource::TemplateMirror))
        }
        (Some(ProviderPricingSource::Template | ProviderPricingSource::TemplateMirror), true) => {
            Err(invalid(
                format!("providers.{profile}.pricing.source"),
                "template mirror pricing requires a bundled release rate card",
            ))
        }
        (Some(ProviderPricingSource::TemplateMirror), false) => Err(invalid(
            format!("providers.{profile}.pricing.source"),
            "template_mirror pricing requires a custom Provider origin",
        )),
        (Some(source), _) => Ok(Some(source)),
        (None, true) if has_release_rate_card => Ok(Some(ProviderPricingSource::TemplateMirror)),
        (None, true) => Ok(None),
        (None, false) => Ok(template_default),
    }
}

fn resolve_price_schedule_book(
    document: &ConfigDocument,
) -> Result<PriceScheduleBook, ConfigRuntimeError> {
    let mut schedules = document
        .price_schedules
        .iter()
        .map(|(id, layer)| resolve_price_schedule(document, id, layer))
        .collect::<Result<Vec<_>, _>>()?;
    for (profile, definition) in &document.providers {
        let Some(template) = definition.template.as_deref() else {
            continue;
        };
        let defaults = ProviderCatalog::release().template(template);
        let pricing_source = resolve_provider_pricing_source(
            profile,
            template,
            definition.pricing.source,
            definition.base_url.is_some(),
            defaults.map(|template| template.pricing_source().value()),
        )?;
        if matches!(
            pricing_source,
            Some(ProviderPricingSource::Template | ProviderPricingSource::TemplateMirror)
        ) {
            let source = match pricing_source {
                Some(ProviderPricingSource::Template) => PriceScheduleSource::Template,
                Some(ProviderPricingSource::TemplateMirror) => PriceScheduleSource::TemplateMirror,
                _ => unreachable!("matched release Price Schedule source"),
            };
            schedules.extend(
                release_price_schedules_for_profile(profile, template, source)
                    .map_err(|source| invalid("price_schedules", source.to_string()))?,
            );
        }
    }
    PriceScheduleBook::new(schedules)
        .map_err(|source| invalid("price_schedules", source.to_string()))
}

fn resolve_price_schedule(
    document: &ConfigDocument,
    id: &str,
    layer: &PriceScheduleLayer,
) -> Result<PriceSchedule, ConfigRuntimeError> {
    let prefix = format!("price_schedules.{id}");
    let required_string = |field: &str, value: &Option<String>| {
        value.clone().ok_or_else(|| {
            invalid(
                format!("{prefix}.{field}"),
                "Price Schedule field is required",
            )
        })
    };
    let required_integer = |field: &str, value: Option<u64>| {
        value.ok_or_else(|| {
            invalid(
                format!("{prefix}.rates.{field}"),
                "Price Schedule rate is required",
            )
        })
    };
    let provider = required_string("provider", &layer.provider)?;
    validate_id(&format!("{prefix}.provider"), &provider)?;
    let profile = document.providers.get(&provider).ok_or_else(|| {
        invalid(
            format!("{prefix}.provider"),
            "Price Schedule references an unknown Provider Profile",
        )
    })?;
    let source = layer.source.ok_or_else(|| {
        invalid(
            format!("{prefix}.source"),
            "Price Schedule source is required",
        )
    })?;
    if source != PriceScheduleSource::Manual {
        return Err(invalid(
            format!("{prefix}.source"),
            "editable Price Schedules currently require manual provenance",
        ));
    }
    let resolved_pricing_source = provider_profile_snapshot(document, &provider)?.pricing_source();
    let source_matches = resolved_pricing_source == Some(ProviderPricingSource::Manual);
    if !source_matches {
        return Err(invalid(
            format!("{prefix}.source"),
            "Price Schedule source conflicts with the Provider Profile pricing decision",
        ));
    }
    if profile.template.is_none() {
        return Err(invalid(
            format!("providers.{provider}.template"),
            "provider profile requires a template",
        ));
    }
    let effective_from = parse_price_timestamp(
        &format!("{prefix}.effective_from"),
        &required_string("effective_from", &layer.effective_from)?,
    )?;
    let effective_until = layer
        .effective_until
        .as_deref()
        .map(|value| parse_price_timestamp(&format!("{prefix}.effective_until"), value))
        .transpose()?;
    PriceSchedule::new(PriceScheduleDefinition {
        id: id.to_owned(),
        version: required_string("version", &layer.version)?,
        currency: required_string("currency", &layer.currency)?,
        provider_profile: provider,
        model: required_string("model", &layer.model)?,
        dialect: layer.dialect,
        service_tier: layer.service_tier.clone(),
        minimum_context_tokens: layer.minimum_context_tokens.ok_or_else(|| {
            invalid(
                format!("{prefix}.minimum_context_tokens"),
                "Price Schedule context range start is required",
            )
        })?,
        maximum_context_tokens: layer.maximum_context_tokens,
        effective_from,
        effective_until,
        source,
        source_ref: required_string("source_ref", &layer.source_ref)?,
        rates: TokenRates::new(
            required_integer(
                "input_micros_per_million",
                layer.rates.input_micros_per_million,
            )?,
            required_integer(
                "cached_input_micros_per_million",
                layer.rates.cached_input_micros_per_million,
            )?,
            required_integer(
                "cache_write_micros_per_million",
                layer.rates.cache_write_micros_per_million,
            )?,
            required_integer(
                "output_micros_per_million",
                layer.rates.output_micros_per_million,
            )?,
            required_integer(
                "reasoning_output_micros_per_million",
                layer.rates.reasoning_output_micros_per_million,
            )?,
        ),
    })
    .map_err(|source| invalid(prefix, source.to_string()))
}

fn parse_price_timestamp(
    path: &str,
    value: &str,
) -> Result<crate::usage::UsageTimestamp, ConfigRuntimeError> {
    let timestamp: jiff::Timestamp = value
        .parse()
        .map_err(|_| invalid(path, "expected an RFC 3339 timestamp"))?;
    crate::usage::UsageTimestamp::from_unix_millis(timestamp.as_millisecond())
        .map_err(|source| invalid(path, source.to_string()))
}

fn resolve_documents(
    built_in: &ConfigDocument,
    user: &ConfigDocument,
    project: &ConfigDocument,
    cli: &ConfigDocument,
) -> Result<ResolvedState, ConfigRuntimeError> {
    for document in [built_in, user, project, cli] {
        document.validate_layer()?;
    }
    let mut document = built_in.clone();
    document.merge_from(user);
    document.merge_from(project);
    document.merge_from(cli);
    validate_effective(&document)?;

    let mut effective = BTreeMap::<String, EffectiveConfigEntry>::new();
    for (source, layer) in [
        (ConfigScope::BuiltIn, built_in),
        (ConfigScope::User, user),
        (ConfigScope::Project, project),
        (ConfigScope::Cli, cli),
    ] {
        for (path, value) in layer.flatten() {
            effective.insert(
                path.clone(),
                EffectiveConfigEntry {
                    path,
                    value,
                    source,
                },
            );
        }
    }
    let layers = ConfigLayers {
        built_in: built_in.bootstrap_layer(),
        user: user.bootstrap_layer(),
        project: project.bootstrap_layer(),
        cli: cli.bootstrap_layer(),
    };
    layers
        .resolve()
        .map_err(|source| invalid("<effective>", source.to_string()))?;
    let internal_entries = effective.into_values().collect::<Vec<_>>();
    let entries = internal_entries
        .iter()
        .filter(|entry| {
            !require_schema_entry(&entry.path)
                .expect("effective entries are schema-owned")
                .credential_reference
        })
        .cloned()
        .collect();
    Ok(ResolvedState {
        document,
        entries,
        internal_entries,
        layers,
    })
}

fn diff_documents(before: &ConfigDocument, after: &ConfigDocument) -> Vec<ConfigChange> {
    let before = before.flatten();
    let after = after.flatten();
    let paths = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    paths
        .into_iter()
        .filter_map(|path| {
            let old = before.get(&path).cloned();
            let new = after.get(&path).cloned();
            if old == new {
                None
            } else {
                let descriptor =
                    require_schema_entry(&path).expect("flatten emits only schema-owned paths");
                let (old, new, credential_binding) = if descriptor.credential_reference {
                    (
                        None,
                        None,
                        Some(ConfigCredentialBindingChange {
                            before_bound: old.is_some(),
                            after_bound: new.is_some(),
                        }),
                    )
                } else {
                    (old, new, None)
                };
                Some(ConfigChange {
                    timing: descriptor.timing,
                    path,
                    before: old,
                    after: new,
                    credential_binding,
                })
            }
        })
        .collect()
}

fn load_layer_state(scope: ConfigScope, path: &Path) -> Result<LayerState, ConfigRuntimeError> {
    match read_layer(path) {
        Ok(layer) => Ok(LayerState::valid(layer)),
        Err(source) => {
            let backup = read_layer(&backup_path(path))
                .ok()
                .filter(|layer| layer.exists);
            Ok(LayerState {
                current: None,
                issue: Some(ConfigRepairIssue {
                    scope,
                    path: path.to_owned(),
                    category: source.category(),
                    detail: source.to_string(),
                    backup_available: backup.is_some(),
                }),
            })
        }
    }
}

fn read_layer(path: &Path) -> Result<LoadedLayer, ConfigRuntimeError> {
    let mut layer = read_layer_unvalidated(path)?;
    if layer.exists {
        let text = std::str::from_utf8(&layer.bytes).map_err(|_| ConfigRuntimeError::Parse {
            path: Some(path.to_owned()),
            detail: "config file is not UTF-8".to_owned(),
        })?;
        layer.document = ConfigDocument::parse(text).map_err(|source| source.with_path(path))?;
    }
    Ok(layer)
}

fn read_layer_unvalidated(path: &Path) -> Result<LoadedLayer, ConfigRuntimeError> {
    let Some(mut file) = open_existing_no_follow(path)? else {
        return Ok(LoadedLayer {
            document: ConfigDocument::empty(),
            revision: revision(b"<missing-config-layer>"),
            bytes: Vec::new(),
            exists: false,
        });
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_CONFIG_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ConfigRuntimeError::Io)?;
    if bytes.len() > MAX_CONFIG_FILE_BYTES {
        return Err(invalid(
            "<document>",
            "config file exceeds the supported size",
        ));
    }
    Ok(LoadedLayer {
        document: ConfigDocument::empty(),
        revision: revision(&bytes),
        bytes,
        exists: true,
    })
}

fn open_existing_no_follow(path: &Path) -> Result<Option<File>, ConfigRuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ConfigRuntimeError::SymlinkPath(path.to_owned()));
            }
            if !metadata.is_file() {
                return Err(ConfigRuntimeError::NotRegularFile(path.to_owned()));
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigRuntimeError::Io(source)),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options.open(path).map_err(ConfigRuntimeError::Io)?;
    if !file.metadata().map_err(ConfigRuntimeError::Io)?.is_file() {
        return Err(ConfigRuntimeError::NotRegularFile(path.to_owned()));
    }
    Ok(Some(file))
}

struct ConfigLocks {
    _first: File,
    _second: Option<File>,
}

fn lock_config_paths(paths: &ConfigPaths) -> Result<ConfigLocks, ConfigRuntimeError> {
    let mut lock_paths = [lock_path(paths.user()), lock_path(paths.project())];
    lock_paths.sort();
    let first = lock_one(&lock_paths[0])?;
    let second = if lock_paths[0] == lock_paths[1] {
        None
    } else {
        Some(lock_one(&lock_paths[1])?)
    };
    Ok(ConfigLocks {
        _first: first,
        _second: second,
    })
}

fn lock_one(path: &Path) -> Result<File, ConfigRuntimeError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigRuntimeError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "config lock path has no parent",
        ))
    })?;
    fs::create_dir_all(parent).map_err(ConfigRuntimeError::Io)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    configure_no_follow(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(ConfigRuntimeError::Io)?;
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => ConfigRuntimeError::Locked(path.to_owned()),
        TryLockError::Error(source) => ConfigRuntimeError::Io(source),
    })?;
    Ok(file)
}

fn configure_no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ConfigRuntimeError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigRuntimeError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "config path has no parent",
        ))
    })?;
    fs::create_dir_all(parent).map_err(ConfigRuntimeError::Io)?;
    reject_non_regular_write_target(path)?;
    #[cfg(unix)]
    let mut options = atomic_write_file::OpenOptions::new();
    #[cfg(not(unix))]
    let options = atomic_write_file::OpenOptions::new();
    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as AtomicOpenOptionsExt;
        use std::os::unix::fs::OpenOptionsExt as StdOpenOptionsExt;

        AtomicOpenOptionsExt::preserve_mode(&mut options, false);
        StdOpenOptionsExt::mode(&mut options, 0o600);
    }
    let mut file = options.open(path).map_err(ConfigRuntimeError::Io)?;
    file.write_all(bytes).map_err(ConfigRuntimeError::Io)?;
    file.flush().map_err(ConfigRuntimeError::Io)?;
    file.commit().map_err(ConfigRuntimeError::Io)?;
    Ok(())
}

fn reject_non_regular_write_target(path: &Path) -> Result<(), ConfigRuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ConfigRuntimeError::SymlinkPath(path.to_owned()))
        }
        Ok(metadata) if !metadata.is_file() => {
            Err(ConfigRuntimeError::NotRegularFile(path.to_owned()))
        }
        Ok(_) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ConfigRuntimeError::Io(source)),
    }
}

fn revision(bytes: &[u8]) -> ConfigRevision {
    let digest = Sha256::digest(bytes);
    ConfigRevision(digest.into())
}

fn backup_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".bak");
    PathBuf::from(value)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".lock");
    PathBuf::from(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigErrorCategory {
    UnknownObject,
    WrongType,
    InvalidValue,
    ReadOnlyScope,
    RevisionConflict,
    SecretReadForbidden,
    RepairRequired,
    ResourceBusy,
    Io,
}

#[derive(Debug)]
pub enum ConfigRuntimeError {
    UnknownObject(String),
    WrongType {
        path: String,
        expected: ConfigValueKind,
        actual: ConfigValueKind,
    },
    InvalidValue {
        path: String,
        reason: String,
    },
    ReadOnlyScope(ConfigScope),
    RevisionConflict {
        expected: ConfigRevision,
        actual: ConfigRevision,
    },
    SecretReadForbidden(String),
    RepairRequired(Vec<ConfigRepairIssue>),
    BackupUnavailable {
        scope: ConfigScope,
        detail: String,
    },
    UnsupportedSchema {
        supported: u16,
        actual: u16,
    },
    Parse {
        path: Option<PathBuf>,
        detail: String,
    },
    SymlinkPath(PathBuf),
    NotRegularFile(PathBuf),
    Locked(PathBuf),
    Io(io::Error),
}

impl ConfigRuntimeError {
    #[must_use]
    pub const fn category(&self) -> ConfigErrorCategory {
        match self {
            Self::UnknownObject(_) => ConfigErrorCategory::UnknownObject,
            Self::WrongType { .. } => ConfigErrorCategory::WrongType,
            Self::InvalidValue { .. }
            | Self::BackupUnavailable { .. }
            | Self::UnsupportedSchema { .. }
            | Self::Parse { .. }
            | Self::SymlinkPath(_)
            | Self::NotRegularFile(_) => ConfigErrorCategory::InvalidValue,
            Self::ReadOnlyScope(_) => ConfigErrorCategory::ReadOnlyScope,
            Self::RevisionConflict { .. } => ConfigErrorCategory::RevisionConflict,
            Self::SecretReadForbidden(_) => ConfigErrorCategory::SecretReadForbidden,
            Self::RepairRequired(_) => ConfigErrorCategory::RepairRequired,
            Self::Locked(_) => ConfigErrorCategory::ResourceBusy,
            Self::Io(_) => ConfigErrorCategory::Io,
        }
    }

    fn with_path(self, path: &Path) -> Self {
        match self {
            Self::Parse { detail, .. } => Self::Parse {
                path: Some(path.to_owned()),
                detail,
            },
            other => other,
        }
    }
}

impl fmt::Display for ConfigRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownObject(path) => write!(formatter, "unknown config object {path}"),
            Self::WrongType {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "config object {path} expects {expected:?}, got {actual:?}"
            ),
            Self::InvalidValue { path, reason } => {
                write!(formatter, "invalid config value at {path}: {reason}")
            }
            Self::ReadOnlyScope(scope) => write!(formatter, "config scope {scope} is read-only"),
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "config revision conflict: expected {expected}, found {actual}"
            ),
            Self::SecretReadForbidden(path) => {
                write!(formatter, "secret config value cannot be read at {path}")
            }
            Self::RepairRequired(issues) => write!(
                formatter,
                "config repair required for {} layer(s)",
                issues.len()
            ),
            Self::BackupUnavailable { scope, detail } => {
                write!(formatter, "config backup unavailable for {scope}: {detail}")
            }
            Self::UnsupportedSchema { supported, actual } => write!(
                formatter,
                "unsupported config schema version {actual}; expected {supported}"
            ),
            Self::Parse { path, detail } => {
                if let Some(path) = path {
                    write!(
                        formatter,
                        "cannot parse config {}: {detail}",
                        path.display()
                    )
                } else {
                    write!(formatter, "cannot parse config: {detail}")
                }
            }
            Self::SymlinkPath(path) => {
                write!(
                    formatter,
                    "config path is a symbolic link: {}",
                    path.display()
                )
            }
            Self::NotRegularFile(path) => {
                write!(
                    formatter,
                    "config path is not a regular file: {}",
                    path.display()
                )
            }
            Self::Locked(path) => {
                write!(
                    formatter,
                    "config is locked by another writer: {}",
                    path.display()
                )
            }
            Self::Io(source) => write!(formatter, "config I/O failed: {source}"),
        }
    }
}

impl Error for ConfigRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            _ => None,
        }
    }
}

impl StatuslinePreset {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Balanced => "balanced",
            Self::Diagnostic => "diagnostic",
            Self::Custom => "custom",
        }
    }
}

impl StatuslineExpansion {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Compact => "compact",
            Self::Expanded => "expanded",
        }
    }
}

impl Weekday {
    const fn as_str(self) -> &'static str {
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
}

fn parse_dialect(path: &str, value: &str) -> Result<ProviderDialect, ConfigRuntimeError> {
    match value {
        "responses" => Ok(ProviderDialect::Responses),
        "chat_completions" => Ok(ProviderDialect::ChatCompletions),
        "messages" => Ok(ProviderDialect::Messages),
        _ => Err(invalid(path, "unknown provider dialect")),
    }
}

fn parse_catalog_mode(path: &str, value: &str) -> Result<ProviderCatalogMode, ConfigRuntimeError> {
    match value {
        "template" => Ok(ProviderCatalogMode::Template),
        "discovery" => Ok(ProviderCatalogMode::Discovery),
        "template_and_discovery" => Ok(ProviderCatalogMode::TemplateAndDiscovery),
        "manual" => Ok(ProviderCatalogMode::Manual),
        _ => Err(invalid(path, "unknown catalog mode")),
    }
}

fn parse_pricing_source(
    path: &str,
    value: &str,
) -> Result<ProviderPricingSource, ConfigRuntimeError> {
    match value {
        "unknown" => Ok(ProviderPricingSource::Unknown),
        "template" => Ok(ProviderPricingSource::Template),
        "template_mirror" => Ok(ProviderPricingSource::TemplateMirror),
        "manual" => Ok(ProviderPricingSource::Manual),
        "provider_reported" => Ok(ProviderPricingSource::ProviderReported),
        _ => Err(invalid(path, "unknown pricing source")),
    }
}

fn parse_price_schedule_source(
    path: &str,
    value: &str,
) -> Result<PriceScheduleSource, ConfigRuntimeError> {
    match value {
        "template" => Ok(PriceScheduleSource::Template),
        "template_mirror" => Ok(PriceScheduleSource::TemplateMirror),
        "manual" => Ok(PriceScheduleSource::Manual),
        "provider_reported" => Ok(PriceScheduleSource::ProviderReported),
        _ => Err(invalid(path, "unknown Price Schedule source")),
    }
}

fn parse_statusline_preset(
    path: &str,
    value: &str,
) -> Result<StatuslinePreset, ConfigRuntimeError> {
    match value {
        "minimal" => Ok(StatuslinePreset::Minimal),
        "balanced" => Ok(StatuslinePreset::Balanced),
        "diagnostic" => Ok(StatuslinePreset::Diagnostic),
        "custom" => Ok(StatuslinePreset::Custom),
        _ => Err(invalid(path, "unknown statusline preset")),
    }
}

fn parse_statusline_expansion(
    path: &str,
    value: &str,
) -> Result<StatuslineExpansion, ConfigRuntimeError> {
    match value {
        "auto" => Ok(StatuslineExpansion::Auto),
        "compact" => Ok(StatuslineExpansion::Compact),
        "expanded" => Ok(StatuslineExpansion::Expanded),
        _ => Err(invalid(path, "unknown statusline expansion policy")),
    }
}

fn parse_weekday(path: &str, value: &str) -> Result<Weekday, ConfigRuntimeError> {
    match value {
        "mon" => Ok(Weekday::Mon),
        "tue" => Ok(Weekday::Tue),
        "wed" => Ok(Weekday::Wed),
        "thu" => Ok(Weekday::Thu),
        "fri" => Ok(Weekday::Fri),
        "sat" => Ok(Weekday::Sat),
        "sun" => Ok(Weekday::Sun),
        _ => Err(invalid(path, "unknown weekday")),
    }
}

fn take_string(value: ConfigValue) -> String {
    match value {
        ConfigValue::String(value) => value,
        _ => unreachable!("value kind checked before mutation"),
    }
}

fn take_positive_integer(value: ConfigValue) -> u32 {
    match value {
        ConfigValue::PositiveInteger(value) => value,
        _ => unreachable!("value kind checked before mutation"),
    }
}

fn take_non_negative_integer(value: ConfigValue) -> u64 {
    match value {
        ConfigValue::NonNegativeInteger(value) => value,
        _ => unreachable!("value kind checked before mutation"),
    }
}

fn take_boolean(value: ConfigValue) -> bool {
    match value {
        ConfigValue::Boolean(value) => value,
        _ => unreachable!("value kind checked before mutation"),
    }
}

fn take_string_list(value: ConfigValue) -> Vec<String> {
    match value {
        ConfigValue::StringList(value) => value,
        _ => unreachable!("value kind checked before mutation"),
    }
}

fn merge_option<T: Clone>(base: &mut Option<T>, overlay: &Option<T>) {
    if let Some(value) = overlay {
        *base = Some(value.clone());
    }
}

fn insert_string(values: &mut BTreeMap<String, ConfigValue>, path: &str, value: &Option<String>) {
    if let Some(value) = value {
        values.insert(path.to_owned(), ConfigValue::String(value.clone()));
    }
}

fn split_path(path: &str) -> Result<Vec<&str>, ConfigRuntimeError> {
    let segments: Vec<_> = path.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        Err(ConfigRuntimeError::UnknownObject(path.to_owned()))
    } else {
        Ok(segments)
    }
}

fn require_schema_entry(path: &str) -> Result<&'static ConfigSchemaEntry, ConfigRuntimeError> {
    if path.starts_with("credentials.") || path.starts_with("credential_values.") {
        return Err(ConfigRuntimeError::SecretReadForbidden(path.to_owned()));
    }
    let entry = CONFIG_SCHEMA
        .iter()
        .find(|entry| path_matches(entry.path_pattern, path))
        .ok_or_else(|| ConfigRuntimeError::UnknownObject(path.to_owned()))?;
    for (pattern_segment, path_segment) in entry.path_pattern.split('.').zip(path.split('.')) {
        if pattern_segment == "<id>" {
            validate_id(path, path_segment)?;
        }
    }
    Ok(entry)
}

fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern_segments: Vec<_> = pattern.split('.').collect();
    let path_segments: Vec<_> = path.split('.').collect();
    pattern_segments.len() == path_segments.len()
        && pattern_segments
            .iter()
            .zip(path_segments)
            .all(|(expected, actual)| *expected == "<id>" || *expected == actual)
}

fn require_scope(
    descriptor: &ConfigSchemaEntry,
    scope: ConfigScope,
) -> Result<(), ConfigRuntimeError> {
    if !scope.is_writable() || !descriptor.scopes.contains(&scope) {
        Err(ConfigRuntimeError::ReadOnlyScope(scope))
    } else {
        Ok(())
    }
}

pub fn parse_config_value(path: &str, raw: &str) -> Result<ConfigValue, ConfigRuntimeError> {
    let descriptor = require_schema_entry(path)?;
    match descriptor.value_kind {
        ConfigValueKind::String => Ok(ConfigValue::String(raw.to_owned())),
        ConfigValueKind::PositiveInteger => raw
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .map(ConfigValue::PositiveInteger)
            .ok_or_else(|| invalid(path, "expected a positive 32-bit integer")),
        ConfigValueKind::NonNegativeInteger => raw
            .parse::<u64>()
            .map(ConfigValue::NonNegativeInteger)
            .map_err(|_| invalid(path, "expected a non-negative 64-bit integer")),
        ConfigValueKind::Boolean => match raw {
            "true" => Ok(ConfigValue::Boolean(true)),
            "false" => Ok(ConfigValue::Boolean(false)),
            _ => Err(invalid(path, "expected true or false")),
        },
        ConfigValueKind::StringList => {
            #[derive(Deserialize)]
            struct ListLiteral {
                value: Vec<String>,
            }
            let literal = format!("value = {raw}");
            let parsed: ListLiteral = toml::from_str(&literal)
                .map_err(|_| invalid(path, "expected a TOML array of strings"))?;
            Ok(ConfigValue::StringList(parsed.value))
        }
    }
}

fn validate_value(path: &str, value: &ConfigValue) -> Result<(), ConfigRuntimeError> {
    match value {
        ConfigValue::String(value) => {
            validate_string(path, value)?;
            if path_matches("model_presets.<id>.reasoning_effort", path)
                && ReasoningEffort::parse(value).is_none()
            {
                Err(invalid(path, "unknown reasoning effort"))
            } else if path_matches("model_presets.<id>.service_tier", path)
                && ServiceTier::parse(value).is_none()
            {
                Err(invalid(path, "unknown service tier"))
            } else {
                Ok(())
            }
        }
        ConfigValue::PositiveInteger(value) => {
            if *value == 0 {
                Err(invalid(path, "value must be greater than zero"))
            } else if path == "runtime.max_output_bytes" && *value > super::MAX_OUTPUT_BYTES {
                Err(invalid(path, "value exceeds the supported output limit"))
            } else if path_matches("model_presets.<id>.max_output_tokens", path)
                && *value > super::MAX_OUTPUT_TOKENS
            {
                Err(invalid(
                    path,
                    "value exceeds the supported output-token limit",
                ))
            } else {
                Ok(())
            }
        }
        ConfigValue::NonNegativeInteger(_) => Ok(()),
        ConfigValue::Boolean(_) => Ok(()),
        ConfigValue::StringList(values) => {
            if values.len() > MAX_CONFIG_LIST_ITEMS {
                return Err(invalid(path, "list exceeds the supported item count"));
            }
            let mut unique = BTreeSet::new();
            for value in values {
                validate_string(path, value)?;
                if !unique.insert(value) {
                    return Err(invalid(path, "list items must be unique"));
                }
            }
            Ok(())
        }
    }
}

fn validate_string(path: &str, value: &str) -> Result<(), ConfigRuntimeError> {
    if value.trim().is_empty() {
        Err(invalid(path, "value cannot be empty"))
    } else if value.trim() != value {
        Err(invalid(path, "value has surrounding whitespace"))
    } else if value.len() > super::MAX_CONFIG_STRING_BYTES {
        Err(invalid(path, "value exceeds the supported size"))
    } else if value.chars().any(char::is_control) {
        Err(invalid(path, "value contains control characters"))
    } else {
        Ok(())
    }
}

fn validate_id(path: &str, id: &str) -> Result<(), ConfigRuntimeError> {
    if id.is_empty()
        || id.len() > MAX_CONFIG_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
    {
        Err(invalid(
            path,
            "ID must start with a lowercase letter or digit and contain only lowercase ASCII letters, digits, or hyphens",
        ))
    } else {
        Ok(())
    }
}

fn normalize_route(path: &str, route: &str) -> Result<String, ConfigRuntimeError> {
    validate_string(path, route)?;
    if route.contains('?')
        || route.contains('#')
        || route.contains("://")
        || route.contains('\\')
        || route
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(invalid(
            path,
            "route must be a path without authority, dot segments, query, or fragment",
        ));
    }
    let trimmed = route.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(invalid(path, "route cannot be the origin root"));
    }
    Ok(format!("/{trimmed}"))
}

fn validate_effective(document: &ConfigDocument) -> Result<(), ConfigRuntimeError> {
    document.validate_layer()?;
    let bootstrap = ConfigLayers {
        built_in: document.bootstrap_layer(),
        user: ConfigLayer::default(),
        project: ConfigLayer::default(),
        cli: ConfigLayer::default(),
    };
    bootstrap
        .resolve()
        .map_err(|source| invalid("<effective>", source.to_string()))?;

    if let Some(profile) = &document.provider.profile
        && profile != "simulator"
        && !document.providers.contains_key(profile)
    {
        return Err(invalid(
            "provider.profile",
            "selected provider profile does not exist",
        ));
    }

    for (id, profile) in &document.providers {
        let prefix = format!("providers.{id}");
        if profile.template.is_none() {
            return Err(invalid(
                format!("{prefix}.template"),
                "provider profile requires a template",
            ));
        }
        if let Some(base_url) = &profile.base_url {
            validate_provider_origin(&prefix, base_url, profile)?;
            if profile.credential.is_none() {
                return Err(invalid(
                    format!("{prefix}.credential"),
                    "custom origin requires an explicit credential binding",
                ));
            }
            let has_template_mirror = profile
                .template
                .as_deref()
                .is_some_and(has_release_price_schedules);
            if profile.pricing.source.is_none() && !has_template_mirror {
                return Err(invalid(
                    format!("{prefix}.pricing.source"),
                    "custom origin without a release rate card requires an explicit pricing decision",
                ));
            }
        }
        if profile.base_url.is_none() && profile.allow_insecure_loopback == Some(true) {
            return Err(invalid(
                format!("{prefix}.allow_insecure_loopback"),
                "insecure-loopback opt-in requires an explicit loopback base URL",
            ));
        }
        if profile.dialects.as_ref().is_some_and(Vec::is_empty) {
            return Err(invalid(
                format!("{prefix}.dialects"),
                "dialect set cannot be empty",
            ));
        }
    }
    resolve_price_schedule_book(document)?;

    for (id, preset) in &document.model_presets {
        let prefix = format!("model_presets.{id}");
        let provider = preset.provider.as_ref().ok_or_else(|| {
            invalid(
                format!("{prefix}.provider"),
                "model preset requires a provider",
            )
        })?;
        if !document.providers.contains_key(provider) && provider != "simulator" {
            return Err(invalid(
                format!("{prefix}.provider"),
                "model preset references an unknown provider profile",
            ));
        }
        if preset.model.is_none() {
            return Err(invalid(
                format!("{prefix}.model"),
                "model preset requires a model",
            ));
        }
        if preset.dialect.is_none() {
            return Err(invalid(
                format!("{prefix}.dialect"),
                "model preset requires an explicit dialect",
            ));
        }
        for fallback in preset.fallback.iter().flatten() {
            if fallback == id {
                return Err(invalid(
                    format!("{prefix}.fallback"),
                    "model preset cannot fall back to itself",
                ));
            }
            if !document.model_presets.contains_key(fallback) {
                return Err(invalid(
                    format!("{prefix}.fallback"),
                    "fallback references an unknown model preset",
                ));
            }
        }
    }
    validate_fallback_cycles(document)?;

    let usage_windows = document.stats.windows.as_deref().unwrap_or_default();
    if usage_windows.len() > MAX_USAGE_WINDOWS {
        return Err(invalid(
            "stats.windows",
            "usage window count exceeds the supported limit",
        ));
    }
    let windows: BTreeSet<_> = usage_windows
        .iter()
        .map(|window| window.id.as_str())
        .collect();
    for window in usage_windows {
        validate_usage_window(window)?;
    }
    if let Some(primary) = &document.ui.statusline.primary_usage_window
        && !windows.contains(primary.as_str())
    {
        return Err(invalid(
            "ui.statusline.primary_usage_window",
            "primary usage window does not exist",
        ));
    }
    let has_custom_segment = document
        .ui
        .statusline
        .custom
        .left
        .as_ref()
        .is_some_and(|segments| !segments.is_empty())
        || document
            .ui
            .statusline
            .custom
            .right
            .as_ref()
            .is_some_and(|segments| !segments.is_empty());
    if document.ui.statusline.preset == Some(StatuslinePreset::Custom) && !has_custom_segment {
        return Err(invalid(
            "ui.statusline.custom",
            "custom statusline requires at least one segment list",
        ));
    }
    Ok(())
}

fn validate_provider_origin(
    profile_path: &str,
    value: &str,
    profile: &ProviderProfileLayer,
) -> Result<(), ConfigRuntimeError> {
    let path = format!("{profile_path}.base_url");
    let url = Url::parse(value).map_err(|_| invalid(&path, "invalid absolute HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            &path,
            "base URL must be absolute HTTP(S) with no user info, query, or fragment",
        ));
    }
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if url.scheme() == "http" && (!loopback || profile.allow_insecure_loopback != Some(true)) {
        return Err(invalid(
            &path,
            "plain HTTP requires a loopback host and explicit insecure-loopback opt-in",
        ));
    }
    if !loopback && profile.allow_insecure_loopback == Some(true) {
        return Err(invalid(
            format!("{profile_path}.allow_insecure_loopback"),
            "insecure-loopback opt-in is invalid for a remote origin",
        ));
    }
    Ok(())
}

fn validate_usage_window(window: &UsageWindowLayer) -> Result<(), ConfigRuntimeError> {
    let prefix = format!("stats.windows.{}", window.id);
    let start = window
        .start
        .as_deref()
        .ok_or_else(|| invalid(format!("{prefix}.start"), "usage window requires a start"))?;
    let end = window
        .end
        .as_deref()
        .ok_or_else(|| invalid(format!("{prefix}.end"), "usage window requires an end"))?;
    let days = window.days.as_ref().ok_or_else(|| {
        invalid(
            format!("{prefix}.days"),
            "usage window requires at least one day",
        )
    })?;
    if days.is_empty() {
        return Err(invalid(
            format!("{prefix}.days"),
            "usage window requires at least one day",
        ));
    }
    let timezone = window.timezone.as_deref().ok_or_else(|| {
        invalid(
            format!("{prefix}.timezone"),
            "usage window requires a time zone",
        )
    })?;
    UsageWindow::resolve(
        window.id.clone(),
        start,
        end,
        days.iter().copied().map(usage_weekday),
        timezone,
    )
    .map(|_| ())
    .map_err(|source| invalid(prefix, source.to_string()))
}

fn resolve_usage_window(window: &UsageWindowLayer) -> Result<UsageWindow, ConfigRuntimeError> {
    let prefix = format!("stats.windows.{}", window.id);
    UsageWindow::resolve(
        window.id.clone(),
        window
            .start
            .as_deref()
            .ok_or_else(|| invalid(format!("{prefix}.start"), "usage window requires a start"))?,
        window
            .end
            .as_deref()
            .ok_or_else(|| invalid(format!("{prefix}.end"), "usage window requires an end"))?,
        window
            .days
            .as_deref()
            .ok_or_else(|| {
                invalid(
                    format!("{prefix}.days"),
                    "usage window requires at least one day",
                )
            })?
            .iter()
            .copied()
            .map(usage_weekday),
        window.timezone.as_deref().ok_or_else(|| {
            invalid(
                format!("{prefix}.timezone"),
                "usage window requires a time zone",
            )
        })?,
    )
    .map_err(|source| invalid(prefix, source.to_string()))
}

const fn usage_weekday(day: Weekday) -> UsageWeekday {
    match day {
        Weekday::Mon => UsageWeekday::Mon,
        Weekday::Tue => UsageWeekday::Tue,
        Weekday::Wed => UsageWeekday::Wed,
        Weekday::Thu => UsageWeekday::Thu,
        Weekday::Fri => UsageWeekday::Fri,
        Weekday::Sat => UsageWeekday::Sat,
        Weekday::Sun => UsageWeekday::Sun,
    }
}

fn validate_fallback_cycles(document: &ConfigDocument) -> Result<(), ConfigRuntimeError> {
    fn visit<'a>(
        current: &'a str,
        root: &str,
        document: &'a ConfigDocument,
        active: &mut BTreeSet<&'a str>,
        complete: &mut BTreeSet<&'a str>,
    ) -> Result<(), ConfigRuntimeError> {
        if complete.contains(current) {
            return Ok(());
        }
        if !active.insert(current) {
            return Err(invalid(
                format!("model_presets.{root}.fallback"),
                "fallback chain contains a cycle",
            ));
        }
        if let Some(preset) = document.model_presets.get(current) {
            for fallback in preset.fallback.iter().flatten() {
                visit(fallback, root, document, active, complete)?;
            }
        }
        active.remove(current);
        complete.insert(current);
        Ok(())
    }

    let mut complete = BTreeSet::new();
    for root in document.model_presets.keys() {
        visit(root, root, document, &mut BTreeSet::new(), &mut complete)?;
    }
    Ok(())
}

fn invalid(path: impl Into<String>, reason: impl Into<String>) -> ConfigRuntimeError {
    ConfigRuntimeError::InvalidValue {
        path: path.into(),
        reason: reason.into(),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct StatuslineLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    preset: Option<StatuslinePreset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expand: Option<StatuslineExpansion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_usage_window: Option<String>,
    #[serde(skip_serializing_if = "StatuslineCustomLayer::is_empty")]
    custom: StatuslineCustomLayer,
}

impl StatuslineLayer {
    fn is_empty(&self) -> bool {
        self.preset.is_none()
            && self.expand.is_none()
            && self.primary_usage_window.is_none()
            && self.custom.is_empty()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct StatuslineCustomLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    left: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    right: Option<Vec<String>>,
}

impl StatuslineCustomLayer {
    fn is_empty(&self) -> bool {
        self.left.is_none() && self.right.is_none()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StatuslinePreset {
    Minimal,
    Balanced,
    Diagnostic,
    Custom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StatuslineExpansion {
    Auto,
    Compact,
    Expanded,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct StatsLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    windows: Option<Vec<UsageWindowLayer>>,
}

impl StatsLayer {
    fn is_empty(&self) -> bool {
        self.windows.is_none()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UsageWindowLayer {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    days: Option<Vec<Weekday>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,
}

impl UsageWindowLayer {
    fn is_empty_except_id(&self) -> bool {
        self.start.is_none() && self.end.is_none() && self.days.is_none() && self.timezone.is_none()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl ConfigDocument {
    pub fn parse(input: &str) -> Result<Self, ConfigRuntimeError> {
        if input.len() > MAX_CONFIG_FILE_BYTES {
            return Err(ConfigRuntimeError::InvalidValue {
                path: "<document>".to_owned(),
                reason: "config file exceeds the supported size".to_owned(),
            });
        }
        let mut document: Self =
            toml::from_str(input).map_err(|source| ConfigRuntimeError::Parse {
                path: None,
                detail: source.to_string(),
            })?;
        document.normalize_routes()?;
        document.validate_layer()?;
        Ok(document)
    }

    pub fn to_toml(&self) -> Result<String, ConfigRuntimeError> {
        let mut normalized = self.clone();
        normalized.normalize_routes()?;
        normalized.validate_layer()?;
        let mut output =
            toml::to_string_pretty(&normalized).map_err(|source| ConfigRuntimeError::Parse {
                path: None,
                detail: source.to_string(),
            })?;
        if !output.ends_with('\n') {
            output.push('\n');
        }
        Ok(output)
    }

    pub fn get(&self, path: &str) -> Result<Option<ConfigValue>, ConfigRuntimeError> {
        require_schema_entry(path)?;
        Ok(self.flatten().remove(path))
    }

    fn contains_object(&self, object: &ConfigObjectRef) -> Result<bool, ConfigRuntimeError> {
        validate_id("<id>", object.id())?;
        Ok(match object.kind() {
            ConfigObjectKind::ProviderProfile => self.providers.contains_key(object.id()),
            ConfigObjectKind::ModelPreset => self.model_presets.contains_key(object.id()),
            ConfigObjectKind::PriceSchedule => self.price_schedules.contains_key(object.id()),
            ConfigObjectKind::UsageWindow => self
                .stats
                .windows
                .as_ref()
                .is_some_and(|windows| windows.iter().any(|window| window.id == object.id())),
        })
    }

    fn delete_object(&mut self, object: &ConfigObjectRef) -> Result<(), ConfigRuntimeError> {
        validate_id("<id>", object.id())?;
        let removed = match object.kind() {
            ConfigObjectKind::ProviderProfile => self.providers.remove(object.id()).is_some(),
            ConfigObjectKind::ModelPreset => self.model_presets.remove(object.id()).is_some(),
            ConfigObjectKind::PriceSchedule => self.price_schedules.remove(object.id()).is_some(),
            ConfigObjectKind::UsageWindow => {
                let Some(windows) = self.stats.windows.as_mut() else {
                    return Err(ConfigRuntimeError::UnknownObject(
                        object.kind().object_path(object.id()),
                    ));
                };
                let Some(index) = windows.iter().position(|window| window.id == object.id()) else {
                    return Err(ConfigRuntimeError::UnknownObject(
                        object.kind().object_path(object.id()),
                    ));
                };
                windows.remove(index);
                if windows.is_empty() {
                    self.stats.windows = None;
                }
                true
            }
        };
        if !removed {
            return Err(ConfigRuntimeError::UnknownObject(
                object.kind().object_path(object.id()),
            ));
        }
        self.validate_layer()
    }

    fn set(
        &mut self,
        scope: ConfigScope,
        path: &str,
        value: ConfigValue,
    ) -> Result<(), ConfigRuntimeError> {
        let descriptor = require_schema_entry(path)?;
        require_scope(descriptor, scope)?;
        if descriptor.value_kind != value.kind() {
            return Err(ConfigRuntimeError::WrongType {
                path: path.to_owned(),
                expected: descriptor.value_kind,
                actual: value.kind(),
            });
        }
        let segments = split_path(path)?;
        match segments.as_slice() {
            ["provider", "profile"] => self.provider.profile = Some(take_string(value)),
            ["provider", "model"] => self.provider.model = Some(take_string(value)),
            ["runtime", "max_output_bytes"] => {
                self.runtime.max_output_bytes = Some(take_positive_integer(value))
            }
            ["providers", id, field] => {
                validate_id("providers.<id>", id)?;
                let profile = self.providers.entry((*id).to_owned()).or_default();
                match *field {
                    "template" => profile.template = Some(take_string(value)),
                    "credential" => profile.credential = Some(take_string(value)),
                    "base_url" => profile.base_url = Some(take_string(value)),
                    "dialects" => {
                        profile.dialects = Some(
                            take_string_list(value)
                                .into_iter()
                                .map(|value| parse_dialect(path, &value))
                                .collect::<Result<_, _>>()?,
                        );
                    }
                    "allow_insecure_loopback" => {
                        profile.allow_insecure_loopback = Some(take_boolean(value));
                    }
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["providers", id, "routes", route] => {
                validate_id("providers.<id>", id)?;
                let route_value = normalize_route(path, &take_string(value))?;
                let routes = &mut self.providers.entry((*id).to_owned()).or_default().routes;
                match *route {
                    "responses" => routes.responses = Some(route_value),
                    "chat_completions" => routes.chat_completions = Some(route_value),
                    "messages" => routes.messages = Some(route_value),
                    "models" => routes.models = Some(route_value),
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["providers", id, "catalog", "mode"] => {
                validate_id("providers.<id>", id)?;
                self.providers
                    .entry((*id).to_owned())
                    .or_default()
                    .catalog
                    .mode = Some(parse_catalog_mode(path, &take_string(value))?);
            }
            ["providers", id, "pricing", "source"] => {
                validate_id("providers.<id>", id)?;
                self.providers
                    .entry((*id).to_owned())
                    .or_default()
                    .pricing
                    .source = Some(parse_pricing_source(path, &take_string(value))?);
            }
            ["model_presets", id, field] => {
                validate_id("model_presets.<id>", id)?;
                let preset = self.model_presets.entry((*id).to_owned()).or_default();
                match *field {
                    "provider" => preset.provider = Some(take_string(value)),
                    "model" => preset.model = Some(take_string(value)),
                    "dialect" => preset.dialect = Some(parse_dialect(path, &take_string(value))?),
                    "reasoning_effort" => {
                        preset.reasoning_effort = Some(take_string(value));
                    }
                    "service_tier" => preset.service_tier = Some(take_string(value)),
                    "max_output_tokens" => {
                        preset.max_output_tokens = Some(take_positive_integer(value));
                    }
                    "context_mode" => preset.context_mode = Some(take_string(value)),
                    "favorite" => preset.favorite = Some(take_boolean(value)),
                    "fallback" => preset.fallback = Some(take_string_list(value)),
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["price_schedules", id, field] => {
                validate_id("price_schedules.<id>", id)?;
                let schedule = self.price_schedules.entry((*id).to_owned()).or_default();
                match *field {
                    "version" => schedule.version = Some(take_string(value)),
                    "currency" => schedule.currency = Some(take_string(value)),
                    "provider" => schedule.provider = Some(take_string(value)),
                    "model" => schedule.model = Some(take_string(value)),
                    "dialect" => {
                        schedule.dialect = Some(parse_dialect(path, &take_string(value))?);
                    }
                    "service_tier" => schedule.service_tier = Some(take_string(value)),
                    "minimum_context_tokens" => {
                        schedule.minimum_context_tokens = Some(take_non_negative_integer(value));
                    }
                    "maximum_context_tokens" => {
                        schedule.maximum_context_tokens = Some(take_non_negative_integer(value));
                    }
                    "effective_from" => schedule.effective_from = Some(take_string(value)),
                    "effective_until" => schedule.effective_until = Some(take_string(value)),
                    "source" => {
                        schedule.source =
                            Some(parse_price_schedule_source(path, &take_string(value))?);
                    }
                    "source_ref" => schedule.source_ref = Some(take_string(value)),
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["price_schedules", id, "rates", rate] => {
                validate_id("price_schedules.<id>", id)?;
                let rates = &mut self
                    .price_schedules
                    .entry((*id).to_owned())
                    .or_default()
                    .rates;
                let value = Some(take_non_negative_integer(value));
                match *rate {
                    "input_micros_per_million" => rates.input_micros_per_million = value,
                    "cached_input_micros_per_million" => {
                        rates.cached_input_micros_per_million = value;
                    }
                    "cache_write_micros_per_million" => {
                        rates.cache_write_micros_per_million = value;
                    }
                    "output_micros_per_million" => rates.output_micros_per_million = value,
                    "reasoning_output_micros_per_million" => {
                        rates.reasoning_output_micros_per_million = value;
                    }
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["ui", "statusline", field] => match *field {
                "preset" => {
                    self.ui.statusline.preset =
                        Some(parse_statusline_preset(path, &take_string(value))?);
                }
                "expand" => {
                    self.ui.statusline.expand =
                        Some(parse_statusline_expansion(path, &take_string(value))?);
                }
                "primary_usage_window" => {
                    self.ui.statusline.primary_usage_window = Some(take_string(value));
                }
                _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
            },
            ["ui", "statusline", "custom", side] => match *side {
                "left" => self.ui.statusline.custom.left = Some(take_string_list(value)),
                "right" => self.ui.statusline.custom.right = Some(take_string_list(value)),
                _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
            },
            ["stats", "windows", id, field] => {
                validate_id("stats.windows.<id>", id)?;
                let window = self.window_mut(id);
                match *field {
                    "start" => window.start = Some(take_string(value)),
                    "end" => window.end = Some(take_string(value)),
                    "days" => {
                        window.days = Some(
                            take_string_list(value)
                                .into_iter()
                                .map(|value| parse_weekday(path, &value))
                                .collect::<Result<_, _>>()?,
                        );
                    }
                    "timezone" => window.timezone = Some(take_string(value)),
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
        }
        self.normalize_routes()?;
        self.validate_layer()
    }

    fn reset(&mut self, scope: ConfigScope, path: &str) -> Result<(), ConfigRuntimeError> {
        let descriptor = require_schema_entry(path)?;
        require_scope(descriptor, scope)?;
        let segments = split_path(path)?;
        match segments.as_slice() {
            ["provider", "profile"] => self.provider.profile = None,
            ["provider", "model"] => self.provider.model = None,
            ["runtime", "max_output_bytes"] => self.runtime.max_output_bytes = None,
            ["providers", id, field] => {
                let Some(profile) = self.providers.get_mut(*id) else {
                    return Ok(());
                };
                match *field {
                    "template" => profile.template = None,
                    "credential" => profile.credential = None,
                    "base_url" => profile.base_url = None,
                    "dialects" => profile.dialects = None,
                    "allow_insecure_loopback" => profile.allow_insecure_loopback = None,
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["providers", id, "routes", route] => {
                let Some(profile) = self.providers.get_mut(*id) else {
                    return Ok(());
                };
                match *route {
                    "responses" => profile.routes.responses = None,
                    "chat_completions" => profile.routes.chat_completions = None,
                    "messages" => profile.routes.messages = None,
                    "models" => profile.routes.models = None,
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["providers", id, "catalog", "mode"] => {
                if let Some(profile) = self.providers.get_mut(*id) {
                    profile.catalog.mode = None;
                }
            }
            ["providers", id, "pricing", "source"] => {
                if let Some(profile) = self.providers.get_mut(*id) {
                    profile.pricing.source = None;
                }
            }
            ["model_presets", id, field] => {
                let Some(preset) = self.model_presets.get_mut(*id) else {
                    return Ok(());
                };
                match *field {
                    "provider" => preset.provider = None,
                    "model" => preset.model = None,
                    "dialect" => preset.dialect = None,
                    "reasoning_effort" => preset.reasoning_effort = None,
                    "service_tier" => preset.service_tier = None,
                    "max_output_tokens" => preset.max_output_tokens = None,
                    "context_mode" => preset.context_mode = None,
                    "favorite" => preset.favorite = None,
                    "fallback" => preset.fallback = None,
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["price_schedules", id, field] => {
                let Some(schedule) = self.price_schedules.get_mut(*id) else {
                    return Ok(());
                };
                match *field {
                    "version" => schedule.version = None,
                    "currency" => schedule.currency = None,
                    "provider" => schedule.provider = None,
                    "model" => schedule.model = None,
                    "dialect" => schedule.dialect = None,
                    "service_tier" => schedule.service_tier = None,
                    "minimum_context_tokens" => schedule.minimum_context_tokens = None,
                    "maximum_context_tokens" => schedule.maximum_context_tokens = None,
                    "effective_from" => schedule.effective_from = None,
                    "effective_until" => schedule.effective_until = None,
                    "source" => schedule.source = None,
                    "source_ref" => schedule.source_ref = None,
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["price_schedules", id, "rates", rate] => {
                let Some(schedule) = self.price_schedules.get_mut(*id) else {
                    return Ok(());
                };
                match *rate {
                    "input_micros_per_million" => {
                        schedule.rates.input_micros_per_million = None;
                    }
                    "cached_input_micros_per_million" => {
                        schedule.rates.cached_input_micros_per_million = None;
                    }
                    "cache_write_micros_per_million" => {
                        schedule.rates.cache_write_micros_per_million = None;
                    }
                    "output_micros_per_million" => {
                        schedule.rates.output_micros_per_million = None;
                    }
                    "reasoning_output_micros_per_million" => {
                        schedule.rates.reasoning_output_micros_per_million = None;
                    }
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            ["ui", "statusline", field] => match *field {
                "preset" => self.ui.statusline.preset = None,
                "expand" => self.ui.statusline.expand = None,
                "primary_usage_window" => self.ui.statusline.primary_usage_window = None,
                _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
            },
            ["ui", "statusline", "custom", side] => match *side {
                "left" => self.ui.statusline.custom.left = None,
                "right" => self.ui.statusline.custom.right = None,
                _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
            },
            ["stats", "windows", id, field] => {
                let Some(window) = self
                    .stats
                    .windows
                    .as_mut()
                    .and_then(|windows| windows.iter_mut().find(|window| window.id == *id))
                else {
                    return Ok(());
                };
                match *field {
                    "start" => window.start = None,
                    "end" => window.end = None,
                    "days" => window.days = None,
                    "timezone" => window.timezone = None,
                    _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
                }
            }
            _ => return Err(ConfigRuntimeError::UnknownObject(path.to_owned())),
        }
        self.prune_empty();
        self.validate_layer()
    }

    fn window_mut(&mut self, id: &str) -> &mut UsageWindowLayer {
        let windows = self.stats.windows.get_or_insert_with(Vec::new);
        let index = windows
            .iter()
            .position(|window| window.id == id)
            .unwrap_or_else(|| {
                windows.push(UsageWindowLayer {
                    id: id.to_owned(),
                    start: None,
                    end: None,
                    days: None,
                    timezone: None,
                });
                windows.len() - 1
            });
        &mut windows[index]
    }

    fn prune_empty(&mut self) {
        self.providers.retain(|_, profile| !profile.is_empty());
        self.model_presets.retain(|_, preset| !preset.is_empty());
        self.price_schedules
            .retain(|_, schedule| !schedule.is_empty());
        if let Some(windows) = &mut self.stats.windows {
            windows.retain(|window| !window.is_empty_except_id());
            if windows.is_empty() {
                self.stats.windows = None;
            }
        }
    }

    fn normalize_routes(&mut self) -> Result<(), ConfigRuntimeError> {
        for (id, profile) in &mut self.providers {
            for (name, route) in [
                ("responses", &mut profile.routes.responses),
                ("chat_completions", &mut profile.routes.chat_completions),
                ("messages", &mut profile.routes.messages),
                ("models", &mut profile.routes.models),
            ] {
                if let Some(value) = route {
                    *value = normalize_route(&format!("providers.{id}.routes.{name}"), value)?;
                }
            }
        }
        Ok(())
    }

    fn validate_layer(&self) -> Result<(), ConfigRuntimeError> {
        SchemaKind::ConfigFile
            .require_current(self.schema_version)
            .map_err(|source| ConfigRuntimeError::UnsupportedSchema {
                supported: CONFIG_FILE_SCHEMA_VERSION,
                actual: match source {
                    crate::schema::SchemaError::Unsupported { actual, .. } => actual.get(),
                    crate::schema::SchemaError::ZeroVersion => 0,
                },
            })?;
        for (path, value) in self.flatten() {
            let descriptor = require_schema_entry(&path)?;
            if descriptor.value_kind != value.kind() {
                return Err(ConfigRuntimeError::WrongType {
                    path,
                    expected: descriptor.value_kind,
                    actual: value.kind(),
                });
            }
            validate_value(&path, &value)?;
            if descriptor.credential_reference {
                let ConfigValue::String(reference) = &value else {
                    unreachable!("credential-reference schema requires a string")
                };
                validate_id(&path, reference)?;
            }
        }
        for id in self.providers.keys() {
            validate_id("providers.<id>", id)?;
        }
        for id in self.model_presets.keys() {
            validate_id("model_presets.<id>", id)?;
        }
        for id in self.price_schedules.keys() {
            validate_id("price_schedules.<id>", id)?;
        }
        let mut window_ids = BTreeSet::new();
        for window in self.stats.windows.iter().flatten() {
            validate_id("stats.windows.<id>", &window.id)?;
            if !window_ids.insert(window.id.clone()) {
                return Err(invalid(
                    format!("stats.windows.{}", window.id),
                    "usage window IDs must be unique",
                ));
            }
        }
        Ok(())
    }

    fn merge_from(&mut self, overlay: &Self) {
        merge_option(&mut self.provider.profile, &overlay.provider.profile);
        merge_option(&mut self.provider.model, &overlay.provider.model);
        merge_option(
            &mut self.runtime.max_output_bytes,
            &overlay.runtime.max_output_bytes,
        );
        for (id, overlay_profile) in &overlay.providers {
            self.providers
                .entry(id.clone())
                .or_default()
                .merge_from(overlay_profile);
        }
        for (id, overlay_preset) in &overlay.model_presets {
            self.model_presets
                .entry(id.clone())
                .or_default()
                .merge_from(overlay_preset);
        }
        for (id, overlay_schedule) in &overlay.price_schedules {
            self.price_schedules
                .entry(id.clone())
                .or_default()
                .merge_from(overlay_schedule);
        }
        self.ui.merge_from(&overlay.ui);
        self.stats.merge_from(&overlay.stats);
    }

    fn bootstrap_layer(&self) -> ConfigLayer {
        ConfigLayer {
            provider_profile: self.provider.profile.clone(),
            provider_model: self.provider.model.clone(),
            max_output_bytes: self.runtime.max_output_bytes,
            max_output_tokens: None,
            reasoning_effort: None,
            service_tier: None,
        }
    }

    fn flatten(&self) -> BTreeMap<String, ConfigValue> {
        let mut values = BTreeMap::new();
        insert_string(&mut values, "provider.profile", &self.provider.profile);
        insert_string(&mut values, "provider.model", &self.provider.model);
        if let Some(value) = self.runtime.max_output_bytes {
            values.insert(
                "runtime.max_output_bytes".to_owned(),
                ConfigValue::PositiveInteger(value),
            );
        }
        for (id, profile) in &self.providers {
            let prefix = format!("providers.{id}");
            insert_string(
                &mut values,
                &format!("{prefix}.template"),
                &profile.template,
            );
            insert_string(
                &mut values,
                &format!("{prefix}.credential"),
                &profile.credential,
            );
            insert_string(
                &mut values,
                &format!("{prefix}.base_url"),
                &profile.base_url,
            );
            for (name, route) in [
                ("responses", &profile.routes.responses),
                ("chat_completions", &profile.routes.chat_completions),
                ("messages", &profile.routes.messages),
                ("models", &profile.routes.models),
            ] {
                insert_string(&mut values, &format!("{prefix}.routes.{name}"), route);
            }
            if let Some(dialects) = &profile.dialects {
                values.insert(
                    format!("{prefix}.dialects"),
                    ConfigValue::StringList(
                        dialects
                            .iter()
                            .map(|dialect| dialect.as_str().to_owned())
                            .collect(),
                    ),
                );
            }
            if let Some(mode) = profile.catalog.mode {
                values.insert(
                    format!("{prefix}.catalog.mode"),
                    ConfigValue::String(mode.as_str().to_owned()),
                );
            }
            if let Some(source) = profile.pricing.source {
                values.insert(
                    format!("{prefix}.pricing.source"),
                    ConfigValue::String(source.as_str().to_owned()),
                );
            }
            if let Some(value) = profile.allow_insecure_loopback {
                values.insert(
                    format!("{prefix}.allow_insecure_loopback"),
                    ConfigValue::Boolean(value),
                );
            }
        }
        for (id, preset) in &self.model_presets {
            let prefix = format!("model_presets.{id}");
            insert_string(&mut values, &format!("{prefix}.provider"), &preset.provider);
            insert_string(&mut values, &format!("{prefix}.model"), &preset.model);
            if let Some(dialect) = preset.dialect {
                values.insert(
                    format!("{prefix}.dialect"),
                    ConfigValue::String(dialect.as_str().to_owned()),
                );
            }
            insert_string(
                &mut values,
                &format!("{prefix}.reasoning_effort"),
                &preset.reasoning_effort,
            );
            insert_string(
                &mut values,
                &format!("{prefix}.service_tier"),
                &preset.service_tier,
            );
            if let Some(value) = preset.max_output_tokens {
                values.insert(
                    format!("{prefix}.max_output_tokens"),
                    ConfigValue::PositiveInteger(value),
                );
            }
            insert_string(
                &mut values,
                &format!("{prefix}.context_mode"),
                &preset.context_mode,
            );
            if let Some(value) = preset.favorite {
                values.insert(format!("{prefix}.favorite"), ConfigValue::Boolean(value));
            }
            if let Some(value) = &preset.fallback {
                values.insert(
                    format!("{prefix}.fallback"),
                    ConfigValue::StringList(value.clone()),
                );
            }
        }
        for (id, schedule) in &self.price_schedules {
            let prefix = format!("price_schedules.{id}");
            insert_string(&mut values, &format!("{prefix}.version"), &schedule.version);
            insert_string(
                &mut values,
                &format!("{prefix}.currency"),
                &schedule.currency,
            );
            insert_string(
                &mut values,
                &format!("{prefix}.provider"),
                &schedule.provider,
            );
            insert_string(&mut values, &format!("{prefix}.model"), &schedule.model);
            if let Some(dialect) = schedule.dialect {
                values.insert(
                    format!("{prefix}.dialect"),
                    ConfigValue::String(dialect.as_str().to_owned()),
                );
            }
            insert_string(
                &mut values,
                &format!("{prefix}.service_tier"),
                &schedule.service_tier,
            );
            for (field, value) in [
                ("minimum_context_tokens", schedule.minimum_context_tokens),
                ("maximum_context_tokens", schedule.maximum_context_tokens),
            ] {
                if let Some(value) = value {
                    values.insert(
                        format!("{prefix}.{field}"),
                        ConfigValue::NonNegativeInteger(value),
                    );
                }
            }
            insert_string(
                &mut values,
                &format!("{prefix}.effective_from"),
                &schedule.effective_from,
            );
            insert_string(
                &mut values,
                &format!("{prefix}.effective_until"),
                &schedule.effective_until,
            );
            if let Some(source) = schedule.source {
                values.insert(
                    format!("{prefix}.source"),
                    ConfigValue::String(source.as_str().to_owned()),
                );
            }
            insert_string(
                &mut values,
                &format!("{prefix}.source_ref"),
                &schedule.source_ref,
            );
            for (field, value) in [
                (
                    "input_micros_per_million",
                    schedule.rates.input_micros_per_million,
                ),
                (
                    "cached_input_micros_per_million",
                    schedule.rates.cached_input_micros_per_million,
                ),
                (
                    "cache_write_micros_per_million",
                    schedule.rates.cache_write_micros_per_million,
                ),
                (
                    "output_micros_per_million",
                    schedule.rates.output_micros_per_million,
                ),
                (
                    "reasoning_output_micros_per_million",
                    schedule.rates.reasoning_output_micros_per_million,
                ),
            ] {
                if let Some(value) = value {
                    values.insert(
                        format!("{prefix}.rates.{field}"),
                        ConfigValue::NonNegativeInteger(value),
                    );
                }
            }
        }
        if let Some(preset) = self.ui.statusline.preset {
            values.insert(
                "ui.statusline.preset".to_owned(),
                ConfigValue::String(preset.as_str().to_owned()),
            );
        }
        if let Some(expand) = self.ui.statusline.expand {
            values.insert(
                "ui.statusline.expand".to_owned(),
                ConfigValue::String(expand.as_str().to_owned()),
            );
        }
        insert_string(
            &mut values,
            "ui.statusline.primary_usage_window",
            &self.ui.statusline.primary_usage_window,
        );
        if let Some(value) = &self.ui.statusline.custom.left {
            values.insert(
                "ui.statusline.custom.left".to_owned(),
                ConfigValue::StringList(value.clone()),
            );
        }
        if let Some(value) = &self.ui.statusline.custom.right {
            values.insert(
                "ui.statusline.custom.right".to_owned(),
                ConfigValue::StringList(value.clone()),
            );
        }
        for window in self.stats.windows.iter().flatten() {
            let prefix = format!("stats.windows.{}", window.id);
            insert_string(&mut values, &format!("{prefix}.start"), &window.start);
            insert_string(&mut values, &format!("{prefix}.end"), &window.end);
            if let Some(days) = &window.days {
                values.insert(
                    format!("{prefix}.days"),
                    ConfigValue::StringList(
                        days.iter().map(|day| day.as_str().to_owned()).collect(),
                    ),
                );
            }
            insert_string(&mut values, &format!("{prefix}.timezone"), &window.timezone);
        }
        values
    }
}

impl ProviderProfileLayer {
    fn merge_from(&mut self, overlay: &Self) {
        merge_option(&mut self.template, &overlay.template);
        merge_option(&mut self.credential, &overlay.credential);
        merge_option(&mut self.base_url, &overlay.base_url);
        self.routes.merge_from(&overlay.routes);
        merge_option(&mut self.dialects, &overlay.dialects);
        merge_option(&mut self.catalog.mode, &overlay.catalog.mode);
        merge_option(&mut self.pricing.source, &overlay.pricing.source);
        merge_option(
            &mut self.allow_insecure_loopback,
            &overlay.allow_insecure_loopback,
        );
    }
}

impl ProviderRoutesLayer {
    fn merge_from(&mut self, overlay: &Self) {
        merge_option(&mut self.responses, &overlay.responses);
        merge_option(&mut self.chat_completions, &overlay.chat_completions);
        merge_option(&mut self.messages, &overlay.messages);
        merge_option(&mut self.models, &overlay.models);
    }
}

impl ModelPresetLayer {
    fn merge_from(&mut self, overlay: &Self) {
        merge_option(&mut self.provider, &overlay.provider);
        merge_option(&mut self.model, &overlay.model);
        merge_option(&mut self.dialect, &overlay.dialect);
        merge_option(&mut self.reasoning_effort, &overlay.reasoning_effort);
        merge_option(&mut self.service_tier, &overlay.service_tier);
        merge_option(&mut self.max_output_tokens, &overlay.max_output_tokens);
        merge_option(&mut self.context_mode, &overlay.context_mode);
        merge_option(&mut self.favorite, &overlay.favorite);
        merge_option(&mut self.fallback, &overlay.fallback);
    }
}

impl PriceScheduleLayer {
    fn merge_from(&mut self, overlay: &Self) {
        merge_option(&mut self.version, &overlay.version);
        merge_option(&mut self.currency, &overlay.currency);
        merge_option(&mut self.provider, &overlay.provider);
        merge_option(&mut self.model, &overlay.model);
        merge_option(&mut self.dialect, &overlay.dialect);
        merge_option(&mut self.service_tier, &overlay.service_tier);
        merge_option(
            &mut self.minimum_context_tokens,
            &overlay.minimum_context_tokens,
        );
        merge_option(
            &mut self.maximum_context_tokens,
            &overlay.maximum_context_tokens,
        );
        merge_option(&mut self.effective_from, &overlay.effective_from);
        merge_option(&mut self.effective_until, &overlay.effective_until);
        merge_option(&mut self.source, &overlay.source);
        merge_option(&mut self.source_ref, &overlay.source_ref);
        self.rates.merge_from(&overlay.rates);
    }
}

impl PriceRatesLayer {
    fn merge_from(&mut self, overlay: &Self) {
        merge_option(
            &mut self.input_micros_per_million,
            &overlay.input_micros_per_million,
        );
        merge_option(
            &mut self.cached_input_micros_per_million,
            &overlay.cached_input_micros_per_million,
        );
        merge_option(
            &mut self.cache_write_micros_per_million,
            &overlay.cache_write_micros_per_million,
        );
        merge_option(
            &mut self.output_micros_per_million,
            &overlay.output_micros_per_million,
        );
        merge_option(
            &mut self.reasoning_output_micros_per_million,
            &overlay.reasoning_output_micros_per_million,
        );
    }
}

impl UiLayer {
    fn merge_from(&mut self, overlay: &Self) {
        merge_option(&mut self.statusline.preset, &overlay.statusline.preset);
        merge_option(&mut self.statusline.expand, &overlay.statusline.expand);
        merge_option(
            &mut self.statusline.primary_usage_window,
            &overlay.statusline.primary_usage_window,
        );
        merge_option(
            &mut self.statusline.custom.left,
            &overlay.statusline.custom.left,
        );
        merge_option(
            &mut self.statusline.custom.right,
            &overlay.statusline.custom.right,
        );
    }
}

impl StatsLayer {
    fn merge_from(&mut self, overlay: &Self) {
        let Some(overlay_windows) = &overlay.windows else {
            return;
        };
        let windows = self.windows.get_or_insert_with(Vec::new);
        for overlay_window in overlay_windows {
            if let Some(window) = windows
                .iter_mut()
                .find(|window| window.id == overlay_window.id)
            {
                window.merge_from(overlay_window);
            } else {
                windows.push(overlay_window.clone());
            }
        }
    }
}

impl UsageWindowLayer {
    fn merge_from(&mut self, overlay: &Self) {
        merge_option(&mut self.start, &overlay.start);
        merge_option(&mut self.end, &overlay.end);
        merge_option(&mut self.days, &overlay.days);
        merge_option(&mut self.timezone, &overlay.timezone);
    }
}
