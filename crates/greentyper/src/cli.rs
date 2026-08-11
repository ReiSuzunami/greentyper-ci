use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use greentyper_core::agent_team::TeamOperationRecord;
use greentyper_core::config::{
    CONFIG_FILE_SCHEMA_VERSION, ConfigDocument, ConfigPaths, ConfigRuntime, ConfigRuntimeError,
    ConfigScope, config_schema,
};
use greentyper_core::model::{DeliveryId, TurnId};
use greentyper_core::pricing::PriceScheduleBook;
use greentyper_core::provider::{ProviderDialect, ProviderError};
use greentyper_core::provider_catalog::ProviderCatalog;
use greentyper_core::runtime::{
    AcknowledgeOutcome, CancelTurnOutcome, PreparedOutput, ProviderToolApproval, RecoveryStatus,
    RuntimeKernel,
};
use greentyper_core::tool_runtime::{
    ToolCallRecord, ToolCallStatus, ToolEffectExecutor, ToolReconciliationDecision, ToolSnapshot,
};
use greentyper_core::usage::{
    RuntimeUsageQuery, RuntimeUsageSnapshot, UsageCursor, UsageError, UsageTimestamp, UsageWindow,
};

use crate::credential_vault::{
    CredentialVault, CredentialVaultError, MAX_SECRET_BYTES, PlatformCredentialVault,
    ProviderCredentialScope, SecretValue,
};
use crate::local_process::{
    LOCAL_ECHO_TOOL, LocalProcessChildMode, LocalProcessError, LocalProcessExecutor,
    LocalProcessSmokeOutcome, LocalProcessSmokeScenario,
};
use crate::presentation::PresentationSmokeError;
use crate::product_driver::{
    ProductDriver, ProductDriverError, ProductInteraction, ProductToolDecision,
    apply_model_preset_to_next_turn, cancel_product_provider_turn, freeze_model_selection,
    has_product_driver_state, inspect_product_tools, reconcile_product_tool,
    request_product_provider_turn_retry,
};
use crate::provider_connection::{ModelsHttpConnectionTester, ProviderConnectionTester};
use crate::provider_http::{
    ConfiguredProvider, ProviderHttpError, ProviderHttpSmokeOutcome, ProviderHttpSmokeScenario,
};

