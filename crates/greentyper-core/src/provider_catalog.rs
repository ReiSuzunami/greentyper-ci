//! Versioned, release-bundled Provider Templates and seed Model Catalog facts.
//!
//! This module performs no discovery and carries no credentials, instructions,
//! or provider payloads. Unknown catalog facts remain explicit `None` values.

use serde::{Deserialize, Serialize};

use crate::pricing::{
    PriceSchedule, PriceScheduleDefinition, PriceScheduleSource, PricingError, TokenRates,
};
use crate::provider::{ProviderDialect, ProviderPricingSource};
use crate::schema::SchemaKind;
use crate::usage::UsageTimestamp;

pub const PROVIDER_CATALOG_SCHEMA_VERSION: u16 = SchemaKind::ProviderCatalog.current().get();
pub const RELEASE_SEED_REVISION: &str = "2026-08-10.2";
pub const RELEASE_SEED_OBSERVED_AT: &str = "2026-08-10T00:00:00Z";

const OPENAI_MODEL_SOURCE: &str = "https://developers.openai.com/api/docs/guides/latest-model";
const OPENAI_API_SOURCE: &str =
    "https://developers.openai.com/api/reference/resources/models/methods/list";
const DEEPSEEK_API_SOURCE: &str = "https://api-docs.deepseek.com/";
const DEEPSEEK_MODEL_SOURCE: &str = "https://api-docs.deepseek.com/updates/";
const DEEPSEEK_PRICING_SOURCE: &str = "https://api-docs.deepseek.com/quick_start/pricing/";
const RELEASE_PRICE_EFFECTIVE_FROM_UNIX_MS: i64 = 1_786_320_000_000;
const OPENCODE_GO_SOURCE: &str = "https://opencode.ai/docs/go/";

const RESPONSES: &[ProviderDialect] = &[ProviderDialect::Responses];
const CHAT_COMPLETIONS: &[ProviderDialect] = &[ProviderDialect::ChatCompletions];
const MESSAGES: &[ProviderDialect] = &[ProviderDialect::Messages];
const OPENAI_DIALECTS: &[ProviderDialect] =
    &[ProviderDialect::Responses, ProviderDialect::ChatCompletions];
