//! Layered configuration resolved into immutable per-Turn epochs.

use std::error::Error;
use std::fmt;

use crate::model::ConfigEpochId;
use crate::schema::SchemaKind;

mod runtime;
pub use runtime::*;

pub const DEFAULT_MAX_OUTPUT_BYTES: u32 = 64 * 1024;
pub const MAX_OUTPUT_BYTES: u32 = 512 * 1024;
pub const MAX_CONFIG_STRING_BYTES: usize = 512;
pub const CONFIG_SCHEMA_VERSION: u16 = SchemaKind::ConfigEpoch.current().get();

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
}

impl ConfigLayer {
    #[must_use]
    pub fn built_in() -> Self {
        Self {
            provider_profile: Some("simulator".to_owned()),
            provider_model: Some("deterministic-v1".to_owned()),
            max_output_bytes: Some(DEFAULT_MAX_OUTPUT_BYTES),
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigEpoch {
    id: ConfigEpochId,
    fingerprint: u64,
    resolved: ResolvedConfig,
}

impl ConfigEpoch {
    pub fn freeze(id: ConfigEpochId, layers: &ConfigLayers) -> Result<Self, ConfigError> {
        let resolved = layers.resolve()?;
        let fingerprint = fingerprint(&resolved);
        Ok(Self {
            id,
            fingerprint,
            resolved,
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

        Ok(ResolvedConfig {
            provider_profile,
            provider_model,
            max_output_bytes,
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

fn fingerprint(config: &ResolvedConfig) -> u64 {
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
    hash
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
    MaxOutputBytesTooLarge,
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
            Self::MaxOutputBytesTooLarge => {
                write!(
                    formatter,
                    "runtime.max_output_bytes exceeds the supported limit"
                )
            }
        }
    }
}

impl Error for ConfigError {}