pub fn run(arguments: impl Iterator<Item = String>) -> Result<(), CliError> {
    match parse(arguments)? {
        Command::AppServer { ledger } => {
            let config = open_config_runtime(default_config_paths()?)?;
            let stdin = io::stdin();
            let stdout = io::stdout();
            crate::app_server::run_stdio(stdin.lock(), stdout.lock(), config, ledger)?;
            Ok(())
        }
        Command::Tui { ledger } => {
            crate::terminal::require_interactive()?;
            let mut config = open_config_runtime(default_config_paths()?)?;
            crate::terminal::run(&ledger, &mut config)?;
            Ok(())
        }
        Command::Headless {
            ledger,
            input,
            local_echo,
            dialect,
            preset,
        } => {
            let pending_selection = RuntimeKernel::inspect(&ledger)?.pending_model_selection;
            let config = open_config_runtime(default_config_paths()?)?;
            let mut layers = config.config_layers()?.clone();
            let preset =
                match (preset, pending_selection.as_ref()) {
                    (Some(id), Some(pending)) if id != pending.selection().preset_id() => {
                        return Err(greentyper_core::runtime::RuntimeError::InvalidModelSelection(
                        "explicit Preset conflicts with the pending current-Agent selection",
                    )
                    .into());
                    }
                    (Some(id), _) => Some(id),
                    (None, Some(pending)) => Some(pending.selection().preset_id().to_owned()),
                    (None, None) => None,
                };
            let (profile, dialect, applied_preset) = match preset {
                Some(id) => {
                    let preset = config.model_preset(&id)?;
                    let profile = config.provider_profile(&preset.provider)?;
                    let dialect = apply_model_preset_to_next_turn(&mut layers, &preset);
                    (profile, Some(dialect), Some(preset))
                }
                None => (config.selected_provider_profile()?, dialect, None),
            };
            let usage_windows = config.resolved_usage_windows()?;
            let price_schedules = config.resolved_price_schedules()?;
            if let Some(pending) = pending_selection.as_ref() {
                let applied = freeze_model_selection(
                    &layers,
                    &usage_windows,
                    &price_schedules,
                    applied_preset
                        .as_ref()
                        .expect("pending selection chose a Preset"),
                )?;
                if &applied != pending.selection() {
                    return Err(
                        greentyper_core::runtime::RuntimeError::InvalidModelSelection(
                            "pending Preset changed before the next Turn",
                        )
                        .into(),
                    );
                }
            }
            let selected_model = layers
                .resolve()
                .map_err(|_| {
                    ProviderError::InvalidConfiguration(
                        "selected Provider model configuration is invalid",
                    )
                })?
                .provider_model()
                .value()
                .clone();
            let mut provider = match (profile, dialect) {
                (Some(profile), dialect) => {
                    ConfiguredProvider::for_new_turn_with_preferred_dialect(
                        profile,
                        &selected_model,
                        dialect.unwrap_or(ProviderDialect::Responses),
                        PlatformCredentialVault,
                    )?
                }
                (None, Some(_)) => {
                    return Err(ProviderError::InvalidConfiguration(
                        "simulator Provider cannot select a wire dialect",
                    )
                    .into());
                }
                (None, None) => ConfiguredProvider::for_new_turn(None, PlatformCredentialVault)?,
            };
            let has_product_state = has_product_driver_state(&ledger)?;
            if local_echo || has_product_state {
                provider.enable_local_echo();
                run_product_turn(
                    &ledger,
                    &layers,
                    usage_windows,
                    price_schedules,
                    input,
                    &mut provider,
                )
            } else {
                let mut runtime = open_runtime(&ledger)?;
                let output = runtime.execute_with_observability(
                    &layers,
                    usage_windows,
                    price_schedules,
                    input,
                    &mut provider,
                )?;
                deliver_and_ack(&mut runtime, output)
            }
        }
        Command::Resume { ledger, local_echo } => {
            let has_product_state = has_product_driver_state(&ledger)?;
            if local_echo || has_product_state {
                resume_product_turn(&ledger)
            } else {
                let mut runtime = open_runtime(&ledger)?;
                let mut provider = match runtime.pending_provider_epoch() {
                    Some(epoch) => ConfiguredProvider::from_epoch(epoch, PlatformCredentialVault)?,
                    None => ConfiguredProvider::for_new_turn(None, PlatformCredentialVault)?,
                };
                let output = runtime.resume(&mut provider)?;
                deliver_and_ack(&mut runtime, output)
            }
        }
        Command::Status { ledger } => {
            let snapshot = RuntimeKernel::inspect(&ledger)?;
            print_status(&snapshot.status)
        }
        Command::Stats { ledger, at, query } => {
            let at = match at {
                Some(at) => at,
                None => UsageTimestamp::now()?,
            };
            match query {
                Some(query) => {
                    let report = RuntimeKernel::inspect_usage_report(&ledger, at, query)?;
                    write_json(&report)
                }
                None => {
                    let stats: RuntimeUsageSnapshot = RuntimeKernel::inspect_usage(&ledger, at)?;
                    write_json(&stats)
                }
            }
        }
        Command::Cancel { ledger, turn } => {
            let outcome = if has_product_driver_state(&ledger)? {
                cancel_product_provider_turn(&ledger, turn)?
            } else {
                let mut runtime = RuntimeKernel::open_existing_strict(&ledger)?;
                runtime.cancel_blocked_turn(turn)?
            };
            match outcome {
                CancelTurnOutcome::Durable(_) => write_stdout_line("cancelled")?,
                CancelTurnOutcome::AlreadyCancelled => write_stdout_line("already-cancelled")?,
            }
            Ok(())
        }
        Command::Retry { ledger, turn } => {
            if has_product_driver_state(&ledger)? {
                return retry_product_turn(&ledger, turn);
            }
            let mut runtime = RuntimeKernel::open_existing_strict(&ledger)?;
            runtime.request_blocked_turn_retry(turn)?;
            let epoch = runtime.pending_provider_epoch().ok_or(
                greentyper_core::runtime::RuntimeError::CorruptState(
                    "blocked Turn is missing its frozen Provider Epoch",
                ),
            )?;
            let mut provider = ConfiguredProvider::from_epoch(epoch, PlatformCredentialVault)?;
            let output = runtime.resume(&mut provider)?;
            deliver_and_ack(&mut runtime, output)
        }
        Command::Reconcile { ledger, delivery } => {
            let mut runtime = open_runtime(&ledger)?;
            match runtime.acknowledge(delivery)? {
                AcknowledgeOutcome::Durable(_) => write_stdout_line("reconciled")?,
                AcknowledgeOutcome::AlreadyAcknowledged => {
                    write_stdout_line("already-acknowledged")?
                }
            }
            Ok(())
        }
        Command::Tool(command) => match command {
            ToolCommand::Status { ledger } => {
                let snapshot = inspect_product_tools(&ledger)?;
                write_tool_snapshot(&snapshot)
            }
            ToolCommand::Reconcile {
                ledger,
                call,
                decision,
            } => {
                let record = reconcile_product_tool(&ledger, call, decision)?;
                write_json(&tool_record_json(&record))
            }
        },
        Command::Config(command) => run_config(command),
        Command::Credential(command) => {
            let mut vault = PlatformCredentialVault;
            let stdin = io::stdin();
            let outcome = if stdin.is_terminal()
                && matches!(
                    &command,
                    CredentialCommand::Bind { .. } | CredentialCommand::Replace { .. }
                ) {
                let secret = rpassword::prompt_password("Provider credential: ")?;
                execute_credential_with_secret(
                    &mut vault,
                    command,
                    Some(SecretValue::new(secret.into_bytes())?),
                )?
            } else {
                execute_credential_command(&mut vault, command, &mut stdin.lock())?
            };
            write_stdout_line(outcome.as_str())
        }
        Command::LocalProcessChild { mode } => {
            crate::local_process::run_local_process_child(mode).map_err(CliError::Io)
        }
        Command::LocalProcessSmoke {
            run_dir,
            scenario,
            message,
        } => {
            match crate::local_process::run_smoke(&run_dir, scenario, &message)? {
                LocalProcessSmokeOutcome::Succeeded(output) => {
                    let stdout = io::stdout();
                    let mut stdout = stdout.lock();
                    stdout.write_all(&output)?;
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
                LocalProcessSmokeOutcome::SucceededWithoutOutput => {
                    write_stdout_line("succeeded-existing")?;
                }
                LocalProcessSmokeOutcome::Failed => write_stdout_line("failed")?,
                LocalProcessSmokeOutcome::ReconciliationRequired => {
                    write_stdout_line("reconciliation-required")?;
                }
                LocalProcessSmokeOutcome::ReconciliationRequiredExisting => {
                    write_stdout_line("reconciliation-required-existing")?;
                }
            }
            Ok(())
        }
        Command::ProviderHttpSmoke {
            ledger,
            scenario,
            input,
        } => {
            match crate::provider_http::run_smoke(&ledger, scenario, &input)? {
                ProviderHttpSmokeOutcome::Succeeded(output) => write_stdout_line(&output)?,
                ProviderHttpSmokeOutcome::Unavailable => write_stdout_line("provider-unavailable")?,
            }
            Ok(())
        }
        Command::PresentationSmoke { query } => {
            let view = crate::presentation::build_smoke_view(&query)?;
            write_json(&view)
        }
        Command::Help => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

fn run_config(command: ConfigCommand) -> Result<(), CliError> {
    match command {
        ConfigCommand::Schema => write_json(&serde_json::json!({
            "schema_version": CONFIG_FILE_SCHEMA_VERSION,
            "entries": config_schema(),
        })),
        ConfigCommand::Catalog => write_json(ProviderCatalog::release()),
        ConfigCommand::AcceptStarter {
            paths,
            scope,
            preset,
            provider,
            catalog_key,
            dry_run,
        } => {
            let mut runtime = open_config_runtime(paths)?;
            let draft = runtime.begin_model_starter(scope, &preset, &provider, &catalog_key)?;
            let commit = runtime.commit(draft, dry_run)?;
            write_json(&commit)
        }
        ConfigCommand::Get { paths, path } => {
            let runtime = open_config_runtime(paths)?;
            let entry = runtime.get_effective(&path)?;
            write_json(&serde_json::json!({
                "path": path,
                "entry": entry,
                "status": runtime.status(),
            }))
        }
        ConfigCommand::Set {
            paths,
            scope,
            path,
            value,
            dry_run,
        } => {
            let mut runtime = open_config_runtime(paths)?;
            let mut draft = runtime.begin_draft(scope)?;
            draft.set_raw(&path, &value)?;
            let commit = runtime.commit(draft, dry_run)?;
            write_json(&commit)
        }
        ConfigCommand::Reset {
            paths,
            scope,
            path,
            dry_run,
        } => {
            let mut runtime = open_config_runtime(paths)?;
            let mut draft = runtime.begin_draft(scope)?;
            draft.reset(&path)?;
            let commit = runtime.commit(draft, dry_run)?;
            write_json(&commit)
        }
        ConfigCommand::Repair { paths, scope } => {
            let mut runtime = open_config_runtime(paths)?;
            let commit = runtime.restore_backup(scope)?;
            write_json(&commit)
        }
        ConfigCommand::TestProvider { paths } => {
            let runtime = open_config_runtime(paths)?;
            let profile =
                runtime
                    .selected_provider_profile()?
                    .ok_or(ProviderError::InvalidConfiguration(
                        "selected simulator profile has no external connection",
                    ))?;
            let vault = PlatformCredentialVault;
            let mut tester = ModelsHttpConnectionTester::new(&vault);
            write_json(&tester.test(&profile))
        }
    }
}

fn open_config_runtime(paths: ConfigPaths) -> Result<ConfigRuntime, CliError> {
    ConfigRuntime::open(paths, ConfigDocument::empty()).map_err(CliError::Config)
}

fn write_json(value: &impl serde::Serialize) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value).map_err(CliError::Json)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn write_tool_snapshot(snapshot: &ToolSnapshot) -> Result<(), CliError> {
    let calls: Vec<_> = snapshot.calls.iter().map(tool_record_json).collect();
    write_json(&serde_json::json!({
        "ledger_head": {
            "transaction": snapshot.ledger_head.transaction,
            "sequence": snapshot.ledger_head.sequence,
        },
        "recovered_tail_bytes": snapshot.recovered_tail_bytes,
        "calls": calls,
    }))
}

fn tool_record_json(record: &ToolCallRecord) -> serde_json::Value {
    serde_json::json!({
        "call": record.call.get(),
        "identity": record.identity,
        "agent": record.agent.get(),
        "tool": record.tool,
        "status": tool_status_name(record.status),
        "result_sha256": record.result_digest.map(encode_sha256),
        "reason": record.reason,
    })
}

const fn tool_status_name(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::AwaitingApproval => "awaiting_approval",
        ToolCallStatus::Denied => "denied",
        ToolCallStatus::ReconciliationRequired => "reconciliation_required",
        ToolCallStatus::Succeeded => "succeeded",
        ToolCallStatus::Failed => "failed",
    }
}

fn encode_sha256(digest: [u8; 32]) -> String {
    digest
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn deliver_and_ack(
    runtime: &mut RuntimeKernel,
    output: greentyper_core::runtime::PreparedOutput,
) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    deliver_and_ack_to(runtime, output, &mut stdout)
}

fn deliver_and_ack_to(
    runtime: &mut RuntimeKernel,
    output: greentyper_core::runtime::PreparedOutput,
    writer: &mut impl Write,
) -> Result<(), CliError> {
    writer.write_all(output.text().as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    runtime.acknowledge(output.delivery())?;
    Ok(())
}

fn run_product_turn(
    ledger: &Path,
    layers: &greentyper_core::config::ConfigLayers,
    usage_windows: Vec<UsageWindow>,
    price_schedules: PriceScheduleBook,
    input: String,
    provider: &mut ConfiguredProvider<PlatformCredentialVault>,
) -> Result<(), CliError> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    let mut interaction = CliProductInteraction {
        input: stdin.lock(),
        output: stderr.lock(),
    };
    let executor = LocalProcessExecutor::current()?;
    let mut driver = ProductDriver::open_with_executor(ledger, executor, &mut interaction)?;
    let output = driver.execute_with_observability(
        layers,
        usage_windows,
        price_schedules,
        input,
        provider,
        &mut interaction,
    )?;
    deliver_product_and_ack(&mut driver, output)
}

fn resume_product_turn(ledger: &Path) -> Result<(), CliError> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    let mut interaction = CliProductInteraction {
        input: stdin.lock(),
        output: stderr.lock(),
    };
    let executor = LocalProcessExecutor::current()?;
    let mut driver = ProductDriver::open_with_executor(ledger, executor, &mut interaction)?;
    let mut provider = match driver.pending_provider_epoch() {
        Some(epoch) => ConfiguredProvider::from_epoch(epoch, PlatformCredentialVault)?,
        None => ConfiguredProvider::for_new_turn(None, PlatformCredentialVault)?,
    };
    provider.enable_local_echo();
    let output = driver.resume(&mut provider, &mut interaction)?;
    deliver_product_and_ack(&mut driver, output)
}

fn retry_product_turn(ledger: &Path, turn: TurnId) -> Result<(), CliError> {
    request_product_provider_turn_retry(ledger, turn)?;
    let stdin = io::stdin();
    let stderr = io::stderr();
    let mut interaction = CliProductInteraction {
        input: stdin.lock(),
        output: stderr.lock(),
    };
    let executor = LocalProcessExecutor::current()?;
    let mut driver = ProductDriver::open_existing_for_provider_recovery(ledger, turn, executor)?;
    let epoch = driver.pending_provider_epoch().ok_or(
        greentyper_core::runtime::RuntimeError::CorruptState(
            "blocked Turn is missing its frozen Provider Epoch",
        ),
    )?;
    let mut provider = ConfiguredProvider::from_epoch(epoch, PlatformCredentialVault)?;
    provider.enable_local_echo();
    let output = driver.resume(&mut provider, &mut interaction)?;
    deliver_product_and_ack(&mut driver, output)
}

fn deliver_product_and_ack<E: ToolEffectExecutor>(
    driver: &mut ProductDriver<E>,
    output: PreparedOutput,
) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    deliver_product_and_ack_to(driver, output, &mut stdout)
}

