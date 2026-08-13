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

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git is installed for Unix workspace tests");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_repo() -> (PathBuf, PathBuf) {
    let root = temp_root();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "test@example.invalid"]);
    git(&root, &["config", "user.name", "GreenTyper Test"]);
    fs::write(root.join("tracked.txt"), b"base\n").expect("base file");
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-qm", "base"]);
    let parent = root.parent().expect("temp root parent").to_path_buf();
    (root, parent)
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

#[test]
fn workspace_cli_allocates_isolated_git_worktrees() {
    let (root, parent) = git_repo();
    let left = parent.join(format!(
        "{}-left",
        root.file_name().unwrap().to_string_lossy()
    ));
    let right = parent.join(format!(
        "{}-right",
        root.file_name().unwrap().to_string_lossy()
    ));

    for (branch, path) in [("agent-left", &left), ("agent-right", &right)] {
        let output = Command::new(env!("CARGO_BIN_EXE_greentyper"))
            .args(["workspace", "allocate", "--root"])
            .arg(&root)
            .args(["--worktree"])
            .arg(path)
            .args(["--branch", branch])
            .output()
            .expect("allocate worktree");
        assert!(output.status.success(), "{output:?}");
        let json: Value = serde_json::from_slice(&output.stdout).expect("allocation JSON");
        assert_eq!(json["status"], "created");
        assert_eq!(json["branch"], branch);
        assert_eq!(json["base_commit"], json["head_commit"]);
        assert!(path.is_dir());
    }

    fs::write(left.join("left-only.txt"), b"left\n").expect("left edit");
    assert!(!right.join("left-only.txt").exists());
    git(&left, &["status", "--porcelain"]);

    fs::remove_dir_all(&left).expect("remove left worktree");
    fs::remove_dir_all(&right).expect("remove right worktree");
    git(&root, &["worktree", "prune"]);
    fs::remove_dir_all(&root).expect("remove git repo");
}

#[test]
fn workspace_cli_reports_mergeable_and_conflicting_heads() {
    let (root, parent) = git_repo();
    let clean = parent.join(format!(
        "{}-clean",
        root.file_name().unwrap().to_string_lossy()
    ));
    let conflict = parent.join(format!(
        "{}-conflict",
        root.file_name().unwrap().to_string_lossy()
    ));

    for (branch, path) in [("agent-clean", &clean), ("agent-conflict", &conflict)] {
        let output = Command::new(env!("CARGO_BIN_EXE_greentyper"))
            .args(["workspace", "allocate", "--root"])
            .arg(&root)
            .args(["--worktree"])
            .arg(path)
            .args(["--branch", branch])
            .output()
            .expect("allocate worktree");
        assert!(output.status.success(), "{output:?}");
    }

    fs::write(clean.join("clean.txt"), b"clean\n").expect("clean edit");
    git(&clean, &["add", "clean.txt"]);
    git(&clean, &["commit", "-qm", "clean change"]);
    let clean_check = Command::new(env!("CARGO_BIN_EXE_greentyper"))
        .args(["workspace", "merge-check", "--root"])
        .arg(&root)
        .args(["--target", "main", "--source", "agent-clean"])
        .output()
        .expect("clean merge check");
    assert!(clean_check.status.success(), "{clean_check:?}");
    let clean_json: Value = serde_json::from_slice(&clean_check.stdout).expect("clean JSON");
    assert_eq!(clean_json["status"], "mergeable");
    assert_eq!(clean_json["conflict_paths"], serde_json::json!([]));

    fs::write(root.join("tracked.txt"), b"target\n").expect("target edit");
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-qm", "target change"]);
    fs::write(conflict.join("tracked.txt"), b"source\n").expect("source edit");
    git(&conflict, &["add", "tracked.txt"]);
    git(&conflict, &["commit", "-qm", "source change"]);
    let conflict_check = Command::new(env!("CARGO_BIN_EXE_greentyper"))
        .args(["workspace", "merge-check", "--root"])
        .arg(&root)
        .args(["--target", "main", "--source", "agent-conflict"])
        .output()
        .expect("conflict merge check");
    assert!(conflict_check.status.success(), "{conflict_check:?}");
    let conflict_json: Value =
        serde_json::from_slice(&conflict_check.stdout).expect("conflict JSON");
    assert_eq!(conflict_json["status"], "conflict");
    assert_eq!(
        conflict_json["conflict_paths"],
        serde_json::json!(["tracked.txt"])
    );
    assert_eq!(
        fs::read_to_string(root.join("tracked.txt")).expect("target bytes"),
        "target\n"
    );

    fs::remove_dir_all(&clean).expect("remove clean worktree");
    fs::remove_dir_all(&conflict).expect("remove conflict worktree");
    git(&root, &["worktree", "prune"]);
    fs::remove_dir_all(&root).expect("remove git repo");
}
