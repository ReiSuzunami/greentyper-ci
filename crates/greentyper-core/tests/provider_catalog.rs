use greentyper_core::provider::{ProviderDialect, ProviderPricingSource};
use greentyper_core::provider_catalog::{
    CatalogAvailability, CatalogSourceKind, PROVIDER_CATALOG_SCHEMA_VERSION, ProviderCatalog,
    ProviderCatalogMode,
};

#[test]
fn release_catalog_freezes_versioned_templates_and_seed_provenance() {
    let catalog = ProviderCatalog::release();

    assert_eq!(catalog.schema_version(), PROVIDER_CATALOG_SCHEMA_VERSION);
    assert_eq!(catalog.seed_revision(), "2026-08-10.1");
    assert_eq!(catalog.observed_at(), "2026-08-10T00:00:00Z");
    assert_eq!(
        catalog
            .templates()
            .iter()
            .map(|template| template.id())
            .collect::<Vec<_>>(),
        vec!["deepseek", "openai", "opencode-go"]
    );

    let openai = catalog.template("openai").expect("OpenAI template");
    assert_eq!(openai.base_url().value(), "https://api.openai.com/v1");
    assert_eq!(openai.responses_route().value(), Some("/responses"));
    assert_eq!(
        openai.chat_completions_route().value(),
        Some("/chat/completions")
    );
    assert_eq!(openai.messages_route().value(), None);
    assert_eq!(openai.models_route().value(), Some("/models"));
    assert_eq!(
        openai.dialects().value(),
        &[ProviderDialect::Responses, ProviderDialect::ChatCompletions]
    );
    assert_eq!(
        openai.catalog_mode().value(),
        ProviderCatalogMode::TemplateAndDiscovery
    );
    assert_eq!(
        openai.pricing_source().value(),
        ProviderPricingSource::Template
    );
    assert_eq!(
        openai.base_url().provenance().source_kind(),
        CatalogSourceKind::ReleaseSeed
    );
    assert_eq!(
        openai.base_url().provenance().observed_at(),
        catalog.observed_at()
    );

    let deepseek = catalog.template("deepseek").expect("DeepSeek template");
    assert_eq!(deepseek.base_url().value(), "https://api.deepseek.com");
    assert_eq!(
        deepseek.messages_route().value(),
        Some("/anthropic/v1/messages")
    );
    assert_eq!(
        deepseek.dialects().value(),
        &[
            ProviderDialect::Responses,
            ProviderDialect::ChatCompletions,
            ProviderDialect::Messages,
        ]
    );

    let opencode = catalog
        .template("opencode-go")
        .expect("OpenCode Go template");
    assert_eq!(opencode.base_url().value(), "https://opencode.ai/zen/go/v1");
    assert_eq!(opencode.messages_route().value(), Some("/messages"));

    assert!(catalog.template("missing").is_none());
    assert!(catalog.model("missing/model").is_none());

    let keys = catalog
        .models()
        .iter()
        .map(|model| model.key())
        .collect::<Vec<_>>();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(keys, sorted, "release model keys must be sorted and unique");
    assert!(keys.contains(&"openai/gpt-5.6-sol"));
    assert!(keys.contains(&"openai/gpt-5.6-terra"));
    assert!(keys.contains(&"openai/gpt-5.6-luna"));
    assert!(keys.contains(&"deepseek/deepseek-v4-flash"));
    assert!(keys.contains(&"deepseek/deepseek-v4-pro"));
    assert!(keys.contains(&"opencode-go/gpt-5.6-luna"));
    assert!(keys.contains(&"opencode-go/qwen3.8-max"));

    let flash = catalog
        .model("deepseek/deepseek-v4-flash")
        .expect("DeepSeek Flash seed");
    assert_eq!(flash.catalog_schema_version(), catalog.schema_version());
    assert_eq!(flash.seed_revision(), catalog.seed_revision());
    assert_eq!(flash.observed_at(), catalog.observed_at());
    assert_eq!(flash.provider_template(), "deepseek");
    assert_eq!(flash.model_id().value(), "deepseek-v4-flash");
    assert_eq!(flash.primary_dialect().value(), ProviderDialect::Responses);
    assert_eq!(
        flash.supported_dialects().value(),
        &[
            ProviderDialect::Responses,
            ProviderDialect::ChatCompletions,
            ProviderDialect::Messages,
        ]
    );
    assert_eq!(
        flash.availability().value(),
        CatalogAvailability::Unverified
    );
    assert_eq!(flash.context_window_tokens().value(), None);
    assert_eq!(flash.price_schedule_ref().value(), None);
    assert_eq!(
        flash.primary_dialect().provenance().source_kind(),
        CatalogSourceKind::ReleaseSeed
    );
    assert!(
        flash
            .primary_dialect()
            .provenance()
            .source_ref()
            .starts_with("https://api-docs.deepseek.com/")
    );
}

#[test]
fn opencode_seed_preserves_per_model_dialects_without_gateway_inference() {
    let catalog = ProviderCatalog::release();

    for (key, dialect) in [
        ("opencode-go/gpt-5.6-luna", ProviderDialect::Responses),
        (
            "opencode-go/deepseek-v4-pro",
            ProviderDialect::ChatCompletions,
        ),
        ("opencode-go/minimax-m3", ProviderDialect::Messages),
    ] {
        let model = catalog.model(key).expect("OpenCode Go seed model");
        assert_eq!(model.primary_dialect().value(), dialect);
        assert_eq!(model.supported_dialects().value(), &[dialect]);
        assert_eq!(
            model.availability().value(),
            CatalogAvailability::Unverified
        );
        assert_eq!(model.capabilities().value(), None);
    }
}

#[test]
fn release_catalog_records_are_referentially_complete_and_explicitly_unknown() {
    let catalog = ProviderCatalog::release();

    for model in catalog.models() {
        assert!(catalog.template(model.provider_template()).is_some());
        assert_eq!(
            model.key(),
            format!("{}/{}", model.provider_template(), model.model_id().value())
        );
        assert!(
            model
                .supported_dialects()
                .value()
                .contains(&model.primary_dialect().value())
        );
        assert_eq!(model.catalog_schema_version(), catalog.schema_version());
        assert_eq!(model.seed_revision(), catalog.seed_revision());
        assert_eq!(model.observed_at(), catalog.observed_at());
        assert_eq!(model.context_window_tokens().value(), None);
        assert_eq!(model.capabilities().value(), None);
        assert_eq!(model.price_schedule_ref().value(), None);
        assert_eq!(
            model.availability().value(),
            CatalogAvailability::Unverified
        );
        for provenance in [
            model.model_id().provenance(),
            model.display_name().provenance(),
            model.primary_dialect().provenance(),
            model.supported_dialects().provenance(),
            model.context_window_tokens().provenance(),
            model.capabilities().provenance(),
            model.price_schedule_ref().provenance(),
            model.availability().provenance(),
        ] {
            assert_eq!(provenance.source_kind(), CatalogSourceKind::ReleaseSeed);
            assert_eq!(provenance.observed_at(), catalog.observed_at());
            assert!(provenance.source_ref().starts_with("https://"));
        }
    }
}
