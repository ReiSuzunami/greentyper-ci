use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::config::{
    ConfigApplicationTiming, ConfigDocument, ConfigEditorError, ConfigEditorOperation,
    ConfigEditorSession, ConfigErrorCategory, ConfigFieldContents, ConfigFieldInteraction,
    ConfigObjectKind, ConfigObjectRef, ConfigPaths, ConfigRuntime, ConfigRuntimeError, ConfigScope,
    ConfigValue, ConfigValueKind, MAX_CONFIG_ID_BYTES, MAX_CONFIG_STRING_BYTES, MAX_OUTPUT_TOKENS,
    config_schema, parse_config_value,
};
use greentyper_core::pricing::PriceScheduleSource;
use greentyper_core::provider::{ProviderDialect, ProviderPricingSource};
use greentyper_core::provider_catalog::{CatalogAvailability, CatalogSourceKind, ProviderCatalog};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-config-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create config test directory");
        Self { root }
    }

    fn paths(&self) -> ConfigPaths {
        ConfigPaths::new(
            self.root.join("user/config.toml"),
            self.root.join("project/.greentyper/config.toml"),
        )
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove config test directory");
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".bak");
    PathBuf::from(value)
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("config parent")).expect("create config parent");
    fs::write(path, contents).expect("write config fixture");
}

fn value_string(value: &str) -> ConfigValue {
    ConfigValue::String(value.to_owned())
}

fn stage_price_schedule(
    draft: &mut greentyper_core::config::ConfigDraft,
    id: &str,
    effective_from: &str,
) {
    for (field, value) in [
        ("version", "2026-08-10.1"),
        ("currency", "USD"),
        ("provider", "openai-main"),
        ("model", "gpt-5.6-sol"),
        ("dialect", "responses"),
        ("service_tier", "standard"),
        ("minimum_context_tokens", "0"),
        ("maximum_context_tokens", "200000"),
        ("effective_from", effective_from),
        ("source", "manual"),
        ("source_ref", "synthetic-manual-rate-card"),
    ] {
        draft
            .set_raw(&format!("price_schedules.{id}.{field}"), value)
            .expect("stage Price Schedule field");
    }
    for (field, value) in [
        ("input_micros_per_million", "1000000"),
        ("cached_input_micros_per_million", "500000"),
        ("cache_write_micros_per_million", "0"),
        ("output_micros_per_million", "2000000"),
        ("reasoning_output_micros_per_million", "3000000"),
    ] {
        draft
            .set_raw(&format!("price_schedules.{id}.rates.{field}"), value)
            .expect("stage Price Schedule rate");
    }
}

#[test]
fn price_schedule_is_a_schema_owned_resolved_config_object() {
    let temp = TempTree::new("price-schedule-object");
    let config = ConfigDocument::parse(
        r#"
schema_version = 1

[providers.openai-main]
template = "openai"
credential = "synthetic-openai-credential-reference"

[providers.openai-main.pricing]
source = "manual"

[price_schedules.openai-sol]
version = "2026-08-10.1"
currency = "USD"
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
service_tier = "standard"
minimum_context_tokens = 0
maximum_context_tokens = 200000
effective_from = "2026-08-10T00:00:00Z"
source = "manual"
source_ref = "synthetic-manual-rate-card"

[price_schedules.openai-sol.rates]
input_micros_per_million = 1000000
cached_input_micros_per_million = 500000
cache_write_micros_per_million = 0
output_micros_per_million = 2000000
reasoning_output_micros_per_million = 3000000
"#,
    )
    .expect("parse Price Schedule");
    let runtime = ConfigRuntime::open(temp.paths(), config).expect("resolve Price Schedule");
    let book = runtime
        .resolved_price_schedules()
        .expect("resolved Price Schedule book");
    assert_eq!(book.schedules().len(), 1);
    let schedule = &book.schedules()[0];
    assert_eq!(schedule.id(), "openai-sol");
    assert_eq!(schedule.version(), "2026-08-10.1");
    assert_eq!(schedule.currency(), "USD");
    assert_eq!(schedule.provider_profile(), "openai-main");
    assert_eq!(schedule.model(), "gpt-5.6-sol");
    assert_eq!(schedule.source(), PriceScheduleSource::Manual);
    assert_eq!(schedule.rates().cache_write_micros_per_million(), 0);

    assert!(
        runtime
            .addressable_objects()
            .unwrap()
            .contains(&ConfigObjectRef::new(
                ConfigObjectKind::PriceSchedule,
                "openai-sol",
            ))
    );
    let rate = config_schema()
        .iter()
        .find(|entry| entry.path_pattern == "price_schedules.<id>.rates.input_micros_per_million")
        .expect("schema-owned input rate");
    assert_eq!(rate.value_kind, ConfigValueKind::NonNegativeInteger);
    assert_eq!(rate.timing, ConfigApplicationTiming::NextConfigEpoch);
    assert_eq!(
        parse_config_value(
            "price_schedules.openai-sol.rates.cache_write_micros_per_million",
            "0",
        )
        .unwrap(),
        ConfigValue::NonNegativeInteger(0)
    );
}

#[test]
fn editable_price_schedule_cannot_claim_template_or_provider_provenance() {
    for source in ["template", "provider_reported"] {
        let temp = TempTree::new("price-schedule-provenance");
        let paths = temp.paths();
        write(
            paths.user(),
            r#"
schema_version = 1

[providers.openai-main]
template = "openai"
credential = "synthetic-openai-credential-reference"
"#,
        );
        let mut runtime =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open provenance fixture");
        let mut draft = runtime
            .begin_draft(ConfigScope::Project)
            .expect("begin untrusted provenance draft");
        stage_price_schedule(&mut draft, "openai-sol", "2026-08-10T00:00:00Z");
        draft
            .set_raw("price_schedules.openai-sol.source", source)
            .expect("stage untrusted provenance label");
        assert!(matches!(
            runtime.commit(draft, true),
            Err(ConfigRuntimeError::InvalidValue { path, .. })
                if path == "price_schedules.openai-sol.source"
        ));
    }
}

#[test]
fn price_schedule_overlap_is_rejected_by_draft_validation_without_mutation() {
    let temp = TempTree::new("price-schedule-overlap");
    write(
        temp.paths().user(),
        r#"schema_version = 1

[providers.openai-main]
template = "openai"
credential = "synthetic-openai-credential-reference"

[providers.openai-main.pricing]
source = "manual"

[price_schedules.first]
version = "2026-08-10.1"
currency = "USD"
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
service_tier = "standard"
minimum_context_tokens = 0
maximum_context_tokens = 200000
effective_from = "2026-08-10T00:00:00Z"
source = "manual"
source_ref = "synthetic-manual-rate-card"

[price_schedules.first.rates]
input_micros_per_million = 1000000
cached_input_micros_per_million = 500000
cache_write_micros_per_million = 0
output_micros_per_million = 2000000
reasoning_output_micros_per_million = 3000000
"#,
    );
    write(temp.paths().project(), "schema_version = 1\n");
    let before = fs::read(temp.paths().project()).expect("read project layer");
    let mut runtime = ConfigRuntime::open(temp.paths(), ConfigDocument::empty()).unwrap();
    let mut draft = runtime.begin_draft(ConfigScope::Project).unwrap();
    stage_price_schedule(&mut draft, "second", "2026-08-11T00:00:00Z");
    assert!(matches!(
        runtime.validate_draft(&draft),
        Err(ConfigRuntimeError::InvalidValue { path, .. }) if path == "price_schedules"
    ));
    assert!(matches!(
        runtime.commit(draft, false),
        Err(ConfigRuntimeError::InvalidValue { path, .. }) if path == "price_schedules"
    ));
    assert_eq!(fs::read(temp.paths().project()).unwrap(), before);
    assert_eq!(
        runtime
            .resolved_price_schedules()
            .unwrap()
            .schedules()
            .len(),
        1
    );
}

