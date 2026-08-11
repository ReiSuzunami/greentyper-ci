use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::agent_team::{
    Capability, CapabilitySnapshot, CommandOutcome, MessageRecipient, ResourceBudget, TaskScope,
    TaskSpec, TeamCommand, TeamOperationAcknowledgeOutcome,
};
use greentyper_core::runtime::RuntimeKernel;
use greentyper_core::tool_runtime::{ToolArguments, ToolIntent, ToolRequestOutcome, ToolResources};
use serde_json::Value;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

const CREDENTIAL_PROFILE: &str = "app-server";
const CREDENTIAL_ORIGIN: &str = "https://app-server-credential.invalid/v1";
const FIRST_CREDENTIAL: &str = "private-app-server-platform-first";
const SECOND_CREDENTIAL: &str = "private-app-server-platform-second";

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

    fn runtime_ledger(&self) -> PathBuf {
        self.root.join("runtime.ledger")
    }

    fn sidecar_ledger(&self, suffix: &str) -> PathBuf {
        let mut path = self.runtime_ledger().into_os_string();
        path.push(".");
        path.push(suffix);
        PathBuf::from(path)
    }

    fn team_ledger(&self) -> PathBuf {
        self.sidecar_ledger("team")
    }

    fn tool_ledger(&self) -> PathBuf {
        self.sidecar_ledger("tool")
    }

    fn run(&self, requests: &[u8]) -> std::process::Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_greentyper"))
            .args(["app-server", "--stdio", "--ledger"])
            .arg(self.runtime_ledger())
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
            .args(["app-server", "--stdio", "--ledger"])
            .arg(self.runtime_ledger())
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

    fn run_headless(&self, input: &str) {
        let output = Command::new(env!("CARGO_BIN_EXE_greentyper"))
            .args(["headless", "--ledger"])
            .arg(self.runtime_ledger())
            .args(["--input", input])
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("APPDATA", &self.root)
            .env("XDG_CONFIG_HOME", &self.root)
            .output()
            .expect("run deterministic headless Turn");
        assert!(
            output.status.success(),
            "status={:?}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn create_agent_state(&self) {
        let (mut kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
            self.runtime_ledger(),
            self.team_ledger(),
            self.tool_ledger(),
            1,
        )
        .expect("open App Server Agent fixture");
        assert!(recovery.into_sessions().is_empty());
        let admission = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "private App Server task title",
                    TaskScope::from_labels(["private-app-server-scope"]),
                ),
                budget: ResourceBudget::new(2_000, 4),
                capabilities: CapabilitySnapshot::from_capabilities([
                    Capability::WorkspaceRead,
                    Capability::Tool("private-app-server-tool".into()),
                ]),
            })
            .expect("admit App Server Agent fixture");
        assert!(matches!(
            kernel
                .acknowledge_team_operation(admission.operation)
                .expect("acknowledge App Server Agent admission"),
            TeamOperationAcknowledgeOutcome::Durable(_)
        ));
        let root = match admission.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root admission: {other:?}"),
        };
        let message = kernel
            .dispatch_team(TeamCommand::SendMessage {
                from: root,
                recipient: MessageRecipient::Team,
                body: "private App Server message body".into(),
            })
            .expect("send App Server Agent fixture message");
        assert!(matches!(
            kernel
                .acknowledge_team_operation(message.operation)
                .expect("acknowledge App Server Agent message"),
            TeamOperationAcknowledgeOutcome::Durable(_)
        ));
    }

    fn create_tool_state(&self) {
        let (mut kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
            self.runtime_ledger(),
            self.team_ledger(),
            self.tool_ledger(),
            1,
        )
        .expect("open App Server Tool fixture");
        assert!(recovery.into_sessions().is_empty());
        let admission = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "private App Server Tool task",
                    TaskScope::from_labels(["private-tool-scope"]),
                ),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::from_capabilities([Capability::Tool(
                    "local.echo".into(),
                )]),
            })
            .expect("admit App Server Tool owner");
        assert!(matches!(
            kernel
                .acknowledge_team_operation(admission.operation)
                .expect("acknowledge App Server Tool owner"),
            TeamOperationAcknowledgeOutcome::Durable(_)
        ));
        let root = match admission.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected Tool owner admission: {other:?}"),
        };
        let intent = ToolIntent::new(
            "private-tool-call-identity",
            "local.echo",
            ToolArguments::parse(r#"{"message":"private Tool argument"}"#).expect("Tool arguments"),
            ToolResources::default(),
        )
        .expect("Tool intent");
        assert!(matches!(
            kernel
                .request_tool_call(root, intent)
                .expect("request App Server Tool call"),
            ToolRequestOutcome::ApprovalRequired(_)
        ));
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

fn credential_reference() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    format!("app-server-{}-{nonce}", std::process::id())
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
fn app_server_runtime_status_inspects_missing_state_without_creating_it() {
    let temp = TempTree::new();
    let output = temp.run(
        concat!(
            "{\"id\":1,\"operation\":\"runtime.status\"}\n",
            "{\"id\":2,\"operation\":\"runtime.status\",\"params\":{\"extra\":true}}\n",
            "{\"id\":3,\"operation\":\"config.schema\"}\n",
        )
        .as_bytes(),
    );
    let results = responses(&output);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["result"]["status"], "ready");
    assert_eq!(results[0]["result"]["ledger"]["transaction"], 0);
    assert_eq!(results[0]["result"]["ledger"]["sequence"], 0);
    assert_eq!(results[0]["result"]["recovered_tail_bytes"], 0);
    assert_eq!(results[0]["result"]["thread"], Value::Null);
    assert_eq!(results[0]["result"]["item_count"], 0);
    assert_eq!(results[0]["result"]["pending_model_selection"], false);
    assert_eq!(results[1]["error"]["category"], "invalid_request");
    assert_eq!(results[2]["result"]["schema_version"], 1);
    assert!(!temp.runtime_ledger().exists());
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_runtime_stats_pages_a_frozen_read_only_report() {
    let temp = TempTree::new();
    temp.run_headless("first App Server usage Turn");
    temp.run_headless("second App Server usage Turn");
    let before = fs::read(temp.runtime_ledger()).expect("read Runtime Ledger before stats");
    let mut server = temp.spawn();

    let summary = server.request(r#"{"id":1,"operation":"runtime.stats"}"#);
    assert_eq!(summary["result"]["summary"]["total"]["attempts"], 2);
    assert_eq!(summary["result"]["page"], Value::Null);

    let first = server.request(r#"{"id":2,"operation":"runtime.stats","params":{"limit":1}}"#);
    let first_page = first["result"]["page"]["attempts"]
        .as_array()
        .expect("first Usage page");
    assert_eq!(first_page.len(), 1);
    assert_eq!(first_page[0]["turn"], 1);
    let cursor = first["result"]["page"]["next_cursor"]
        .as_str()
        .expect("next Usage cursor");
    let as_of = first["result"]["summary"]["as_of"]
        .as_i64()
        .expect("Usage report instant");

    let second = server.request(&format!(
        r#"{{"id":3,"operation":"runtime.stats","params":{{"as_of_unix_ms":{as_of},"limit":1,"cursor":"{cursor}"}}}}"#
    ));
    let second_page = second["result"]["page"]["attempts"]
        .as_array()
        .expect("second Usage page");
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page[0]["turn"], 2);
    assert_eq!(second["result"]["page"]["next_cursor"], Value::Null);

    let invalid =
        server.request(r#"{"id":4,"operation":"runtime.stats","params":{"cursor":"v1:invalid"}}"#);
    assert_eq!(invalid["error"]["category"], "invalid_value");
    let status = server.request(r#"{"id":5,"operation":"runtime.status"}"#);
    assert_eq!(status["result"]["status"], "ready");
    server.finish();

    assert_eq!(
        fs::read(temp.runtime_ledger()).expect("read Runtime Ledger after stats"),
        before
    );
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_agent_list_is_read_only_and_redacts_team_content() {
    let temp = TempTree::new();
    let missing = temp.run(b"{\"id\":1,\"operation\":\"agent.list\"}\n");
    let missing_results = responses(&missing);
    assert_eq!(missing_results[0]["result"]["available"], false);
    assert_eq!(missing_results[0]["result"]["team"], Value::Null);
    assert!(!temp.runtime_ledger().exists());
    assert!(!temp.team_ledger().exists());
    assert!(!temp.tool_ledger().exists());

    temp.create_agent_state();
    let runtime_before = fs::read(temp.runtime_ledger()).expect("read Runtime Ledger before Agent");
    let team_before = fs::read(temp.team_ledger()).expect("read Team Ledger before Agent");
    let tool_before = fs::read(temp.tool_ledger()).expect("read Tool Ledger before Agent");
    let output = temp.run(
        concat!(
            "{\"id\":2,\"operation\":\"agent.list\"}\n",
            "{\"id\":3,\"operation\":\"runtime.status\"}\n",
        )
        .as_bytes(),
    );
    let results = responses(&output);
    let team = &results[0]["result"]["team"];
    assert_eq!(results[0]["result"]["available"], true);
    assert!(
        team["revision"]
            .as_u64()
            .is_some_and(|revision| revision > 0)
    );
    assert_eq!(team["message_count"], 1);
    assert_eq!(team["operations_awaiting_acknowledgement"], 0);
    let agents = team["agents"].as_array().expect("Agent list");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], 1);
    assert_eq!(agents[0]["status"], "active");
    assert_eq!(agents[0]["token_budget"], 2_000);
    assert_eq!(agents[0]["tool_budget"], 4);
    assert_eq!(agents[0]["capability_count"], 2);
    assert_eq!(agents[0]["scope_count"], 1);
    assert_eq!(results[1]["result"]["status"], "ready");
    let output_text = String::from_utf8(output.stdout).expect("UTF-8 App Server Agent output");
    for private in [
        "private App Server task title",
        "private-app-server-scope",
        "private-app-server-tool",
        "private App Server message body",
    ] {
        assert!(!output_text.contains(private));
    }
    assert_eq!(
        fs::read(temp.runtime_ledger()).expect("read Runtime Ledger after Agent"),
        runtime_before
    );
    assert_eq!(
        fs::read(temp.team_ledger()).expect("read Team Ledger after Agent"),
        team_before
    );
    assert_eq!(
        fs::read(temp.tool_ledger()).expect("read Tool Ledger after Agent"),
        tool_before
    );
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_tool_status_is_redacted_read_only_and_recovers_after_sidecar_failure() {
    let missing = TempTree::new();
    let missing_output = missing.run(b"{\"id\":1,\"operation\":\"tool.status\"}\n");
    let missing_results = responses(&missing_output);
    assert_eq!(
        missing_results[0]["result"]["calls"]
            .as_array()
            .expect("empty Tool calls")
            .len(),
        0
    );
    assert!(!missing.runtime_ledger().exists());
    assert!(!missing.team_ledger().exists());
    assert!(!missing.tool_ledger().exists());

    let temp = TempTree::new();
    temp.create_tool_state();
    let runtime_before = fs::read(temp.runtime_ledger()).expect("read Runtime Ledger before Tool");
    let team_before = fs::read(temp.team_ledger()).expect("read Team Ledger before Tool");
    let tool_before = fs::read(temp.tool_ledger()).expect("read Tool Ledger before Tool");
    let output = temp.run(b"{\"id\":2,\"operation\":\"tool.status\"}\n");
    let results = responses(&output);
    let calls = results[0]["result"]["calls"]
        .as_array()
        .expect("Tool calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["call"], 1);
    assert_eq!(calls[0]["agent"], 1);
    assert_eq!(calls[0]["tool"], "local.echo");
    assert_eq!(calls[0]["status"], "awaiting_approval");
    assert_eq!(calls[0]["approval_expires_at_unix_ms"], Value::Null);
    assert_eq!(calls[0]["result_sha256"], Value::Null);
    let output_text = String::from_utf8(output.stdout).expect("UTF-8 App Server Tool output");
    for private in [
        "private-tool-call-identity",
        "private Tool argument",
        "private App Server Tool task",
        "private-tool-scope",
    ] {
        assert!(!output_text.contains(private));
    }
    assert_eq!(
        fs::read(temp.runtime_ledger()).expect("read Runtime Ledger after Tool"),
        runtime_before
    );
    assert_eq!(
        fs::read(temp.team_ledger()).expect("read Team Ledger after Tool"),
        team_before
    );
    assert_eq!(
        fs::read(temp.tool_ledger()).expect("read Tool Ledger after Tool"),
        tool_before
    );

    let incomplete = TempTree::new();
    fs::write(incomplete.team_ledger(), b"private-incomplete-sidecar")
        .expect("write incomplete Team sidecar");
    let before = fs::read(incomplete.team_ledger()).expect("read incomplete Team sidecar");
    let failed = incomplete.run(
        concat!(
            "{\"id\":3,\"operation\":\"tool.status\"}\n",
            "{\"id\":4,\"operation\":\"runtime.status\"}\n",
        )
        .as_bytes(),
    );
    let failed_results = responses(&failed);
    assert_eq!(failed_results[0]["error"]["category"], "tool_unavailable");
    assert_eq!(failed_results[1]["result"]["status"], "ready");
    assert_eq!(
        fs::read(incomplete.team_ledger()).expect("reread incomplete Team sidecar"),
        before
    );
    assert!(!incomplete.runtime_ledger().exists());
    assert!(!incomplete.tool_ledger().exists());
    assert!(!incomplete.user_config().exists());
    assert!(!incomplete.project_config().exists());
}

#[cfg(not(windows))]
#[test]
fn app_server_credential_platform_operations_fail_closed_without_readback() {
    let temp = TempTree::new();
    let reference = credential_reference();
    let requests = format!(
        concat!(
            "{{\"id\":1,\"operation\":\"credential.bind\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\",\"secret\":\"{first}\"}}}}\n",
            "{{\"id\":2,\"operation\":\"credential.replace\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\",\"secret\":\"{second}\"}}}}\n",
            "{{\"id\":3,\"operation\":\"credential.test\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\"}}}}\n",
            "{{\"id\":4,\"operation\":\"credential.forget\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\"}}}}\n",
        ),
        reference = reference,
        profile = CREDENTIAL_PROFILE,
        origin = CREDENTIAL_ORIGIN,
        first = FIRST_CREDENTIAL,
        second = SECOND_CREDENTIAL,
    );

    let output = temp.run(requests.as_bytes());
    let results = responses(&output);
    assert_eq!(results.len(), 4);
    for result in results {
        assert_eq!(result["error"]["category"], "credential_unavailable");
    }
    for secret in [FIRST_CREDENTIAL, SECOND_CREDENTIAL] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[cfg(windows)]
#[test]
fn app_server_credential_platform_round_trips_without_readback() {
    let temp = TempTree::new();
    let reference = credential_reference();
    let _cleanup = WindowsCredentialCleanup(reference.clone());
    let requests = format!(
        concat!(
            "{{\"id\":1,\"operation\":\"credential.forget\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\"}}}}\n",
            "{{\"id\":2,\"operation\":\"credential.bind\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\",\"secret\":\"{first}\"}}}}\n",
            "{{\"id\":3,\"operation\":\"credential.test\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\"}}}}\n",
            "{{\"id\":4,\"operation\":\"credential.bind\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\",\"secret\":\"{second}\"}}}}\n",
            "{{\"id\":5,\"operation\":\"credential.replace\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\",\"secret\":\"{second}\"}}}}\n",
            "{{\"id\":6,\"operation\":\"credential.test\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\"}}}}\n",
            "{{\"id\":7,\"operation\":\"credential.forget\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\"}}}}\n",
            "{{\"id\":8,\"operation\":\"credential.test\",\"params\":{{\"reference\":\"{reference}\",\"profile\":\"{profile}\",\"origin\":\"{origin}\"}}}}\n",
        ),
        reference = reference,
        profile = CREDENTIAL_PROFILE,
        origin = CREDENTIAL_ORIGIN,
        first = FIRST_CREDENTIAL,
        second = SECOND_CREDENTIAL,
    );

    let output = temp.run(requests.as_bytes());
    let results = responses(&output);
    assert!(matches!(
        results[0]["result"]["status"].as_str(),
        Some("forgotten" | "not_found")
    ));
    assert_eq!(results[1]["result"]["status"], "bound");
    assert_eq!(results[2]["result"]["status"], "available");
    assert_eq!(results[3]["error"]["category"], "credential_already_bound");
    assert_eq!(results[4]["result"]["status"], "replaced");
    assert_eq!(results[5]["result"]["status"], "available");
    assert_eq!(results[6]["result"]["status"], "forgotten");
    assert_eq!(results[7]["result"]["status"], "not_found");
    for secret in [FIRST_CREDENTIAL, SECOND_CREDENTIAL] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[cfg(windows)]
struct WindowsCredentialCleanup(String);

#[cfg(windows)]
impl Drop for WindowsCredentialCleanup {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_greentyper"))
            .args([
                "credential",
                "forget",
                &self.0,
                "--profile",
                CREDENTIAL_PROFILE,
                "--origin",
                CREDENTIAL_ORIGIN,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
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
