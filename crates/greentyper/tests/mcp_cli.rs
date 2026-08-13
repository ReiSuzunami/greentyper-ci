use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-mcp-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn command(mode: &str, root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_greentyper"));
    command
        .args([
            "mcp",
            "tools",
            "--",
            env!("CARGO_BIN_EXE_greentyper"),
            "__mcp-fixture",
            mode,
        ])
        .env("HOME", root.join("home"))
        .env("APPDATA", root.join("appdata"))
        .env("XDG_CONFIG_HOME", root.join("xdg"));
    command
}

#[test]
fn mcp_tools_discovers_bounded_stdio_server_without_creating_product_state() {
    let root = temp_path("success");
    let output = command("ok", &root).output().expect("run MCP discovery");
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let json: Value = serde_json::from_slice(&output.stdout).expect("MCP JSON");
    assert_eq!(json["protocol_version"], "2025-11-25");
    assert_eq!(json["tools"].as_array().map(Vec::len), Some(2));
    assert_eq!(json["tools"][0]["name"], "echo");
    assert_eq!(json["tools"][1]["name"], "sum");
    assert_eq!(json["tools"][0]["input_schema"]["type"], "object");
    assert!(
        !output
            .stdout
            .windows(b"__mcp-fixture".len())
            .any(|window| { window == b"__mcp-fixture" })
    );
    assert!(!root.exists(), "discovery must not create product state");
}

#[test]
fn mcp_tools_rejects_malformed_or_oversized_server_output_without_writes() {
    for mode in ["malformed", "oversized", "hang"] {
        let root = temp_path(mode);
        let started = std::time::Instant::now();
        let output = command(mode, &root)
            .output()
            .expect("run invalid MCP discovery");
        assert!(!output.status.success(), "mode={mode} output={output:?}");
        assert!(output.stdout.is_empty(), "mode={mode} output={output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("MCP server"), "mode={mode} stderr={stderr}");
        assert!(
            !stderr.contains("__mcp-fixture"),
            "mode={mode} stderr={stderr}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(!root.exists(), "mode={mode} must not create product state");
    }
}