#[test]
fn schema_and_parser_are_versioned_typed_and_secret_safe() {
    let schema = config_schema();
    assert!(schema.len() >= 30);
    let credential = schema
        .iter()
        .find(|entry| entry.path_pattern == "providers.<id>.credential")
        .expect("credential reference schema");
    assert_eq!(credential.value_kind, ConfigValueKind::String);
    assert!(credential.credential_reference);
    assert_eq!(
        credential.timing,
        ConfigApplicationTiming::NextProviderEpoch
    );
    assert_eq!(
        credential.interaction(),
        ConfigFieldInteraction::CredentialReference {
            max_bytes: MAX_CONFIG_ID_BYTES,
        }
    );
    let provider_template = schema
        .iter()
        .find(|entry| entry.path_pattern == "providers.<id>.template")
        .expect("Provider template schema");
    let ConfigFieldInteraction::Choice { choices } = provider_template.interaction() else {
        panic!("Provider template must use a schema-owned choice interaction")
    };
    assert_eq!(
        choices,
        ProviderCatalog::release()
            .templates()
            .iter()
            .map(|template| template.id())
            .collect::<Vec<_>>()
    );
    let preset = schema
        .iter()
        .find(|entry| entry.path_pattern == "ui.statusline.preset")
        .expect("statusline preset schema");
    assert_eq!(
        preset.interaction(),
        ConfigFieldInteraction::Choice {
            choices: &["minimal", "balanced", "diagnostic", "custom"],
        }
    );
    let expansion = schema
        .iter()
        .find(|entry| entry.path_pattern == "ui.statusline.expand")
        .expect("statusline expansion schema");
    assert_eq!(
        expansion.interaction(),
        ConfigFieldInteraction::Choice {
            choices: &["auto", "compact", "expanded"],
        }
    );
    for (entry, choices) in [
        (provider_template, choices),
        (preset, &["minimal", "balanced", "diagnostic", "custom"][..]),
        (expansion, &["auto", "compact", "expanded"][..]),
    ] {
        for choice in choices {
            let path = entry.path_pattern.replace("<id>", "edge");
            assert!(
                parse_config_value(&path, choice).is_ok(),
                "schema interaction offered an invalid choice: {}={choice}",
                entry.path_pattern
            );
        }
    }
    let base_url = schema
        .iter()
        .find(|entry| entry.path_pattern == "providers.<id>.base_url")
        .expect("Provider base URL schema");
    assert_eq!(
        base_url.interaction(),
        ConfigFieldInteraction::Text {
            max_bytes: MAX_CONFIG_STRING_BYTES,
        }
    );
    let model_provider = schema
        .iter()
        .find(|entry| entry.path_pattern == "model_presets.<id>.provider")
        .expect("Model Preset Provider schema");
    assert_eq!(
        model_provider.interaction(),
        ConfigFieldInteraction::Text {
            max_bytes: MAX_CONFIG_ID_BYTES,
        }
    );
    let model = schema
        .iter()
        .find(|entry| entry.path_pattern == "model_presets.<id>.model")
        .expect("Model Preset model schema");
    assert_eq!(
        model.interaction(),
        ConfigFieldInteraction::Text {
            max_bytes: MAX_CONFIG_STRING_BYTES,
        }
    );
    let dialect = schema
        .iter()
        .find(|entry| entry.path_pattern == "model_presets.<id>.dialect")
        .expect("Model Preset dialect schema");
    assert_eq!(
        dialect.interaction(),
        ConfigFieldInteraction::Choice {
            choices: &["responses", "chat_completions", "messages"],
        }
    );
    for choice in ["responses", "chat_completions", "messages"] {
        assert!(
            parse_config_value("model_presets.fast.dialect", choice).is_ok(),
            "Model Preset dialect interaction offered an invalid choice: {choice}"
        );
    }

    assert_eq!(
        parse_config_value("runtime.max_output_bytes", "4096").expect("positive integer"),
        ConfigValue::PositiveInteger(4096)
    );
    assert_eq!(
        parse_config_value("ui.statusline.custom.left", "[\"model\", \"usage\"]")
            .expect("string list"),
        ConfigValue::StringList(vec!["model".to_owned(), "usage".to_owned()])
    );
    assert!(matches!(
        parse_config_value("providers.Bad.template", "openai-compatible"),
        Err(ConfigRuntimeError::InvalidValue { .. })
    ));
    assert!(matches!(
        parse_config_value("credentials.edge.secret", "do-not-read"),
        Err(ConfigRuntimeError::SecretReadForbidden(_))
    ));
    assert!(matches!(
        ConfigDocument::parse("schema_version = 2\n"),
        Err(ConfigRuntimeError::UnsupportedSchema {
            supported: 1,
            actual: 2
        })
    ));
    assert!(matches!(
        ConfigDocument::parse("schema_version = 1\nunknown = true\n"),
        Err(ConfigRuntimeError::Parse { .. })
    ));
    assert!(matches!(
        ConfigDocument::parse(
            "schema_version = 1\n[providers.edge]\ntemplate = \"openai\"\n[providers.edge.routes]\nresponses = 'v1\\responses'\n"
        ),
        Err(ConfigRuntimeError::InvalidValue { path, .. })
            if path == "providers.edge.routes.responses"
    ));
    let excessive_output_tokens = format!(
        "schema_version = 1\n[model_presets.big]\nmax_output_tokens = {}\n",
        MAX_OUTPUT_TOKENS + 1
    );
    assert!(matches!(
        ConfigDocument::parse(&excessive_output_tokens),
        Err(ConfigRuntimeError::InvalidValue { path, .. })
            if path == "model_presets.big.max_output_tokens"
    ));
    assert!(matches!(
        ConfigDocument::parse(
            "schema_version = 1\n[model_presets.bad]\nreasoning_effort = \"turbo\"\n"
        ),
        Err(ConfigRuntimeError::InvalidValue { path, .. })
            if path == "model_presets.bad.reasoning_effort"
    ));
    assert!(matches!(
        ConfigDocument::parse(
            "schema_version = 1\n[model_presets.bad]\nservice_tier = \"free\"\n"
        ),
        Err(ConfigRuntimeError::InvalidValue { path, .. })
            if path == "model_presets.bad.service_tier"
    ));

    let document = ConfigDocument::parse(
        r#"
schema_version = 1

[providers.edge]
template = "openai-compatible"
credential = "synthetic-edge-credential-reference"
base_url = "https://gateway.example.com/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "responses"

[providers.edge.pricing]
source = "unknown"
"#,
    )
    .expect("parse provider profile");
    assert_eq!(
        document
            .get("providers.edge.routes.responses")
            .expect("known route"),
        Some(value_string("/responses"))
    );
    let encoded = document.to_toml().expect("serialize config");
    assert!(encoded.ends_with('\n'));
    assert_eq!(
        ConfigDocument::parse(&encoded).expect("roundtrip config"),
        document
    );
    assert!(matches!(
        ConfigDocument::parse(
            "schema_version = 1\n[providers.edge]\ntemplate = \"openai\"\ncredential = \"sk_synthetic_fixture_not_a_real_secret\"\n"
        ),
        Err(ConfigRuntimeError::InvalidValue { path, .. })
            if path == "providers.edge.credential"
    ));
}

