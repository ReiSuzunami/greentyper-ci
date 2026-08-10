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
    assert_eq!(schema["schema_version"], 1);
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
