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
use greentyper_core::config::{ConfigDocument, ConfigLayers, ConfigPaths, ConfigRuntime};
use greentyper_core::provider::{
    DeterministicProvider, ProviderDialect, ProviderError, ProviderEvent, ProviderProfileSnapshot,
    ProviderRequest, ProviderRuntime, ProviderUnavailableStage,
};
use greentyper_core::runtime::{RecoveryStatus, RuntimeKernel};
use greentyper_core::tool_runtime::{
    ApprovalDecision, AuthorizedToolCall, ToolArguments, ToolCallOutcome, ToolEffectExecutor,
    ToolExecution, ToolIntent, ToolRequestOutcome, ToolResources,
};
use serde_json::Value;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

const CREDENTIAL_PROFILE: &str = "app-server";
const CREDENTIAL_ORIGIN: &str = "https://app-server-credential.invalid/v1";
const FIRST_CREDENTIAL: &str = "private-app-server-platform-first";
const SECOND_CREDENTIAL: &str = "private-app-server-platform-second";

struct AmbiguousExecutor;

impl ToolEffectExecutor for AmbiguousExecutor {
    fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
        ToolExecution::Ambiguous {
            reason: "private ambiguous App Server Tool result".into(),
        }
    }
}

struct UnavailableProvider {
    stage: ProviderUnavailableStage,
}

struct PanicProfileProvider {
    profile: ProviderProfileSnapshot,
}

impl ProviderRuntime for PanicProfileProvider {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.profile)
    }

    fn dialect(&self) -> Option<ProviderDialect> {
        Some(ProviderDialect::Responses)
    }

    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        panic!("injected App Server Provider crash after admission")
    }
}