#[test]
fn config_center_views_are_schema_driven_provenanced_and_secret_safe() {
    let temp = TempTree::new("center-views");
    let paths = temp.paths();
    write(
        paths.project(),
        r#"
schema_version = 1

[providers.edge]
template = "openai-compatible"
credential = "synthetic-edge-credential-reference"
base_url = "https://gateway.example.com/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"

[providers.edge.pricing]
source = "unknown"

[model_presets.fast]
provider = "edge"
model = "fixture-model"
dialect = "responses"
reasoning_effort = "high"
service_tier = "priority"
max_output_tokens = 2048

[[stats.windows]]
id = "work"
start = "09:00"
end = "17:00"
days = ["mon", "tue", "wed", "thu", "fri"]
timezone = "Asia/Hong_Kong"
"#,
    );
    let runtime =
        ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open Config Center fixture");

    assert_eq!(
        runtime.addressable_objects().expect("addressable objects"),
        vec![
            ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge"),
            ConfigObjectRef::new(ConfigObjectKind::ModelPreset, "fast"),
            ConfigObjectRef::new(ConfigObjectKind::UsageWindow, "work"),
        ]
    );

    let base_url = runtime
        .inspect_field(ConfigScope::User, "providers.edge.base_url")
        .expect("inspect inherited base URL");
    assert_eq!(base_url.path_pattern, "providers.<id>.base_url");
    assert_eq!(base_url.command_path, "/config provider url");
    assert_eq!(base_url.target_scope, ConfigScope::User);
    assert_eq!(
        base_url.contents,
        ConfigFieldContents::Value {
            effective: Some(value_string("https://gateway.example.com/v1")),
            source: Some(ConfigScope::Project),
            target: None,
        }
    );

    let credential = runtime
        .inspect_field(ConfigScope::Project, "providers.edge.credential")
        .expect("inspect credential binding");
    assert_eq!(
        credential.contents,
        ConfigFieldContents::CredentialBinding {
            effective_bound: true,
            source: Some(ConfigScope::Project),
            target_bound: true,
        }
    );
    let encoded = serde_json::to_string(&credential).expect("serialize credential view");
    assert!(!encoded.contains("synthetic-edge-credential-reference"));
    assert!(matches!(
        runtime.get_effective("providers.edge.credential"),
        Err(ConfigRuntimeError::SecretReadForbidden(path))
            if path == "providers.edge.credential"
    ));
    assert!(
        runtime
            .effective_entries()
            .expect("safe effective entries")
            .iter()
            .all(|entry| entry.path != "providers.edge.credential")
    );
    let project_draft = runtime
        .begin_draft(ConfigScope::Project)
        .expect("begin credential-safe draft");
    assert!(matches!(
        project_draft.get("providers.edge.credential"),
        Err(ConfigRuntimeError::SecretReadForbidden(path))
            if path == "providers.edge.credential"
    ));
    assert!(!format!("{project_draft:?}").contains("synthetic-edge-credential-reference"));

    let provider_fields = runtime
        .object_fields(ConfigScope::User, ConfigObjectKind::ProviderProfile, "edge")
        .expect("provider fields");
    assert_eq!(
        provider_fields.len(),
        config_schema()
            .iter()
            .filter(|entry| entry.path_pattern.starts_with("providers.<id>."))
            .count()
    );
    assert!(
        provider_fields
            .iter()
            .all(|field| field.path.starts_with("providers.edge."))
    );
    assert!(matches!(
        runtime.object_fields(
            ConfigScope::User,
            ConfigObjectKind::ProviderProfile,
            "missing"
        ),
        Err(ConfigRuntimeError::UnknownObject(path)) if path == "providers.missing"
    ));

    let presets = runtime.model_presets().expect("resolved model presets");
    assert_eq!(presets.len(), 1);
    assert_eq!(presets[0].id, "fast");
    assert_eq!(presets[0].provider, "edge");
    assert_eq!(presets[0].model, "fixture-model");
    assert_eq!(presets[0].dialect, ProviderDialect::Responses);
    assert_eq!(
        presets[0].reasoning_effort,
        Some(greentyper_core::config::ReasoningEffort::High)
    );
    assert_eq!(
        presets[0].service_tier,
        Some(greentyper_core::config::ServiceTier::Priority)
    );
    assert_eq!(presets[0].max_output_tokens, Some(2_048));
    assert!(!presets[0].favorite);
    assert!(presets[0].fallback.is_empty());
    assert_eq!(
        runtime.model_preset("fast").expect("exact model preset"),
        presets[0]
    );
    assert!(matches!(
        runtime.model_preset("missing"),
        Err(ConfigRuntimeError::UnknownObject(path)) if path == "model_presets.missing"
    ));
    assert_eq!(
        runtime
            .provider_profile("edge")
            .expect("resolve exact Provider Profile")
            .expect("external Provider Profile")
            .profile(),
        "edge"
    );
}

#[test]
fn config_editor_creates_a_provider_profile_through_one_atomic_draft() {
    let temp = TempTree::new("create-provider-profile");
    let paths = temp.paths();
    let mut runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open config runtime");
    let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");

    let mut editor =
        ConfigEditorSession::create_object(&runtime, ConfigScope::Project, object.clone())
            .expect("begin provider profile creation");
    assert_eq!(editor.operation(), ConfigEditorOperation::Create);
    assert_eq!(editor.object(), Some(&object));
    assert_eq!(
        editor.field(&runtime).expect("initial field").path,
        "providers.edge.template"
    );

    editor
        .stage_raw("openai-compatible")
        .expect("stage provider template");
    let preview = editor.preview(&mut runtime).expect("preview creation");
    assert_eq!(preview.changes.len(), 1);
    assert_eq!(preview.changes[0].path, "providers.edge.template");
    assert!(!paths.project().exists(), "dry-run must not write config");

    let commit = editor.commit(&mut runtime).expect("commit creation");
    assert!(commit.written);
    assert_eq!(
        runtime.addressable_objects().expect("addressable objects"),
        vec![object]
    );
}