const ALL_DIALECTS: &[ProviderDialect] = &[
    ProviderDialect::Responses,
    ProviderDialect::ChatCompletions,
    ProviderDialect::Messages,
];
const DEEPSEEK_PRO_DIALECTS: &[ProviderDialect] =
    &[ProviderDialect::ChatCompletions, ProviderDialect::Messages];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSourceKind {
    ReleaseSeed,
    Discovery,
    UserOverride,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogProvenance {
    source_kind: CatalogSourceKind,
    source_ref: &'static str,
    observed_at: &'static str,
}

impl CatalogProvenance {
    const fn release(source_ref: &'static str) -> Self {
        Self {
            source_kind: CatalogSourceKind::ReleaseSeed,
            source_ref,
            observed_at: RELEASE_SEED_OBSERVED_AT,
        }
    }

    #[must_use]
    pub const fn source_kind(self) -> CatalogSourceKind {
        self.source_kind
    }

    #[must_use]
    pub const fn source_ref(self) -> &'static str {
        self.source_ref
    }

    #[must_use]
    pub const fn observed_at(self) -> &'static str {
        self.observed_at
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogField<T> {
    value: T,
    provenance: CatalogProvenance,
}

impl<T: Copy> CatalogField<T> {
    const fn release(value: T, source_ref: &'static str) -> Self {
        Self {
            value,
            provenance: CatalogProvenance::release(source_ref),
        }
    }

    #[must_use]
    pub const fn value(&self) -> T {
        self.value
    }

    #[must_use]
    pub const fn provenance(&self) -> CatalogProvenance {
        self.provenance
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCatalogMode {
    Template,
    Discovery,
    TemplateAndDiscovery,
    Manual,
}

impl ProviderCatalogMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Discovery => "discovery",
            Self::TemplateAndDiscovery => "template_and_discovery",
            Self::Manual => "manual",
        }
    }

    #[must_use]
    pub const fn includes_release_seed(self) -> bool {
        matches!(self, Self::Template | Self::TemplateAndDiscovery)
    }

    #[must_use]
    pub const fn includes_discovery(self) -> bool {
        matches!(self, Self::Discovery | Self::TemplateAndDiscovery)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogAvailability {
    Unverified,
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    ImageInput,
    Reasoning,
    ToolCalling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderTemplate {
    id: &'static str,
    base_url: CatalogField<&'static str>,
    responses_route: CatalogField<Option<&'static str>>,
    chat_completions_route: CatalogField<Option<&'static str>>,
    messages_route: CatalogField<Option<&'static str>>,
    models_route: CatalogField<Option<&'static str>>,
    dialects: CatalogField<&'static [ProviderDialect]>,
    catalog_mode: CatalogField<ProviderCatalogMode>,
    pricing_source: CatalogField<ProviderPricingSource>,
}

impl ProviderTemplate {
    #[must_use]
    pub const fn id(self) -> &'static str {
        self.id
    }

    #[must_use]
    pub const fn base_url(&self) -> &CatalogField<&'static str> {
        &self.base_url
    }

    #[must_use]
    pub const fn responses_route(&self) -> &CatalogField<Option<&'static str>> {
        &self.responses_route
    }

    #[must_use]
    pub const fn chat_completions_route(&self) -> &CatalogField<Option<&'static str>> {
        &self.chat_completions_route
    }

    #[must_use]
    pub const fn messages_route(&self) -> &CatalogField<Option<&'static str>> {
        &self.messages_route
    }

    #[must_use]
    pub const fn models_route(&self) -> &CatalogField<Option<&'static str>> {
        &self.models_route
    }

    #[must_use]
    pub const fn dialects(&self) -> &CatalogField<&'static [ProviderDialect]> {
        &self.dialects
    }

    #[must_use]
    pub const fn catalog_mode(&self) -> &CatalogField<ProviderCatalogMode> {
        &self.catalog_mode
    }

    #[must_use]
    pub const fn pricing_source(&self) -> &CatalogField<ProviderPricingSource> {
        &self.pricing_source
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ModelCatalogRecord {
    key: &'static str,
    provider_template: &'static str,
    catalog_schema_version: u16,
    seed_revision: &'static str,
    observed_at: &'static str,
    model_id: CatalogField<&'static str>,
    display_name: CatalogField<&'static str>,
    primary_dialect: CatalogField<ProviderDialect>,
    supported_dialects: CatalogField<&'static [ProviderDialect]>,
    context_window_tokens: CatalogField<Option<u64>>,
    capabilities: CatalogField<Option<&'static [ModelCapability]>>,
    price_schedule_ref: CatalogField<Option<&'static str>>,
    availability: CatalogField<CatalogAvailability>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReleaseModelIdentity {
    pub catalog_key: &'static str,
    pub provider_template: &'static str,
    pub model: &'static str,
    pub dialect: ProviderDialect,
}

struct HistoricalReleaseModels {
    seed_revision: &'static str,
    models: &'static [ReleaseModelIdentity],
}

const HISTORICAL_RELEASE_MODELS: &[HistoricalReleaseModels] = &[HistoricalReleaseModels {
    seed_revision: "2026-08-10.1",
    models: &[
        ReleaseModelIdentity {
            catalog_key: "deepseek/deepseek-v4-flash",
            provider_template: "deepseek",
            model: "deepseek-v4-flash",
            dialect: ProviderDialect::Responses,
        },
        ReleaseModelIdentity {
            catalog_key: "deepseek/deepseek-v4-pro",
            provider_template: "deepseek",
            model: "deepseek-v4-pro",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "openai/gpt-5.6-luna",
            provider_template: "openai",
            model: "gpt-5.6-luna",
            dialect: ProviderDialect::Responses,
        },
        ReleaseModelIdentity {
            catalog_key: "openai/gpt-5.6-sol",
            provider_template: "openai",
            model: "gpt-5.6-sol",
            dialect: ProviderDialect::Responses,
        },
        ReleaseModelIdentity {
            catalog_key: "openai/gpt-5.6-terra",
            provider_template: "openai",
            model: "gpt-5.6-terra",
            dialect: ProviderDialect::Responses,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/deepseek-v4-flash",
            provider_template: "opencode-go",
            model: "deepseek-v4-flash",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/deepseek-v4-pro",
            provider_template: "opencode-go",
            model: "deepseek-v4-pro",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/glm-5.1",
            provider_template: "opencode-go",
            model: "glm-5.1",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/glm-5.2",
            provider_template: "opencode-go",
            model: "glm-5.2",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/gpt-5.6-luna",
            provider_template: "opencode-go",
            model: "gpt-5.6-luna",
            dialect: ProviderDialect::Responses,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/grok-4.5",
            provider_template: "opencode-go",
            model: "grok-4.5",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/hy3",
            provider_template: "opencode-go",
            model: "hy3",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/kimi-k2.6",
            provider_template: "opencode-go",
            model: "kimi-k2.6",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/kimi-k2.7-code",
            provider_template: "opencode-go",
            model: "kimi-k2.7-code",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/kimi-k3",
            provider_template: "opencode-go",
            model: "kimi-k3",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/mimo-v2.5",
            provider_template: "opencode-go",
            model: "mimo-v2.5",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/mimo-v2.5-pro",
            provider_template: "opencode-go",
            model: "mimo-v2.5-pro",
            dialect: ProviderDialect::ChatCompletions,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/minimax-m2.7",
            provider_template: "opencode-go",
            model: "minimax-m2.7",
            dialect: ProviderDialect::Messages,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/minimax-m3",
            provider_template: "opencode-go",
            model: "minimax-m3",
            dialect: ProviderDialect::Messages,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/qwen3.6-plus",
            provider_template: "opencode-go",
            model: "qwen3.6-plus",
            dialect: ProviderDialect::Messages,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/qwen3.7-max",
            provider_template: "opencode-go",
            model: "qwen3.7-max",
            dialect: ProviderDialect::Messages,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/qwen3.7-plus",
            provider_template: "opencode-go",
            model: "qwen3.7-plus",
            dialect: ProviderDialect::Messages,
        },
        ReleaseModelIdentity {
            catalog_key: "opencode-go/qwen3.8-max",
            provider_template: "opencode-go",
            model: "qwen3.8-max",
            dialect: ProviderDialect::Messages,
        },
    ],
}];

impl ModelCatalogRecord {
    #[must_use]
    pub const fn key(self) -> &'static str {
        self.key
    }

    #[must_use]
    pub const fn provider_template(self) -> &'static str {
        self.provider_template
    }

    #[must_use]
    pub const fn catalog_schema_version(self) -> u16 {
        self.catalog_schema_version
    }

    #[must_use]
    pub const fn seed_revision(self) -> &'static str {
        self.seed_revision
    }

    #[must_use]
    pub const fn observed_at(self) -> &'static str {
        self.observed_at
    }

    #[must_use]
    pub const fn model_id(&self) -> &CatalogField<&'static str> {
        &self.model_id
    }

    #[must_use]
    pub const fn display_name(&self) -> &CatalogField<&'static str> {
        &self.display_name
    }

    #[must_use]
    pub const fn primary_dialect(&self) -> &CatalogField<ProviderDialect> {
        &self.primary_dialect
    }

    #[must_use]
    pub const fn supported_dialects(&self) -> &CatalogField<&'static [ProviderDialect]> {
        &self.supported_dialects
    }

    #[must_use]
    pub const fn context_window_tokens(&self) -> &CatalogField<Option<u64>> {
        &self.context_window_tokens
    }

    #[must_use]
    pub const fn capabilities(&self) -> &CatalogField<Option<&'static [ModelCapability]>> {
        &self.capabilities
    }

    #[must_use]
    pub const fn price_schedule_ref(&self) -> &CatalogField<Option<&'static str>> {
        &self.price_schedule_ref
    }

    #[must_use]
    pub const fn availability(&self) -> &CatalogField<CatalogAvailability> {
        &self.availability
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderCatalog {
    schema_version: u16,
    seed_revision: &'static str,
    observed_at: &'static str,
    templates: &'static [ProviderTemplate],
    models: &'static [ModelCatalogRecord],
}

impl ProviderCatalog {
    #[must_use]
    pub const fn release() -> &'static Self {
        &RELEASE_CATALOG
    }

    #[must_use]
    pub const fn schema_version(self) -> u16 {
        self.schema_version
    }

    #[must_use]
    pub const fn seed_revision(self) -> &'static str {
        self.seed_revision
    }

    #[must_use]
    pub const fn observed_at(self) -> &'static str {
        self.observed_at
    }

    #[must_use]
    pub const fn templates(self) -> &'static [ProviderTemplate] {
        self.templates
    }

    #[must_use]
    pub const fn models(self) -> &'static [ModelCatalogRecord] {
        self.models
    }

    #[must_use]
    pub fn template(self, id: &str) -> Option<&'static ProviderTemplate> {
        self.templates
            .binary_search_by(|template| template.id.cmp(id))
            .ok()
            .map(|index| &self.templates[index])
    }

    #[must_use]
    pub fn model(self, key: &str) -> Option<&'static ModelCatalogRecord> {
        self.models
            .binary_search_by(|model| model.key.cmp(key))
            .ok()
            .map(|index| &self.models[index])
    }

    pub(crate) fn release_model_identity(
        self,
        seed_revision: &str,
        catalog_key: &str,
    ) -> Option<ReleaseModelIdentity> {
        if seed_revision == self.seed_revision {
            return self.model(catalog_key).map(|record| ReleaseModelIdentity {
                catalog_key: record.key(),
                provider_template: record.provider_template(),
                model: record.model_id().value(),
                dialect: record.primary_dialect().value(),
            });
        }
        HISTORICAL_RELEASE_MODELS
            .iter()
            .find(|history| history.seed_revision == seed_revision)?
            .models
            .iter()
            .copied()
            .find(|identity| identity.catalog_key == catalog_key)
    }
}

