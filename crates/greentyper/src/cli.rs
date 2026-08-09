use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use greentyper_core::config::ConfigLayers;
use greentyper_core::model::DeliveryId;
use greentyper_core::provider::DeterministicProvider;
use greentyper_core::runtime::{AcknowledgeOutcome, RecoveryStatus, RuntimeKernel};

pub fn run(arguments: impl Iterator<Item = String>) -> Result<(), CliError> {
    match parse(arguments)? {
        Command::Headless { ledger, input } => {
            let mut runtime = open_runtime(&ledger)?;
            let mut provider = DeterministicProvider::default();
            let output = runtime.execute(&ConfigLayers::default(), input, &mut provider)?;
            deliver_and_ack(&mut runtime, output)
        }
        Command::Resume { ledger } => {
            let mut runtime = open_runtime(&ledger)?;
            let mut provider = DeterministicProvider::default();
            let output = runtime.resume(&mut provider)?;
            deliver_and_ack(&mut runtime, output)
        }
        Command::Status { ledger } => {
            let snapshot = RuntimeKernel::inspect(&ledger)?;
            print_status(&snapshot.status)
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
        Command::Help => {
            print!("{USAGE}");
            Ok(())
        }
    }
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
    },
    Resume {
        ledger: PathBuf,
    },
    Status {
        ledger: PathBuf,
    },
    Reconcile {
        ledger: PathBuf,
        delivery: DeliveryId,
    },
    Help,
}

fn parse(mut arguments: impl Iterator<Item = String>) -> Result<Command, CliError> {
    let Some(command) = arguments.next() else {
        return Ok(Command::Help);
    };
    if command == "help" || command == "--help" || command == "-h" {
        require_no_arguments(arguments)?;
        return Ok(Command::Help);
    }
    let mut ledger = None;
    let mut input = None;
    let mut delivery = None;
    while let Some(argument) = arguments.next() {
        let slot = match argument.as_str() {
            "--ledger" => &mut ledger,
            "--input" => &mut input,
            "--delivery" => &mut delivery,
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
    match command.as_str() {
        "headless" => {
            reject_option(&delivery, "--delivery is not valid for headless")?;
            Ok(Command::Headless {
                ledger,
                input: input.ok_or(CliError::Usage("headless requires --input"))?,
            })
        }
        "resume" => {
            reject_option(&input, "--input is not valid for resume")?;
            reject_option(&delivery, "--delivery is not valid for resume")?;
            Ok(Command::Resume { ledger })
        }
        "status" => {
            reject_option(&input, "--input is not valid for status")?;
            reject_option(&delivery, "--delivery is not valid for status")?;
            Ok(Command::Status { ledger })
        }
        "reconcile" => {
            reject_option(&input, "--input is not valid for reconcile")?;
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
  greentyper headless [--ledger PATH] --input TEXT\n\
  greentyper resume [--ledger PATH]\n\
  greentyper status [--ledger PATH]\n\
  greentyper reconcile [--ledger PATH] --delivery ID\n";

#[derive(Debug)]
pub enum CliError {
    Usage(&'static str),
    Io(io::Error),
    Runtime(greentyper_core::runtime::RuntimeError),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(formatter, "{message}\n\n{USAGE}"),
            Self::Io(source) => write!(formatter, "I/O failed: {source}"),
            Self::Runtime(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Usage(_) => None,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<greentyper_core::runtime::RuntimeError> for CliError {
    fn from(source: greentyper_core::runtime::RuntimeError) -> Self {
        Self::Runtime(source)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use greentyper_core::config::ConfigLayers;
    use greentyper_core::provider::DeterministicProvider;
    use greentyper_core::runtime::{RecoveryStatus, RuntimeKernel};

    use super::{Command, deliver_and_ack_to, parse};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "greentyper-cli-write-failure-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
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

    struct BrokenWriter;

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
}