#[test]
fn provider_editor_builds_a_redacted_profile_snapshot_from_the_uncommitted_draft() {
    let temp = TempTree::new("draft-provider-snapshot");
    let paths = temp.paths();
    let runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open config runtime");
    let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
    let mut editor =
        ConfigEditorSession::create_object(&runtime, ConfigScope::Project, object.clone())
            .expect("begin provider profile creation");

    editor
        .stage_raw("openai-compatible")
        .expect("stage provider template");
    editor
        .focus_from_query(&runtime, "/config provider credential", 0)
        .expect("focus credential binding");
    editor
        .stage_credential_reference("synthetic-edge-credential-reference")
        .expect("stage opaque credential reference");
    for (query, value) in [
        ("/config provider url", "http://127.0.0.1:43123/v1"),
        ("/config provider route responses", "responses"),
        ("/config provider route models", "/models"),
        ("/config provider dialects", "[\"responses\"]"),
        ("/config provider pricing", "unknown"),
        ("/config provider insecure-loopback", "true"),
    ] {
        editor
            .focus_from_query(&runtime, query, 0)
            .expect("focus provider field");
        editor.stage_raw(value).expect("stage provider field");
    }

    let snapshot = editor
        .provider_profile(&runtime)
        .expect("build Provider Profile snapshot from draft");
    assert_eq!(snapshot.profile(), "edge");
    assert_eq!(snapshot.template(), "openai-compatible");
    assert_eq!(snapshot.base_url(), Some("http://127.0.0.1:43123/v1"));
    assert_eq!(snapshot.models_route(), Some("/models"));
    assert_eq!(
        snapshot.endpoint(ProviderDialect::Responses).as_deref(),
        Some("http://127.0.0.1:43123/v1/responses")
    );
    assert!(snapshot.allow_insecure_loopback());
    assert!(
        !paths.project().exists(),
        "snapshot preview must not write config"
    );

    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("synthetic-edge-credential-reference"));
    assert!(!debug.contains("127.0.0.1"));
    assert_eq!(editor.object(), Some(&object));
}

#[test]
fn config_editor_create_checks_the_target_layer_instead_of_the_effective_union() {
    let temp = TempTree::new("create-layer-overlay");
    let paths = temp.paths();
    write(
        paths.user(),
        r#"
schema_version = 1

[providers.edge]
template = "openai-compatible"
"#,
    );
    let mut runtime =
        ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open config runtime");
    let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");

    assert!(matches!(
        ConfigEditorSession::create_object(&runtime, ConfigScope::BuiltIn, object.clone(),),
        Err(ConfigEditorError::Config(
            ConfigRuntimeError::ReadOnlyScope(ConfigScope::BuiltIn)
        ))
    ));

    let mut editor = ConfigEditorSession::create_object(&runtime, ConfigScope::Project, object)
        .expect("begin project-layer overlay");
    assert!(matches!(
        editor.field(&runtime).expect("overlay field").contents,
        ConfigFieldContents::Value {
            effective: Some(ConfigValue::String(ref effective)),
            source: Some(ConfigScope::User),
            target: None,
        } if effective == "openai-compatible"
    ));
    editor
        .stage_raw("openai-compatible")
        .expect("stage project-layer value");
    editor.commit(&mut runtime).expect("commit overlay");
    assert!(matches!(
        runtime
            .inspect_field(ConfigScope::Project, "providers.edge.template")
            .expect("inspect project overlay")
            .contents,
        ConfigFieldContents::Value {
            source: Some(ConfigScope::Project),
            target: Some(ConfigValue::String(ref target)),
            ..
        } if target == "openai-compatible"
    ));
    assert!(matches!(
        ConfigEditorSession::create_object(
            &runtime,
            ConfigScope::Project,
            ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge"),
        ),
        Err(ConfigEditorError::ConfigObjectAlreadyExists)
    ));
}

#[test]
fn config_editor_builds_a_multi_field_model_preset_without_losing_invalid_draft_state() {
    let temp = TempTree::new("create-model-preset");
    let paths = temp.paths();
    write(
        paths.project(),
        r#"
schema_version = 1

[providers.edge]
template = "openai-compatible"
"#,
    );
    let mut runtime =
        ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open config runtime");
    let object = ConfigObjectRef::new(ConfigObjectKind::ModelPreset, "fast");
    let mut editor =
        ConfigEditorSession::create_object(&runtime, ConfigScope::Project, object.clone())
            .expect("begin model preset creation");

    editor.stage_raw("edge").expect("stage provider");
    assert!(matches!(
        editor.preview(&mut runtime),
        Err(ConfigEditorError::Config(ConfigRuntimeError::InvalidValue { path, .. }))
            if path == "model_presets.fast.model"
    ));

    editor
        .focus_from_query(&runtime, "/config model model", 0)
        .expect("focus model field");
    editor.stage_raw("fixture-model").expect("stage model");
    editor
        .focus_from_query(&runtime, "/config model dialect", 0)
        .expect("focus dialect field");
    editor.stage_raw("responses").expect("stage dialect");

    let preview = editor
        .preview(&mut runtime)
        .expect("preview complete preset");
    assert_eq!(preview.changes.len(), 3);
    editor.commit(&mut runtime).expect("commit model preset");
    assert_eq!(
        runtime.model_presets().expect("model presets")[0].id,
        object.id()
    );
}

#[test]
fn config_editor_deletes_one_target_layer_object_atomically_with_backup() {
    let temp = TempTree::new("delete-provider-profile");
    let paths = temp.paths();
    write(
        paths.project(),
        r#"
schema_version = 1

[providers.edge]
template = "openai-compatible"
"#,
    );
    let mut runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open config runtime");
    let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
    let mut editor =
        ConfigEditorSession::delete_object(&runtime, ConfigScope::Project, object.clone())
            .expect("stage object deletion");

    assert_eq!(editor.operation(), ConfigEditorOperation::Delete);
    assert_eq!(editor.object(), Some(&object));
    assert!(matches!(
        editor.stage_raw("replacement"),
        Err(ConfigEditorError::ObjectDeletionStaged)
    ));
    let preview = editor.preview(&mut runtime).expect("preview deletion");
    assert_eq!(preview.changes.len(), 1);
    assert_eq!(preview.changes[0].path, "providers.edge.template");
    assert_eq!(preview.changes[0].after, None);

    editor.commit(&mut runtime).expect("commit deletion");
    assert!(
        runtime
            .addressable_objects()
            .expect("addressable objects")
            .is_empty()
    );
    assert!(backup_path(paths.project()).exists());
}

#[test]
fn config_editor_creates_and_deletes_a_usage_window_as_one_object() {
    let temp = TempTree::new("usage-window-lifecycle");
    let paths = temp.paths();
    let mut runtime =
        ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open config runtime");
    let object = ConfigObjectRef::new(ConfigObjectKind::UsageWindow, "work");
    let mut create =
        ConfigEditorSession::create_object(&runtime, ConfigScope::Project, object.clone())
            .expect("begin usage window creation");

    create.stage_raw("09:00").expect("stage start");
    for (query, value) in [
        ("/config stats-window end", "17:00"),
        (
            "/config stats-window days",
            "[\"mon\", \"tue\", \"wed\", \"thu\", \"fri\"]",
        ),
        ("/config stats-window timezone", "Asia/Hong_Kong"),
    ] {
        create
            .focus_from_query(&runtime, query, 0)
            .expect("focus usage window field");
        create.stage_raw(value).expect("stage usage window field");
    }
    assert_eq!(
        create
            .preview(&mut runtime)
            .expect("preview usage window")
            .changes
            .len(),
        4
    );
    create.commit(&mut runtime).expect("commit usage window");

    let delete = ConfigEditorSession::delete_object(&runtime, ConfigScope::Project, object.clone())
        .expect("stage usage window deletion");
    assert_eq!(
        delete
            .preview(&mut runtime)
            .expect("preview usage window deletion")
            .changes
            .len(),
        4
    );
    delete
        .commit(&mut runtime)
        .expect("commit usage window deletion");
    assert!(!runtime.addressable_objects().unwrap().contains(&object));
}