#[derive(Clone, Copy)]
struct TemplateRoutes {
    responses: Option<&'static str>,
    chat_completions: Option<&'static str>,
    messages: Option<&'static str>,
    models: Option<&'static str>,
}

const fn template(
    id: &'static str,
    base_url: &'static str,
    routes: TemplateRoutes,
    dialects: &'static [ProviderDialect],
    source_ref: &'static str,
) -> ProviderTemplate {
    ProviderTemplate {
        id,
        base_url: CatalogField::release(base_url, source_ref),
        responses_route: CatalogField::release(routes.responses, source_ref),
        chat_completions_route: CatalogField::release(routes.chat_completions, source_ref),
        messages_route: CatalogField::release(routes.messages, source_ref),
        models_route: CatalogField::release(routes.models, source_ref),
        dialects: CatalogField::release(dialects, source_ref),
        catalog_mode: CatalogField::release(ProviderCatalogMode::TemplateAndDiscovery, source_ref),
        pricing_source: CatalogField::release(ProviderPricingSource::Template, source_ref),
    }
}

const PROVIDER_TEMPLATES: &[ProviderTemplate] = &[
    template(
        "deepseek",
        "https://api.deepseek.com",
        TemplateRoutes {
            responses: Some("/responses"),
            chat_completions: Some("/chat/completions"),
            messages: Some("/anthropic/v1/messages"),
            models: Some("/models"),
        },
        ALL_DIALECTS,
        DEEPSEEK_API_SOURCE,
    ),
    template(
        "openai",
        "https://api.openai.com/v1",
        TemplateRoutes {
            responses: Some("/responses"),
            chat_completions: Some("/chat/completions"),
            messages: None,
            models: Some("/models"),
        },
        OPENAI_DIALECTS,
        OPENAI_API_SOURCE,
    ),
    template(
        "opencode-go",
        "https://opencode.ai/zen/go/v1",
        TemplateRoutes {
            responses: Some("/responses"),
            chat_completions: Some("/chat/completions"),
            messages: Some("/messages"),
            models: Some("/models"),
        },
        ALL_DIALECTS,
        OPENCODE_GO_SOURCE,
    ),
];

