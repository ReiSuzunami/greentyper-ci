#![cfg(feature = "bench-storage")]

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn both_storage_candidates_survive_the_cross_process_crash_workload() {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let run_dir: PathBuf = std::env::temp_dir().join(format!(
        "greentyper-storage-crash-integration-{}-{timestamp}",
        std::process::id()
    ));
    fs::create_dir(&run_dir).expect("run directory");
    let mut digests = Vec::new();

    for implementation in ["sqlite-wal", "append-log"] {
        let output_path = run_dir.join(format!("{implementation}.json"));
        let output = Command::new(env!("CARGO_BIN_EXE_greentyper-acceptance"))
            .args([
                "bench",
                "--comparison",
                "storage",
                "--implementation",
                implementation,
                "--workload",
                "cross-process-crash-replay",
                "--candidate-id",
                "integration-test",
                "--source-revision",
                "0123456789abcdef0123456789abcdef01234567",
                "--output",
            ])
            .arg(&output_path)
            .args([
                "--runs",
                "1",
                "--warmup-runs",
                "1",
                "--machine-identifiers",
                "redacted",
            ])
            .output()
            .expect("acceptance child");
        assert!(
            output.status.success(),
            "{implementation} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let evidence: Value =
            serde_json::from_slice(&fs::read(&output_path).expect("evidence")).expect("JSON");
        assert_eq!(evidence["workload"]["process_mode"], "cross-process");
        assert_eq!(evidence["samples"][0]["operation_units"], 6);
        assert_eq!(evidence["samples"][0]["gauges"]["known_not_repeated"], 2);
        assert_eq!(evidence["samples"][0]["gauges"]["ambiguous_blocked"], 4);
        digests.push(
            evidence["samples"][0]["output_digest"]
                .as_str()
                .expect("digest")
                .to_owned(),
        );
    }

    assert_eq!(digests[0], digests[1]);
    fs::remove_dir_all(run_dir).expect("cleanup");
}