#[test]
fn config_editor_creates_and_deletes_a_price_schedule_as_one_object() {
    let temp = TempTree::new("price-schedule-lifecycle");
    let paths = temp.paths();
    write(
        paths.project(),
        r#"
schema_version = 1

[providers.openai-main]
template = "openai"
credential = "synthetic-openai-credential-reference"

[providers.openai-main.pricing]
source = "manual"
"#,
    );
    let mut runtime =
        ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open config runtime");
    let object = ConfigObjectRef::new(ConfigObjectKind::PriceSchedule, "openai-sol");
    let mut create =
        ConfigEditorSession::create_object(&runtime, ConfigScope::Project, object.clone())
            .expect("begin Price Schedule creation");

    create.stage_raw("2026-08-10.1").expect("stage version");
    for (query, value) in [
        ("/config pricing currency", "USD"),
        ("/config pricing provider", "openai-main"),
        ("/config pricing model", "gpt-5.6-sol"),
        ("/config pricing dialect", "responses"),
        ("/config pricing context-min", "0"),
        ("/config pricing effective-from", "2026-08-10T00:00:00Z"),
        ("/config pricing source", "manual"),
        ("/config pricing source-ref", "synthetic-manual-rate-card"),
        ("/config pricing rate-input", "1000000"),
        ("/config pricing rate-cached-input", "500000"),
        ("/config pricing rate-cache-write", "0"),
        ("/config pricing rate-output", "2000000"),
        ("/config pricing rate-reasoning", "3000000"),
    ] {
        create
            .focus_from_query(&runtime, query, 0)
            .expect("focus Price Schedule field");
        create.stage_raw(value).expect("stage Price Schedule field");
    }
    assert_eq!(
        create
            .preview(&mut runtime)
            .expect("preview Price Schedule")
            .changes
            .len(),
        14
    );
    create.commit(&mut runtime).expect("commit Price Schedule");
    assert_eq!(
        runtime
            .resolved_price_schedules()
            .expect("resolved Price Schedule")
            .schedules()[0]
            .id(),
        object.id()
    );

    let delete = ConfigEditorSession::delete_object(&runtime, ConfigScope::Project, object.clone())
        .expect("stage Price Schedule deletion");
    assert_eq!(
        delete
            .preview(&mut runtime)
            .expect("preview Price Schedule deletion")
            .changes
            .len(),
        14
    );
    delete
        .commit(&mut runtime)
        .expect("commit Price Schedule deletion");
    assert!(!runtime.addressable_objects().unwrap().contains(&object));
}

#[test]
fn config_object_deletion_is_reference_safe_and_target_layer_explicit() {
    let temp = TempTree::new("delete-reference-safety");
    let paths = temp.paths();
    write(
        paths.user(),
        r#"
schema_version = 1

[providers.inherited]
template = "openai-compatible"
"#,
    );
    write(
        paths.project(),
        r#"
schema_version = 1

[provider]
profile = "edge"

[providers.edge]
template = "openai-compatible"
"#,
    );
    let original = fs::read(paths.project()).expect("read original project config");
    let mut runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open config runtime");

    assert!(matches!(
        ConfigEditorSession::delete_object(
            &runtime,
            ConfigScope::Project,
            ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "inherited"),
        ),
        Err(ConfigEditorError::ConfigObjectNotInTargetScope)
    ));

    let deletion = ConfigEditorSession::delete_object(
        &runtime,
        ConfigScope::Project,
        ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge"),
    )
    .expect("stage selected provider deletion");
    assert!(matches!(
        deletion.preview(&mut runtime),
        Err(ConfigEditorError::Config(ConfigRuntimeError::InvalidValue { path, .. }))
            if path == "provider.profile"
    ));
    assert_eq!(
        fs::read(paths.project()).expect("read project config after failed preview"),
        original
    );
}

#[test]
fn draft_field_view_preserves_effective_provenance_and_staged_target() {
    let temp = TempTree::new("draft-field-view");
    let runtime =
        ConfigRuntime::open(temp.paths(), ConfigDocument::empty()).expect("open config runtime");
    let mut draft = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin user draft");
    draft
        .set_raw("provider.model", "staged-model")
        .expect("stage model");

    let field = runtime
        .inspect_draft_field(&draft, "provider.model")
        .expect("inspect staged model");
    assert_eq!(field.target_scope, ConfigScope::User);
    assert_eq!(
        field.contents,
        ConfigFieldContents::Value {
            effective: Some(value_string("deterministic-v1")),
            source: Some(ConfigScope::BuiltIn),
            target: Some(value_string("staged-model")),
        }
    );
    assert_eq!(
        runtime.validate_draft(&draft).expect("validate draft"),
        vec![greentyper_core::config::ConfigChange {
            path: "provider.model".into(),
            before: None,
            after: Some(value_string("staged-model")),
            credential_binding: None,
            timing: ConfigApplicationTiming::NextProviderEpoch,
        }]
    );
}

#[test]
fn precedence_provenance_and_dry_run_are_deterministic() {
    let temp = TempTree::new("precedence");
    let paths = temp.paths();
    let cli = ConfigDocument::parse(
        "schema_version = 1\n[provider]\nprofile = \"cli-profile\"\n[providers.cli-profile]\ntemplate = \"fixture\"\n",
    )
    .expect("CLI config");
    let mut runtime = ConfigRuntime::open(paths.clone(), cli).expect("open config runtime");

    let initial = runtime
        .get_effective("provider.profile")
        .expect("effective provider")
        .expect("provider exists");
    assert_eq!(initial.value, value_string("cli-profile"));
    assert_eq!(initial.source, ConfigScope::Cli);

    let mut dry_run = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin user draft");
    dry_run
        .set_raw("provider.profile", "user-profile")
        .expect("set user profile");
    let preview = runtime.commit(dry_run, true).expect("validate dry run");
    assert!(!preview.written);
    assert_eq!(preview.changes.len(), 1);
    assert_eq!(
        preview.changes[0].timing,
        ConfigApplicationTiming::NextProviderEpoch
    );
    assert!(!paths.user().exists());

    let mut user = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin user draft");
    user.set_raw("provider.profile", "user-profile")
        .expect("set user profile");
    runtime.commit(user, false).expect("commit user profile");

    let mut project = runtime
        .begin_draft(ConfigScope::Project)
        .expect("begin project draft");
    project
        .set_raw("provider.profile", "project-profile")
        .expect("set project profile");
    runtime
        .commit(project, false)
        .expect("commit project profile");

    let effective = runtime
        .get_effective("provider.profile")
        .expect("effective provider")
        .expect("provider exists");
    assert_eq!(effective.value, value_string("cli-profile"));
    assert_eq!(effective.source, ConfigScope::Cli);
    assert_eq!(
        runtime
            .config_layers()
            .expect("bootstrap projection")
            .resolve()
            .expect("resolve projected layers")
            .provider_profile()
            .value(),
        "cli-profile"
    );
}

