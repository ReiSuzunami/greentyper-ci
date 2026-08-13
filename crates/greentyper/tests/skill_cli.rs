use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-skill-{label}-{}-{nanos}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn binary(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_greentyper"));
    command
        .env("HOME", project)
        .env("APPDATA", project)
        .env("XDG_CONFIG_HOME", project);
    command
}

fn manifest(root: &Path) {
    let path = root.join(".greentyper").join("skills").join("echo");
    fs::create_dir_all(&path).expect("create Skill directory");
    fs::write(
        path.join("skill.toml"),
        "id = \"echo\"\nname = \"Echo\"\ndescription = \"bounded echo\"\ntool = \"local.echo\"\nmessage = \"skill hello\"\n",
    )
    .expect("write Skill manifest");
}

#[test]
fn skill_list_reports_project_manifest_hash_without_private_path() {
    let root = temp_root("list");
    manifest(&root);
    let output = binary(&root)
        .args(["skill", "list", "--project"])
        .arg(&root)
        .output()
        .expect("run Skill list");
    assert!(output.status.success(), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("Skill JSON");
    assert_eq!(json["skills"][0]["id"], "echo");
    assert_eq!(json["skills"][0]["tool"], "local.echo");
    assert_eq!(json["skills"][0]["source"], "project");
    assert_eq!(
        json["skills"][0]["content_sha256"].as_str().unwrap().len(),
        64
    );
    fs::remove_dir_all(root).expect("cleanup Skill root");
}

#[test]
fn skill_run_records_local_echo_and_reuses_durable_call() {
    let root = temp_root("run");
    manifest(&root);
    let ledger = root.join("runtime.ledger");
    let first = binary(&root)
        .args(["skill", "run", "--project"])
        .arg(&root)
        .args(["--ledger"])
        .arg(&ledger)
        .args(["--id", "echo", "--approve"])
        .output()
        .expect("run Skill");
    assert!(first.status.success(), "{first:?}");
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).expect("run JSON");
    assert_eq!(first_json["status"], "succeeded");
    assert_eq!(first_json["output"], "skill hello");
    assert_eq!(first_json["reused"], false);

    let second = binary(&root)
        .args(["skill", "run", "--project"])
        .arg(&root)
        .args(["--ledger"])
        .arg(&ledger)
        .args(["--id", "echo", "--approve"])
        .output()
        .expect("rerun Skill");
    assert!(second.status.success(), "{second:?}");
    let second_json: serde_json::Value =
        serde_json::from_slice(&second.stdout).expect("rerun JSON");
    assert_eq!(second_json["status"], "succeeded");
    assert_eq!(second_json["reused"], true);
    fs::remove_dir_all(root).expect("cleanup Skill root");
}

#[test]
fn skill_run_requires_explicit_approval_before_creating_state() {
    let root = temp_root("approval");
    manifest(&root);
    let ledger = root.join("runtime.ledger");
    let output = binary(&root)
        .args(["skill", "run", "--project"])
        .arg(&root)
        .args(["--ledger"])
        .arg(&ledger)
        .args(["--id", "echo"])
        .output()
        .expect("run Skill without approval");
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        !ledger.exists(),
        "Skill approval failure must not create Runtime state"
    );
    assert!(!ledger.with_extension("ledger.team").exists());
    fs::remove_dir_all(root).expect("cleanup Skill root");
}
