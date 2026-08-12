use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-config-cli-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create config CLI test directory");
        Self { root }
    }

    fn user_config(&self) -> PathBuf {
        #[cfg(windows)]
        {
            self.root.join("GreenTyper").join("config.toml")
        }
        #[cfg(target_os = "macos")]
        {
            self.root
                .join("Library")
                .join("Application Support")
                .join("GreenTyper")
                .join("config.toml")
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            self.root.join("greentyper").join("config.toml")
        }
    }

    fn project_config(&self) -> PathBuf {
        self.root.join(".greentyper").join("config.toml")
    }

    fn discovery_state(&self) -> PathBuf {
        self.root.join("state").join("provider-discovery.json")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_greentyper"));
        command
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("APPDATA", &self.root)
            .env("XDG_CONFIG_HOME", &self.root);
        command
    }

    fn config_command(&self, action: &str) -> Command {
        let mut command = self.command();
        command
            .args(["config", action])
            .arg("--user-config")
            .arg(self.user_config())
            .arg("--project-config")
            .arg(self.project_config());
        command
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove config CLI test directory");
    }
}

fn json(stdout: &[u8]) -> Value {
    serde_json::from_slice(stdout).expect("valid JSON output")
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".lock");
    PathBuf::from(value)
}

fn set_model(temp: &TempTree, value: &str) -> Value {
    let output = temp
        .config_command("set")
        .args(["provider.model", value, "--scope", "user"])
        .output()
        .expect("run config set");
    assert_success(&output);
    json(&output.stdout)
}

#[test]
fn config_cli_dry_run_commit_get_repair_and_headless_gate_are_wired() {
    let temp = TempTree::new();

    let schema = temp
        .command()
        .args(["config", "schema"])
        .output()
        .expect("run config schema");
    assert_success(&schema);
    let schema = json(&schema.stdout);
    assert_eq!(schema["schema_version"], 2);
    assert!(
        schema["entries"]
            .as_array()
            .is_some_and(|items| items.len() >= 30)
    );

    let dry_run = temp
        .config_command("set")
        .args([
            "provider.model",
            "preview-model",
            "--scope",
            "user",
            "--dry-run",
        ])
        .output()
        .expect("run config dry-run");
    assert_success(&dry_run);
    assert_eq!(json(&dry_run.stdout)["written"], false);
    assert!(!temp.user_config().exists());

    assert_eq!(set_model(&temp, "first-model")["written"], true);
    assert_eq!(set_model(&temp, "second-model")["written"], true);
    assert!(temp.user_config().exists());
    assert!(backup_path(&temp.user_config()).exists());

    let get = temp
        .config_command("get")
        .arg("provider.model")
        .output()
        .expect("run config get");
    assert_success(&get);
    let get = json(&get.stdout);
    assert_eq!(get["entry"]["source"], "user");
    assert_eq!(get["entry"]["value"]["type"], "string");
    assert_eq!(get["entry"]["value"]["value"], "second-model");

    fs::write(
        temp.user_config(),
        "schema_version = 1\n[model_presets.broken]\nprovider = \"simulator\"\n",
    )
    .expect("inject invalid external config");
    let ledger = temp.root.join("runtime.ledger");
    let blocked = temp
        .command()
        .args(["headless", "--ledger"])
        .arg(&ledger)
        .args(["--input", "must not be admitted"])
        .output()
        .expect("run config-blocked headless");
    assert!(!blocked.status.success(), "{blocked:?}");
    assert!(blocked.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("repair_required"),
        "{blocked:?}"
    );
    assert!(
        !ledger.exists(),
        "invalid config must fail before Ledger open"
    );

    let repaired = temp
        .config_command("repair")
        .args(["--scope", "user"])
        .output()
        .expect("run config repair");
    assert_success(&repaired);
    assert_eq!(json(&repaired.stdout)["written"], true);

    let restored = temp
        .config_command("get")
        .arg("provider.model")
        .output()
        .expect("get repaired model");
    assert_success(&restored);
    assert_eq!(
        json(&restored.stdout)["entry"]["value"]["value"],
        "first-model"
    );
}