#[test]
fn commit_revision_backup_and_restore_are_recoverable() {
    let temp = TempTree::new("backup");
    let paths = temp.paths();
    let mut runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open config runtime");

    let mut first = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin first draft");
    first
        .set_raw("provider.model", "first-model")
        .expect("set first model");
    let first_commit = runtime.commit(first, false).expect("first commit");
    assert!(first_commit.written);
    let first_bytes = fs::read(paths.user()).expect("read first config");
    assert!(!backup_path(paths.user()).exists());

    let mut second = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin second draft");
    second
        .set_raw("provider.model", "second-model")
        .expect("set second model");
    let second_commit = runtime.commit(second, false).expect("second commit");
    assert_ne!(first_commit.revision, second_commit.revision);
    assert_eq!(
        fs::read(backup_path(paths.user())).expect("read backup"),
        first_bytes
    );

    let restored = runtime
        .restore_backup(ConfigScope::User)
        .expect("restore backup");
    assert!(restored.written);
    assert_eq!(restored.revision, first_commit.revision);
    assert_eq!(
        runtime
            .get_effective("provider.model")
            .expect("effective model")
            .expect("model exists")
            .value,
        value_string("first-model")
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(paths.user())
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn stale_draft_loses_the_revision_race_without_mutation() {
    let temp = TempTree::new("revision-conflict");
    let paths = temp.paths();
    let mut first_runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open first runtime");
    let mut second_runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open second runtime");
    let mut first = first_runtime
        .begin_draft(ConfigScope::User)
        .expect("begin first draft");
    let mut second = second_runtime
        .begin_draft(ConfigScope::User)
        .expect("begin second draft");
    first
        .set_raw("provider.model", "winner")
        .expect("set winner");
    second
        .set_raw("provider.model", "loser")
        .expect("set loser");
    first_runtime.commit(first, false).expect("winning commit");
    let bytes_after_winner = fs::read(paths.user()).expect("read winner");

    assert!(matches!(
        second_runtime.commit(second, false),
        Err(ConfigRuntimeError::RevisionConflict { .. })
    ));
    assert_eq!(
        fs::read(paths.user()).expect("read after conflict"),
        bytes_after_winner
    );
}

#[test]
fn invalid_external_edit_retains_last_valid_state_and_can_be_repaired() {
    let temp = TempTree::new("last-valid");
    let paths = temp.paths();
    let mut runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open config runtime");
    let mut draft = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin valid draft");
    draft
        .set_raw("provider.model", "last-valid-model")
        .expect("set valid model");
    runtime.commit(draft, false).expect("commit valid config");

    write(
        paths.user(),
        "schema_version = 1\n[model_presets.broken]\nprovider = \"simulator\"\n",
    );
    let status = runtime.reload().expect("reload invalid external edit");
    assert!(!status.ready);
    assert_eq!(status.issues.len(), 1);
    assert_eq!(status.issues[0].scope, ConfigScope::User);
    assert_eq!(status.issues[0].category, ConfigErrorCategory::InvalidValue);
    assert_eq!(
        runtime
            .get_effective("provider.model")
            .expect("last-valid model")
            .expect("model exists")
            .value,
        value_string("last-valid-model")
    );

    let mut repair = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin repair draft");
    repair
        .reset("model_presets.broken.provider")
        .expect("remove broken preset");
    runtime.commit(repair, false).expect("commit repair");
    assert!(runtime.status().ready, "{:?}", runtime.status());

    write(
        paths.user(),
        "schema_version = 1\n[model_presets.broken]\nprovider = \"simulator\"\n",
    );
    let fresh = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open repair runtime");
    assert!(!fresh.status().ready);
    assert!(matches!(
        fresh.config_layers(),
        Err(ConfigRuntimeError::RepairRequired(_))
    ));
}

#[test]
fn fallback_dag_is_valid_but_a_true_cycle_enters_repair() {
    let temp = TempTree::new("fallback");
    let paths = temp.paths();
    write(
        paths.user(),
        r#"
schema_version = 1

[model_presets.a]
provider = "simulator"
model = "a"
dialect = "responses"
fallback = ["b", "c"]

[model_presets.b]
provider = "simulator"
model = "b"
dialect = "responses"
fallback = ["d"]

[model_presets.c]
provider = "simulator"
model = "c"
dialect = "responses"
fallback = ["d"]

[model_presets.d]
provider = "simulator"
model = "d"
dialect = "responses"
"#,
    );
    let mut runtime = ConfigRuntime::open(paths.clone(), ConfigDocument::empty())
        .expect("shared fallback is acyclic");
    assert!(runtime.status().ready);

    write(
        paths.user(),
        r#"
schema_version = 1

[model_presets.a]
provider = "simulator"
model = "a"
dialect = "responses"
fallback = ["b"]

[model_presets.b]
provider = "simulator"
model = "b"
dialect = "responses"
fallback = ["a"]
"#,
    );
    let status = runtime.reload().expect("reload cycle into repair");
    assert!(!status.ready);
    assert!(status.issues[0].detail.contains("cycle"));
}

#[test]
fn usage_windows_merge_by_id_across_layers() {
    let temp = TempTree::new("usage-window-merge");
    let paths = temp.paths();
    write(
        paths.user(),
        r#"
schema_version = 1

[[stats.windows]]
id = "business"
start = "09:00"
end = "17:00"
days = ["mon", "tue", "wed", "thu", "fri"]
timezone = "local"
"#,
    );
    write(
        paths.project(),
        r#"
schema_version = 1

[[stats.windows]]
id = "business"
timezone = "America/New_York"

[[stats.windows]]
id = "after-hours"
start = "17:00"
end = "23:00"
days = ["mon", "tue", "wed", "thu", "fri"]
timezone = "America/New_York"
"#,
    );
    let runtime = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("merge usage windows");
    assert!(runtime.status().ready, "{:?}", runtime.status());
    let business_start = runtime
        .get_effective("stats.windows.business.start")
        .expect("business start")
        .expect("business start exists");
    assert_eq!(business_start.value, value_string("09:00"));
    assert_eq!(business_start.source, ConfigScope::User);
    let business_timezone = runtime
        .get_effective("stats.windows.business.timezone")
        .expect("business timezone")
        .expect("business timezone exists");
    assert_eq!(business_timezone.value, value_string("America/New_York"));
    assert_eq!(business_timezone.source, ConfigScope::Project);
    assert!(
        runtime
            .get_effective("stats.windows.after-hours.start")
            .expect("after-hours start")
            .is_some()
    );
}

#[test]
fn provider_origin_validation_reports_the_exact_profile_path() {
    let temp = TempTree::new("origin-path");
    let paths = temp.paths();
    let mut runtime =
        ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open config runtime");
    let mut draft = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin provider draft");
    for (path, value) in [
        ("providers.edge.template", "openai-compatible"),
        (
            "providers.edge.credential",
            "synthetic-edge-credential-reference",
        ),
        ("providers.edge.base_url", "https://gateway.example.com/v1"),
        ("providers.edge.pricing.source", "unknown"),
    ] {
        draft.set_raw(path, value).expect("set provider field");
    }
    draft
        .set_raw("providers.edge.allow_insecure_loopback", "true")
        .expect("set loopback flag");
    assert!(matches!(
        runtime.commit(draft, true),
        Err(ConfigRuntimeError::InvalidValue { path, .. })
            if path == "providers.edge.allow_insecure_loopback"
    ));
}

#[test]
fn selected_provider_profile_is_typed_immutable_and_debug_redacted() {
    let temp = TempTree::new("provider-snapshot");
    let config = ConfigDocument::parse(
        r#"
schema_version = 1

[provider]
profile = "edge"
model = "fixture-model"

[providers.edge]
template = "openai-compatible"
credential = "synthetic-edge-credential-reference"
base_url = "https://gateway.example.com/v1/"
dialects = ["responses", "chat_completions"]

[providers.edge.routes]
responses = "responses"
chat_completions = "/chat/completions"

[providers.edge.pricing]
source = "unknown"
"#,
    )
    .expect("parse selected Provider profile");
    let runtime = ConfigRuntime::open(temp.paths(), config).expect("resolve selected profile");
    let snapshot = runtime
        .selected_provider_profile()
        .expect("resolve Provider snapshot")
        .expect("non-simulator snapshot");

    assert_eq!(snapshot.profile(), "edge");
    assert_eq!(snapshot.template(), "openai-compatible");
    assert_eq!(
        snapshot.credential_reference(),
        Some("synthetic-edge-credential-reference")
    );
    assert_eq!(snapshot.base_url(), Some("https://gateway.example.com/v1"));
    assert_eq!(
        snapshot.route(ProviderDialect::Responses),
        Some("/responses")
    );
    assert_eq!(
        snapshot.endpoint(ProviderDialect::Responses).as_deref(),
        Some("https://gateway.example.com/v1/responses")
    );
    assert!(snapshot.supports(ProviderDialect::ChatCompletions));
    assert_eq!(
        snapshot.pricing_source(),
        Some(ProviderPricingSource::Unknown)
    );
    assert!(!snapshot.allow_insecure_loopback());

    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("synthetic-edge-credential-reference"));
    assert!(!debug.contains("gateway.example.com"));

    let simulator = ConfigRuntime::open(temp.paths(), ConfigDocument::empty())
        .expect("resolve built-in simulator");
    assert_eq!(
        simulator
            .selected_provider_profile()
            .expect("resolve simulator profile"),
        None
    );
}

#[test]
fn official_provider_template_resolves_defaults_without_copying_authority() {
    let temp = TempTree::new("official-provider-template");
    let config = ConfigDocument::parse(
        r#"
schema_version = 1

[provider]
profile = "openai-main"
model = "gpt-5.6-sol"

[providers.openai-main]
template = "openai"
credential = "synthetic-openai-credential-reference"
"#,
    )
    .expect("parse official Provider profile");
    let runtime = ConfigRuntime::open(temp.paths(), config).expect("resolve official profile");
    let snapshot = runtime
        .selected_provider_profile()
        .expect("resolve Provider snapshot")
        .expect("non-simulator snapshot");

    assert_eq!(snapshot.template(), "openai");
    assert_eq!(snapshot.base_url(), Some("https://api.openai.com/v1"));
    assert_eq!(
        snapshot.endpoint(ProviderDialect::Responses).as_deref(),
        Some("https://api.openai.com/v1/responses")
    );
    assert_eq!(snapshot.models_route(), Some("/models"));
    assert!(snapshot.supports(ProviderDialect::Responses));
    assert!(snapshot.supports(ProviderDialect::ChatCompletions));
    assert!(!snapshot.supports(ProviderDialect::Messages));
    assert_eq!(
        snapshot.pricing_source(),
        Some(ProviderPricingSource::Template)
    );
    assert_eq!(
        snapshot.credential_reference(),
        Some("synthetic-openai-credential-reference")
    );

    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("synthetic-openai-credential-reference"));
    assert!(!debug.contains("api.openai.com"));
}

