#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[test]
fn ze74_cli_rejects_unknown_cases() {
    let r = std::process::Command::new(env!("CARGO_BIN_EXE_graph-profile"))
        .args(["run", "missing", "rust-cypher"])
        .output()
        .unwrap();
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("unknown case"));
}