#[test]
fn config_cli_sets_rejects_and_resets_the_project_default_model_preset() {
    let temp = TempTree::new();
    fs::create_dir_all(
        temp.project_config()
            .parent()
            .expect("project Config parent"),
    )
    .expect("create project Config parent");
    let before = br#"schema_version = 1

[model_presets.fast]
provider = "simulator"
model = "deterministic-v1"
dialect = "responses"
"#;
    fs::write(temp.project_config(), before).expect("write project Preset");

    let rejected = temp
        .config_command("set")
        .args([
            "agent.default_model_preset",
            "missing",
            "--scope",
            "project",
        ])
        .output()
        .expect("reject missing default Preset");
    assert!(!rejected.status.success(), "{rejected:?}");
    assert!(rejected.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("agent.default_model_preset"),
        "{rejected:?}"
    );
    assert_eq!(
        fs::read(temp.project_config()).expect("read rejected project Config"),
        before
    );

    let committed = temp
        .config_command("set")
        .args(["agent.default_model_preset", "fast", "--scope", "project"])
        .output()
        .expect("set project default Preset");
    assert_success(&committed);
    assert_eq!(json(&committed.stdout)["written"], true);

    let selected = temp
        .config_command("get")
        .arg("agent.default_model_preset")
        .output()
        .expect("read project default Preset");
    assert_success(&selected);
    let selected = json(&selected.stdout);
    assert_eq!(selected["entry"]["source"], "project");
    assert_eq!(selected["entry"]["value"]["value"], "fast");

    let reset = temp
        .config_command("reset")
        .args(["agent.default_model_preset", "--scope", "project"])
        .output()
        .expect("reset project default Preset");
    assert_success(&reset);
    assert_eq!(json(&reset.stdout)["written"], true);

    let cleared = temp
        .config_command("get")
        .arg("agent.default_model_preset")
        .output()
        .expect("read cleared project default Preset");
    assert_success(&cleared);
    assert!(json(&cleared.stdout)["entry"].is_null());

    let cleared_bytes = fs::read(temp.project_config()).expect("read cleared project Config");
    let repeated_reset = temp
        .config_command("reset")
        .args(["agent.default_model_preset", "--scope", "project"])
        .output()
        .expect("repeat project default Preset reset");
    assert_success(&repeated_reset);
    assert_eq!(json(&repeated_reset.stdout)["written"], false);
    assert_eq!(
        fs::read(temp.project_config()).expect("read repeated-reset project Config"),
        cleared_bytes
    );
    assert!(!temp.root.join("runtime.ledger").exists());
}

#[test]
fn config_catalog_emits_the_versioned_public_release_seed_only() {
    let temp = TempTree::new();
    let output = temp
        .command()
        .args(["config", "catalog"])
        .output()
        .expect("run config catalog");
    assert_success(&output);

    let catalog = json(&output.stdout);
    assert_eq!(catalog["schema_version"], 1);
    assert_eq!(catalog["seed_revision"], "2026-08-10.2");
    assert_eq!(catalog["templates"].as_array().map(Vec::len), Some(3));
    assert!(catalog["models"].as_array().is_some_and(|models| {
        models
            .iter()
            .any(|model| model["key"] == "openai/gpt-5.6-sol")
    }));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(temp.root.to_string_lossy().as_ref()));
    assert!(!stdout.contains("credential"));
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn config_discovery_status_is_read_only_and_fails_closed_on_corruption() {
    let temp = TempTree::new();
    let state = temp.discovery_state();

    let missing = temp
        .config_command("discovery")
        .arg("status")
        .arg("--discovery-state")
        .arg(&state)
        .output()
        .expect("inspect missing Provider discovery state");
    assert_success(&missing);
    assert_eq!(
        json(&missing.stdout),
        serde_json::json!({"schema_version": 1, "profiles": []})
    );
    assert!(missing.stderr.is_empty());
    assert!(!state.exists());
    assert!(!lock_path(&state).exists());

    fs::create_dir_all(state.parent().expect("discovery state parent"))
        .expect("create discovery state parent");
    let corrupt = b"not Provider discovery JSON\n";
    fs::write(&state, corrupt).expect("write corrupt discovery state");
    let rejected = temp
        .config_command("discovery")
        .arg("status")
        .arg("--discovery-state")
        .arg(&state)
        .output()
        .expect("inspect corrupt Provider discovery state");
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("Provider discovery state is corrupt")
    );
    assert_eq!(
        fs::read(&state).expect("read unchanged discovery state"),
        corrupt
    );
    assert!(!lock_path(&state).exists());
}