fn deliver_product_and_ack_to<E: ToolEffectExecutor>(
    driver: &mut ProductDriver<E>,
    output: PreparedOutput,
    writer: &mut impl Write,
) -> Result<(), CliError> {
    writer.write_all(output.text().as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    driver.acknowledge(output.delivery())?;
    Ok(())
}

struct CliProductInteraction<R, W> {
    input: R,
    output: W,
}

impl<R: BufRead, W: Write> ProductInteraction for CliProductInteraction<R, W> {
    fn present_team_operation(&mut self, record: TeamOperationRecord) -> io::Result<()> {
        self.write_event(&serde_json::json!({
            "event": "team-operation-committed",
            "operation": record.operation.get(),
            "transaction": record.transaction.get(),
            "first_sequence": record.first_sequence.get(),
            "last_sequence": record.last_sequence.get(),
            "event_count": record.event_count,
        }))
    }

    fn decide_tool(&mut self, approval: &ProviderToolApproval) -> io::Result<ProductToolDecision> {
        let arguments: serde_json::Value =
            serde_json::from_str(approval.arguments().canonical_json()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "Tool arguments are invalid")
            })?;
        let filesystem_reads: Vec<_> = approval.resources().filesystem_reads().collect();
        let filesystem_writes: Vec<_> = approval.resources().filesystem_writes().collect();
        let network_targets: Vec<_> = approval.resources().network_targets().collect();
        self.write_event(&serde_json::json!({
            "event": "approval-required",
            "call": approval.call().get(),
            "tool": approval.tool(),
            "identity": approval.identity(),
            "arguments": arguments,
            "resources": {
                "filesystem_reads": filesystem_reads,
                "filesystem_writes": filesystem_writes,
                "process": approval.resources().process(),
                "network_targets": network_targets,
            },
        }))?;
        self.output
            .write_all(b"Approve local.echo? Type approve or deny: ")?;
        self.output.flush()?;
        let mut decision = String::new();
        if self.input.read_line(&mut decision)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Tool approval input ended",
            ));
        }
        match decision.trim() {
            "approve" => Ok(ProductToolDecision::Approve),
            "deny" => Ok(ProductToolDecision::Deny),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Tool approval must be approve or deny",
            )),
        }
    }
}

impl<R, W: Write> CliProductInteraction<R, W> {
    fn write_event(&mut self, value: &impl serde::Serialize) -> io::Result<()> {
        serde_json::to_writer(&mut self.output, value).map_err(io::Error::other)?;
        self.output.write_all(b"\n")?;
        self.output.flush()
    }
}

fn print_status(status: &RecoveryStatus) -> Result<(), CliError> {
    write_stdout_line(&status.to_string())
}

fn write_stdout_line(value: &str) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{value}")?;
    stdout.flush()?;
    Ok(())
}

fn open_runtime(path: &Path) -> Result<RuntimeKernel, CliError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    RuntimeKernel::open(path).map_err(CliError::Runtime)
}

#[derive(Debug, Eq, PartialEq)]
enum Command {
    AppServer {
        ledger: PathBuf,
    },
    Tui {
        ledger: PathBuf,
    },
    Headless {
        ledger: PathBuf,
        input: String,
        local_echo: bool,
        dialect: Option<ProviderDialect>,
        preset: Option<String>,
    },
    Resume {
        ledger: PathBuf,
        local_echo: bool,
    },
    Status {
        ledger: PathBuf,
    },
    Stats {
        ledger: PathBuf,
        at: Option<UsageTimestamp>,
        query: Option<RuntimeUsageQuery>,
    },
    Cancel {
        ledger: PathBuf,
        turn: TurnId,
    },
    Retry {
        ledger: PathBuf,
        turn: TurnId,
    },
    Reconcile {
        ledger: PathBuf,
        delivery: DeliveryId,
    },
    Tool(ToolCommand),
    Config(ConfigCommand),
    Credential(CredentialCommand),
    LocalProcessChild {
        mode: LocalProcessChildMode,
    },
    LocalProcessSmoke {
        run_dir: PathBuf,
        scenario: LocalProcessSmokeScenario,
        message: String,
    },
    ProviderHttpSmoke {
        ledger: PathBuf,
        scenario: ProviderHttpSmokeScenario,
        input: String,
    },
    PresentationSmoke {
        query: String,
    },
    Help,
}

#[derive(Debug, Eq, PartialEq)]
enum ToolCommand {
    Status {
        ledger: PathBuf,
    },
    Reconcile {
        ledger: PathBuf,
        call: u64,
        decision: ToolReconciliationDecision,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum ConfigCommand {
    Schema,
    Catalog,
    AcceptStarter {
        paths: ConfigPaths,
        scope: ConfigScope,
        preset: String,
        provider: String,
        catalog_key: String,
        dry_run: bool,
    },
    Get {
        paths: ConfigPaths,
        path: String,
    },
    Set {
        paths: ConfigPaths,
        scope: ConfigScope,
        path: String,
        value: String,
        dry_run: bool,
    },
    Reset {
        paths: ConfigPaths,
        scope: ConfigScope,
        path: String,
        dry_run: bool,
    },
    Repair {
        paths: ConfigPaths,
        scope: ConfigScope,
    },
    TestProvider {
        paths: ConfigPaths,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum CredentialCommand {
    Bind {
        reference: String,
        profile: String,
        origin: String,
    },
    Replace {
        reference: String,
        profile: String,
        origin: String,
    },
    Test {
        reference: String,
        profile: String,
        origin: String,
    },
    Forget {
        reference: String,
        profile: String,
        origin: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialOutcome {
    Bound,
    Replaced,
    Available,
    Forgotten,
    NotFound,
}

impl CredentialOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Replaced => "replaced",
            Self::Available => "available",
            Self::Forgotten => "forgotten",
            Self::NotFound => "not-found",
        }
    }
}

#[cfg(test)]
impl Command {
    fn credential_action_name(&self) -> Option<&'static str> {
        match self {
            Self::Credential(CredentialCommand::Bind { .. }) => Some("bind"),
            Self::Credential(CredentialCommand::Replace { .. }) => Some("replace"),
            Self::Credential(CredentialCommand::Test { .. }) => Some("test"),
            Self::Credential(CredentialCommand::Forget { .. }) => Some("forget"),
            _ => None,
        }
    }
}

fn parse(mut arguments: impl Iterator<Item = String>) -> Result<Command, CliError> {
    let Some(command) = arguments.next() else {
        return Ok(Command::Help);
    };
    if command == "help" || command == "--help" || command == "-h" {
        require_no_arguments(arguments)?;
        return Ok(Command::Help);
    }
    if command == "config" {
        return parse_config(arguments).map(Command::Config);
    }
    if command == "credential" {
        return parse_credential(arguments).map(Command::Credential);
    }
    if command == "tool" {
        return parse_tool(arguments).map(Command::Tool);
    }
    if command == "app-server" {
        return parse_app_server(arguments);
    }
    if command == "cancel" {
        let (ledger, turn) = parse_turn_target(arguments, "cancel")?;
        return Ok(Command::Cancel { ledger, turn });
    }
    if command == "retry" {
        let (ledger, turn) = parse_turn_target(arguments, "retry")?;
        return Ok(Command::Retry { ledger, turn });
    }
    if command == "__local-process-child" {
        let mode = LocalProcessChildMode::parse(arguments.next().as_deref())
            .ok_or(CliError::Usage("local-process child requires a valid mode"))?;
        if arguments.next().is_some() {
            return Err(CliError::Usage(
                "local-process child does not accept options",
            ));
        }
        return Ok(Command::LocalProcessChild { mode });
    }
    if command == "__local-process-smoke" {
        return parse_local_process_smoke(arguments);
    }
    if command == "__provider-http-smoke" {
        return parse_provider_http_smoke(arguments);
    }
    if command == "__presentation-smoke" {
        return parse_presentation_smoke(arguments);
    }
    let mut ledger = None;
    let mut input = None;
    let mut delivery = None;
    let mut tool = None;
    let mut dialect = None;
    let mut preset = None;
    let mut at = None;
    let mut summary_only = false;
    let mut limit = None;
    let mut cursor = None;
    while let Some(argument) = arguments.next() {
        if argument == "--summary-only" {
            if summary_only {
                return Err(CliError::Usage("duplicate option"));
            }
            summary_only = true;
            continue;
        }
        let slot = match argument.as_str() {
            "--ledger" => &mut ledger,
            "--input" => &mut input,
            "--delivery" => &mut delivery,
            "--tool" => &mut tool,
            "--dialect" => &mut dialect,
            "--preset" => &mut preset,
            "--at" => &mut at,
            "--limit" => &mut limit,
            "--cursor" => &mut cursor,
            _ => return Err(CliError::Usage("unknown option")),
        };
        if slot.is_some() {
            return Err(CliError::Usage("duplicate option"));
        }
        let value = arguments
            .next()
            .ok_or(CliError::Usage("option is missing its value"))?;
        if argument != "--input" && value.starts_with('-') {
            return Err(CliError::Usage("option is missing its value"));
        }
        *slot = Some(value);
    }
    if command != "stats" {
        if summary_only {
            return Err(CliError::Usage("--summary-only is only valid for stats"));
        }
        reject_option(&limit, "--limit is only valid for stats")?;
        reject_option(&cursor, "--cursor is only valid for stats")?;
    }
    let ledger = match ledger {
        Some(path) if path.is_empty() => {
            return Err(CliError::Usage("ledger path cannot be empty"));
        }
        Some(path) => PathBuf::from(path),
        None => default_ledger_path()?,
    };
    let local_echo = match tool.as_deref() {
        Some(LOCAL_ECHO_TOOL) => true,
        Some(_) => return Err(CliError::Usage("--tool must be local.echo")),
        None => false,
    };
    match command.as_str() {
        "tui" => {
            reject_option(&input, "--input is not valid for tui")?;
            reject_option(&delivery, "--delivery is not valid for tui")?;
            reject_option(&tool, "--tool is not valid for tui")?;
            reject_option(&at, "--at is not valid for tui")?;
            reject_option(&dialect, "--dialect is not valid for tui")?;
            reject_option(&preset, "--preset is not valid for tui")?;
            Ok(Command::Tui { ledger })
        }
        "headless" => {
            reject_option(&delivery, "--delivery is not valid for headless")?;
            reject_option(&at, "--at is not valid for headless")?;
            if preset.is_some() && dialect.is_some() {
                return Err(CliError::Usage(
                    "--preset cannot be combined with --dialect",
                ));
            }
            let dialect = dialect.as_deref().map(parse_provider_dialect).transpose()?;
            Ok(Command::Headless {
                ledger,
                input: input.ok_or(CliError::Usage("headless requires --input"))?,
                local_echo,
                dialect,
                preset,
            })
        }
        "resume" => {
            reject_option(&input, "--input is not valid for resume")?;
            reject_option(&delivery, "--delivery is not valid for resume")?;
            reject_option(&at, "--at is not valid for resume")?;
            reject_option(&dialect, "--dialect is not valid for resume")?;
            reject_option(&preset, "--preset is not valid for resume")?;
            Ok(Command::Resume { ledger, local_echo })
        }
        "status" => {
            reject_option(&input, "--input is not valid for status")?;
            reject_option(&delivery, "--delivery is not valid for status")?;
            reject_option(&tool, "--tool is not valid for status")?;
            reject_option(&at, "--at is not valid for status")?;
            reject_option(&dialect, "--dialect is not valid for status")?;
            reject_option(&preset, "--preset is not valid for status")?;
            Ok(Command::Status { ledger })
        }
        "stats" => {
            reject_option(&input, "--input is not valid for stats")?;
            reject_option(&delivery, "--delivery is not valid for stats")?;
            reject_option(&tool, "--tool is not valid for stats")?;
            reject_option(&dialect, "--dialect is not valid for stats")?;
            reject_option(&preset, "--preset is not valid for stats")?;
            let at = at
                .map(|value| {
                    value
                        .parse::<i64>()
                        .map_err(|_| CliError::Usage("--at must be Unix milliseconds"))
                        .and_then(|value| {
                            UsageTimestamp::from_unix_millis(value)
                                .map_err(|_| CliError::Usage("--at must be Unix milliseconds"))
                        })
                })
                .transpose()?;
            if summary_only && (limit.is_some() || cursor.is_some()) {
                return Err(CliError::Usage(
                    "--summary-only cannot be combined with --limit or --cursor",
                ));
            }
            if cursor.is_some() && limit.is_none() {
                return Err(CliError::Usage("--cursor requires --limit"));
            }
            let query = if summary_only {
                Some(RuntimeUsageQuery::summary_only())
            } else if let Some(limit) = limit {
                let limit = limit
                    .parse::<usize>()
                    .map_err(|_| CliError::Usage("--limit must be a positive integer"))?;
                let cursor = cursor
                    .map(|value| value.parse::<UsageCursor>())
                    .transpose()?;
                Some(RuntimeUsageQuery::page(limit, cursor)?)
            } else {
                None
            };
            Ok(Command::Stats { ledger, at, query })
        }
        "reconcile" => {
            reject_option(&input, "--input is not valid for reconcile")?;
            reject_option(&tool, "--tool is not valid for reconcile")?;
            reject_option(&at, "--at is not valid for reconcile")?;
            reject_option(&dialect, "--dialect is not valid for reconcile")?;
            reject_option(&preset, "--preset is not valid for reconcile")?;
            let delivery = delivery
                .ok_or(CliError::Usage("reconcile requires --delivery"))?
                .parse::<u64>()
                .map_err(|_| CliError::Usage("delivery must be a positive integer"))?;
            let delivery = DeliveryId::new(delivery)
                .map_err(|_| CliError::Usage("delivery must be a positive integer"))?;
            Ok(Command::Reconcile { ledger, delivery })
        }
        _ => Err(CliError::Usage("unknown command")),
    }
}

fn parse_turn_target(
    mut arguments: impl Iterator<Item = String>,
    command: &'static str,
) -> Result<(PathBuf, TurnId), CliError> {
    let mut ledger = None;
    let mut turn = None;
    while let Some(argument) = arguments.next() {
        let slot = match argument.as_str() {
            "--ledger" => &mut ledger,
            "--turn" => &mut turn,
            _ => return Err(CliError::Usage("unknown option")),
        };
        if slot.is_some() {
            return Err(CliError::Usage("duplicate option"));
        }
        let value = arguments
            .next()
            .ok_or(CliError::Usage("option is missing its value"))?;
        if value.starts_with('-') {
            return Err(CliError::Usage("option is missing its value"));
        }
        *slot = Some(value);
    }
    let ledger = match ledger {
        Some(path) if path.is_empty() => {
            return Err(CliError::Usage("ledger path cannot be empty"));
        }
        Some(path) => PathBuf::from(path),
        None => default_ledger_path()?,
    };
    let turn = turn
        .ok_or(CliError::Usage(match command {
            "cancel" => "cancel requires --turn",
            "retry" => "retry requires --turn",
            _ => "command requires --turn",
        }))?
        .parse::<u64>()
        .map_err(|_| CliError::Usage("Turn must be a positive integer"))?;
    let turn = TurnId::new(turn).map_err(|_| CliError::Usage("Turn must be a positive integer"))?;
    Ok((ledger, turn))
}

fn parse_app_server(mut arguments: impl Iterator<Item = String>) -> Result<Command, CliError> {
    let mut stdio = false;
    let mut ledger = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--stdio" if !stdio => stdio = true,
            "--stdio" => return Err(CliError::Usage("duplicate --stdio")),
            "--ledger" if ledger.is_none() => {
                let value = arguments
                    .next()
                    .ok_or(CliError::Usage("--ledger is missing its value"))?;
                if value.is_empty() || value.starts_with('-') {
                    return Err(CliError::Usage("--ledger is missing its value"));
                }
                ledger = Some(PathBuf::from(value));
            }
            "--ledger" => return Err(CliError::Usage("duplicate --ledger")),
            _ => return Err(CliError::Usage("unknown app-server option")),
        }
    }
    if !stdio {
        return Err(CliError::Usage("app-server requires --stdio"));
    }
    Ok(Command::AppServer {
        ledger: ledger.unwrap_or(default_ledger_path()?),
    })
}