const fn model(
    key: &'static str,
    provider_template: &'static str,
    model_id: &'static str,
    display_name: &'static str,
    primary_dialect: ProviderDialect,
    supported_dialects: &'static [ProviderDialect],
    source_ref: &'static str,
) -> ModelCatalogRecord {
    ModelCatalogRecord {
        key,
        provider_template,
        catalog_schema_version: PROVIDER_CATALOG_SCHEMA_VERSION,
        seed_revision: RELEASE_SEED_REVISION,
        observed_at: RELEASE_SEED_OBSERVED_AT,
        model_id: CatalogField::release(model_id, source_ref),
        display_name: CatalogField::release(display_name, source_ref),
        primary_dialect: CatalogField::release(primary_dialect, source_ref),
        supported_dialects: CatalogField::release(supported_dialects, source_ref),
        context_window_tokens: CatalogField::release(None, source_ref),
        capabilities: CatalogField::release(None, source_ref),
        price_schedule_ref: CatalogField::release(None, source_ref),
        availability: CatalogField::release(CatalogAvailability::Unverified, source_ref),
    }
}

const fn deepseek_priced_model(
    key: &'static str,
    provider_template: &'static str,
    model_id: &'static str,
    display_name: &'static str,
    primary_dialect: ProviderDialect,
    supported_dialects: &'static [ProviderDialect],
    price_schedule_ref: &'static str,
) -> ModelCatalogRecord {
    let mut record = model(
        key,
        provider_template,
        model_id,
        display_name,
        primary_dialect,
        supported_dialects,
        DEEPSEEK_MODEL_SOURCE,
    );
    record.price_schedule_ref =
        CatalogField::release(Some(price_schedule_ref), DEEPSEEK_PRICING_SOURCE);
    record
}