impl ProviderRuntime for UnavailableProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        Err(ProviderError::unavailable_during(
            self.stage,
            "private App Server Provider failure",
        ))
    }
}

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
        let output = self.headless_output(input);
        assert!(
            output.status.success(),
            "status={:?}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn headless_output(&self, input: &str) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_greentyper"))
            .args(["headless", "--ledger"])
            .arg(self.runtime_ledger())
            .args(["--input", input])
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("APPDATA", &self.root)
            .env("XDG_CONFIG_HOME", &self.root)
            .output()
            .expect("run deterministic headless Turn")
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

    fn create_prepared_runtime(&self) -> u64 {
        let mut runtime = RuntimeKernel::open(self.runtime_ledger())
            .expect("open App Server prepared Runtime fixture");
        let output = runtime
            .execute(
                &ConfigLayers::default(),
                "private App Server prepared input",
                &mut DeterministicProvider::default(),
            )
            .expect("prepare App Server Runtime output");
        output.delivery().get()
    }

    fn create_blocked_runtime(&self, stage: ProviderUnavailableStage) -> u64 {
        let mut runtime =
            RuntimeKernel::open(self.runtime_ledger()).expect("open blocked Runtime fixture");
        let mut provider = UnavailableProvider { stage };
        assert!(matches!(
            runtime.execute(
                &ConfigLayers::default(),
                "private App Server blocked input",
                &mut provider,
            ),
            Err(greentyper_core::runtime::RuntimeError::Provider(
                ProviderError::Unavailable { .. }
            ))
        ));
        let RecoveryStatus::Blocked { turn, .. } = runtime.snapshot().status else {
            panic!("Provider failure did not block the Runtime fixture")
        };
        turn.get()
    }

    fn create_blocked_product_runtime(&self, stage: ProviderUnavailableStage) -> u64 {
        let (mut kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
            self.runtime_ledger(),
            self.team_ledger(),
            self.tool_ledger(),
            1,
        )
        .expect("open blocked Product fixture");
        assert!(recovery.into_sessions().is_empty());
        let admission = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "private Provider recovery task",
                    TaskScope::from_labels(["private-provider-recovery-scope"]),
                ),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::from_capabilities([Capability::Process]),
            })
            .expect("admit Provider recovery owner");
        kernel
            .acknowledge_team_operation(admission.operation)
            .expect("acknowledge Provider recovery owner");
        let root = match admission.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected Provider recovery admission: {other:?}"),
        };
        let mut provider = UnavailableProvider { stage };
        assert!(matches!(
            kernel.execute_provider_turn(
                root,
                &ConfigLayers::default(),
                "private Product Provider blocked input",
                &mut provider,
                |_| Ok(ToolResources::default()),
            ),
            Err(greentyper_core::runtime::RuntimeError::Provider(
                ProviderError::Unavailable { .. }
            ))
        ));
        let RecoveryStatus::Blocked { turn, .. } = kernel.snapshot().status else {
            panic!("Provider failure did not block the Product fixture")
        };
        turn.get()
    }

    fn create_external_resume_required_runtime(&self) -> u64 {
        let document = ConfigDocument::parse(
            r#"
schema_version = 1

[provider]
profile = "recovery-profile"
model = "fixture-model"

[providers.recovery-profile]
template = "openai-compatible"
credential = "private-recovery-credential"
base_url = "https://private-recovery.invalid"
dialects = ["responses"]

[providers.recovery-profile.routes]
responses = "/v1/responses"

[providers.recovery-profile.pricing]
source = "unknown"
"#,
        )
        .expect("parse external Provider recovery Config");
        let config = ConfigRuntime::open(
            ConfigPaths::new(self.user_config(), self.project_config()),
            document,
        )
        .expect("open external Provider recovery Config");
        let layers = config
            .config_layers()
            .expect("external Provider recovery layers")
            .clone();
        let profile = config
            .selected_provider_profile()
            .expect("resolve external Provider recovery profile")
            .expect("external Provider recovery profile");
        let mut runtime =
            RuntimeKernel::open(self.runtime_ledger()).expect("open external recovery Runtime");
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut provider = PanicProfileProvider { profile };
            let _ = runtime.execute(
                &layers,
                "private external Provider recovery input",
                &mut provider,
            );
        }));
        assert!(crashed.is_err());
        let RecoveryStatus::ResumeRequired { turn } = runtime.snapshot().status else {
            panic!("crashed external Provider Turn was not resumable")
        };
        turn.get()
    }

    fn create_reconciliation_tool_state(&self) -> u64 {
        let (mut kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
            self.runtime_ledger(),
            self.team_ledger(),
            self.tool_ledger(),
            1,
        )
        .expect("open App Server Tool reconciliation fixture");
        assert!(recovery.into_sessions().is_empty());
        let admission = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "private reconciliation task",
                    TaskScope::from_labels(["private-reconciliation-scope"]),
                ),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::from_capabilities([
                    Capability::Tool("local.echo".into()),
                    Capability::Process,
                ]),
            })
            .expect("admit App Server Tool reconciliation owner");
        assert!(matches!(
            kernel
                .acknowledge_team_operation(admission.operation)
                .expect("acknowledge App Server Tool reconciliation owner"),
            TeamOperationAcknowledgeOutcome::Durable(_)
        ));
        let root = match admission.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected Tool reconciliation owner admission: {other:?}"),
        };
        let intent = ToolIntent::new(
            "private-reconciliation-identity",
            "local.echo",
            ToolArguments::parse(r#"{"message":"private reconciliation argument"}"#)
                .expect("Tool reconciliation arguments"),
            ToolResources::default().with_process("local.echo"),
        )
        .expect("Tool reconciliation intent");
        let request = match kernel
            .request_tool_call(root, intent)
            .expect("request App Server Tool reconciliation call")
        {
            ToolRequestOutcome::ApprovalRequired(request) => request,
            other => panic!("unexpected Tool reconciliation request: {other:?}"),
        };
        match kernel
            .resolve_tool_call(
                request,
                ApprovalDecision::Grant {
                    expires_at_unix_ms: u64::MAX,
                },
                &mut AmbiguousExecutor,
            )
            .expect("record ambiguous App Server Tool effect")
        {
            ToolCallOutcome::ReconciliationRequired(record) => record.call.get(),
            other => panic!("unexpected Tool reconciliation outcome: {other:?}"),
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
    assert_eq!(responses[0]["result"]["schema_version"], 2);
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
    assert_eq!(responses[6]["result"]["schema_version"], 2);
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
    assert_eq!(results[0]["result"]["retryable"], false);
    assert_eq!(results[0]["result"]["pending_model_selection"], false);
    assert_eq!(results[1]["error"]["category"], "invalid_request");
    assert_eq!(results[2]["result"]["schema_version"], 2);
    assert!(!temp.runtime_ledger().exists());
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_cancels_provider_blocks_strictly_without_cross_ledger_mutation() {
    let missing = TempTree::new();
    let missing_output = missing.run(
        concat!(
            "{\"id\":1,\"operation\":\"runtime.cancel\",\"params\":{\"turn\":1}}\n",
            "{\"id\":2,\"operation\":\"runtime.cancel\",\"params\":{\"turn\":0}}\n",
        )
        .as_bytes(),
    );
    let missing_results = responses(&missing_output);
    assert_eq!(
        missing_results[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(missing_results[1]["error"]["category"], "invalid_value");
    assert!(!missing.runtime_ledger().exists());

    let ordinary = TempTree::new();
    let turn = ordinary.create_blocked_runtime(ProviderUnavailableStage::BeforeFirstEvent);
    let blocked_bytes = fs::read(ordinary.runtime_ledger()).expect("read blocked Runtime Ledger");
    let mut server = ordinary.spawn();
    let unknown = server.request(&format!(
        "{{\"id\":3,\"operation\":\"runtime.cancel\",\"params\":{{\"turn\":{}}}}}",
        turn + 1
    ));
    assert_eq!(unknown["error"]["category"], "unknown_turn");
    assert_eq!(
        fs::read(ordinary.runtime_ledger()).expect("read Runtime after unknown cancellation"),
        blocked_bytes
    );
    let cancelled = server.request(&format!(
        "{{\"id\":4,\"operation\":\"runtime.cancel\",\"params\":{{\"turn\":{turn}}}}}"
    ));
    assert_eq!(cancelled["result"]["status"], "cancelled");
    assert_eq!(cancelled["result"]["turn"], turn);
    let cancelled_bytes =
        fs::read(ordinary.runtime_ledger()).expect("read cancelled Runtime Ledger");
    assert_ne!(cancelled_bytes, blocked_bytes);
    let repeated = server.request(&format!(
        "{{\"id\":5,\"operation\":\"runtime.cancel\",\"params\":{{\"turn\":{turn}}}}}"
    ));
    assert_eq!(repeated["result"]["status"], "already_cancelled");
    assert_eq!(repeated["result"]["ledger"], cancelled["result"]["ledger"]);
    assert_eq!(
        fs::read(ordinary.runtime_ledger()).expect("read Runtime after repeated cancellation"),
        cancelled_bytes
    );
    server.finish();
    assert_eq!(
        RuntimeKernel::inspect(ordinary.runtime_ledger())
            .expect("inspect cancelled Runtime")
            .status,
        RecoveryStatus::Ready
    );

    let product = TempTree::new();
    let product_turn =
        product.create_blocked_product_runtime(ProviderUnavailableStage::BeforeResponse);
    let product_runtime_before =
        fs::read(product.runtime_ledger()).expect("read Product Runtime Ledger");
    let team_before = fs::read(product.team_ledger()).expect("read Product Team Ledger");
    let tool_before = fs::read(product.tool_ledger()).expect("read Product Tool Ledger");
    let product_output = product.run(
        format!(
            "{{\"id\":6,\"operation\":\"runtime.cancel\",\"params\":{{\"turn\":{product_turn}}}}}\n"
        )
        .as_bytes(),
    );
    let product_result = responses(&product_output);
    assert_eq!(product_result[0]["result"]["status"], "cancelled");
    assert_ne!(
        fs::read(product.runtime_ledger()).expect("read cancelled Product Runtime Ledger"),
        product_runtime_before
    );
    assert_eq!(
        fs::read(product.team_ledger()).expect("reread Product Team Ledger"),
        team_before
    );
    assert_eq!(
        fs::read(product.tool_ledger()).expect("reread Product Tool Ledger"),
        tool_before
    );

    let torn = TempTree::new();
    let torn_turn = torn.create_blocked_runtime(ProviderUnavailableStage::BeforeResponse);
    let mut torn_bytes = fs::read(torn.runtime_ledger()).expect("read Runtime before torn tail");
    torn_bytes.extend_from_slice(b"bad");
    fs::write(torn.runtime_ledger(), &torn_bytes).expect("append torn cancellation tail");
    let torn_output = torn.run(
        format!(
            "{{\"id\":7,\"operation\":\"runtime.cancel\",\"params\":{{\"turn\":{torn_turn}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&torn_output)[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(
        fs::read(torn.runtime_ledger()).expect("read unmodified torn Runtime Ledger"),
        torn_bytes
    );
}

#[test]
fn app_server_rearms_retryable_provider_blocks_without_running_the_provider() {
    let missing = TempTree::new();
    let missing_output = missing.run(
        concat!(
            "{\"id\":1,\"operation\":\"runtime.retry\",\"params\":{\"turn\":1}}\n",
            "{\"id\":2,\"operation\":\"runtime.retry\",\"params\":{\"turn\":0}}\n",
        )
        .as_bytes(),
    );
    let missing_results = responses(&missing_output);
    assert_eq!(
        missing_results[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(missing_results[1]["error"]["category"], "invalid_value");
    assert!(!missing.runtime_ledger().exists());

    let partial = TempTree::new();
    let partial_turn = partial.create_blocked_runtime(ProviderUnavailableStage::AfterFirstEvent);
    let partial_before =
        fs::read(partial.runtime_ledger()).expect("read non-retryable Runtime Ledger");
    let partial_output = partial.run(
        format!(
            "{{\"id\":3,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{partial_turn}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&partial_output)[0]["error"]["category"],
        "turn_not_retryable"
    );
    assert_eq!(
        fs::read(partial.runtime_ledger()).expect("reread non-retryable Runtime Ledger"),
        partial_before
    );

    let ordinary = TempTree::new();
    let turn = ordinary.create_blocked_runtime(ProviderUnavailableStage::BeforeFirstEvent);
    let blocked_bytes = fs::read(ordinary.runtime_ledger()).expect("read retryable Runtime Ledger");
    let mut server = ordinary.spawn();
    let unknown = server.request(&format!(
        "{{\"id\":4,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{}}}}}",
        turn + 1
    ));
    assert_eq!(unknown["error"]["category"], "unknown_turn");
    assert_eq!(
        fs::read(ordinary.runtime_ledger()).expect("read Runtime after unknown retry"),
        blocked_bytes
    );
    let rearmed = server.request(&format!(
        "{{\"id\":5,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{turn}}}}}"
    ));
    assert_eq!(rearmed["result"]["status"], "resume_required");
    assert_eq!(rearmed["result"]["turn"], turn);
    let rearmed_bytes = fs::read(ordinary.runtime_ledger()).expect("read rearmed Runtime Ledger");
    assert_ne!(rearmed_bytes, blocked_bytes);
    let repeated = server.request(&format!(
        "{{\"id\":6,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{turn}}}}}"
    ));
    assert_eq!(repeated["error"]["category"], "turn_not_retryable");
    assert_eq!(
        fs::read(ordinary.runtime_ledger()).expect("read Runtime after repeated retry"),
        rearmed_bytes
    );
    let status = server.request(r#"{"id":7,"operation":"runtime.status"}"#);
    assert_eq!(status["result"]["status"], "resume_required");
    assert_eq!(status["result"]["turn"], turn);
    server.finish();

    let product = TempTree::new();
    let product_turn =
        product.create_blocked_product_runtime(ProviderUnavailableStage::BeforeResponse);
    let runtime_before = fs::read(product.runtime_ledger()).expect("read Product Runtime Ledger");
    let team_before = fs::read(product.team_ledger()).expect("read Product Team Ledger");
    let tool_before = fs::read(product.tool_ledger()).expect("read Product Tool Ledger");
    let product_output = product.run(
        format!(
            "{{\"id\":8,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{product_turn}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&product_output)[0]["result"]["status"],
        "resume_required"
    );
    assert_ne!(
        fs::read(product.runtime_ledger()).expect("read rearmed Product Runtime Ledger"),
        runtime_before
    );
    assert_eq!(
        fs::read(product.team_ledger()).expect("reread Product Team Ledger"),
        team_before
    );
    assert_eq!(
        fs::read(product.tool_ledger()).expect("reread Product Tool Ledger"),
        tool_before
    );

    let torn = TempTree::new();
    let torn_turn = torn.create_blocked_runtime(ProviderUnavailableStage::BeforeResponse);
    let mut torn_bytes = fs::read(torn.runtime_ledger()).expect("read Runtime before torn retry");
    torn_bytes.extend_from_slice(b"bad");
    fs::write(torn.runtime_ledger(), &torn_bytes).expect("append torn retry tail");
    let torn_output = torn.run(
        format!(
            "{{\"id\":9,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{torn_turn}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&torn_output)[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(
        fs::read(torn.runtime_ledger()).expect("read unmodified torn retry Ledger"),
        torn_bytes
    );
}

#[test]
fn app_server_resumes_the_exact_turn_and_leaves_output_pending_until_acknowledged() {
    let missing = TempTree::new();
    let missing_output = missing.run(
        concat!(
            "{\"id\":1,\"operation\":\"runtime.resume\",\"params\":{\"turn\":1}}\n",
            "{\"id\":2,\"operation\":\"runtime.resume\",\"params\":{\"turn\":0}}\n",
        )
        .as_bytes(),
    );
    let missing_results = responses(&missing_output);
    assert_eq!(
        missing_results[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(missing_results[1]["error"]["category"], "invalid_value");
    assert!(!missing.runtime_ledger().exists());

    let ordinary = TempTree::new();
    let turn = ordinary.create_blocked_runtime(ProviderUnavailableStage::BeforeFirstEvent);
    let mut server = ordinary.spawn();
    let rearmed = server.request(&format!(
        "{{\"id\":3,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{turn}}}}}"
    ));
    assert_eq!(rearmed["result"]["status"], "resume_required");
    let before_wrong =
        fs::read(ordinary.runtime_ledger()).expect("read Runtime before wrong resume");
    let wrong = server.request(&format!(
        "{{\"id\":4,\"operation\":\"runtime.resume\",\"params\":{{\"turn\":{}}}}}",
        turn + 1
    ));
    assert_eq!(wrong["error"]["category"], "turn_not_resumable");
    assert_eq!(
        fs::read(ordinary.runtime_ledger()).expect("read Runtime after wrong resume"),
        before_wrong
    );
    let resumed = server.request(&format!(
        "{{\"id\":5,\"operation\":\"runtime.resume\",\"params\":{{\"turn\":{turn}}}}}"
    ));
    assert_eq!(resumed["result"]["status"], "prepared");
    assert_eq!(resumed["result"]["turn"], turn);
    assert_eq!(
        resumed["result"]["text"],
        "simulated: private App Server blocked input"
    );
    assert_eq!(resumed["result"]["usage_record_count"], 1);
    let delivery = resumed["result"]["delivery"]
        .as_u64()
        .expect("prepared delivery ID");
    let prepared_bytes = fs::read(ordinary.runtime_ledger()).expect("read prepared Runtime Ledger");
    let status = server.request(r#"{"id":6,"operation":"runtime.status"}"#);
    assert_eq!(status["result"]["status"], "reconciliation_required");
    assert_eq!(status["result"]["delivery"], delivery);
    let repeated = server.request(&format!(
        "{{\"id\":7,\"operation\":\"runtime.resume\",\"params\":{{\"turn\":{turn}}}}}"
    ));
    assert_eq!(repeated["error"]["category"], "turn_not_resumable");
    assert_eq!(
        fs::read(ordinary.runtime_ledger()).expect("read Runtime after repeated resume"),
        prepared_bytes
    );
    let recovered = server.request(&format!(
        "{{\"id\":8,\"operation\":\"runtime.delivery\",\"params\":{{\"delivery\":{delivery}}}}}"
    ));
    assert_eq!(recovered["result"]["text"], resumed["result"]["text"]);
    assert_eq!(
        fs::read(ordinary.runtime_ledger()).expect("read Runtime after delivery recovery"),
        prepared_bytes
    );
    let acknowledged = server.request(&format!(
        "{{\"id\":9,\"operation\":\"runtime.acknowledge\",\"params\":{{\"delivery\":{delivery}}}}}"
    ));
    assert_eq!(acknowledged["result"]["status"], "acknowledged");
    let ready = server.request(r#"{"id":10,"operation":"runtime.status"}"#);
    assert_eq!(ready["result"]["status"], "ready");
    server.finish();

    let product = TempTree::new();
    let product_turn =
        product.create_blocked_product_runtime(ProviderUnavailableStage::BeforeResponse);
    let team_before = fs::read(product.team_ledger()).expect("read Team before Product resume");
    let tool_before = fs::read(product.tool_ledger()).expect("read Tool before Product resume");
    let mut product_server = product.spawn();
    let product_retry = product_server.request(&format!(
        "{{\"id\":11,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{product_turn}}}}}"
    ));
    assert_eq!(product_retry["result"]["status"], "resume_required");
    let product_resume = product_server.request(&format!(
        "{{\"id\":12,\"operation\":\"runtime.resume\",\"params\":{{\"turn\":{product_turn}}}}}"
    ));
    assert_eq!(product_resume["result"]["status"], "prepared");
    assert_eq!(
        product_resume["result"]["text"],
        "simulated: private Product Provider blocked input"
    );
    let product_delivery = product_resume["result"]["delivery"]
        .as_u64()
        .expect("Product delivery ID");
    let product_status = product_server.request(r#"{"id":13,"operation":"runtime.status"}"#);
    assert_eq!(
        product_status["result"]["status"],
        "reconciliation_required"
    );
    let product_recovered = product_server.request(&format!(
        "{{\"id\":14,\"operation\":\"runtime.delivery\",\"params\":{{\"delivery\":{product_delivery}}}}}"
    ));
    assert_eq!(
        product_recovered["result"]["text"],
        product_resume["result"]["text"]
    );
    let product_acknowledged = product_server.request(&format!(
        "{{\"id\":15,\"operation\":\"runtime.acknowledge\",\"params\":{{\"delivery\":{product_delivery}}}}}"
    ));
    assert_eq!(product_acknowledged["result"]["status"], "acknowledged");
    let product_ready = product_server.request(r#"{"id":16,"operation":"runtime.status"}"#);
    assert_eq!(product_ready["result"]["status"], "ready");
    product_server.finish();
    assert_eq!(
        fs::read(product.team_ledger()).expect("reread Team after Product resume"),
        team_before
    );
    assert_eq!(
        fs::read(product.tool_ledger()).expect("reread Tool after Product resume"),
        tool_before
    );
    let product_stdout = [
        product_retry,
        product_resume,
        product_status,
        product_recovered,
        product_acknowledged,
        product_ready,
    ]
    .into_iter()
    .map(|response| response.to_string())
    .collect::<String>();
    assert!(!product_stdout.contains("private Provider recovery task"));
    assert!(!product_stdout.contains("private-provider-recovery-scope"));

    let credential = TempTree::new();
    let credential_turn = credential.create_external_resume_required_runtime();
    let credential_runtime_before =
        fs::read(credential.runtime_ledger()).expect("read credential Runtime Ledger");
    let credential_output = credential.run(
        format!(
            concat!(
                "{{\"id\":17,\"operation\":\"runtime.resume\",\"params\":{{\"turn\":{0}}}}}\n",
                "{{\"id\":18,\"operation\":\"runtime.status\"}}\n",
            ),
            credential_turn,
        )
        .as_bytes(),
    );
    let credential_results = responses(&credential_output);
    assert_eq!(
        credential_results[0]["error"]["category"],
        "provider_unavailable"
    );
    assert_eq!(credential_results[1]["result"]["status"], "resume_required");
    assert_eq!(
        fs::read(credential.runtime_ledger()).expect("reread credential Runtime Ledger"),
        credential_runtime_before
    );
    let credential_stdout =
        String::from_utf8(credential_output.stdout).expect("UTF-8 credential recovery output");
    for private in [
        "private-recovery-credential",
        "private-recovery.invalid",
        "private external Provider recovery input",
    ] {
        assert!(!credential_stdout.contains(private));
    }
    assert!(!credential.user_config().exists());
    assert!(!credential.project_config().exists());

    let torn = TempTree::new();
    let torn_turn = torn.create_blocked_runtime(ProviderUnavailableStage::BeforeResponse);
    let retry_output = torn.run(
        format!(
            "{{\"id\":14,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{torn_turn}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&retry_output)[0]["result"]["status"],
        "resume_required"
    );
    let mut torn_bytes = fs::read(torn.runtime_ledger()).expect("read Runtime before torn resume");
    torn_bytes.extend_from_slice(b"bad");
    fs::write(torn.runtime_ledger(), &torn_bytes).expect("append torn resume tail");
    let torn_output = torn.run(
        format!(
            "{{\"id\":15,\"operation\":\"runtime.resume\",\"params\":{{\"turn\":{torn_turn}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&torn_output)[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(
        fs::read(torn.runtime_ledger()).expect("read unmodified torn resume Ledger"),
        torn_bytes
    );

    let sidecar = TempTree::new();
    let sidecar_turn =
        sidecar.create_blocked_product_runtime(ProviderUnavailableStage::BeforeResponse);
    let sidecar_retry = sidecar.run(
        format!(
            "{{\"id\":19,\"operation\":\"runtime.retry\",\"params\":{{\"turn\":{sidecar_turn}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&sidecar_retry)[0]["result"]["status"],
        "resume_required"
    );
    let runtime_before_sidecar_failure =
        fs::read(sidecar.runtime_ledger()).expect("read Runtime before sidecar failure");
    let tool_before_sidecar_failure =
        fs::read(sidecar.tool_ledger()).expect("read Tool before sidecar failure");
    let mut torn_team = fs::read(sidecar.team_ledger()).expect("read Team before torn tail");
    torn_team.extend_from_slice(b"bad");
    fs::write(sidecar.team_ledger(), &torn_team).expect("append torn Team tail");
    let sidecar_output = sidecar.run(
        format!(
            "{{\"id\":20,\"operation\":\"runtime.resume\",\"params\":{{\"turn\":{sidecar_turn}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&sidecar_output)[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(
        fs::read(sidecar.runtime_ledger()).expect("reread Runtime after sidecar failure"),
        runtime_before_sidecar_failure
    );
    assert_eq!(
        fs::read(sidecar.team_ledger()).expect("reread torn Team Ledger"),
        torn_team
    );
    assert_eq!(
        fs::read(sidecar.tool_ledger()).expect("reread Tool after sidecar failure"),
        tool_before_sidecar_failure
    );
}

#[test]
fn app_server_acknowledges_a_prepared_delivery_once_and_preserves_failures() {
    let missing = TempTree::new();
    let missing_output = missing.run(
        concat!(
            "{\"id\":1,\"operation\":\"runtime.acknowledge\",\"params\":{\"delivery\":1}}\n",
            "{\"id\":2,\"operation\":\"runtime.acknowledge\",\"params\":{\"delivery\":0}}\n",
        )
        .as_bytes(),
    );
    let missing_results = responses(&missing_output);
    assert_eq!(
        missing_results[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(missing_results[1]["error"]["category"], "invalid_value");
    assert!(!missing.runtime_ledger().exists());

    let temp = TempTree::new();
    let delivery = temp.create_prepared_runtime();
    let before = fs::read(temp.runtime_ledger()).expect("read prepared Runtime Ledger");
    let wrong = delivery.checked_add(1).expect("next delivery identifier");
    let output = temp.run(
        format!(
            concat!(
                "{{\"id\":3,\"operation\":\"runtime.acknowledge\",\"params\":{{\"delivery\":{wrong}}}}}\n",
                "{{\"id\":4,\"operation\":\"runtime.status\"}}\n",
                "{{\"id\":5,\"operation\":\"runtime.acknowledge\",\"params\":{{\"delivery\":{delivery}}}}}\n",
                "{{\"id\":6,\"operation\":\"runtime.status\"}}\n",
                "{{\"id\":7,\"operation\":\"runtime.acknowledge\",\"params\":{{\"delivery\":{delivery}}}}}\n",
            ),
            wrong = wrong,
            delivery = delivery,
        )
        .as_bytes(),
    );
    let results = responses(&output);
    assert_eq!(results[0]["error"]["category"], "unknown_delivery");
    assert_eq!(results[1]["result"]["status"], "reconciliation_required");
    assert_eq!(results[1]["result"]["delivery"], delivery);
    assert_eq!(results[2]["result"]["status"], "acknowledged");
    assert_eq!(results[2]["result"]["delivery"], delivery);
    assert_eq!(results[3]["result"]["status"], "ready");
    assert_eq!(results[4]["result"]["status"], "already_acknowledged");
    assert_eq!(
        results[2]["result"]["ledger"],
        results[4]["result"]["ledger"]
    );
    assert_ne!(
        fs::read(temp.runtime_ledger()).expect("read acknowledged Runtime Ledger"),
        before
    );
    let recovered = RuntimeKernel::open(temp.runtime_ledger())
        .expect("recover acknowledged App Server Runtime");
    assert_eq!(recovered.snapshot().status, RecoveryStatus::Ready);
    assert!(!temp.team_ledger().exists());
    assert!(!temp.tool_ledger().exists());
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());

    let torn = TempTree::new();
    let torn_delivery = torn.create_prepared_runtime();
    let mut torn_bytes = fs::read(torn.runtime_ledger()).expect("read Runtime before torn ack");
    torn_bytes.extend_from_slice(b"bad");
    fs::write(torn.runtime_ledger(), &torn_bytes).expect("append torn Runtime ack tail");
    let response = torn.run(
        format!(
            "{{\"id\":8,\"operation\":\"runtime.acknowledge\",\"params\":{{\"delivery\":{torn_delivery}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&response)[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(
        fs::read(torn.runtime_ledger()).expect("read unmodified torn Runtime ack Ledger"),
        torn_bytes
    );
}

#[test]
fn app_server_recovers_prepared_delivery_text_without_writing_or_repairing() {
    let missing = TempTree::new();
    let missing_output =
        missing.run(b"{\"id\":1,\"operation\":\"runtime.delivery\",\"params\":{\"delivery\":1}}\n");
    assert_eq!(
        responses(&missing_output)[0]["error"]["category"],
        "unknown_delivery"
    );
    assert!(!missing.runtime_ledger().exists());

    let temp = TempTree::new();
    let delivery = temp.create_prepared_runtime();
    let before = fs::read(temp.runtime_ledger()).expect("read Runtime before delivery recovery");
    let output = temp.run(
        format!(
            concat!(
                "{{\"id\":2,\"operation\":\"runtime.delivery\",\"params\":{{\"delivery\":{wrong}}}}}\n",
                "{{\"id\":3,\"operation\":\"runtime.delivery\",\"params\":{{\"delivery\":{delivery}}}}}\n",
            ),
            wrong = delivery + 1,
            delivery = delivery,
        )
        .as_bytes(),
    );
    let results = responses(&output);
    assert_eq!(results[0]["error"]["category"], "unknown_delivery");
    assert_eq!(results[1]["result"]["status"], "prepared");
    assert_eq!(results[1]["result"]["delivery"], delivery);
    assert_eq!(results[1]["result"]["turn"], 1);
    assert_eq!(
        results[1]["result"]["text"],
        "simulated: private App Server prepared input"
    );
    assert_eq!(
        fs::read(temp.runtime_ledger()).expect("read Runtime after delivery recovery"),
        before
    );

    let torn = TempTree::new();
    let torn_delivery = torn.create_prepared_runtime();
    let mut torn_bytes = fs::read(torn.runtime_ledger()).expect("read Runtime before torn tail");
    torn_bytes.extend_from_slice(b"bad");
    fs::write(torn.runtime_ledger(), &torn_bytes).expect("append torn Runtime tail");
    let response = torn.run(
        format!(
            "{{\"id\":4,\"operation\":\"runtime.delivery\",\"params\":{{\"delivery\":{torn_delivery}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&response)[0]["error"]["category"],
        "runtime_unavailable"
    );
    assert_eq!(
        fs::read(torn.runtime_ledger()).expect("read unmodified torn Runtime"),
        torn_bytes
    );
    assert!(!torn.team_ledger().exists());
    assert!(!torn.tool_ledger().exists());
    assert!(!torn.user_config().exists());
    assert!(!torn.project_config().exists());
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
fn app_server_delegates_and_recovers_the_team_acknowledgement_boundary() {
    let temp = TempTree::new();
    temp.create_agent_state();
    fs::create_dir_all(
        temp.project_config()
            .parent()
            .expect("project Config directory"),
    )
    .expect("create project Config directory");
    let config = concat!(
        "schema_version = 2\n",
        "[agent]\n",
        "default_model_preset = \"child-default\"\n",
        "[model_presets.child-default]\n",
        "provider = \"simulator\"\n",
        "model = \"deterministic-v1\"\n",
        "dialect = \"responses\"\n",
    );
    fs::write(temp.project_config(), config).expect("write child default Config");
    let config_before = fs::read(temp.project_config()).expect("read child default Config");
    let runtime_before =
        fs::read(temp.runtime_ledger()).expect("read Runtime Ledger before delegate");
    let tool_before = fs::read(temp.tool_ledger()).expect("read Tool Ledger before delegate");
    let private_title = "private delegated App Server task";
    let first = temp.run(
        format!(
            concat!(
                "{{\"id\":1,\"operation\":\"agent.delegate\",\"params\":{{",
                "\"title\":\"{private_title}\",\"token_budget\":500,\"tool_budget\":1,",
                "\"capabilities\":[{{\"kind\":\"workspace_read\"}}]}}}}\n"
            ),
            private_title = private_title,
        )
        .as_bytes(),
    );
    let first_text = String::from_utf8(first.stdout.clone()).expect("UTF-8 delegate response");
    assert!(!first_text.contains(private_title));
    let first = responses(&first);
    assert_eq!(
        first[0]["result"]["status"],
        "committed_awaiting_acknowledgement"
    );
    assert_eq!(first[0]["result"]["outcome"]["kind"], "delegated");
    assert_eq!(first[0]["result"]["outcome"]["agent"], 2);
    let operation = first[0]["result"]["operation"]
        .as_u64()
        .expect("Team operation ID");

    let team_pending =
        fs::read(temp.team_ledger()).expect("read pending Team Ledger before headless open");
    let headless = temp.headless_output("must not acknowledge a pending lifecycle operation");
    assert!(!headless.status.success());
    assert!(String::from_utf8_lossy(&headless.stdout).is_empty());
    assert!(!String::from_utf8_lossy(&headless.stderr).contains(private_title));
    assert_eq!(
        fs::read(temp.team_ledger()).expect("reread pending Team Ledger after headless open"),
        team_pending
    );

    let blocked = temp.run(
        concat!(
            "{\"id\":2,\"operation\":\"agent.list\"}\n",
            "{\"id\":3,\"operation\":\"agent.delegate\",\"params\":{",
            "\"title\":\"blocked private task\",\"token_budget\":100,\"tool_budget\":1}}\n",
        )
        .as_bytes(),
    );
    let blocked_text = String::from_utf8(blocked.stdout.clone()).expect("UTF-8 blocked response");
    assert!(!blocked_text.contains(private_title));
    assert!(!blocked_text.contains("blocked private task"));
    let blocked = responses(&blocked);
    assert_eq!(
        blocked[0]["result"]["pending_operations"][0]["operation"],
        operation
    );
    assert_eq!(
        blocked[1]["error"]["category"],
        "team_acknowledgement_required"
    );
    assert_eq!(blocked[1]["error"]["operation"], operation);

    let acknowledged = temp.run(
        format!(
            "{{\"id\":4,\"operation\":\"agent.acknowledge\",\"params\":{{\"operation\":{operation}}}}}\n"
        )
        .as_bytes(),
    );
    let acknowledged = responses(&acknowledged);
    assert_eq!(acknowledged[0]["result"]["status"], "acknowledged");

    let reopened = responses(&temp.run(b"{\"id\":5,\"operation\":\"agent.list\"}\n"));
    assert_eq!(
        reopened[0]["result"]["pending_operations"],
        Value::Array(Vec::new())
    );
    let agents = reopened[0]["result"]["team"]["agents"]
        .as_array()
        .expect("recovered Agents");
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0]["status"], "active");
    assert_eq!(agents[1]["status"], "active");
    assert_eq!(agents[1]["token_budget"], 500);
    assert_eq!(agents[1]["tool_budget"], 1);
    assert_eq!(agents[1]["capability_count"], 1);
    assert_eq!(agents[1]["scope_count"], 1);
    assert_eq!(agents[1]["inherited_model_preset"], "child-default");
    assert_eq!(agents[0]["inherited_model_preset"], Value::Null);
    assert_eq!(
        fs::read(temp.project_config()).expect("reread child default Config"),
        config_before
    );
    fs::remove_file(temp.project_config()).expect("remove child default Config fixture");
    assert_eq!(
        fs::read(temp.runtime_ledger()).expect("read Runtime Ledger after delegate"),
        runtime_before
    );
    temp.run_headless("root continues while delegated Agent is active");
    let runtime_after_root_turn =
        fs::read(temp.runtime_ledger()).expect("read Runtime Ledger after root Turn");

    let private_message = "private delegated App Server message";
    let message = temp.run(
        format!(
            "{{\"id\":6,\"operation\":\"agent.message\",\"params\":{{\"agent\":2,\"recipient\":1,\"body\":\"{private_message}\"}}}}\n"
        )
        .as_bytes(),
    );
    let message_text = String::from_utf8(message.stdout.clone()).expect("UTF-8 message response");
    assert!(!message_text.contains(private_message));
    let message = responses(&message);
    assert_eq!(message[0]["result"]["outcome"]["kind"], "message_accepted");
    let message_operation = message[0]["result"]["operation"]
        .as_u64()
        .expect("message Team operation ID");
    let message_recovery = responses(&temp.run(b"{\"id\":7,\"operation\":\"agent.list\"}\n"));
    assert_eq!(message_recovery[0]["result"]["team"]["message_count"], 2);
    assert_eq!(
        message_recovery[0]["result"]["pending_operations"][0]["operation"],
        message_operation
    );
    let message_ack = responses(&temp.run(
        format!(
            "{{\"id\":8,\"operation\":\"agent.acknowledge\",\"params\":{{\"operation\":{message_operation}}}}}\n"
        )
        .as_bytes(),
    ));
    assert_eq!(message_ack[0]["result"]["status"], "acknowledged");

    let private_child_outcome = "private child completion outcome";
    let private_child_evidence = "private child completion evidence";
    let child_complete = temp.run(
        format!(
            concat!(
                "{{\"id\":9,\"operation\":\"agent.complete\",\"params\":{{\"agent\":2,",
                "\"outcome\":\"{private_child_outcome}\",",
                "\"evidence\":[\"{private_child_evidence}\"]}}}}\n"
            ),
            private_child_outcome = private_child_outcome,
            private_child_evidence = private_child_evidence,
        )
        .as_bytes(),
    );
    let child_complete_text =
        String::from_utf8(child_complete.stdout.clone()).expect("UTF-8 child complete response");
    assert!(!child_complete_text.contains(private_child_outcome));
    assert!(!child_complete_text.contains(private_child_evidence));
    let child_complete = responses(&child_complete);
    assert_eq!(
        child_complete[0]["result"]["outcome"]["kind"],
        "state_changed"
    );
    assert_eq!(child_complete[0]["result"]["outcome"]["agent"], 2);
    let child_complete_operation = child_complete[0]["result"]["operation"]
        .as_u64()
        .expect("child complete Team operation ID");
    let child_complete_ack = responses(&temp.run(
        format!(
            "{{\"id\":10,\"operation\":\"agent.acknowledge\",\"params\":{{\"operation\":{child_complete_operation}}}}}\n"
        )
        .as_bytes(),
    ));
    assert_eq!(child_complete_ack[0]["result"]["status"], "acknowledged");

    let private_outcome = "private root completion outcome";
    let private_evidence = "private completion evidence";
    let complete = temp.run(
        format!(
            concat!(
                "{{\"id\":11,\"operation\":\"agent.complete\",\"params\":{{",
                "\"outcome\":\"{private_outcome}\",\"evidence\":[\"{private_evidence}\"]}}}}\n"
            ),
            private_outcome = private_outcome,
            private_evidence = private_evidence,
        )
        .as_bytes(),
    );
    let complete_text =
        String::from_utf8(complete.stdout.clone()).expect("UTF-8 complete response");
    assert!(!complete_text.contains(private_outcome));
    assert!(!complete_text.contains(private_evidence));
    let complete = responses(&complete);
    assert_eq!(complete[0]["result"]["outcome"]["kind"], "state_changed");
    assert_eq!(complete[0]["result"]["outcome"]["agent"], 1);
    let complete_operation = complete[0]["result"]["operation"]
        .as_u64()
        .expect("complete Team operation ID");
    let complete_ack = responses(&temp.run(
        format!(
            "{{\"id\":12,\"operation\":\"agent.acknowledge\",\"params\":{{\"operation\":{complete_operation}}}}}\n"
        )
        .as_bytes(),
    ));
    assert_eq!(complete_ack[0]["result"]["status"], "acknowledged");
    let terminal = responses(&temp.run(b"{\"id\":13,\"operation\":\"agent.list\"}\n"));
    let terminal_agents = terminal[0]["result"]["team"]["agents"]
        .as_array()
        .expect("terminal Agents");
    assert_eq!(terminal_agents[0]["status"], "succeeded");
    assert_eq!(terminal_agents[1]["status"], "succeeded");
    assert_eq!(
        terminal[0]["result"]["pending_operations"],
        Value::Array(Vec::new())
    );
    assert_eq!(
        fs::read(temp.runtime_ledger()).expect("read Runtime Ledger after delegate"),
        runtime_after_root_turn
    );
    assert_eq!(
        fs::read(temp.tool_ledger()).expect("read Tool Ledger after delegate"),
        tool_before
    );
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());
}

#[test]
fn app_server_fails_and_cancels_agents_durably_without_echoing_reasons() {
    let temp = TempTree::new();
    temp.create_agent_state();
    let runtime_before = fs::read(temp.runtime_ledger()).expect("read Runtime Ledger before fail");
    let tool_before = fs::read(temp.tool_ledger()).expect("read Tool Ledger before fail");
    let private_reason = "private App Server Agent failure reason";
    let failed = temp.run(
        format!(
            "{{\"id\":1,\"operation\":\"agent.fail\",\"params\":{{\"reason\":\"{private_reason}\"}}}}\n"
        )
        .as_bytes(),
    );
    let failed_text = String::from_utf8(failed.stdout.clone()).expect("UTF-8 fail response");
    assert!(!failed_text.contains(private_reason));
    let failed = responses(&failed);
    assert_eq!(failed[0]["result"]["outcome"]["kind"], "state_changed");
    let operation = failed[0]["result"]["operation"]
        .as_u64()
        .expect("fail Team operation ID");
    let reopened = responses(&temp.run(b"{\"id\":2,\"operation\":\"agent.list\"}\n"));
    assert_eq!(
        reopened[0]["result"]["team"]["agents"][0]["status"],
        "failed"
    );
    assert_eq!(
        reopened[0]["result"]["pending_operations"][0]["operation"],
        operation
    );
    let ack = responses(&temp.run(
        format!(
            "{{\"id\":3,\"operation\":\"agent.acknowledge\",\"params\":{{\"operation\":{operation}}}}}\n"
        )
        .as_bytes(),
    ));
    assert_eq!(ack[0]["result"]["status"], "acknowledged");
    assert_eq!(
        fs::read(temp.runtime_ledger()).expect("read Runtime Ledger after fail"),
        runtime_before
    );
    assert_eq!(
        fs::read(temp.tool_ledger()).expect("read Tool Ledger after fail"),
        tool_before
    );
    assert!(!temp.user_config().exists());
    assert!(!temp.project_config().exists());

    let cancelled = TempTree::new();
    cancelled.create_agent_state();
    let runtime_before =
        fs::read(cancelled.runtime_ledger()).expect("read Runtime Ledger before cancel");
    let tool_before = fs::read(cancelled.tool_ledger()).expect("read Tool Ledger before cancel");
    let private_reason = "private App Server Agent cancellation reason";
    let response = cancelled.run(
        format!(
            "{{\"id\":4,\"operation\":\"agent.cancel\",\"params\":{{\"reason\":\"{private_reason}\"}}}}\n"
        )
        .as_bytes(),
    );
    let response_text = String::from_utf8(response.stdout.clone()).expect("UTF-8 cancel response");
    assert!(!response_text.contains(private_reason));
    let response = responses(&response);
    let operation = response[0]["result"]["operation"]
        .as_u64()
        .expect("cancel Team operation ID");
    let reopened = responses(&cancelled.run(b"{\"id\":5,\"operation\":\"agent.list\"}\n"));
    assert_eq!(
        reopened[0]["result"]["team"]["agents"][0]["status"],
        "cancelled"
    );
    assert_eq!(
        reopened[0]["result"]["pending_operations"][0]["operation"],
        operation
    );
    let ack = responses(&cancelled.run(
        format!(
            "{{\"id\":6,\"operation\":\"agent.acknowledge\",\"params\":{{\"operation\":{operation}}}}}\n"
        )
        .as_bytes(),
    ));
    assert_eq!(ack[0]["result"]["status"], "acknowledged");
    assert_eq!(
        fs::read(cancelled.runtime_ledger()).expect("read Runtime Ledger after cancel"),
        runtime_before
    );
    assert_eq!(
        fs::read(cancelled.tool_ledger()).expect("read Tool Ledger after cancel"),
        tool_before
    );
    assert!(!cancelled.user_config().exists());
    assert!(!cancelled.project_config().exists());
}

#[test]
fn app_server_rejects_agent_lifecycle_changes_while_provider_recovery_is_pending() {
    let temp = TempTree::new();
    temp.create_blocked_product_runtime(ProviderUnavailableStage::BeforeResponse);
    let runtime_before = fs::read(temp.runtime_ledger()).expect("read blocked Runtime Ledger");
    let team_before = fs::read(temp.team_ledger()).expect("read blocked Team Ledger");
    let tool_before = fs::read(temp.tool_ledger()).expect("read blocked Tool Ledger");
    let private_reason = "private busy Agent cancellation reason";

    let response = temp.run(
        format!(
            "{{\"id\":1,\"operation\":\"agent.cancel\",\"params\":{{\"reason\":\"{private_reason}\"}}}}\n"
        )
        .as_bytes(),
    );
    let response_text = String::from_utf8(response.stdout.clone()).expect("UTF-8 busy response");
    assert!(!response_text.contains(private_reason));
    let response = responses(&response);
    assert_eq!(response[0]["error"]["category"], "runtime_busy");
    assert_eq!(
        fs::read(temp.runtime_ledger()).expect("reread blocked Runtime Ledger"),
        runtime_before
    );
    assert_eq!(
        fs::read(temp.team_ledger()).expect("reread blocked Team Ledger"),
        team_before
    );
    assert_eq!(
        fs::read(temp.tool_ledger()).expect("reread blocked Tool Ledger"),
        tool_before
    );

    let raced = TempTree::new();
    raced.create_agent_state();
    let pending = responses(&raced.run(
        b"{\"id\":2,\"operation\":\"agent.message\",\"params\":{\"body\":\"private raced message\"}}\n",
    ));
    let operation = pending[0]["result"]["operation"]
        .as_u64()
        .expect("pending Team operation ID");
    raced.create_blocked_runtime(ProviderUnavailableStage::BeforeResponse);
    let runtime_before = fs::read(raced.runtime_ledger()).expect("read raced Runtime Ledger");
    let team_before = fs::read(raced.team_ledger()).expect("read raced Team Ledger");
    let tool_before = fs::read(raced.tool_ledger()).expect("read raced Tool Ledger");
    let response = responses(&raced.run(
        format!(
            "{{\"id\":3,\"operation\":\"agent.acknowledge\",\"params\":{{\"operation\":{operation}}}}}\n"
        )
        .as_bytes(),
    ));
    assert_eq!(response[0]["error"]["category"], "runtime_busy");
    assert_eq!(
        fs::read(raced.runtime_ledger()).expect("reread raced Runtime Ledger"),
        runtime_before
    );
    assert_eq!(
        fs::read(raced.team_ledger()).expect("reread raced Team Ledger"),
        team_before
    );
    assert_eq!(
        fs::read(raced.tool_ledger()).expect("reread raced Tool Ledger"),
        tool_before
    );
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

#[test]
fn app_server_reconciles_tool_effects_without_reexecution_or_cross_ledger_writes() {
    let missing = TempTree::new();
    let missing_output = missing.run(
        b"{\"id\":1,\"operation\":\"tool.reconcile\",\"params\":{\"outcome\":\"failed\",\"call\":1}}\n",
    );
    assert_eq!(
        responses(&missing_output)[0]["error"]["category"],
        "tool_unavailable"
    );
    assert!(!missing.runtime_ledger().exists());
    assert!(!missing.team_ledger().exists());
    assert!(!missing.tool_ledger().exists());

    let succeeded = TempTree::new();
    let call = succeeded.create_reconciliation_tool_state();
    let runtime_before =
        fs::read(succeeded.runtime_ledger()).expect("read Runtime before Tool reconcile");
    let team_before = fs::read(succeeded.team_ledger()).expect("read Team before Tool reconcile");
    let tool_before = fs::read(succeeded.tool_ledger()).expect("read Tool before reconcile");
    let mut server = succeeded.spawn();
    let invalid = server.request(&format!(
        r#"{{"id":2,"operation":"tool.reconcile","params":{{"outcome":"succeeded","call":{call},"result_sha256":"ABC"}}}}"#
    ));
    assert_eq!(invalid["error"]["category"], "invalid_value");
    assert_eq!(
        fs::read(succeeded.tool_ledger()).expect("read Tool after invalid digest"),
        tool_before
    );
    let unknown = server.request(&format!(
        r#"{{"id":3,"operation":"tool.reconcile","params":{{"outcome":"failed","call":{}}}}}"#,
        call + 1
    ));
    assert_eq!(unknown["error"]["category"], "unknown_tool_call");
    assert_eq!(
        fs::read(succeeded.tool_ledger()).expect("read Tool after unknown call"),
        tool_before
    );
    let digest = "11".repeat(32);
    let reconciled = server.request(&format!(
        r#"{{"id":4,"operation":"tool.reconcile","params":{{"outcome":"succeeded","call":{call},"result_sha256":"{digest}"}}}}"#
    ));
    assert_eq!(reconciled["result"]["call"], call);
    assert_eq!(reconciled["result"]["status"], "succeeded");
    assert_eq!(reconciled["result"]["result_sha256"], digest);
    let tool_after = fs::read(succeeded.tool_ledger()).expect("read reconciled Tool Ledger");
    assert_ne!(tool_after, tool_before);
    let duplicate = server.request(&format!(
        r#"{{"id":5,"operation":"tool.reconcile","params":{{"outcome":"failed","call":{call}}}}}"#
    ));
    assert_eq!(duplicate["result"], reconciled["result"]);
    assert_eq!(
        fs::read(succeeded.tool_ledger()).expect("read Tool after duplicate reconcile"),
        tool_after
    );
    server.finish();
    assert_eq!(
        fs::read(succeeded.runtime_ledger()).expect("read Runtime after Tool reconcile"),
        runtime_before
    );
    assert_eq!(
        fs::read(succeeded.team_ledger()).expect("read Team after Tool reconcile"),
        team_before
    );

    let failed = TempTree::new();
    let failed_call = failed.create_reconciliation_tool_state();
    let failed_runtime_before =
        fs::read(failed.runtime_ledger()).expect("read Runtime before failed reconcile");
    let failed_team_before =
        fs::read(failed.team_ledger()).expect("read Team before failed reconcile");
    let before = fs::read(failed.tool_ledger()).expect("read Tool before failed reconcile");
    let mut server = failed.spawn();
    let response = server.request(&format!(
        r#"{{"id":6,"operation":"tool.reconcile","params":{{"outcome":"failed","call":{failed_call}}}}}"#
    ));
    assert_eq!(response["result"]["status"], "failed");
    assert_eq!(response["result"]["result_sha256"], Value::Null);
    let after = fs::read(failed.tool_ledger()).expect("read Tool after failed reconcile");
    assert_ne!(after, before);
    let output = response.to_string();
    assert!(!output.contains("private ambiguous App Server Tool result"));
    assert!(!output.contains("private-reconciliation-identity"));
    assert!(!output.contains("private reconciliation argument"));
    server.finish();
    assert_eq!(
        fs::read(failed.runtime_ledger()).expect("read Runtime after failed reconcile"),
        failed_runtime_before
    );
    assert_eq!(
        fs::read(failed.team_ledger()).expect("read Team after failed reconcile"),
        failed_team_before
    );

    let torn = TempTree::new();
    let torn_call = torn.create_reconciliation_tool_state();
    let mut torn_bytes = fs::read(torn.tool_ledger()).expect("read Tool before torn reconcile");
    torn_bytes.extend_from_slice(b"bad");
    fs::write(torn.tool_ledger(), &torn_bytes).expect("append torn Tool reconcile tail");
    let response = torn.run(
        format!(
            "{{\"id\":7,\"operation\":\"tool.reconcile\",\"params\":{{\"outcome\":\"failed\",\"call\":{torn_call}}}}}\n"
        )
        .as_bytes(),
    );
    assert_eq!(
        responses(&response)[0]["error"]["category"],
        "tool_unavailable"
    );
    assert_eq!(
        fs::read(torn.tool_ledger()).expect("read unmodified torn Tool reconcile Ledger"),
        torn_bytes
    );
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
fn app_server_starter_draft_validates_commits_and_survives_reopen() {
    let temp = TempTree::new();
    fs::create_dir_all(temp.user_config().parent().expect("user Config parent"))
        .expect("create user Config parent");
    fs::write(
        temp.user_config(),
        "schema_version = 1\n\n[providers.openai-main]\ntemplate = \"openai\"\ncredential = \"private-app-server-starter-reference\"\n",
    )
    .expect("write starter Provider profile");
    let requests = concat!(
        "{\"id\":1,\"operation\":\"config.starter.begin\",\"params\":{\"scope\":\"user\",\"preset\":\"frontier\",\"provider\":\"openai-main\",\"catalog_key\":\"openai/gpt-5.6-sol\"}}\n",
        "{\"id\":2,\"operation\":\"config.draft.validate\",\"params\":{\"draft_id\":1}}\n",
        "{\"id\":3,\"operation\":\"config.draft.commit\",\"params\":{\"draft_id\":1}}\n",
        "{\"id\":4,\"operation\":\"config.get\",\"params\":{\"path\":\"model_presets.frontier.dialect\"}}\n",
    );

    let output = temp.run(requests.as_bytes());
    let results = responses(&output);
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["result"]["draft_id"], 1);
    assert_eq!(results[0]["result"]["scope"], "user");
    assert_eq!(
        results[1]["result"]["changes"].as_array().map(Vec::len),
        Some(8)
    );
    assert_eq!(results[2]["result"]["written"], true);
    assert_eq!(results[3]["result"]["entry"]["value"]["value"], "responses");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("private-app-server-starter-reference"));
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!temp.runtime_ledger().exists());
    assert!(!temp.team_ledger().exists());
    assert!(!temp.tool_ledger().exists());

    let reopened = temp.run(
        b"{\"id\":5,\"operation\":\"config.get\",\"params\":{\"path\":\"model_presets.frontier.model\"}}\n",
    );
    let reopened = responses(&reopened);
    assert_eq!(
        reopened[0]["result"]["entry"]["value"]["value"],
        "gpt-5.6-sol"
    );
}

#[test]
fn app_server_starter_update_draft_validates_commits_and_survives_reopen() {
    let temp = TempTree::new();
    fs::create_dir_all(temp.user_config().parent().expect("user Config parent"))
        .expect("create user Config parent");
    let before = r#"schema_version = 2

[providers.openai-main]
template = "openai"
credential = "private-app-server-update-reference"
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
    let output = temp.run(
        concat!(
            "{\"id\":1,\"operation\":\"config.starter.update.begin\",\"params\":{\"scope\":\"user\",\"preset\":\"frontier\"}}\n",
            "{\"id\":2,\"operation\":\"config.draft.validate\",\"params\":{\"draft_id\":1}}\n",
            "{\"id\":3,\"operation\":\"config.draft.commit\",\"params\":{\"draft_id\":1}}\n",
            "{\"id\":4,\"operation\":\"config.get\",\"params\":{\"path\":\"model_presets.frontier.starter.seed_revision\"}}\n",
            "{\"id\":5,\"operation\":\"config.get\",\"params\":{\"path\":\"model_presets.frontier.favorite\"}}\n",
            "{\"id\":6,\"operation\":\"config.get\",\"params\":{\"path\":\"model_presets.frontier.starter.catalog_key\"}}\n",
        )
        .as_bytes(),
    );
    let results = responses(&output);
    assert_eq!(results[0]["result"]["draft_id"], 1);
    assert_eq!(results[0]["result"]["scope"], "user");
    assert_eq!(
        results[1]["result"]["changes"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(results[2]["result"]["written"], true);
    assert_eq!(
        results[3]["result"]["entry"]["value"]["value"],
        "2026-08-10.2"
    );
    assert_eq!(results[4]["result"]["entry"]["value"]["value"], true);
    assert_eq!(
        results[5]["result"]["entry"]["value"]["value"],
        "openai/gpt-5.6-sol"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("private-app-server-update-reference"));
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!temp.runtime_ledger().exists());
    assert!(!temp.team_ledger().exists());
    assert!(!temp.tool_ledger().exists());

    let reopened = temp.run(
        b"{\"id\":5,\"operation\":\"config.get\",\"params\":{\"path\":\"model_presets.frontier.model\"}}\n",
    );
    let reopened = responses(&reopened);
    assert_eq!(
        reopened[0]["result"]["entry"]["value"]["value"],
        "gpt-5.6-sol"
    );
}

#[test]
fn app_server_starter_update_conflict_retains_stale_draft_without_overwriting_winner() {
    let temp = TempTree::new();
    fs::create_dir_all(temp.user_config().parent().expect("user Config parent"))
        .expect("create user Config parent");
    fs::write(
        temp.user_config(),
        r#"schema_version = 2
[providers.openai-main]
template = "openai"
dialects = ["responses", "chat_completions"]
[model_presets.frontier]
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
[model_presets.frontier.starter]
catalog_key = "openai/gpt-5.6-sol"
seed_revision = "2026-08-10.1"
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
"#,
    )
    .expect("write stale starter");
    let mut winner = temp.spawn();
    let mut loser = temp.spawn();
    let begin = r#"{"id":1,"operation":"config.starter.update.begin","params":{"scope":"user","preset":"frontier"}}"#;
    let winner_begin = winner.request(begin);
    let loser_begin = loser.request(begin);
    assert_eq!(
        winner_begin["result"]["base_revision"],
        loser_begin["result"]["base_revision"]
    );
    let committed =
        winner.request(r#"{"id":2,"operation":"config.draft.commit","params":{"draft_id":1}}"#);
    assert_eq!(committed["result"]["written"], true);
    let winner_bytes = fs::read(temp.user_config()).expect("winner bytes");

    let conflict =
        loser.request(r#"{"id":2,"operation":"config.draft.commit","params":{"draft_id":1}}"#);
    assert_eq!(conflict["error"]["category"], "revision_conflict");
    let retained =
        loser.request(r#"{"id":3,"operation":"config.draft.validate","params":{"draft_id":1}}"#);
    assert_eq!(retained["error"]["category"], "revision_conflict");
    assert_eq!(
        fs::read(temp.user_config()).expect("bytes after conflict"),
        winner_bytes
    );
    winner.finish();
    loser.finish();
}

#[test]
fn app_server_rejected_starter_keeps_capacity_and_recovers_with_a_valid_profile() {
    let temp = TempTree::new();
    fs::create_dir_all(temp.user_config().parent().expect("user Config parent"))
        .expect("create user Config parent");
    let before = "schema_version = 1\n\n[providers.chat-only]\ntemplate = \"openai\"\ncredential = \"private-chat-reference\"\ndialects = [\"chat_completions\"]\n\n[providers.openai-main]\ntemplate = \"openai\"\ncredential = \"private-openai-reference\"\n";
    fs::write(temp.user_config(), before).expect("write starter recovery profiles");
    let requests = concat!(
        "{\"id\":1,\"operation\":\"config.starter.begin\",\"params\":{\"scope\":\"user\",\"preset\":\"frontier\",\"provider\":\"chat-only\",\"catalog_key\":\"openai/gpt-5.6-sol\"}}\n",
        "{\"id\":2,\"operation\":\"config.starter.begin\",\"params\":{\"scope\":\"user\",\"preset\":\"frontier\",\"provider\":\"openai-main\",\"catalog_key\":\"openai/gpt-5.6-sol\"}}\n",
        "{\"id\":3,\"operation\":\"config.draft.commit\",\"params\":{\"draft_id\":1}}\n",
    );

    let output = temp.run(requests.as_bytes());
    let results = responses(&output);
    assert_eq!(results[0]["error"]["category"], "invalid_value");
    assert_eq!(
        results[0]["error"]["path"],
        "model_presets.frontier.dialect"
    );
    assert_eq!(results[1]["result"]["draft_id"], 1);
    assert_eq!(results[2]["result"]["written"], true);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("private-chat-reference"));
    assert!(!stdout.contains("private-openai-reference"));
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!temp.runtime_ledger().exists());
    assert!(!temp.team_ledger().exists());
    assert!(!temp.tool_ledger().exists());
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