fn parse_provider_dialect(value: &str) -> Result<ProviderDialect, CliError> {
    match value {
        "responses" => Ok(ProviderDialect::Responses),
        "chat_completions" => Ok(ProviderDialect::ChatCompletions),
        "messages" => Ok(ProviderDialect::Messages),
        _ => Err(CliError::Usage(
            "--dialect must be responses, chat_completions, or messages",
        )),
    }
}

fn parse_tool(mut arguments: impl Iterator<Item = String>) -> Result<ToolCommand, CliError> {
    let action = arguments
        .next()
        .ok_or(CliError::Usage("tool requires an action"))?;
    if !matches!(action.as_str(), "status" | "reconcile") {
        return Err(CliError::Usage("unknown tool action"));
    }
    let mut ledger = None;
    let mut call = None;
    let mut failed = false;
    let mut succeeded_digest = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--ledger" => {
                if ledger.is_some() {
                    return Err(CliError::Usage("duplicate --ledger"));
                }
                ledger = Some(PathBuf::from(next_tool_option_value(
                    &mut arguments,
                    "--ledger",
                )?));
            }
            "--call" => {
                if call.is_some() {
                    return Err(CliError::Usage("duplicate --call"));
                }
                let value = next_tool_option_value(&mut arguments, "--call")?;
                let value = value
                    .parse::<u64>()
                    .map_err(|_| CliError::Usage("Tool call must be a positive integer"))?;
                if value == 0 || value == u64::MAX {
                    return Err(CliError::Usage("Tool call must be a positive integer"));
                }
                call = Some(value);
            }
            "--failed" => {
                if failed {
                    return Err(CliError::Usage("duplicate --failed"));
                }
                failed = true;
            }
            "--succeeded-digest" => {
                if succeeded_digest.is_some() {
                    return Err(CliError::Usage("duplicate --succeeded-digest"));
                }
                let value = next_tool_option_value(&mut arguments, "--succeeded-digest")?;
                succeeded_digest = Some(parse_sha256(&value)?);
            }
            _ => return Err(CliError::Usage("unknown tool option")),
        }
    }
    let ledger = match ledger {
        Some(path) if path.as_os_str().is_empty() => {
            return Err(CliError::Usage("ledger path cannot be empty"));
        }
        Some(path) => path,
        None => default_ledger_path()?,
    };
    match action.as_str() {
        "status" => {
            if call.is_some() || failed || succeeded_digest.is_some() {
                return Err(CliError::Usage("tool status accepts only --ledger"));
            }
            Ok(ToolCommand::Status { ledger })
        }
        "reconcile" => {
            let call = call.ok_or(CliError::Usage("tool reconcile requires --call"))?;
            let decision = match (failed, succeeded_digest) {
                (true, None) => ToolReconciliationDecision::ObservedFailed {
                    reason: "user observed Tool effect failure".into(),
                },
                (false, Some(result_digest)) => {
                    ToolReconciliationDecision::ObservedSucceeded { result_digest }
                }
                _ => {
                    return Err(CliError::Usage(
                        "tool reconcile requires exactly one of --failed or --succeeded-digest",
                    ));
                }
            };
            Ok(ToolCommand::Reconcile {
                ledger,
                call,
                decision,
            })
        }
        _ => unreachable!("tool action was validated"),
    }
}

fn next_tool_option_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &'static str,
) -> Result<String, CliError> {
    let value = arguments
        .next()
        .ok_or(CliError::Usage("tool option is missing its value"))?;
    if value.starts_with('-') {
        return Err(CliError::Usage(match option {
            "--ledger" => "--ledger is missing its value",
            "--call" => "--call is missing its value",
            "--succeeded-digest" => "--succeeded-digest is missing its value",
            _ => "tool option is missing its value",
        }));
    }
    Ok(value)
}

fn parse_sha256(value: &str) -> Result<[u8; 32], CliError> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return Err(CliError::Usage(
            "--succeeded-digest must be 64 lowercase hexadecimal characters",
        ));
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        digest[index] = (parse_hex_digit(pair[0])? << 4) | parse_hex_digit(pair[1])?;
    }
    Ok(digest)
}