#[test]
fn release_catalog_models_bind_to_profiles_without_becoming_user_presets() {
    let temp = TempTree::new("release-catalog-models");
    let config = ConfigDocument::parse(
        r#"
schema_version = 1

[providers.openai-main]
template = "openai"
credential = "synthetic-openai-credential-reference"

[providers.manual-openai]
template = "openai"
credential = "synthetic-manual-credential-reference"

[providers.manual-openai.catalog]
mode = "manual"

[providers.discovery-openai]
template = "openai"
credential = "synthetic-discovery-credential-reference"

[providers.discovery-openai.catalog]
mode = "discovery"
"#,
    )
    .expect("parse catalog-backed profiles");
    let runtime = ConfigRuntime::open(temp.paths(), config).expect("resolve catalog profiles");

    let models = runtime.catalog_models().expect("catalog model candidates");
    assert_eq!(
        models
            .iter()
            .map(|model| (model.provider(), model.record().key()))
            .collect::<Vec<_>>(),
        vec![
            ("openai-main", "openai/gpt-5.6-luna"),
            ("openai-main", "openai/gpt-5.6-sol"),
            ("openai-main", "openai/gpt-5.6-terra"),
        ]
    );
    for model in &models {
        assert!(model.profile_compatible());
        assert_eq!(
            model.record().availability().value(),
            CatalogAvailability::Unverified
        );
        assert_eq!(
            model.record().model_id().provenance().source_kind(),
            CatalogSourceKind::ReleaseSeed
        );
    }
    assert!(runtime.model_presets().expect("user presets").is_empty());
    assert!(
        models
            .iter()
            .all(|model| model.provider() != "manual-openai")
    );
    assert!(
        models
            .iter()
            .all(|model| model.provider() != "discovery-openai")
    );
}

#[test]
fn custom_origin_inherits_template_routes_but_requires_an_available_rate_card() {
    let temp = TempTree::new("custom-origin-template-boundary");
    let paths = temp.paths();
    let runtime = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open Config Runtime");
    let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "gateway");
    let mut editor = ConfigEditorSession::create_object(&runtime, ConfigScope::Project, object)
        .expect("create gateway profile");
    editor.stage_raw("openai").expect("stage official template");
    editor
        .focus_from_query(&runtime, "/config provider credential", 0)
        .expect("focus credential");
    editor
        .stage_credential_reference("synthetic-gateway-credential-reference")
        .expect("stage credential binding");
    editor
        .focus_from_query(&runtime, "/config provider url", 0)
        .expect("focus custom origin");
    editor
        .stage_raw("https://gateway.example.com/v1")
        .expect("stage custom origin");

    assert!(matches!(
        editor.provider_profile(&runtime),
        Err(ConfigEditorError::Config(ConfigRuntimeError::InvalidValue { path, .. }))
            if path == "providers.gateway.pricing.source"
    ));

    editor
        .focus_from_query(&runtime, "/config provider pricing", 0)
        .expect("focus pricing decision");
    editor.stage_raw("unknown").expect("stage unknown pricing");
    let snapshot = editor
        .provider_profile(&runtime)
        .expect("resolve custom gateway snapshot");
    assert_eq!(snapshot.base_url(), Some("https://gateway.example.com/v1"));
    assert_eq!(
        snapshot.endpoint(ProviderDialect::Responses).as_deref(),
        Some("https://gateway.example.com/v1/responses")
    );
    assert!(snapshot.supports(ProviderDialect::ChatCompletions));
    assert_eq!(
        snapshot.pricing_source(),
        Some(ProviderPricingSource::Unknown)
    );
}