#[test]
fn config_discovery_refresh_failure_preserves_the_last_successful_snapshot() {
    let temp = TempTree::new();
    let state = temp.discovery_state();
    fs::create_dir_all(state.parent().expect("discovery state parent"))
        .expect("create discovery state parent");
    let before = br#"{"schema_version":1,"profiles":[{"profile":"edge","template":"openai","fingerprint":1,"observed_at_unix_ms":1,"models":[{"id":"last-known-model","release_catalog_key":null}]}]}"#;
    fs::write(&state, before).expect("write last successful discovery snapshot");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    let reference = format!("synthetic-never-bound-{}-{nonce}", std::process::id());
    fs::create_dir_all(
        temp.project_config()
            .parent()
            .expect("project config parent"),
    )
    .expect("create project config parent");
    fs::write(
        temp.project_config(),
        format!(
            r#"schema_version = 1

[providers.edge]
template = "openai"
credential = "{reference}"
base_url = "https://provider.invalid/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"
models = "/models"

[providers.edge.pricing]
source = "unknown"
"#
        ),
    )
    .expect("write discovery Provider Profile");

    let output = temp
        .config_command("discovery")
        .args(["refresh", "edge"])
        .arg("--discovery-state")
        .arg(&state)
        .output()
        .expect("refresh Provider discovery state");
    assert_success(&output);
    let status = json(&output.stdout);
    assert_eq!(status["state"], "failed");
    assert!(matches!(
        status["category"].as_str(),
        Some("credential_missing" | "credential_unavailable")
    ));
    assert_eq!(fs::read(&state).expect("read preserved snapshot"), before);
    assert!(!lock_path(&state).exists());
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!rendered.contains(&reference));
    assert!(!rendered.contains("provider.invalid"));
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn config_discovery_catalog_exposes_stale_observations_without_writing_state() {
    let temp = TempTree::new();
    let state = temp.discovery_state();
    fs::create_dir_all(state.parent().expect("discovery state parent"))
        .expect("create discovery state parent");
    let discovery = br#"{"schema_version":1,"profiles":[{"profile":"edge","template":"openai","fingerprint":1,"observed_at_unix_ms":1,"models":[{"id":"stale-model","release_catalog_key":null}]}]}"#;
    fs::write(&state, discovery).expect("write stale discovery state");
    fs::create_dir_all(
        temp.project_config()
            .parent()
            .expect("project config parent"),
    )
    .expect("create project config parent");
    let config = br#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-edge-reference"
base_url = "https://provider.invalid/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"
models = "/models"

[providers.edge.pricing]
source = "unknown"
"#;
    fs::write(temp.project_config(), config).expect("write discovery catalog Config");

    let output = temp
        .config_command("discovery")
        .args(["catalog", "edge"])
        .arg("--discovery-state")
        .arg(&state)
        .output()
        .expect("read merged Provider discovery catalog");
    assert_success(&output);
    let catalog = json(&output.stdout);
    assert_eq!(catalog["profile"], "edge");
    assert_eq!(catalog["freshness"], "stale");
    let stale = catalog["models"]
        .as_array()
        .and_then(|models| models.iter().find(|model| model["id"] == "stale-model"))
        .expect("stale discovered model remains visible");
    assert_eq!(stale["availability"], "stale");
    assert_eq!(stale["suggestion"], "refresh_required");
    assert_eq!(fs::read(&state).expect("read unchanged state"), discovery);
    assert_eq!(
        fs::read(temp.project_config()).expect("read unchanged Config"),
        config
    );
    assert!(!lock_path(&state).exists());
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn config_discovery_accept_rejects_stale_observations_without_writing_config() {
    let temp = TempTree::new();
    let state = temp.discovery_state();
    fs::create_dir_all(state.parent().expect("discovery state parent"))
        .expect("create discovery state parent");
    let discovery = br#"{"schema_version":1,"profiles":[{"profile":"edge","template":"openai","fingerprint":1,"observed_at_unix_ms":1,"models":[{"id":"stale-model","release_catalog_key":null}]}]}"#;
    fs::write(&state, discovery).expect("write stale discovery state");
    fs::create_dir_all(
        temp.project_config()
            .parent()
            .expect("project config parent"),
    )
    .expect("create project config parent");
    let config = br#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-edge-reference"
base_url = "https://provider.invalid/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"
models = "/models"

[providers.edge.pricing]
source = "unknown"
"#;
    fs::write(temp.project_config(), config).expect("write discovery acceptance Config");

    let output = temp
        .config_command("discovery")
        .args(["accept", "edge-stale", "edge", "stale-model"])
        .args(["--dialect", "responses", "--scope", "project"])
        .arg("--discovery-state")
        .arg(&state)
        .output()
        .expect("reject stale discovered model");
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("stale"));
    assert_eq!(fs::read(&state).expect("read unchanged state"), discovery);
    assert_eq!(
        fs::read(temp.project_config()).expect("read unchanged Config"),
        config
    );
    assert!(!lock_path(&state).exists());
    assert!(!lock_path(&temp.project_config()).exists());
    assert!(!temp.user_config().exists());
}

