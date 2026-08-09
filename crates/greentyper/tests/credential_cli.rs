use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const PROFILE: &str = "credential-cli";
const ORIGIN: &str = "https://credential-cli.invalid/v1";
const FIRST_SECRET: &str = "synthetic-credential-cli-first";
const SECOND_SECRET: &str = "synthetic-credential-cli-second";

fn reference() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    format!("ci-{}-{nonce}", std::process::id())
}

fn credential_command(action: &str, reference: &str, secret: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_greentyper"));
    command.args([
        "credential",
        action,
        reference,
        "--profile",
        PROFILE,
        "--origin",
        ORIGIN,
    ]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if secret.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command.spawn().expect("spawn credential command");
    if let Some(secret) = secret {
        let mut stdin = child.stdin.take().expect("credential stdin");
        stdin
            .write_all(format!("{secret}\n").as_bytes())
            .expect("write credential stdin");
    }
    child
        .wait_with_output()
        .expect("wait for credential command")
}

fn assert_redacted(output: &Output) {
    for secret in [FIRST_SECRET, SECOND_SECRET] {
        assert!(
            !output
                .stdout
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        );
        assert!(
            !output
                .stderr
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        );
    }
}

#[cfg(not(windows))]
#[test]
fn credential_cli_fails_closed_when_the_platform_vault_is_unavailable() {
    let output = credential_command("bind", &reference(), Some(FIRST_SECRET));

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("platform credential vault is unavailable"),
        "{output:?}"
    );
    assert_redacted(&output);
}

#[cfg(windows)]
#[test]
fn credential_cli_round_trips_windows_credential_manager_without_readback() {
    let reference = reference();
    let _cleanup = CredentialCleanup {
        reference: reference.clone(),
    };

    let bound = credential_command("bind", &reference, Some(FIRST_SECRET));
    assert!(bound.status.success(), "{bound:?}");
    assert_eq!(bound.stdout, b"bound\n");
    assert_redacted(&bound);

    let available = credential_command("test", &reference, None);
    assert!(available.status.success(), "{available:?}");
    assert_eq!(available.stdout, b"available\n");
    assert_redacted(&available);

    let replaced = credential_command("replace", &reference, Some(SECOND_SECRET));
    assert!(replaced.status.success(), "{replaced:?}");
    assert_eq!(replaced.stdout, b"replaced\n");
    assert_redacted(&replaced);

    let forgotten = credential_command("forget", &reference, None);
    assert!(forgotten.status.success(), "{forgotten:?}");
    assert_eq!(forgotten.stdout, b"forgotten\n");
    assert_redacted(&forgotten);

    let missing = credential_command("test", &reference, None);
    assert!(missing.status.success(), "{missing:?}");
    assert_eq!(missing.stdout, b"not-found\n");
    assert_redacted(&missing);
}

#[cfg(windows)]
struct CredentialCleanup {
    reference: String,
}

#[cfg(windows)]
impl Drop for CredentialCleanup {
    fn drop(&mut self) {
        let _ = credential_command("forget", &self.reference, None);
    }
}