#[test]
fn deepseek_custom_origin_mirrors_official_rates_until_manual_pricing_overrides_them() {
    let mirrored = ConfigDocument::parse(
        r#"
schema_version = 1

[provider]
profile = "gateway"
model = "deepseek-v4-flash"

[providers.gateway]
template = "deepseek"
credential = "synthetic-gateway-credential-reference"
base_url = "https://gateway.example.com"
"#,
    )
    .expect("parse mirrored DeepSeek gateway");
    let runtime = ConfigRuntime::open(TempTree::new("deepseek-mirror").paths(), mirrored)
        .expect("resolve mirrored DeepSeek gateway");
    let snapshot = runtime
        .selected_provider_profile()
        .expect("resolve mirrored Profile")
        .expect("external Profile");
    assert_eq!(
        snapshot.pricing_source(),
        Some(ProviderPricingSource::TemplateMirror)
    );
    let schedules = runtime
        .resolved_price_schedules()
        .expect("resolve official mirror schedules");
    assert_eq!(schedules.schedules().len(), 2);
    let flash = schedules
        .schedules()
        .iter()
        .find(|schedule| schedule.model() == "deepseek-v4-flash")
        .expect("mirrored Flash schedule");
    assert_eq!(flash.provider_profile(), "gateway");
    assert_eq!(flash.source(), PriceScheduleSource::TemplateMirror);
    assert_eq!(flash.rates().input_micros_per_million(), 140_000);
    assert_eq!(flash.rates().cached_input_micros_per_million(), 2_800);
    assert_eq!(flash.rates().output_micros_per_million(), 280_000);

    let pro = schedules
        .schedules()
        .iter()
        .find(|schedule| schedule.model() == "deepseek-v4-pro")
        .expect("mirrored Pro schedule");
    assert_eq!(pro.provider_profile(), "gateway");
    assert_eq!(pro.source(), PriceScheduleSource::TemplateMirror);
    assert_eq!(pro.rates().input_micros_per_million(), 435_000);
    assert_eq!(pro.rates().cached_input_micros_per_million(), 3_625);
    assert_eq!(pro.rates().cache_write_micros_per_million(), 0);
    assert_eq!(pro.rates().output_micros_per_million(), 870_000);
    assert_eq!(pro.rates().reasoning_output_micros_per_million(), 870_000);

    let manual = ConfigDocument::parse(
        r#"
schema_version = 1

[provider]
profile = "gateway"
model = "deepseek-v4-flash"

[providers.gateway]
template = "deepseek"
credential = "synthetic-gateway-credential-reference"
base_url = "https://gateway.example.com"

[providers.gateway.pricing]
source = "manual"

[price_schedules.gateway-flash]
version = "custom-1"
currency = "USD"
provider = "gateway"
model = "deepseek-v4-flash"
minimum_context_tokens = 0
effective_from = "2026-08-10T00:00:00Z"
source = "manual"
source_ref = "synthetic-custom-rate-card"

[price_schedules.gateway-flash.rates]
input_micros_per_million = 1
cached_input_micros_per_million = 2
cache_write_micros_per_million = 3
output_micros_per_million = 4
reasoning_output_micros_per_million = 5
"#,
    )
    .expect("parse manually priced DeepSeek gateway");
    let runtime = ConfigRuntime::open(TempTree::new("deepseek-manual").paths(), manual)
        .expect("resolve manually priced DeepSeek gateway");
    let schedules = runtime
        .resolved_price_schedules()
        .expect("resolve manual schedules");
    assert_eq!(schedules.schedules().len(), 1);
    assert_eq!(
        schedules.schedules()[0].source(),
        PriceScheduleSource::Manual
    );
    assert_eq!(
        schedules.schedules()[0].rates().input_micros_per_million(),
        1
    );
}

#[test]
fn selected_non_simulator_profile_must_exist() {
    let temp = TempTree::new("missing-selected-provider");
    let config = ConfigDocument::parse(
        "schema_version = 1\n[provider]\nprofile = \"missing\"\nmodel = \"fixture-model\"\n",
    )
    .expect("parse missing selected profile");
    let runtime = ConfigRuntime::open(temp.paths(), config).expect("open repairable Config");
    assert!(!runtime.status().ready);
    assert!(matches!(
        runtime.selected_provider_profile(),
        Err(ConfigRuntimeError::RepairRequired(_))
    ));
}

#[test]
fn custom_statusline_requires_at_least_one_segment() {
    let temp = TempTree::new("statusline-segments");
    let paths = temp.paths();
    let mut runtime =
        ConfigRuntime::open(paths, ConfigDocument::empty()).expect("open config runtime");
    let mut draft = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin statusline draft");
    draft
        .set_raw("ui.statusline.preset", "custom")
        .expect("set custom preset");
    draft
        .set_raw("ui.statusline.custom.left", "[]")
        .expect("set empty custom segments");
    assert!(matches!(
        runtime.commit(draft, true),
        Err(ConfigRuntimeError::InvalidValue { path, .. })
            if path == "ui.statusline.custom"
    ));
}

#[cfg(unix)]
#[test]
fn symlink_config_and_backup_targets_are_rejected_without_following() {
    use std::os::unix::fs::symlink;

    let temp = TempTree::new("symlink");
    let paths = temp.paths();
    let outside = temp.root.join("outside.toml");
    write(&outside, "outside\n");
    fs::create_dir_all(paths.user().parent().expect("user parent")).expect("create user parent");
    symlink(&outside, paths.user()).expect("create config symlink");
    let runtime = ConfigRuntime::open(paths.clone(), ConfigDocument::empty())
        .expect("symlink enters repair instead of being followed");
    assert!(!runtime.status().ready);
    assert_eq!(
        runtime.status().issues[0].category,
        ConfigErrorCategory::InvalidValue
    );
    assert_eq!(
        fs::read_to_string(&outside).expect("read outside"),
        "outside\n"
    );

    fs::remove_file(paths.user()).expect("remove config symlink");
    let mut runtime =
        ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("open clean runtime");
    let mut first = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin first draft");
    first
        .set_raw("provider.model", "first")
        .expect("set first model");
    runtime.commit(first, false).expect("commit first model");
    symlink(&outside, backup_path(paths.user())).expect("create backup symlink");
    let target_before = fs::read(paths.user()).expect("read config before failed backup");
    let mut second = runtime
        .begin_draft(ConfigScope::User)
        .expect("begin second draft");
    second
        .set_raw("provider.model", "second")
        .expect("set second model");
    assert!(matches!(
        runtime.commit(second, false),
        Err(ConfigRuntimeError::SymlinkPath(_))
    ));
    assert_eq!(
        fs::read(paths.user()).expect("read config after failed backup"),
        target_before
    );
    assert_eq!(
        fs::read_to_string(&outside).expect("read outside"),
        "outside\n"
    );
}
