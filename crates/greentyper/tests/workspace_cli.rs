#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "greentyper-workspace-cli-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).expect("temp workspace");
    root
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_greentyper"));
    command.args(["workspace", "capture", "--root"]);
    command.arg(root);
    command.args(["--path", "tracked.txt"]);
    command
}

#[cfg(unix)]
#[test]
fn workspace_cli_captures_validates_and_rejects_a_stale_read_set() {
    let root = temp_root();
    let tracked = root.join("tracked.txt");
    let read_set_path = root.with_extension("read-set.json");
    fs::write(&tracked, b"before").expect("write tracked file");

    let capture = command(&root).output().expect("capture workspace read set");
    assert!(capture.status.success(), "{capture:?}");
    assert!(capture.stderr.is_empty(), "{capture:?}");
    assert!(!String::from_utf8_lossy(&capture.stdout).contains(root.to_string_lossy().as_ref()));
    fs::write(&read_set_path, &capture.stdout).expect("persist read set fixture");

    let validate = Command::new(env!("CARGO_BIN_EXE_greentyper"))
        .args(["workspace", "validate", "--root"])
        .arg(&root)
        .args(["--read-set"])
        .arg(&read_set_path)
        .output()
        .expect("validate fresh read set");
    assert!(validate.status.success(), "{validate:?}");
    let json: Value = serde_json::from_slice(&validate.stdout).expect("validation JSON");
    assert_eq!(json["valid"], true);
    assert_eq!(fs::read(&tracked).expect("tracked bytes"), b"before");

    let replacement_path = root.with_extension("replacement");
    fs::write(&replacement_path, b"applied").expect("write replacement fixture");
    let apply = Command::new(env!("CARGO_BIN_EXE_greentyper"))
        .args(["workspace", "apply", "--root"])
        .arg(&root)
        .args(["--read-set"])
        .arg(&read_set_path)
        .args(["--path", "tracked.txt", "--input"])
        .arg(&replacement_path)
        .output()
        .expect("apply fresh read set");
    assert!(apply.status.success(), "{apply:?}");
    let applied: Value = serde_json::from_slice(&apply.stdout).expect("write JSON");
    assert_eq!(applied["path"], "tracked.txt");
    assert_eq!(applied["bytes"], 7);
    assert_eq!(fs::read(&tracked).expect("applied bytes"), b"applied");

    fs::write(&tracked, b"after").expect("mutate tracked file");
    let stale = Command::new(env!("CARGO_BIN_EXE_greentyper"))
        .args(["workspace", "validate", "--root"])
        .arg(&root)
        .args(["--read-set"])
        .arg(&read_set_path)
        .output()
        .expect("reject stale read set");
    assert!(!stale.status.success(), "{stale:?}");
    assert!(stale.stdout.is_empty(), "{stale:?}");
    assert!(String::from_utf8_lossy(&stale.stderr).contains("read-set is stale"));

    let stale_apply = Command::new(env!("CARGO_BIN_EXE_greentyper"))
        .args(["workspace", "apply", "--root"])
        .arg(&root)
        .args(["--read-set"])
        .arg(&read_set_path)
        .args(["--path", "tracked.txt", "--input"])
        .arg(&replacement_path)
        .output()
        .expect("reject stale apply");
    assert!(!stale_apply.status.success(), "{stale_apply:?}");
    assert!(stale_apply.stdout.is_empty(), "{stale_apply:?}");
    assert!(String::from_utf8_lossy(&stale_apply.stderr).contains("read-set is stale"));
    assert_eq!(fs::read(&tracked).expect("stale bytes"), b"after");

    fs::remove_file(read_set_path).expect("cleanup read set");
    fs::remove_file(replacement_path).expect("cleanup replacement");
    fs::remove_dir_all(root).expect("cleanup workspace");
}
