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

fn backup_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".bak");
    PathBuf::from(value)
}