#[test]
fn config_cli_accepts_a_release_starter_with_preview_commit_and_reopen() {
    let temp = TempTree::new();
    fs::create_dir_all(temp.user_config().parent().expect("user Config parent"))
        .expect("create user Config parent");
    let before = b"schema_version = 1\n\n[providers.openai-main]\ntemplate = \"openai\"\ncredential = \"synthetic-starter-reference\"\n";
    fs::write(temp.user_config(), before).expect("write Provider profile");

    let preview = temp
        .config_command("accept-starter")
        .args([
            "frontier",
            "openai-main",
            "openai/gpt-5.6-sol",
            "--scope",
            "user",
            "--dry-run",
        ])
        .output()
        .expect("preview release starter");
    assert_success(&preview);
    assert_eq!(json(&preview.stdout)["written"], false);
    assert_eq!(
        fs::read(temp.user_config()).expect("read preview bytes"),
        before
    );
    assert!(!lock_path(&temp.user_config()).exists());
    assert!(!lock_path(&temp.project_config()).exists());

    let committed = temp
        .config_command("accept-starter")
        .args([
            "frontier",
            "openai-main",
            "openai/gpt-5.6-sol",
            "--scope",
            "user",
        ])
        .output()
        .expect("commit release starter");
    assert_success(&committed);
    let commit = json(&committed.stdout);
    assert_eq!(commit["written"], true);
    assert_eq!(commit["scope"], "user");
    assert_eq!(commit["changes"].as_array().map(Vec::len), Some(8));

    let reopened = temp
        .config_command("get")
        .arg("model_presets.frontier.model")
        .output()
        .expect("reopen accepted starter");
    assert_success(&reopened);
    assert_eq!(
        json(&reopened.stdout)["entry"]["value"]["value"],
        "gpt-5.6-sol"
    );
    let output = format!(
        "{}{}{}",
        String::from_utf8_lossy(&preview.stdout),
        String::from_utf8_lossy(&committed.stdout),
        String::from_utf8_lossy(&reopened.stdout)
    );
    assert!(!output.contains("synthetic-starter-reference"));
    assert!(preview.stderr.is_empty(), "{preview:?}");
    assert!(committed.stderr.is_empty(), "{committed:?}");
    assert!(reopened.stderr.is_empty(), "{reopened:?}");
}

