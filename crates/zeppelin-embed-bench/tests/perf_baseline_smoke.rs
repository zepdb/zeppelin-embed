#![allow(clippy::expect_used)]

use std::path::Path;
use std::process::Command;

fn run_smoke(binary: &str) -> String {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace)
        .args([
            "run",
            "--quiet",
            "-p",
            "zeppelin-embed-bench",
            "--bin",
            binary,
            "--",
            "--smoke",
            "--load-limit",
            "3.0",
        ])
        .output()
        .expect("smoke process starts");
    assert!(
        output.status.success(),
        "{binary} smoke failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("smoke output is UTF-8")
}

#[test]
fn perf_query_baseline_smoke_emits_every_query_probe() {
    let stdout = run_smoke("perf-query-baseline");
    assert!(stdout.contains("PERF_LEX_RESULT terms=2"));
    assert!(stdout.contains("PERF_LEX_RESULT terms=4"));
    assert!(stdout.contains("PERF_STORE_RESULT probe=hybrid"));
    assert!(stdout.contains("PERF_STORE_RESULT probe=lexical"));
    assert!(stdout.contains("PERF_STORE_RESULT probe=lexical_structured"));
}

#[test]
fn perf_write_baseline_smoke_emits_ingest_and_mixed_probes() {
    let stdout = run_smoke("perf-write-baseline");
    assert!(stdout.contains("PERF_INGEST_RESULT"));
    assert!(stdout.contains("PERF_MIXED_RESULT durability=derived"));
    assert!(stdout.contains("PERF_MIXED_RESULT durability=durable"));
}