fn parse_hex_digit(value: u8) -> Result<u8, CliError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(CliError::Usage(
            "--succeeded-digest must be 64 lowercase hexadecimal characters",
        )),
    }
}

fn parse_credential(
    mut arguments: impl Iterator<Item = String>,
) -> Result<CredentialCommand, CliError> {
    let action = arguments
        .next()
        .ok_or(CliError::Usage("credential requires an action"))?;
    if !matches!(action.as_str(), "bind" | "replace" | "test" | "forget") {
        return Err(CliError::Usage("unknown credential action"));
    }
    let reference = arguments
        .next()
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .ok_or(CliError::Usage("credential bind requires a reference"))?;
    let mut profile = None;
    let mut origin = None;
    while let Some(argument) = arguments.next() {
        let slot = match argument.as_str() {
            "--profile" => &mut profile,
            "--origin" => &mut origin,
            _ => return Err(CliError::Usage("unknown credential option")),
        };
        if slot.is_some() {
            return Err(CliError::Usage("duplicate credential option"));
        }
        *slot = Some(
            arguments
                .next()
                .filter(|value| !value.is_empty() && !value.starts_with('-'))
                .ok_or(CliError::Usage("credential option is missing its value"))?,
        );
    }
    let profile = profile.ok_or(CliError::Usage("credential operation requires --profile"))?;
    let origin = origin.ok_or(CliError::Usage("credential operation requires --origin"))?;
    Ok(match action.as_str() {
        "bind" => CredentialCommand::Bind {
            reference,
            profile,
            origin,
        },
        "replace" => CredentialCommand::Replace {
            reference,
            profile,
            origin,
        },
        "test" => CredentialCommand::Test {
            reference,
            profile,
            origin,
        },
        "forget" => CredentialCommand::Forget {
            reference,
            profile,
            origin,
        },
        _ => unreachable!("credential action was validated"),
    })
}

fn execute_credential_command(
    vault: &mut impl CredentialVault,
    command: CredentialCommand,
    secret_input: &mut impl Read,
) -> Result<CredentialOutcome, CliError> {
    let secret = if matches!(
        &command,
        CredentialCommand::Bind { .. } | CredentialCommand::Replace { .. }
    ) {
        Some(read_secret(secret_input)?)
    } else {
        None
    };
    execute_credential_with_secret(vault, command, secret)
}

fn execute_credential_with_secret(
    vault: &mut impl CredentialVault,
    command: CredentialCommand,
    secret: Option<SecretValue>,
) -> Result<CredentialOutcome, CliError> {
    let (reference, profile, origin) = match &command {
        CredentialCommand::Bind {
            reference,
            profile,
            origin,
        }
        | CredentialCommand::Replace {
            reference,
            profile,
            origin,
        }
        | CredentialCommand::Test {
            reference,
            profile,
            origin,
        }
        | CredentialCommand::Forget {
            reference,
            profile,
            origin,
        } => (reference, profile, origin),
    };
    let allow_insecure_loopback = reqwest::Url::parse(origin)
        .is_ok_and(|origin| origin.scheme().eq_ignore_ascii_case("http"));
    let scope = ProviderCredentialScope::new(profile, reference, origin, allow_insecure_loopback)?;
    match command {
        CredentialCommand::Bind { .. } => {
            vault.bind(&scope, secret.ok_or(CredentialVaultError::InvalidSecret)?)?;
            Ok(CredentialOutcome::Bound)
        }
        CredentialCommand::Replace { .. } => {
            vault.replace(&scope, secret.ok_or(CredentialVaultError::InvalidSecret)?)?;
            Ok(CredentialOutcome::Replaced)
        }
        CredentialCommand::Test { .. } => match vault.resolve(&scope) {
            Ok(_) => Ok(CredentialOutcome::Available),
            Err(CredentialVaultError::NotFound) => Ok(CredentialOutcome::NotFound),
            Err(error) => Err(error.into()),
        },
        CredentialCommand::Forget { .. } => vault
            .forget(&scope)
            .map(|forgotten| {
                if forgotten {
                    CredentialOutcome::Forgotten
                } else {
                    CredentialOutcome::NotFound
                }
            })
            .map_err(Into::into),
    }
}

fn read_secret(reader: &mut impl Read) -> Result<SecretValue, CliError> {
    let limit =
        u64::try_from(MAX_SECRET_BYTES + 3).map_err(|_| CredentialVaultError::InvalidSecret)?;
    let mut bytes = Vec::new();
    reader.take(limit).read_to_end(&mut bytes)?;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    SecretValue::new(bytes).map_err(Into::into)
}

