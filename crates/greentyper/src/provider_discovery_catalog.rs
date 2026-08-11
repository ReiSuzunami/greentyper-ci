//! Product projection that merges trusted release facts with bounded discovery observations.

use std::collections::BTreeMap;
use std::path::Path;

use greentyper_core::config::{
    ConfigObjectKind, ConfigRuntime, ConfigRuntimeError, ModelPresetView,
};
use greentyper_core::provider::{ProviderDialect, ProviderProfileSnapshot};
use greentyper_core::provider_catalog::{ProviderCatalog, ProviderCatalogMode};
use greentyper_core::provider_discovery::{
    DiscoveredProviderModel, PROVIDER_DISCOVERY_SCHEMA_VERSION, ProviderDiscoveryError,
    ProviderDiscoveryProfile, ProviderDiscoveryState,
};
use serde::Serialize;

use crate::provider_connection::{ProviderConnectionTestStatus, ProviderConnectionTester};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderDiscoveryFreshness {
    Disabled,
    Missing,
    Current,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderDiscoverySource {
    ReleaseSeed,
    Discovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderDiscoveryAvailability {
    Available,
    Stale,
    Unverified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderDiscoverySuggestion {
    None,
    AcceptReleaseStarter,
    AcceptDiscoveredWithDialect,
    RefreshRequired,
    Incompatible,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ProviderDiscoveryCatalogView {
    pub(crate) schema_version: u16,
    pub(crate) profile: String,
    pub(crate) template: String,
    #[serde(skip_serializing)]
    pub(crate) profile_fingerprint: u64,
    #[serde(skip_serializing)]
    pub(crate) dialects: Vec<ProviderDialect>,
    pub(crate) mode: ProviderCatalogMode,
    pub(crate) freshness: ProviderDiscoveryFreshness,
    pub(crate) observed_at_unix_ms: Option<i64>,
    pub(crate) models: Vec<ProviderDiscoveryCatalogModel>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ProviderDiscoveryCatalogModel {
    pub(crate) id: String,
    pub(crate) release_catalog_key: Option<String>,
    pub(crate) sources: Vec<ProviderDiscoverySource>,
    pub(crate) availability: ProviderDiscoveryAvailability,
    pub(crate) primary_dialect: Option<ProviderDialect>,
    pub(crate) profile_compatible: bool,
    pub(crate) configured_presets: Vec<String>,
    pub(crate) executable: bool,
    pub(crate) suggestion: ProviderDiscoverySuggestion,
}

#[derive(Default)]
struct ProviderDiscoveryCatalogModelBuilder {
    release_catalog_key: Option<String>,
    primary_dialect: Option<ProviderDialect>,
    profile_compatible: bool,
    release: bool,
    discovery: bool,
}

pub(crate) fn refresh_provider_discovery<T: ProviderConnectionTester + ?Sized>(
    profile: &ProviderProfileSnapshot,
    state_path: &Path,
    observed_at_unix_ms: i64,
    tester: &mut T,
) -> Result<ProviderConnectionTestStatus, ProviderDiscoveryError> {
    let status = tester.test(profile);
    if let ProviderConnectionTestStatus::Succeeded {
        profile: observed_profile,
        fingerprint,
        models,
    } = &status
    {
        if observed_profile != profile.profile() || *fingerprint != profile.fingerprint() {
            return Err(ProviderDiscoveryError::ObservationMismatch);
        }
        let models = models
            .iter()
            .map(|model| {
                DiscoveredProviderModel::new(model.id.clone(), model.release_catalog_key.clone())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let snapshot = ProviderDiscoveryProfile::new(
            profile.profile(),
            profile.template(),
            profile.fingerprint(),
            observed_at_unix_ms,
            models,
        )?;
        ProviderDiscoveryState::replace_profile(state_path, snapshot)?;
    }
    Ok(status)
}

pub(crate) fn provider_discovery_catalogs(
    runtime: &ConfigRuntime,
    state: &ProviderDiscoveryState,
) -> Result<Vec<ProviderDiscoveryCatalogView>, ConfigRuntimeError> {
    let presets = runtime.model_presets()?;
    runtime
        .addressable_objects()?
        .into_iter()
        .filter(|object| object.kind() == ConfigObjectKind::ProviderProfile)
        .filter_map(|object| {
            let template_path = format!("providers.{}.template", object.id());
            match provider_discovery_catalog(runtime, state, &presets, object.id()) {
                Ok(catalog) => Some(Ok(catalog)),
                Err(ConfigRuntimeError::InvalidValue { path, .. }) if path == template_path => None,
                Err(source) => Some(Err(source)),
            }
        })
        .collect()
}

pub(crate) fn provider_discovery_catalog(
    runtime: &ConfigRuntime,
    state: &ProviderDiscoveryState,
    presets: &[ModelPresetView],
    profile_id: &str,
) -> Result<ProviderDiscoveryCatalogView, ConfigRuntimeError> {
    let profile = runtime
        .provider_profile(profile_id)?
        .ok_or_else(|| ConfigRuntimeError::UnknownObject(format!("providers.{profile_id}")))?;
    let mode = runtime.provider_catalog_mode(profile_id)?;
    let observation = state
        .profiles()
        .iter()
        .find(|candidate| candidate.profile() == profile.profile());
    let freshness = discovery_freshness(mode, &profile, observation);

    let mut builders = BTreeMap::<String, ProviderDiscoveryCatalogModelBuilder>::new();
    if mode.includes_release_seed() {
        for record in ProviderCatalog::release()
            .models()
            .iter()
            .filter(|record| record.provider_template() == profile.template())
        {
            let builder = builders
                .entry(record.model_id().value().to_owned())
                .or_default();
            builder.release_catalog_key = Some(record.key().to_owned());
            builder.primary_dialect = Some(record.primary_dialect().value());
            builder.profile_compatible = profile.supports(record.primary_dialect().value());
            builder.release = true;
        }
    }
    if mode.includes_discovery()
        && let Some(observation) = observation
    {
        for model in observation.models() {
            builders.entry(model.id().to_owned()).or_default().discovery = true;
        }
    }

    let mut configured = BTreeMap::<String, Vec<String>>::new();
    for preset in presets
        .iter()
        .filter(|preset| preset.provider == profile.profile())
    {
        configured
            .entry(preset.model.clone())
            .or_default()
            .push(preset.id.clone());
    }
    let models = builders
        .into_iter()
        .map(|(id, builder)| {
            let configured_presets = configured.remove(&id).unwrap_or_default();
            let executable = !configured_presets.is_empty();
            let availability =
                if builder.discovery && freshness == ProviderDiscoveryFreshness::Current {
                    ProviderDiscoveryAvailability::Available
                } else if builder.discovery && freshness == ProviderDiscoveryFreshness::Stale {
                    ProviderDiscoveryAvailability::Stale
                } else {
                    ProviderDiscoveryAvailability::Unverified
                };
            let suggestion = if executable {
                ProviderDiscoverySuggestion::None
            } else if builder.release && builder.profile_compatible {
                ProviderDiscoverySuggestion::AcceptReleaseStarter
            } else if builder.discovery && freshness == ProviderDiscoveryFreshness::Current {
                ProviderDiscoverySuggestion::AcceptDiscoveredWithDialect
            } else if builder.discovery {
                ProviderDiscoverySuggestion::RefreshRequired
            } else {
                ProviderDiscoverySuggestion::Incompatible
            };
            let mut sources = Vec::with_capacity(2);
            if builder.release {
                sources.push(ProviderDiscoverySource::ReleaseSeed);
            }
            if builder.discovery {
                sources.push(ProviderDiscoverySource::Discovery);
            }
            ProviderDiscoveryCatalogModel {
                id,
                release_catalog_key: builder.release_catalog_key,
                sources,
                availability,
                primary_dialect: builder.primary_dialect,
                profile_compatible: builder.profile_compatible,
                configured_presets,
                executable,
                suggestion,
            }
        })
        .collect();

    Ok(ProviderDiscoveryCatalogView {
        schema_version: PROVIDER_DISCOVERY_SCHEMA_VERSION,
        profile: profile.profile().to_owned(),
        template: profile.template().to_owned(),
        profile_fingerprint: profile.fingerprint(),
        dialects: profile.dialects().collect(),
        mode,
        freshness,
        observed_at_unix_ms: observation.map(ProviderDiscoveryProfile::observed_at_unix_ms),
        models,
    })
}

fn discovery_freshness(
    mode: ProviderCatalogMode,
    profile: &greentyper_core::provider::ProviderProfileSnapshot,
    observation: Option<&ProviderDiscoveryProfile>,
) -> ProviderDiscoveryFreshness {
    if !mode.includes_discovery() {
        return ProviderDiscoveryFreshness::Disabled;
    }
    match observation {
        None => ProviderDiscoveryFreshness::Missing,
        Some(observation)
            if observation.template() == profile.template()
                && observation.fingerprint() == profile.fingerprint() =>
        {
            ProviderDiscoveryFreshness::Current
        }
        Some(_) => ProviderDiscoveryFreshness::Stale,
    }
}
