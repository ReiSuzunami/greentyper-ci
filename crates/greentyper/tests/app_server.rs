use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
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
            "greentyper-app-server-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create App Server test directory");
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

    fn run(&self, requests: &[u8]) -> std::process::Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_greentyper"))
            .args(["app-server", "--stdio"])
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("APPDATA", &self.root)
            .env("XDG_CONFIG_HOME", &self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn App Server");
        child
            .stdin
            .take()
            .expect("App Server stdin")
            .write_all(requests)
            .expect("write App Server requests");
        child.wait_with_output().expect("wait for App Server")
    }

    fn spawn(&self) -> AppServerProcess {
        let mut child = Command::new(env!("CARGO_BIN_EXE_greentyper"))
            .args(["app-server", "--stdio"])
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("APPDATA", &self.root)
            .env("XDG_CONFIG_HOME", &self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn interactive App Server");
        let stdin = child.stdin.take().expect("interactive App Server stdin");
        let stdout = child.stdout.take().expect("interactive App Server stdout");
        let stderr = child.stderr.take().expect("interactive App Server stderr");
        AppServerProcess {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr: Some(stderr),
            finished: false,
        }
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove App Server test directory");
    }
}

struct AppServerProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Option<ChildStderr>,
    finished: bool,
}

impl AppServerProcess {
    fn request(&mut self, request: &str) -> Value {
        let stdin = self.stdin.as_mut().expect("active App Server stdin");
        stdin
            .write_all(request.as_bytes())
            .expect("write App Server request");
        stdin
            .write_all(b"\n")
            .expect("terminate App Server request");
        stdin.flush().expect("flush App Server request");
        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .expect("read App Server response");
        assert!(!response.is_empty(), "App Server closed before responding");
        serde_json::from_str(&response).expect("JSON App Server response")
    }

    fn finish(mut self) {
        self.stdin.take();
        let status = self.child.wait().expect("wait for interactive App Server");
        let mut trailing = String::new();
        self.stdout
            .read_to_string(&mut trailing)
            .expect("read trailing App Server output");
        let mut stderr = String::new();
        self.stderr
            .take()
            .expect("interactive App Server stderr")
            .read_to_string(&mut stderr)
            .expect("read App Server stderr");
        assert!(status.success(), "status={status:?}, stderr={stderr}");
        assert!(
            trailing.is_empty(),
            "unexpected trailing output: {trailing}"
        );
        assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
        self.finished = true;
    }
}