fn parse_local_process_smoke(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, CliError> {
    let mut run_dir = None;
    let mut scenario = None;
    let mut message = None;
    while let Some(argument) = arguments.next() {
        let slot = match argument.as_str() {
            "--run-dir" => &mut run_dir,
            "--scenario" => &mut scenario,
            "--message" => &mut message,
            _ => return Err(CliError::Usage("unknown local-process smoke option")),
        };
        if slot.is_some() {
            return Err(CliError::Usage("duplicate local-process smoke option"));
        }
        *slot = Some(arguments.next().ok_or(CliError::Usage(
            "local-process smoke option is missing its value",
        ))?);
    }
    let run_dir =
        PathBuf::from(run_dir.ok_or(CliError::Usage("local-process smoke requires --run-dir"))?);
    if !run_dir.is_absolute() {
        return Err(CliError::Usage(
            "local-process smoke run directory must be absolute",
        ));
    }
    let scenario = LocalProcessSmokeScenario::parse(
        scenario
            .as_deref()
            .ok_or(CliError::Usage("local-process smoke requires --scenario"))?,
    )
    .ok_or(CliError::Usage("unsupported local-process smoke scenario"))?;
    Ok(Command::LocalProcessSmoke {
        run_dir,
        scenario,
        message: message.ok_or(CliError::Usage("local-process smoke requires --message"))?,
    })
}

fn parse_provider_http_smoke(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, CliError> {
    let mut ledger = None;
    let mut scenario = None;
    let mut input = None;
    while let Some(argument) = arguments.next() {
        let slot = match argument.as_str() {
            "--ledger" => &mut ledger,
            "--scenario" => &mut scenario,
            "--input" => &mut input,
            _ => return Err(CliError::Usage("unknown Provider HTTP smoke option")),
        };
        if slot.is_some() {
            return Err(CliError::Usage("duplicate Provider HTTP smoke option"));
        }
        *slot = Some(arguments.next().ok_or(CliError::Usage(
            "Provider HTTP smoke option is missing its value",
        ))?);
    }
    let ledger =
        PathBuf::from(ledger.ok_or(CliError::Usage("Provider HTTP smoke requires --ledger"))?);
    if !ledger.is_absolute() {
        return Err(CliError::Usage(
            "Provider HTTP smoke Ledger path must be absolute",
        ));
    }
    let scenario = ProviderHttpSmokeScenario::parse(
        scenario
            .as_deref()
            .ok_or(CliError::Usage("Provider HTTP smoke requires --scenario"))?,
    )
    .ok_or(CliError::Usage("unsupported Provider HTTP smoke scenario"))?;
    Ok(Command::ProviderHttpSmoke {
        ledger,
        scenario,
        input: input.ok_or(CliError::Usage("Provider HTTP smoke requires --input"))?,
    })
}

fn parse_presentation_smoke(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, CliError> {
    let mut query = None;
    while let Some(argument) = arguments.next() {
        let slot = match argument.as_str() {
            "--query" => &mut query,
            _ => return Err(CliError::Usage("unknown presentation smoke option")),
        };
        if slot.is_some() {
            return Err(CliError::Usage("duplicate presentation smoke option"));
        }
        *slot = Some(arguments.next().ok_or(CliError::Usage(
            "presentation smoke option is missing its value",
        ))?);
    }
    Ok(Command::PresentationSmoke {
        query: query.ok_or(CliError::Usage("presentation smoke requires --query"))?,
    })
}

fn parse_config(mut arguments: impl Iterator<Item = String>) -> Result<ConfigCommand, CliError> {
    let action = arguments
        .next()
        .ok_or(CliError::Usage("config requires an action"))?;
    if action == "schema" {
        require_no_arguments(arguments)?;
        return Ok(ConfigCommand::Schema);
    }
    if action == "catalog" {
        require_no_arguments(arguments)?;
        return Ok(ConfigCommand::Catalog);
    }

    let mut positionals = Vec::new();
    let mut scope = None;
    let mut dry_run = false;
    let mut user_config = None;
    let mut project_config = None;
    let mut positional_only = false;
    while let Some(argument) = arguments.next() {
        if positional_only {
            positionals.push(argument);
            continue;
        }
        match argument.as_str() {
            "--" => positional_only = true,
            "--dry-run" => {
                if dry_run {
                    return Err(CliError::Usage("duplicate --dry-run"));
                }
                dry_run = true;
            }
            "--scope" => {
                if scope.is_some() {
                    return Err(CliError::Usage("duplicate --scope"));
                }
                scope = Some(parse_config_scope(next_option_value(
                    &mut arguments,
                    "--scope",
                )?)?);
            }
            "--user-config" => {
                if user_config.is_some() {
                    return Err(CliError::Usage("duplicate --user-config"));
                }
                user_config = Some(parse_absolute_config_path(next_option_value(
                    &mut arguments,
                    "--user-config",
                )?)?);
            }
            "--project-config" => {
                if project_config.is_some() {
                    return Err(CliError::Usage("duplicate --project-config"));
                }
                project_config = Some(parse_absolute_config_path(next_option_value(
                    &mut arguments,
                    "--project-config",
                )?)?);
            }
            _ if argument.starts_with('-') => {
                return Err(CliError::Usage("unknown config option"));
            }
            _ => positionals.push(argument),
        }
    }

    let paths = config_paths_with_overrides(user_config, project_config)?;
    match action.as_str() {
        "accept-starter" => {
            let scope = scope.ok_or(CliError::Usage("config accept-starter requires --scope"))?;
            let [preset, provider, catalog_key]: [String; 3] =
                positionals.try_into().map_err(|_| {
                    CliError::Usage(
                        "config accept-starter requires a Preset ID, Provider Profile, and catalog key",
                    )
                })?;
            Ok(ConfigCommand::AcceptStarter {
                paths,
                scope,
                preset,
                provider,
                catalog_key,
                dry_run,
            })
        }
        "get" => {
            reject_config_scope(scope)?;
            reject_dry_run(dry_run, "--dry-run is not valid for config get")?;
            let [path]: [String; 1] = positionals
                .try_into()
                .map_err(|_| CliError::Usage("config get requires exactly one path"))?;
            Ok(ConfigCommand::Get { paths, path })
        }
        "set" => {
            let scope = scope.ok_or(CliError::Usage("config set requires --scope"))?;
            let [path, value]: [String; 2] = positionals
                .try_into()
                .map_err(|_| CliError::Usage("config set requires a path and value"))?;
            Ok(ConfigCommand::Set {
                paths,
                scope,
                path,
                value,
                dry_run,
            })
        }
        "reset" => {
            let scope = scope.ok_or(CliError::Usage("config reset requires --scope"))?;
            let [path]: [String; 1] = positionals
                .try_into()
                .map_err(|_| CliError::Usage("config reset requires exactly one path"))?;
            Ok(ConfigCommand::Reset {
                paths,
                scope,
                path,
                dry_run,
            })
        }
        "repair" => {
            let scope = scope.ok_or(CliError::Usage("config repair requires --scope"))?;
            reject_dry_run(dry_run, "--dry-run is not valid for config repair")?;
            if !positionals.is_empty() {
                return Err(CliError::Usage("config repair does not accept a path"));
            }
            Ok(ConfigCommand::Repair { paths, scope })
        }
        "test-provider" => {
            if scope.is_some() {
                return Err(CliError::Usage(
                    "--scope is not valid for config test-provider",
                ));
            }
            reject_dry_run(dry_run, "--dry-run is not valid for config test-provider")?;
            if !positionals.is_empty() {
                return Err(CliError::Usage(
                    "config test-provider does not accept a path",
                ));
            }
            Ok(ConfigCommand::TestProvider { paths })
        }
        _ => Err(CliError::Usage("unknown config action")),
    }
}

fn next_option_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &'static str,
) -> Result<String, CliError> {
    let value = arguments
        .next()
        .ok_or(CliError::Usage("config option is missing its value"))?;
    if value.starts_with('-') {
        return Err(CliError::Usage(match option {
            "--scope" => "--scope is missing its value",
            "--user-config" => "--user-config is missing its value",
            "--project-config" => "--project-config is missing its value",
            _ => "config option is missing its value",
        }));
    }
    Ok(value)
}

fn parse_config_scope(value: String) -> Result<ConfigScope, CliError> {
    match value.as_str() {
        "user" => Ok(ConfigScope::User),
        "project" => Ok(ConfigScope::Project),
        _ => Err(CliError::Usage("config scope must be user or project")),
    }
}

fn parse_absolute_config_path(value: String) -> Result<PathBuf, CliError> {
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        Err(CliError::Usage("config file path must be absolute"))
    } else {
        Ok(path)
    }
}

fn reject_config_scope(scope: Option<ConfigScope>) -> Result<(), CliError> {
    if scope.is_some() {
        Err(CliError::Usage("--scope is not valid for config get"))
    } else {
        Ok(())
    }
}

fn reject_dry_run(dry_run: bool, message: &'static str) -> Result<(), CliError> {
    if dry_run {
        Err(CliError::Usage(message))
    } else {
        Ok(())
    }
}

fn reject_option(value: &Option<String>, message: &'static str) -> Result<(), CliError> {
    if value.is_some() {
        Err(CliError::Usage(message))
    } else {
        Ok(())
    }
}

fn require_no_arguments(mut arguments: impl Iterator<Item = String>) -> Result<(), CliError> {
    if arguments.next().is_some() {
        Err(CliError::Usage("help does not accept options"))
    } else {
        Ok(())
    }
}

fn config_paths_with_overrides(
    user: Option<PathBuf>,
    project: Option<PathBuf>,
) -> Result<ConfigPaths, CliError> {
    let defaults = default_config_paths()?;
    Ok(ConfigPaths::new(
        user.unwrap_or_else(|| defaults.user().to_owned()),
        project.unwrap_or_else(|| defaults.project().to_owned()),
    ))
}

fn default_config_paths() -> Result<ConfigPaths, CliError> {
    let user = default_user_config_path()?;
    let project = env::current_dir()?.join(".greentyper").join("config.toml");
    Ok(ConfigPaths::new(user, project))
}

fn default_user_config_path() -> Result<PathBuf, CliError> {
    #[cfg(windows)]
    {
        let root = required_absolute_config_env_path("APPDATA")?;
        Ok(root.join("GreenTyper").join("config.toml"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = required_absolute_config_env_path("HOME")?;
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("GreenTyper")
            .join("config.toml"))
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(root) = optional_absolute_config_env_path("XDG_CONFIG_HOME")? {
            return Ok(root.join("greentyper").join("config.toml"));
        }
        let home = required_absolute_config_env_path("HOME")?;
        Ok(home.join(".config").join("greentyper").join("config.toml"))
    }
}

fn required_absolute_config_env_path(name: &'static str) -> Result<PathBuf, CliError> {
    optional_absolute_config_env_path(name)?.ok_or(CliError::Usage(
        "no absolute platform config directory is configured",
    ))
}

fn optional_absolute_config_env_path(name: &'static str) -> Result<Option<PathBuf>, CliError> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(CliError::Usage(
            "platform config directory must be an absolute path",
        ));
    }
    Ok(Some(path))
}

fn default_ledger_path() -> Result<PathBuf, CliError> {
    #[cfg(windows)]
    {
        let root = required_absolute_env_path("LOCALAPPDATA")?;
        Ok(root.join("GreenTyper").join("runtime.ledger"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = required_absolute_env_path("HOME")?;
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("GreenTyper")
            .join("runtime.ledger"))
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(root) = optional_absolute_env_path("XDG_STATE_HOME")? {
            return Ok(root.join("greentyper").join("runtime.ledger"));
        }
        let home = required_absolute_env_path("HOME")?;
        Ok(home
            .join(".local")
            .join("state")
            .join("greentyper")
            .join("runtime.ledger"))
    }
}

fn required_absolute_env_path(name: &'static str) -> Result<PathBuf, CliError> {
    optional_absolute_env_path(name)?.ok_or(CliError::Usage(
        "no absolute platform state directory is configured",
    ))
}

fn optional_absolute_env_path(name: &'static str) -> Result<Option<PathBuf>, CliError> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(CliError::Usage(
            "platform state directory must be an absolute path",
        ));
    }
    Ok(Some(path))
}

const USAGE: &str = "\
GreenTyper Runtime\n\
\n\
Usage:\n\
  greentyper app-server --stdio [--ledger PATH]\n\
  greentyper tui [--ledger PATH]\n\
  greentyper headless [--ledger PATH] [--preset ID | --dialect DIALECT] [--tool local.echo] --input TEXT\n\
  greentyper resume [--ledger PATH] [--tool local.echo]\n\
  greentyper status [--ledger PATH]\n\
  greentyper stats [--ledger PATH] [--at UNIX_MS] [--summary-only | --limit N [--cursor CURSOR]]\n\
  greentyper cancel [--ledger PATH] --turn ID\n\
  greentyper retry [--ledger PATH] --turn ID\n\
  greentyper reconcile [--ledger PATH] --delivery ID\n\
  greentyper tool status [--ledger PATH]\n\
  greentyper tool reconcile [--ledger PATH] --call ID (--failed | --succeeded-digest SHA256)\n\
  greentyper config schema\n\
  greentyper config catalog\n\
  greentyper config accept-starter PRESET_ID PROVIDER CATALOG_KEY --scope user|project [--dry-run]\n\
  greentyper config get PATH [--user-config PATH] [--project-config PATH]\n\
  greentyper config set PATH VALUE --scope user|project [--dry-run]\n\
  greentyper config reset PATH --scope user|project [--dry-run]\n\
  greentyper config repair --scope user|project\n\
  greentyper config test-provider [--user-config PATH] [--project-config PATH]\n\
  greentyper credential bind REFERENCE --profile PROFILE --origin URL\n\
  greentyper credential replace REFERENCE --profile PROFILE --origin URL\n\
  greentyper credential test REFERENCE --profile PROFILE --origin URL\n\
  greentyper credential forget REFERENCE --profile PROFILE --origin URL\n";

