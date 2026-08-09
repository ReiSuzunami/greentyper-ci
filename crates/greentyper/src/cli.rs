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
use greentyper_core::model::DeliveryId;
use greentyper_core::provider::ProviderError;
use greentyper_core::runtime::{
    AcknowledgeOutcome, PreparedOutput, ProviderToolApproval, RecoveryStatus, RuntimeKernel,
};
use greentyper_core::tool_runtime::ToolEffectExecutor;
use greentyper_core::usage::{RuntimeUsageSnapshot, UsageError, UsageTimestamp, UsageWindow};

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
    has_product_driver_state,
};
use crate::provider_http::{
    ConfiguredProvider, ProviderHttpError, ProviderHttpSmokeOutcome, ProviderHttpSmokeScenario,
};

pub fn run(arguments: impl Iterator<Item = String>) -> Result<(), CliError> {
    match parse(arguments)? {
        Command::Headless {
            ledger,
            input,
            local_echo,
        } => {
            let config = open_config_runtime(default_config_paths()?)?;
            let layers = config.config_layers()?.clone();
            let profile = config.selected_provider_profile()?;
            let usage_windows = config.resolved_usage_windows()?;
            let mut provider = ConfiguredProvider::for_new_turn(profile, PlatformCredentialVault)?;
            let has_product_state = has_product_driver_state(&ledger)?;
            if local_echo || has_product_state {
                provider.enable_local_echo();
                run_product_turn(&ledger, &layers, usage_windows, input, &mut provider)
            } else {
                let mut runtime = open_runtime(&ledger)?;
                let output = runtime.execute_with_usage_windows(
                    &layers,
                    usage_windows,
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
        Command::Stats { ledger, at } => {
            let at = match at {
                Some(at) => at,
                None => UsageTimestamp::now()?,
            };
            let stats: RuntimeUsageSnapshot = RuntimeKernel::inspect_usage(&ledger, at)?;
            write_json(&stats)
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
    let output = driver.execute_with_usage_windows(
        layers,
        usage_windows,
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
    Headless {
        ledger: PathBuf,
        input: String,
        local_echo: bool,
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
    },
    Reconcile {
        ledger: PathBuf,
        delivery: DeliveryId,
    },
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
enum ConfigCommand {
    Schema,
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
    let mut at = None;
    while let Some(argument) = arguments.next() {
        let slot = match argument.as_str() {
            "--ledger" => &mut ledger,
            "--input" => &mut input,
            "--delivery" => &mut delivery,
            "--tool" => &mut tool,
            "--at" => &mut at,
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
        "headless" => {
            reject_option(&delivery, "--delivery is not valid for headless")?;
            reject_option(&at, "--at is not valid for headless")?;
            Ok(Command::Headless {
                ledger,
                input: input.ok_or(CliError::Usage("headless requires --input"))?,
                local_echo,
            })
        }
        "resume" => {
            reject_option(&input, "--input is not valid for resume")?;
            reject_option(&delivery, "--delivery is not valid for resume")?;
            reject_option(&at, "--at is not valid for resume")?;
            Ok(Command::Resume { ledger, local_echo })
        }
        "status" => {
            reject_option(&input, "--input is not valid for status")?;
            reject_option(&delivery, "--delivery is not valid for status")?;
            reject_option(&tool, "--tool is not valid for status")?;
            reject_option(&at, "--at is not valid for status")?;
            Ok(Command::Status { ledger })
        }
        "stats" => {
            reject_option(&input, "--input is not valid for stats")?;
            reject_option(&delivery, "--delivery is not valid for stats")?;
            reject_option(&tool, "--tool is not valid for stats")?;
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
            Ok(Command::Stats { ledger, at })
        }
        "reconcile" => {
            reject_option(&input, "--input is not valid for reconcile")?;
            reject_option(&tool, "--tool is not valid for reconcile")?;
            reject_option(&at, "--at is not valid for reconcile")?;
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
GreenTyper headless Runtime\n\
\n\
Usage:\n\
  greentyper headless [--ledger PATH] [--tool local.echo] --input TEXT\n\
  greentyper resume [--ledger PATH] [--tool local.echo]\n\
  greentyper status [--ledger PATH]\n\
  greentyper stats [--ledger PATH] [--at UNIX_MS]\n\
  greentyper reconcile [--ledger PATH] --delivery ID\n\
  greentyper config schema\n\
  greentyper config get PATH [--user-config PATH] [--project-config PATH]\n\
  greentyper config set PATH VALUE --scope user|project [--dry-run]\n\
  greentyper config reset PATH --scope user|project [--dry-run]\n\
  greentyper config repair --scope user|project\n\
  greentyper credential bind REFERENCE --profile PROFILE --origin URL\n\
  greentyper credential replace REFERENCE --profile PROFILE --origin URL\n\
  greentyper credential test REFERENCE --profile PROFILE --origin URL\n\
  greentyper credential forget REFERENCE --profile PROFILE --origin URL\n";

#[derive(Debug)]
pub enum CliError {
    Usage(&'static str),
    UsageRuntime(UsageError),
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
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(formatter, "{message}\n\n{USAGE}"),
            Self::UsageRuntime(source) => write!(formatter, "{source}"),
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
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
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
    use greentyper_core::tool_runtime::{AuthorizedToolCall, ToolEffectExecutor, ToolExecution};

    use crate::credential_vault::InMemoryCredentialVault;
    use crate::product_driver::{ProductDriver, ProductInteraction, ProductToolDecision};

    use super::{
        Command, ConfigCommand, CredentialCommand, CredentialOutcome, deliver_and_ack_to,
        deliver_product_and_ack_to, execute_credential_command, parse,
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