impl Drop for AppServerProcess {
    fn drop(&mut self) {
        if !self.finished {
            self.stdin.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn responses(output: &std::process::Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout.clone())
        .expect("UTF-8 App Server output")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON App Server response"))
        .collect()
}

#[test]
fn app_server_schema_get_and_bounded_errors_are_streamed_without_writes() {
    let temp = TempTree::new();
    let mut requests = Vec::new();
    requests.extend_from_slice(b"{\"id\":1,\"operation\":\"config.schema\"}\n");
    requests.extend_from_slice(
        b"{\"id\":2,\"operation\":\"config.get\",\"params\":{\"path\":\"provider.model\"}}\n",
    );
    requests.extend_from_slice(
        b"{\"id\":3,\"operation\":\"config.get\",\"params\":{\"path\":\"providers.deepseek.credential\"}}\n",
    );
    requests.extend_from_slice(b"{\"id\":4,\"operation\":\"config.future\"}\n");
    requests.extend_from_slice(b"{\n");
    requests.extend(std::iter::repeat_n(b'x', 64 * 1024 + 1));
    requests.push(b'\n');
    requests.extend_from_slice(b"{\"id\":5,\"operation\":\"config.schema\",\"params\":{}}\n");

    let output = temp.run(&requests);
    let responses = responses(&output);
    assert_eq!(responses.len(), 7);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["schema_version"], 1);
    assert!(
        responses[0]["result"]["entries"]
            .as_array()
            .expect("schema entries")
            .len()
            >= 30
    );
    let credential = responses[0]["result"]["entries"]
        .as_array()
        .expect("schema entries")
        .iter()
        .find(|entry| entry["path_pattern"] == "providers.<id>.credential")
        .expect("credential schema entry");
    assert_eq!(credential["value_kind"], "string");
    assert_eq!(credential["credential_reference"], true);
    assert_eq!(credential["editor"], "credential_binding");
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[1]["result"]["path"], "provider.model");
    assert_eq!(responses[1]["result"]["entry"]["source"], "built_in");
    assert_eq!(responses[1]["result"]["status"]["ready"], true);
    assert_eq!(responses[2]["id"], 3);
    assert_eq!(responses[2]["error"]["category"], "secret_read_forbidden");
    assert_eq!(responses[3]["id"], 4);
    assert_eq!(responses[3]["error"]["category"], "unknown_operation");
    assert_eq!(responses[4]["id"], Value::Null);
    assert_eq!(responses[4]["error"]["category"], "invalid_request");
    assert_eq!(responses[5]["id"], Value::Null);
    assert_eq!(responses[5]["error"]["category"], "request_too_large");
    assert_eq!(responses[6]["id"], 5);
    assert_eq!(responses[6]["result"]["schema_version"], 1);
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_stages_and_resets_typed_drafts_without_changing_effective_config() {
    let temp = TempTree::new();
    let requests = concat!(
        "{\"id\":1,\"operation\":\"config.draft.begin\",\"params\":{\"scope\":\"user\"}}\n",
        "{\"id\":2,\"operation\":\"config.get\",\"params\":{\"path\":\"provider.model\"}}\n",
        "{\"id\":3,\"operation\":\"config.draft.set\",\"params\":{\"draft_id\":1,\"path\":\"provider.model\",\"value\":{\"type\":\"boolean\",\"value\":true}}}\n",
        "{\"id\":4,\"operation\":\"config.draft.set\",\"params\":{\"draft_id\":1,\"path\":\"provider.model\",\"value\":{\"type\":\"string\",\"value\":\"staged-model\"}}}\n",
        "{\"id\":5,\"operation\":\"config.get\",\"params\":{\"path\":\"provider.model\"}}\n",
        "{\"id\":6,\"operation\":\"config.draft.reset\",\"params\":{\"draft_id\":1,\"path\":\"provider.model\"}}\n",
        "{\"id\":7,\"operation\":\"config.get\",\"params\":{\"path\":\"provider.model\"}}\n",
        "{\"id\":8,\"operation\":\"config.draft.begin\",\"params\":{\"scope\":\"built_in\"}}\n",
        "{\"id\":9,\"operation\":\"config.draft.reset\",\"params\":{\"draft_id\":99,\"path\":\"provider.model\"}}\n",
    );

    let output = temp.run(requests.as_bytes());
    let responses = responses(&output);
    assert_eq!(responses.len(), 9);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["draft_id"], 1);
    assert_eq!(responses[0]["result"]["scope"], "user");
    assert_eq!(
        responses[0]["result"]["base_revision"]
            .as_str()
            .expect("base revision")
            .len(),
        64
    );
    let effective_before = responses[1]["result"]["entry"].clone();
    assert_eq!(responses[2]["error"]["category"], "wrong_type");
    assert_eq!(responses[3]["result"]["staged"], true);
    assert_eq!(responses[4]["result"]["entry"], effective_before);
    assert_eq!(responses[5]["result"]["staged"], true);
    assert_eq!(responses[6]["result"]["entry"], effective_before);
    assert_eq!(responses[7]["error"]["category"], "read_only_scope");
    assert_eq!(responses[8]["error"]["category"], "unknown_draft");
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_validation_keeps_an_invalid_draft_live_for_repair() {
    let temp = TempTree::new();
    let requests = concat!(
        "{\"id\":1,\"operation\":\"config.draft.begin\",\"params\":{\"scope\":\"user\"}}\n",
        "{\"id\":2,\"operation\":\"config.draft.set\",\"params\":{\"draft_id\":1,\"path\":\"model_presets.broken.provider\",\"value\":{\"type\":\"string\",\"value\":\"simulator\"}}}\n",
        "{\"id\":3,\"operation\":\"config.draft.validate\",\"params\":{\"draft_id\":1}}\n",
        "{\"id\":4,\"operation\":\"config.draft.reset\",\"params\":{\"draft_id\":1,\"path\":\"model_presets.broken.provider\"}}\n",
        "{\"id\":5,\"operation\":\"config.draft.validate\",\"params\":{\"draft_id\":1}}\n",
        "{\"id\":6,\"operation\":\"config.draft.set\",\"params\":{\"draft_id\":1,\"path\":\"provider.model\",\"value\":{\"type\":\"string\",\"value\":\"validated-model\"}}}\n",
        "{\"id\":7,\"operation\":\"config.draft.validate\",\"params\":{\"draft_id\":1}}\n",
        "{\"id\":8,\"operation\":\"config.get\",\"params\":{\"path\":\"provider.model\"}}\n",
    );

    let output = temp.run(requests.as_bytes());
    let responses = responses(&output);
    assert_eq!(responses.len(), 8);
    assert_eq!(responses[1]["result"]["staged"], true);
    assert_eq!(responses[2]["error"]["category"], "invalid_value");
    assert_eq!(responses[2]["error"]["path"], "model_presets.broken.model");
    assert_eq!(responses[3]["result"]["staged"], true);
    assert_eq!(
        responses[4]["result"]["changes"]
            .as_array()
            .expect("empty normalized diff")
            .len(),
        0
    );
    let changes = responses[6]["result"]["changes"]
        .as_array()
        .expect("normalized diff");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["path"], "provider.model");
    assert_eq!(changes[0]["after"]["type"], "string");
    assert_eq!(changes[0]["after"]["value"], "validated-model");
    assert_eq!(changes[0]["timing"], "next_provider_epoch");
    assert_ne!(
        responses[7]["result"]["entry"]["value"]["value"],
        "validated-model"
    );
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_commit_updates_effective_config_and_survives_reopen() {
    let temp = TempTree::new();
    let requests = concat!(
        "{\"id\":1,\"operation\":\"config.draft.begin\",\"params\":{\"scope\":\"user\"}}\n",
        "{\"id\":2,\"operation\":\"config.draft.set\",\"params\":{\"draft_id\":1,\"path\":\"provider.model\",\"value\":{\"type\":\"string\",\"value\":\"committed-model\"}}}\n",
        "{\"id\":3,\"operation\":\"config.draft.validate\",\"params\":{\"draft_id\":1}}\n",
        "{\"id\":4,\"operation\":\"config.draft.commit\",\"params\":{\"draft_id\":1}}\n",
        "{\"id\":5,\"operation\":\"config.get\",\"params\":{\"path\":\"provider.model\"}}\n",
        "{\"id\":6,\"operation\":\"config.draft.commit\",\"params\":{\"draft_id\":1}}\n",
    );

    let output = temp.run(requests.as_bytes());
    let results = responses(&output);
    assert_eq!(results.len(), 6);
    let preview = results[2]["result"]["changes"]
        .as_array()
        .expect("preview changes");
    assert_eq!(preview.len(), 1);
    let commit = &results[3]["result"];
    assert_eq!(commit["draft_id"], 1);
    assert_eq!(commit["scope"], "user");
    assert_eq!(commit["written"], true);
    assert_eq!(commit["changes"], results[2]["result"]["changes"]);
    assert_eq!(
        commit["revision"].as_str().expect("commit revision").len(),
        64
    );
    assert_ne!(commit["revision"], commit["base_revision"]);
    assert_eq!(
        results[4]["result"]["entry"]["value"]["value"],
        "committed-model"
    );
    assert_eq!(results[4]["result"]["entry"]["source"], "user");
    assert_eq!(results[5]["error"]["category"], "unknown_draft");
    assert!(temp.user_config().exists());
    assert!(!temp.project_config().exists());

    let reopened = temp
        .run(b"{\"id\":7,\"operation\":\"config.get\",\"params\":{\"path\":\"provider.model\"}}\n");
    let reopened = responses(&reopened);
    assert_eq!(
        reopened[0]["result"]["entry"]["value"]["value"],
        "committed-model"
    );
    assert_eq!(reopened[0]["result"]["entry"]["source"], "user");
}

#[test]
fn app_server_no_change_commit_consumes_the_draft_without_creating_config() {
    let temp = TempTree::new();
    let output = temp.run(
        concat!(
            "{\"id\":1,\"operation\":\"config.draft.begin\",\"params\":{\"scope\":\"user\"}}\n",
            "{\"id\":2,\"operation\":\"config.draft.commit\",\"params\":{\"draft_id\":1}}\n",
            "{\"id\":3,\"operation\":\"config.draft.commit\",\"params\":{\"draft_id\":1}}\n",
        )
        .as_bytes(),
    );
    let results = responses(&output);
    assert_eq!(results[1]["result"]["written"], false);
    assert_eq!(
        results[1]["result"]["changes"]
            .as_array()
            .expect("no-change commit diff")
            .len(),
        0
    );
    assert_eq!(results[2]["error"]["category"], "unknown_draft");
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_revision_conflict_retains_the_stale_draft_without_overwriting_winner() {
    let temp = TempTree::new();
    let mut winner = temp.spawn();
    let mut loser = temp.spawn();

    let winner_begin =
        winner.request(r#"{"id":1,"operation":"config.draft.begin","params":{"scope":"user"}}"#);
    let loser_begin =
        loser.request(r#"{"id":1,"operation":"config.draft.begin","params":{"scope":"user"}}"#);
    assert_eq!(
        winner_begin["result"]["base_revision"],
        loser_begin["result"]["base_revision"]
    );
    assert_eq!(winner_begin["result"]["draft_id"], 1);
    assert_eq!(loser_begin["result"]["draft_id"], 1);

    let winner_set = winner.request(
        r#"{"id":2,"operation":"config.draft.set","params":{"draft_id":1,"path":"provider.model","value":{"type":"string","value":"winner-model"}}}"#,
    );
    let loser_set = loser.request(
        r#"{"id":2,"operation":"config.draft.set","params":{"draft_id":1,"path":"provider.model","value":{"type":"string","value":"loser-model"}}}"#,
    );
    assert_eq!(winner_set["result"]["staged"], true);
    assert_eq!(loser_set["result"]["staged"], true);

    let committed =
        winner.request(r#"{"id":3,"operation":"config.draft.commit","params":{"draft_id":1}}"#);
    assert_eq!(committed["result"]["written"], true);
    let winner_revision = committed["result"]["revision"].clone();
    let winner_bytes = fs::read(temp.user_config()).expect("read winning config bytes");

    let conflict =
        loser.request(r#"{"id":3,"operation":"config.draft.commit","params":{"draft_id":1}}"#);
    assert_eq!(conflict["error"]["category"], "revision_conflict");
    assert_eq!(
        fs::read(temp.user_config()).expect("read config after conflict"),
        winner_bytes
    );
    let retained = loser.request(
        r#"{"id":4,"operation":"config.draft.set","params":{"draft_id":1,"path":"provider.model","value":{"type":"string","value":"still-live"}}}"#,
    );
    assert_eq!(retained["result"]["staged"], true);
    assert_eq!(
        fs::read(temp.user_config()).expect("read config after stale draft repair"),
        winner_bytes
    );
    let current =
        loser.request(r#"{"id":5,"operation":"config.get","params":{"path":"provider.model"}}"#);
    assert_eq!(current["result"]["entry"]["value"]["value"], "winner-model");
    let rebased =
        loser.request(r#"{"id":6,"operation":"config.draft.begin","params":{"scope":"user"}}"#);
    assert_eq!(rebased["result"]["draft_id"], 2);
    assert_eq!(rebased["result"]["base_revision"], winner_revision);
    let no_change =
        loser.request(r#"{"id":7,"operation":"config.draft.commit","params":{"draft_id":2}}"#);
    assert_eq!(no_change["result"]["written"], false);
    assert_eq!(
        fs::read(temp.user_config()).expect("read config after rebased no-change commit"),
        winner_bytes
    );

    winner.finish();
    loser.finish();
    let reopened = temp
        .run(b"{\"id\":8,\"operation\":\"config.get\",\"params\":{\"path\":\"provider.model\"}}\n");
    let reopened = responses(&reopened);
    assert_eq!(
        reopened[0]["result"]["entry"]["value"]["value"],
        "winner-model"
    );
}

#[test]
fn app_server_boundary_bounds_active_drafts_and_recovers_capacity_after_commit() {
    let temp = TempTree::new();
    let mut server = temp.spawn();
    for request_id in 1..=64 {
        let response = server.request(&format!(
            "{{\"id\":{request_id},\"operation\":\"config.draft.begin\",\"params\":{{\"scope\":\"user\"}}}}"
        ));
        assert_eq!(response["result"]["draft_id"], request_id);
    }
    let full =
        server.request(r#"{"id":65,"operation":"config.draft.begin","params":{"scope":"user"}}"#);
    assert_eq!(full["error"]["category"], "resource_busy");
    let committed =
        server.request(r#"{"id":66,"operation":"config.draft.commit","params":{"draft_id":1}}"#);
    assert_eq!(committed["result"]["written"], false);
    let resumed =
        server.request(r#"{"id":67,"operation":"config.draft.begin","params":{"scope":"user"}}"#);
    assert_eq!(resumed["result"]["draft_id"], 65);
    server.finish();
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_boundary_reports_repair_without_overwriting_invalid_config() {
    let temp = TempTree::new();
    let invalid = b"schema_version = 1\n[model_presets.broken]\nprovider = \"simulator\"\n";
    let user = temp.user_config();
    fs::create_dir_all(user.parent().expect("user config parent"))
        .expect("create user config parent");
    fs::write(&user, invalid).expect("write invalid user config");

    let mut server = temp.spawn();
    let blocked =
        server.request(r#"{"id":1,"operation":"config.get","params":{"path":"provider.model"}}"#);
    assert_eq!(blocked["error"]["category"], "repair_required");
    assert!(
        !blocked
            .to_string()
            .contains(&temp.root.to_string_lossy()[..])
    );
    assert_eq!(fs::read(&user).expect("read invalid config"), invalid);

    let begun =
        server.request(r#"{"id":2,"operation":"config.draft.begin","params":{"scope":"user"}}"#);
    assert_eq!(begun["result"]["draft_id"], 1);
    let reset = server.request(
        r#"{"id":3,"operation":"config.draft.reset","params":{"draft_id":1,"path":"model_presets.broken.provider"}}"#,
    );
    assert_eq!(reset["result"]["staged"], true);
    let validated =
        server.request(r#"{"id":4,"operation":"config.draft.validate","params":{"draft_id":1}}"#);
    assert_eq!(
        validated["result"]["changes"][0]["path"],
        "model_presets.broken.provider"
    );
    assert_eq!(fs::read(&user).expect("read uncommitted config"), invalid);

    let committed =
        server.request(r#"{"id":5,"operation":"config.draft.commit","params":{"draft_id":1}}"#);
    assert_eq!(committed["result"]["written"], true);
    assert_ne!(fs::read(&user).expect("read repaired config"), invalid);
    let repaired =
        server.request(r#"{"id":6,"operation":"config.get","params":{"path":"provider.model"}}"#);
    assert_eq!(repaired["result"]["status"]["ready"], true);
    assert_eq!(
        repaired["result"]["entry"]["value"]["value"],
        "deterministic-v1"
    );
    server.finish();
    assert!(!temp.project_config().exists());
}
