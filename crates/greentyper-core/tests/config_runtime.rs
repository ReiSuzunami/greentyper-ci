use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::config::{
    ConfigApplicationTiming, ConfigDocument, ConfigErrorCategory, ConfigFieldContents,
    ConfigObjectKind, ConfigObjectRef, ConfigPaths, ConfigRuntime, ConfigRuntimeError, ConfigScope,
    ConfigValue, ConfigValueKind, config_schema, parse_config_value,
};
use greentyper_core::provider::{ProviderDialect, ProviderPricingSource};

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
    assert!(!presets[0].favorite);
    assert!(presets[0].fallback.is_empty());
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
