use std::process::Command;

#[test]
fn presentation_smoke_emits_a_read_only_terminal_neutral_snapshot() {
    let output = Command::new(env!("CARGO_BIN_EXE_greentyper"))
        .arg("__presentation-smoke")
        .arg("--query")
        .arg("/config pro url")
        .output()
        .expect("run presentation smoke");
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");

    let snapshot: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("presentation JSON");
    assert_eq!(
        snapshot["slash"]["entries"][0]["canonical"],
        "/config provider url"
    );
    assert_eq!(snapshot["statusline"]["recovery"]["state"], "ready");
    assert_eq!(
        snapshot["statusline"]["provider_profile"]["value"],
        "simulator"
    );
    assert_eq!(snapshot["statusline"]["model"]["value"], "deterministic-v1");
    assert_eq!(
        snapshot["statusline"]["context_pressure_percent"]["state"],
        "unknown"
    );
    assert_eq!(
        snapshot["statusline"]["context_pressure"]["state"],
        "unknown"
    );
    assert_eq!(snapshot["statusline"]["one_hour_usage"]["state"], "unknown");
    assert_eq!(snapshot["statusline"]["active_agents"]["state"], "unknown");
    assert_eq!(snapshot["statusline"]["blocker_count"]["state"], "unknown");
    assert_eq!(snapshot["blockers"], serde_json::json!([]));
    assert_eq!(snapshot["layouts"].as_array().map(Vec::len), Some(3));
    assert_eq!(snapshot["layouts"][0]["viewport"]["width"], 40);
    assert_eq!(snapshot["layouts"][1]["viewport"]["width"], 80);
    assert_eq!(snapshot["layouts"][2]["viewport"]["width"], 160);
    assert_eq!(
        snapshot["layouts"][0]["body"][1]["text"],
        "> /config provider url"
    );
    assert_eq!(
        snapshot["layouts"][0]["statusline"]["rows"][0]["text"],
        "ready | blockers ? | model deterministi…"
    );
    assert_eq!(
        snapshot["layouts"][2]["statusline"]["rows"][1]["text"],
        "thread ? | items 0 | tail 0B"
    );
}

#[test]
fn presentation_smoke_has_no_filesystem_path_option() {
    let output = Command::new(env!("CARGO_BIN_EXE_greentyper"))
        .arg("__presentation-smoke")
        .arg("--run-dir")
        .arg("/presentation-private-marker")
        .arg("--query")
        .arg("/")
        .output()
        .expect("run rejected presentation smoke");

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        !output
            .stderr
            .windows(27)
            .any(|value| value == b"presentation-private-marker")
    );
}