#[derive(Debug)]
pub enum CliError {
    Usage(&'static str),
    UsageRuntime(UsageError),
    AppServer(crate::app_server::AppServerError),
    Io(io::Error),
    Json(serde_json::Error),
    Config(ConfigRuntimeError),
    Runtime(greentyper_core::runtime::RuntimeError),
    LocalProcess(LocalProcessError),
    ProviderHttp(ProviderHttpError),
    Provider(ProviderError),
    Credential(CredentialVaultError),
    ProductDriver(ProductDriverError),
    Presentation(PresentationSmokeError),
    Terminal(crate::terminal::TerminalError),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(formatter, "{message}\n\n{USAGE}"),
            Self::UsageRuntime(source) => write!(formatter, "{source}"),
            Self::AppServer(source) => write!(formatter, "{source}"),
            Self::Io(source) => write!(formatter, "I/O failed: {source}"),
            Self::Json(source) => write!(formatter, "JSON output failed: {source}"),
            Self::Config(source) => {
                let rendered = serde_json::json!({
                    "error": {
                        "category": source.category(),
                        "message": source.to_string(),
                    }
                });
                write!(formatter, "{rendered}")
            }
            Self::Runtime(source) => write!(formatter, "{source}"),
            Self::LocalProcess(source) => write!(formatter, "{source}"),
            Self::ProviderHttp(source) => write!(formatter, "{source}"),
            Self::Provider(source) => write!(formatter, "{source}"),
            Self::Credential(source) => write!(formatter, "{source}"),
            Self::ProductDriver(source) => write!(formatter, "{source}"),
            Self::Presentation(source) => write!(formatter, "{source}"),
            Self::Terminal(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AppServer(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::Json(source) => Some(source),
            Self::Config(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::LocalProcess(source) => Some(source),
            Self::ProviderHttp(source) => Some(source),
            Self::Provider(source) => Some(source),
            Self::Credential(source) => Some(source),
            Self::ProductDriver(source) => Some(source),
            Self::Presentation(source) => Some(source),
            Self::Terminal(source) => Some(source),
            Self::UsageRuntime(source) => Some(source),
            Self::Usage(_) => None,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<crate::app_server::AppServerError> for CliError {
    fn from(source: crate::app_server::AppServerError) -> Self {
        Self::AppServer(source)
    }
}

impl From<ConfigRuntimeError> for CliError {
    fn from(source: ConfigRuntimeError) -> Self {
        Self::Config(source)
    }
}

impl From<greentyper_core::runtime::RuntimeError> for CliError {
    fn from(source: greentyper_core::runtime::RuntimeError) -> Self {
        Self::Runtime(source)
    }
}

impl From<LocalProcessError> for CliError {
    fn from(source: LocalProcessError) -> Self {
        Self::LocalProcess(source)
    }
}

impl From<ProviderHttpError> for CliError {
    fn from(source: ProviderHttpError) -> Self {
        Self::ProviderHttp(source)
    }
}

impl From<CredentialVaultError> for CliError {
    fn from(source: CredentialVaultError) -> Self {
        Self::Credential(source)
    }
}

impl From<ProviderError> for CliError {
    fn from(source: ProviderError) -> Self {
        Self::Provider(source)
    }
}

impl From<ProductDriverError> for CliError {
    fn from(source: ProductDriverError) -> Self {
        Self::ProductDriver(source)
    }
}

impl From<UsageError> for CliError {
    fn from(source: UsageError) -> Self {
        Self::UsageRuntime(source)
    }
}

impl From<PresentationSmokeError> for CliError {
    fn from(source: PresentationSmokeError) -> Self {
        Self::Presentation(source)
    }
}

impl From<crate::terminal::TerminalError> for CliError {
    fn from(source: crate::terminal::TerminalError) -> Self {
        Self::Terminal(source)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::{self, Cursor, Read};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use greentyper_core::agent_team::TeamOperationRecord;
    use greentyper_core::config::{ConfigLayers, ConfigScope};
    use greentyper_core::provider::DeterministicProvider;
    use greentyper_core::runtime::{ProviderToolApproval, RecoveryStatus, RuntimeKernel};
    use greentyper_core::tool_runtime::{
        AuthorizedToolCall, ToolEffectExecutor, ToolExecution, ToolReconciliationDecision,
    };

    use crate::credential_vault::InMemoryCredentialVault;
    use crate::product_driver::{ProductDriver, ProductInteraction, ProductToolDecision};

    use super::{
        Command, ConfigCommand, CredentialCommand, CredentialOutcome, ToolCommand,
        deliver_and_ack_to, deliver_product_and_ack_to, execute_credential_command, parse,
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "greentyper-cli-write-failure-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn sidecar(path: &Path, kind: &str) -> PathBuf {
        let mut sidecar = OsString::from(path.as_os_str());
        sidecar.push(".");
        sidecar.push(kind);
        PathBuf::from(sidecar)
    }

    #[test]
    fn parser_requires_command_specific_options() {
        assert!(matches!(
            parse(
                [
                    "tui".to_owned(),
                    "--ledger".to_owned(),
                    "runtime.ledger".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Tui { ledger }) if ledger == Path::new("runtime.ledger")
        ));
        assert!(
            parse(
                [
                    "tui".to_owned(),
                    "--input".to_owned(),
                    "not-headless".to_owned(),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(parse(["headless".to_owned()].into_iter()).is_err());
        assert!(parse(["reconcile".to_owned()].into_iter()).is_err());
        assert!(matches!(
            parse(
                [
                    "headless".to_owned(),
                    "--input".to_owned(),
                    "hello".to_owned()
                ]
                .into_iter()
            ),
            Ok(Command::Headless { input, .. }) if input == "hello"
        ));
        assert!(matches!(
            parse(
                [
                    "headless".to_owned(),
                    "--input".to_owned(),
                    "hello".to_owned(),
                    "--tool".to_owned(),
                    "local.echo".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Headless {
                local_echo: true,
                ..
            })
        ));
        assert!(
            parse(
                [
                    "headless".to_owned(),
                    "--input".to_owned(),
                    "hello".to_owned(),
                    "--dialect".to_owned(),
                    "future".to_owned(),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(
            parse(
                [
                    "resume".to_owned(),
                    "--dialect".to_owned(),
                    "chat_completions".to_owned(),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(matches!(
            parse(
                [
                    "headless".to_owned(),
                    "--input".to_owned(),
                    "hello".to_owned(),
                    "--dialect".to_owned(),
                    "chat_completions".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Headless {
                dialect: Some(greentyper_core::provider::ProviderDialect::ChatCompletions),
                ..
            })
        ));
        assert!(matches!(
            parse(
                [
                    "headless".to_owned(),
                    "--input".to_owned(),
                    "hello".to_owned(),
                    "--preset".to_owned(),
                    "frontier".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Headless {
                preset: Some(preset),
                ..
            }) if preset == "frontier"
        ));
        assert!(
            parse(
                [
                    "headless".to_owned(),
                    "--input".to_owned(),
                    "hello".to_owned(),
                    "--preset".to_owned(),
                    "frontier".to_owned(),
                    "--dialect".to_owned(),
                    "responses".to_owned(),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(
            parse(
                [
                    "headless".to_owned(),
                    "--input".to_owned(),
                    "hello".to_owned(),
                    "--tool".to_owned(),
                    "shell".to_owned(),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(matches!(
            parse(
                [
                    "stats".to_owned(),
                    "--at".to_owned(),
                    "123456789".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Stats { at: Some(at), .. })
                if at.unix_millis() == 123_456_789
        ));
        assert!(
            parse(["stats".to_owned(), "--at".to_owned(), "later".to_owned()].into_iter()).is_err()
        );
    }

    #[test]
    fn parser_requires_an_exact_turn_for_provider_cancellation() {
        assert!(matches!(
            parse(
                [
                    "cancel".to_owned(),
                    "--ledger".to_owned(),
                    "runtime.ledger".to_owned(),
                    "--turn".to_owned(),
                    "7".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Cancel { ledger, turn })
                if ledger == Path::new("runtime.ledger") && turn.get() == 7
        ));
        assert!(matches!(
            parse(
                [
                    "retry".to_owned(),
                    "--ledger".to_owned(),
                    "runtime.ledger".to_owned(),
                    "--turn".to_owned(),
                    "7".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Retry { ledger, turn })
                if ledger == Path::new("runtime.ledger") && turn.get() == 7
        ));
        for arguments in [
            vec!["cancel"],
            vec!["cancel", "--turn", "0"],
            vec!["cancel", "--turn", "later"],
            vec!["cancel", "--turn", "1", "--turn", "2"],
            vec!["cancel", "--turn", "1", "--delivery", "2"],
        ] {
            assert!(
                parse(arguments.into_iter().map(str::to_owned)).is_err(),
                "accepted invalid cancellation command"
            );
        }
        for arguments in [
            vec!["retry"],
            vec!["retry", "--turn", "0"],
            vec!["retry", "--turn", "later"],
            vec!["retry", "--turn", "1", "--turn", "2"],
            vec!["retry", "--turn", "1", "--delivery", "2"],
        ] {
            assert!(
                parse(arguments.into_iter().map(str::to_owned)).is_err(),
                "accepted invalid retry command"
            );
        }
    }

    #[test]
    fn parser_bounds_tool_inspection_and_reconciliation() {
        assert!(matches!(
            parse(
                [
                    "tool".to_owned(),
                    "status".to_owned(),
                    "--ledger".to_owned(),
                    "runtime.ledger".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Tool(ToolCommand::Status { ledger }))
                if ledger == Path::new("runtime.ledger")
        ));
        assert!(matches!(
            parse(
                [
                    "tool".to_owned(),
                    "reconcile".to_owned(),
                    "--call".to_owned(),
                    "7".to_owned(),
                    "--failed".to_owned(),
                ]
                .into_iter()
            ),
            Ok(Command::Tool(ToolCommand::Reconcile {
                call: 7,
                decision: ToolReconciliationDecision::ObservedFailed { .. },
                ..
            }))
        ));
        let digest = "ab".repeat(32);
        assert!(matches!(
            parse(
                [
                    "tool".to_owned(),
                    "reconcile".to_owned(),
                    "--call".to_owned(),
                    "9".to_owned(),
                    "--succeeded-digest".to_owned(),
                    digest,
                ]
                .into_iter()
            ),
            Ok(Command::Tool(ToolCommand::Reconcile {
                call: 9,
                decision: ToolReconciliationDecision::ObservedSucceeded {
                    result_digest
                },
                ..
            })) if result_digest == [0xab; 32]
        ));

        for arguments in [
            vec!["tool", "status", "--call", "1"],
            vec!["tool", "reconcile", "--call", "0", "--failed"],
            vec!["tool", "reconcile", "--call", "1"],
            vec![
                "tool",
                "reconcile",
                "--call",
                "1",
                "--failed",
                "--succeeded-digest",
                "abababababababababababababababababababababababababababababababab",
            ],
            vec![
                "tool",
                "reconcile",
                "--call",
                "1",
                "--succeeded-digest",
                "ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB",
            ],
        ] {
            assert!(
                parse(arguments.into_iter().map(str::to_owned)).is_err(),
                "accepted invalid Tool arguments"
            );
        }
    }

    #[test]
    fn parser_keeps_full_stats_as_default_and_bounds_explicit_reports() {
        assert!(matches!(
            parse(["stats".to_owned()].into_iter()),
            Ok(Command::Stats { query: None, .. })
        ));
        assert!(matches!(
            parse(["stats".to_owned(), "--summary-only".to_owned()].into_iter()),
            Ok(Command::Stats { query: Some(_), .. })
        ));
        assert!(matches!(
            parse(["stats".to_owned(), "--limit".to_owned(), "10".to_owned()].into_iter()),
            Ok(Command::Stats { query: Some(_), .. })
        ));
        assert!(
            parse(
                [
                    "stats".to_owned(),
                    "--summary-only".to_owned(),
                    "--limit".to_owned(),
                    "10".to_owned(),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(
            parse(
                [
                    "stats".to_owned(),
                    "--cursor".to_owned(),
                    "invalid".to_owned(),
                ]
                .into_iter()
            )
            .is_err()
        );
        assert!(
            parse(["stats".to_owned(), "--limit".to_owned(), "0".to_owned()].into_iter()).is_err()
        );
        assert!(
            parse(
                [
                    "headless".to_owned(),
                    "--input".to_owned(),
                    "hello".to_owned(),
                    "--summary-only".to_owned(),
                ]
                .into_iter()
            )
            .is_err()
        );
    }

    #[test]
    fn parser_requires_explicit_writable_config_scope() {
        let user = temp_path();
        let project = temp_path();
        let parsed = parse(
            [
                "config".to_owned(),
                "set".to_owned(),
                "provider.model".to_owned(),
                "deterministic-v2".to_owned(),
                "--scope".to_owned(),
                "user".to_owned(),
                "--dry-run".to_owned(),
                "--user-config".to_owned(),
                user.display().to_string(),
                "--project-config".to_owned(),
                project.display().to_string(),
            ]
            .into_iter(),
        )
        .expect("parse config set");
        assert!(matches!(
            parsed,
            Command::Config(ConfigCommand::Set {
                scope: ConfigScope::User,
                path,
                value,
                dry_run: true,
                ..
            }) if path == "provider.model" && value == "deterministic-v2"
        ));
        assert!(
            parse(
                [
                    "config".to_owned(),
                    "set".to_owned(),
                    "provider.model".to_owned(),
                    "deterministic-v2".to_owned(),
                ]
                .into_iter(),
            )
            .is_err()
        );
        assert!(
            parse(
                [
                    "config".to_owned(),
                    "get".to_owned(),
                    "provider.model".to_owned(),
                    "--scope".to_owned(),
                    "project".to_owned(),
                ]
                .into_iter(),
            )
            .is_err()
        );
    }

    #[test]
    fn parser_supports_an_explicit_selected_provider_connection_test() {
        let user = temp_path();
        let project = temp_path();
        let parsed = parse(
            [
                "config".to_owned(),
                "test-provider".to_owned(),
                "--user-config".to_owned(),
                user.display().to_string(),
                "--project-config".to_owned(),
                project.display().to_string(),
            ]
            .into_iter(),
        )
        .expect("parse selected Provider connection test");
        assert!(matches!(
            parsed,
            Command::Config(ConfigCommand::TestProvider { paths })
                if paths.user() == user && paths.project() == project
        ));
    }

    #[test]
    fn parser_keeps_provider_credential_material_out_of_arguments() {
        let parsed = parse(
            [
                "credential".to_owned(),
                "bind".to_owned(),
                "openai-main".to_owned(),
                "--profile".to_owned(),
                "openai-main".to_owned(),
                "--origin".to_owned(),
                "https://api.example.com/v1".to_owned(),
            ]
            .into_iter(),
        )
        .expect("parse credential bind");

        assert!(matches!(
            parsed,
            Command::Credential(CredentialCommand::Bind {
                reference,
                profile,
                origin,
            }) if reference == "openai-main"
                && profile == "openai-main"
                && origin == "https://api.example.com/v1"
        ));
    }

    #[test]
    fn parser_supports_replace_test_and_forget_credential_operations() {
        for (action, expected) in [
            ("replace", "replace"),
            ("test", "test"),
            ("forget", "forget"),
        ] {
            let parsed = parse(
                [
                    "credential".to_owned(),
                    action.to_owned(),
                    "openai-main".to_owned(),
                    "--profile".to_owned(),
                    "openai-main".to_owned(),
                    "--origin".to_owned(),
                    "https://api.example.com/v1".to_owned(),
                ]
                .into_iter(),
            )
            .expect("parse credential operation");

            assert_eq!(parsed.credential_action_name(), Some(expected));
        }
    }

    #[test]
    fn credential_operations_bind_replace_test_and_forget_without_readback() {
        let mut vault = InMemoryCredentialVault::default();
        let command = |action| match action {
            "bind" => CredentialCommand::Bind {
                reference: "openai-main".to_owned(),
                profile: "openai-main".to_owned(),
                origin: "https://api.example.com/v1".to_owned(),
            },
            "replace" => CredentialCommand::Replace {
                reference: "openai-main".to_owned(),
                profile: "openai-main".to_owned(),
                origin: "https://api.example.com/v1".to_owned(),
            },
            "test" => CredentialCommand::Test {
                reference: "openai-main".to_owned(),
                profile: "openai-main".to_owned(),
                origin: "https://api.example.com/v1".to_owned(),
            },
            "forget" => CredentialCommand::Forget {
                reference: "openai-main".to_owned(),
                profile: "openai-main".to_owned(),
                origin: "https://api.example.com/v1".to_owned(),
            },
            _ => unreachable!(),
        };

        assert_eq!(
            execute_credential_command(
                &mut vault,
                command("bind"),
                &mut Cursor::new(b"private-first-token\n")
            )
            .unwrap(),
            CredentialOutcome::Bound
        );
        assert_eq!(
            execute_credential_command(&mut vault, command("test"), &mut PanicReader).unwrap(),
            CredentialOutcome::Available
        );
        assert_eq!(
            execute_credential_command(
                &mut vault,
                command("replace"),
                &mut Cursor::new(b"private-second-token\n")
            )
            .unwrap(),
            CredentialOutcome::Replaced
        );
        assert_eq!(
            execute_credential_command(&mut vault, command("forget"), &mut PanicReader).unwrap(),
            CredentialOutcome::Forgotten
        );
        assert_eq!(
            execute_credential_command(&mut vault, command("forget"), &mut PanicReader).unwrap(),
            CredentialOutcome::NotFound
        );
    }

    #[test]
    fn output_write_failure_never_acknowledges_delivery() {
        let path = temp_path();
        let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
        let mut provider = DeterministicProvider::default();
        let output = runtime
            .execute(&ConfigLayers::default(), "visible once", &mut provider)
            .expect("prepare output");
        assert!(
            deliver_and_ack_to(&mut runtime, output, &mut BrokenWriter).is_err(),
            "broken presentation sink must fail"
        );
        assert!(matches!(
            runtime.snapshot().status,
            RecoveryStatus::ReconciliationRequired { .. }
        ));
        drop(runtime);
        std::fs::remove_file(path).expect("cleanup Runtime ledger");
    }

    #[test]
    fn product_output_write_failure_never_acknowledges_delivery() {
        let path = temp_path();
        let mut interaction = SilentInteraction;
        let mut driver = ProductDriver::open_with_executor(&path, NeverExecutor, &mut interaction)
            .expect("open Product driver");
        let mut provider = DeterministicProvider::default();
        let output = driver
            .execute(
                &ConfigLayers::default(),
                "visible once",
                &mut provider,
                &mut interaction,
            )
            .expect("prepare Product output");

        assert!(
            deliver_product_and_ack_to(&mut driver, output, &mut BrokenWriter).is_err(),
            "broken presentation sink must fail"
        );
        assert!(matches!(
            driver.snapshot().status,
            RecoveryStatus::ReconciliationRequired { .. }
        ));
        drop(driver);
        assert!(matches!(
            RuntimeKernel::inspect(&path)
                .expect("replay Runtime")
                .status,
            RecoveryStatus::ReconciliationRequired { .. }
        ));
        std::fs::remove_file(&path).expect("cleanup Runtime ledger");
        std::fs::remove_file(sidecar(&path, "team")).expect("cleanup Team ledger");
        std::fs::remove_file(sidecar(&path, "tool")).expect("cleanup Tool ledger");
    }

    struct BrokenWriter;

    struct NeverExecutor;

    struct SilentInteraction;

    struct PanicReader;

    impl Read for PanicReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            panic!("credential operation unexpectedly read secret input")
        }
    }

    impl io::Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected broken presentation sink",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl ToolEffectExecutor for NeverExecutor {
        fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
            panic!("deterministic Provider must not execute a Tool")
        }
    }

    impl ProductInteraction for SilentInteraction {
        fn present_team_operation(&mut self, _record: TeamOperationRecord) -> io::Result<()> {
            Ok(())
        }

        fn decide_tool(
            &mut self,
            _approval: &ProviderToolApproval,
        ) -> io::Result<ProductToolDecision> {
            panic!("deterministic Provider must not request Tool approval")
        }
    }
}