const MODEL_CATALOG: &[ModelCatalogRecord] = &[
    deepseek_priced_model(
        "deepseek/deepseek-v4-flash",
        "deepseek",
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        ProviderDialect::Responses,
        ALL_DIALECTS,
        "deepseek-v4-flash-2026-08-10",
    ),
    deepseek_priced_model(
        "deepseek/deepseek-v4-pro",
        "deepseek",
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        ProviderDialect::ChatCompletions,
        DEEPSEEK_PRO_DIALECTS,
        "deepseek-v4-pro-2026-08-10",
    ),
    model(
        "openai/gpt-5.6-luna",
        "openai",
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        ProviderDialect::Responses,
        RESPONSES,
        OPENAI_MODEL_SOURCE,
    ),
    model(
        "openai/gpt-5.6-sol",
        "openai",
        "gpt-5.6-sol",
        "GPT-5.6 Sol",
        ProviderDialect::Responses,
        RESPONSES,
        OPENAI_MODEL_SOURCE,
    ),
    model(
        "openai/gpt-5.6-terra",
        "openai",
        "gpt-5.6-terra",
        "GPT-5.6 Terra",
        ProviderDialect::Responses,
        RESPONSES,
        OPENAI_MODEL_SOURCE,
    ),
    model(
        "opencode-go/deepseek-v4-flash",
        "opencode-go",
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/deepseek-v4-pro",
        "opencode-go",
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/glm-5.1",
        "opencode-go",
        "glm-5.1",
        "GLM 5.1",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/glm-5.2",
        "opencode-go",
        "glm-5.2",
        "GLM 5.2",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/gpt-5.6-luna",
        "opencode-go",
        "gpt-5.6-luna",
        "GPT-5.6 Luna",
        ProviderDialect::Responses,
        RESPONSES,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/grok-4.5",
        "opencode-go",
        "grok-4.5",
        "Grok 4.5",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/hy3",
        "opencode-go",
        "hy3",
        "HY 3",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/kimi-k2.6",
        "opencode-go",
        "kimi-k2.6",
        "Kimi K2.6",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/kimi-k2.7-code",
        "opencode-go",
        "kimi-k2.7-code",
        "Kimi K2.7 Code",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/kimi-k3",
        "opencode-go",
        "kimi-k3",
        "Kimi K3",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/mimo-v2.5",
        "opencode-go",
        "mimo-v2.5",
        "MiMo V2.5",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/mimo-v2.5-pro",
        "opencode-go",
        "mimo-v2.5-pro",
        "MiMo V2.5 Pro",
        ProviderDialect::ChatCompletions,
        CHAT_COMPLETIONS,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/minimax-m2.7",
        "opencode-go",
        "minimax-m2.7",
        "MiniMax M2.7",
        ProviderDialect::Messages,
        MESSAGES,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/minimax-m3",
        "opencode-go",
        "minimax-m3",
        "MiniMax M3",
        ProviderDialect::Messages,
        MESSAGES,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/qwen3.6-plus",
        "opencode-go",
        "qwen3.6-plus",
        "Qwen 3.6 Plus",
        ProviderDialect::Messages,
        MESSAGES,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/qwen3.7-max",
        "opencode-go",
        "qwen3.7-max",
        "Qwen 3.7 Max",
        ProviderDialect::Messages,
        MESSAGES,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/qwen3.7-plus",
        "opencode-go",
        "qwen3.7-plus",
        "Qwen 3.7 Plus",
        ProviderDialect::Messages,
        MESSAGES,
        OPENCODE_GO_SOURCE,
    ),
    model(
        "opencode-go/qwen3.8-max",
        "opencode-go",
        "qwen3.8-max",
        "Qwen 3.8 Max",
        ProviderDialect::Messages,
        MESSAGES,
        OPENCODE_GO_SOURCE,
    ),
];