#[test]
fn config_cli_updates_a_release_starter_only_after_preview_and_preserves_recovery() {
    let temp = TempTree::new();
    fs::create_dir_all(temp.user_config().parent().expect("user Config parent"))
        .expect("create user Config parent");
    let before = br#"schema_version = 2

[providers.openai-main]
template = "openai"
credential = "private-update-reference"
dialects = ["responses", "chat_completions"]

[model_presets.frontier]
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
favorite = true

[model_presets.frontier.starter]
catalog_key = "openai/gpt-5.6-sol"
seed_revision = "2026-08-10.1"
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
"#;
    fs::write(temp.user_config(), before).expect("write old starter");

    let preview = temp
        .config_command("update-starter")
        .args(["frontier", "--scope", "user", "--dry-run"])
        .output()
        .expect("preview starter update");
    assert_success(&preview);
    let preview_json = json(&preview.stdout);
    assert_eq!(preview_json["written"], false);
    assert_eq!(preview_json["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(fs::read(temp.user_config()).expect("preview bytes"), before);
    assert!(!lock_path(&temp.user_config()).exists());

    let committed = temp
        .config_command("update-starter")
        .args(["frontier", "--scope", "user"])
        .output()
        .expect("commit starter update");
    assert_success(&committed);
    assert_eq!(json(&committed.stdout)["written"], true);
    let winner = fs::read(temp.user_config()).expect("winner bytes");

    for (path, expected) in [
        ("model_presets.frontier.model", "gpt-5.6-sol"),
        ("model_presets.frontier.favorite", "true"),
        (
            "model_presets.frontier.starter.catalog_key",
            "openai/gpt-5.6-sol",
        ),
        (
            "model_presets.frontier.starter.seed_revision",
            "2026-08-10.2",
        ),
    ] {
        let get = temp
            .config_command("get")
            .arg(path)
            .output()
            .expect("get updated field");
        assert_success(&get);
        let value = &json(&get.stdout)["entry"]["value"]["value"];
        if expected == "true" {
            assert_eq!(value, true);
        } else {
            assert_eq!(value, expected);
        }
    }
    let repeated = temp
        .config_command("update-starter")
        .args(["frontier", "--scope", "user"])
        .output()
        .expect("reject repeated update");
    assert!(!repeated.status.success(), "{repeated:?}");
    assert!(repeated.stdout.is_empty());
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("already current"));
    assert_eq!(
        fs::read(temp.user_config()).expect("bytes after repeat"),
        winner
    );
    let all_output = format!(
        "{}{}{}",
        String::from_utf8_lossy(&preview.stdout),
        String::from_utf8_lossy(&committed.stdout),
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert!(!all_output.contains("private-update-reference"));
    assert!(!temp.root.join("runtime.ledger").exists());
}

#[test]
fn config_get_reports_only_credential_binding_policy_without_reference_readback() {
    let temp = TempTree::new();
    for (path, value) in [
        ("providers.edge.template", "openai-compatible"),
        (
            "providers.edge.credential",
            "synthetic-edge-credential-reference",
        ),
    ] {
        let output = temp
            .config_command("set")
            .args([path, value, "--scope", "user"])
            .output()
            .expect("set credential fixture field");
        assert_success(&output);
        if path.ends_with(".credential") {
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                !stdout.contains("synthetic-edge-credential-reference"),
                "{output:?}"
            );
            let change = &json(&output.stdout)["changes"][0];
            assert_eq!(change["before"], Value::Null);
            assert_eq!(change["after"], Value::Null);
            assert_eq!(change["credential_binding"]["before_bound"], false);
            assert_eq!(change["credential_binding"]["after_bound"], true);
        }
    }

    let get = temp
        .config_command("get")
        .arg("providers.edge.credential")
        .output()
        .expect("run credential config get");
    assert!(!get.status.success(), "{get:?}");
    assert!(get.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&get.stderr);
    assert!(stderr.contains("secret_read_forbidden"), "{get:?}");
    assert!(
        !stderr.contains("synthetic-edge-credential-reference"),
        "{get:?}"
    );
}

#[test]
fn config_test_provider_reports_a_redacted_pre_network_failure() {
    let temp = TempTree::new();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    let reference = format!("synthetic-never-bound-{}-{nonce}", std::process::id());
    fs::create_dir_all(
        temp.project_config()
            .parent()
            .expect("project config parent"),
    )
    .expect("create project config parent");
    fs::write(
        temp.project_config(),
        format!(
            r#"schema_version = 1

[provider]
profile = "edge"
model = "fixture-model"

[providers.edge]
template = "openai-compatible"
credential = "{reference}"
base_url = "https://provider.invalid/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"
models = "/models"

[providers.edge.pricing]
source = "unknown"
"#
        ),
    )
    .expect("write Provider test Config");

    let output = temp
        .config_command("test-provider")
        .output()
        .expect("run selected Provider connection test");
    assert_success(&output);
    let status = json(&output.stdout);
    assert_eq!(status["state"], "failed");
    assert!(matches!(
        status["category"].as_str(),
        Some("credential_missing" | "credential_unavailable")
    ));
    assert_eq!(status["retryable"].as_bool(), Some(cfg!(not(windows))));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(&reference));
    assert!(!stderr.contains(&reference));
    assert!(!stdout.contains("provider.invalid"));
    assert!(stderr.is_empty(), "{output:?}");
}

fn backup_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".bak");
    PathBuf::from(value)
}