const RELEASE_CATALOG: ProviderCatalog = ProviderCatalog {
    schema_version: PROVIDER_CATALOG_SCHEMA_VERSION,
    seed_revision: RELEASE_SEED_REVISION,
    observed_at: RELEASE_SEED_OBSERVED_AT,
    templates: PROVIDER_TEMPLATES,
    models: MODEL_CATALOG,
};

#[derive(Clone, Copy)]
struct ReleasePriceScheduleSeed {
    provider_template: &'static str,
    model: &'static str,
    version: &'static str,
    source_ref: &'static str,
    rates: TokenRates,
}

const RELEASE_PRICE_SCHEDULES: &[ReleasePriceScheduleSeed] = &[
    ReleasePriceScheduleSeed {
        provider_template: "deepseek",
        model: "deepseek-v4-flash",
        version: "2026-08-10.1",
        source_ref: DEEPSEEK_PRICING_SOURCE,
        rates: TokenRates::new(140_000, 2_800, 0, 280_000, 280_000),
    },
    ReleasePriceScheduleSeed {
        provider_template: "deepseek",
        model: "deepseek-v4-pro",
        version: "2026-08-10.1",
        source_ref: DEEPSEEK_PRICING_SOURCE,
        rates: TokenRates::new(435_000, 3_625, 0, 870_000, 870_000),
    },
];

pub(crate) fn release_price_schedules_for_profile(
    provider_profile: &str,
    provider_template: &str,
    source: PriceScheduleSource,
) -> Result<Vec<PriceSchedule>, PricingError> {
    RELEASE_PRICE_SCHEDULES
        .iter()
        .filter(|seed| seed.provider_template == provider_template)
        .map(|seed| {
            PriceSchedule::new_trusted(PriceScheduleDefinition {
                id: release_schedule_id(provider_profile, seed.model),
                version: seed.version.to_owned(),
                currency: "USD".to_owned(),
                provider_profile: provider_profile.to_owned(),
                model: seed.model.to_owned(),
                dialect: None,
                service_tier: None,
                minimum_context_tokens: 0,
                maximum_context_tokens: None,
                effective_from: UsageTimestamp::from_unix_millis(
                    RELEASE_PRICE_EFFECTIVE_FROM_UNIX_MS,
                )
                .expect("release Price Schedule timestamp is valid"),
                effective_until: None,
                source,
                source_ref: seed.source_ref.to_owned(),
                rates: seed.rates,
            })
        })
        .collect()
}

pub(crate) fn has_release_price_schedules(provider_template: &str) -> bool {
    RELEASE_PRICE_SCHEDULES
        .iter()
        .any(|seed| seed.provider_template == provider_template)
}

fn release_schedule_id(provider_profile: &str, model: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for bytes in [provider_profile.as_bytes(), model.as_bytes()] {
        hash ^= bytes.len() as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("release-{hash:016x}")
}
