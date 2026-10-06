use zeppelin_embed_bench::{graph_workload::*, harness_json::json};
#[test]
fn ze77_rejects_missing_partial_or_tainted_cells() {
    let manifest = json!({"samples":3,"warmups":1,"repetitions":5});
    assert!(validate_repetition(&manifest, &json!({})).is_err());
    assert!(validate_repetition(&manifest, &json!({"samples_ns":[1,2],"tainted":false})).is_err());
    assert!(validate_repetition(&manifest, &json!({"samples_ns":[1,2,3],"tainted":true})).is_err());
    let mut matrix = json!({"samples":1000,"warmups":50,"repetitions":5,"imports":200,"reader_requests":1000,"writer_requests":200,"recovery_opens":20,"cells":required_cells()});
    validate_manifest(&matrix).unwrap();
    matrix["cells"].as_array_mut().unwrap().pop();
    assert!(validate_manifest(&matrix).is_err());
    assert!(validate_matrix(&json!({}), &[]).is_err());
}
#[test]
fn ze77_nearest_rank_and_worst_repetition() {
    assert_eq!(nearest_rank(&[40, 10, 30, 20], 50).unwrap(), 20);
    assert_eq!(nearest_rank(&[40, 10, 30, 20], 95).unwrap(), 40);
    assert_eq!(
        nearest_rank(&(1..=100).collect::<Vec<_>>(), 99).unwrap(),
        99
    );
    assert!(nearest_rank(&[], 95).is_err());
    let report = summarize(&[
        json!({"samples_ns":[10,20],"repetition":0}),
        json!({"samples_ns":[30,400_000_000],"repetition":1}),
    ])
    .unwrap();
    assert_eq!(report["worst_repetition_p95_ns"], 400_000_000_u64);
    assert_eq!(report["latency_target_pass"], false);
}
#[test]
fn ze77_recall_uses_eligible_truth_and_full_ids() {
    let a = (1_u128 << 100) | 7;
    let b = (2_u128 << 100) | 7;
    assert_eq!(recall_at_k(&[a, b], &[a], 20).unwrap(), 0.5);
    assert_eq!(recall_at_k(&[a], &[b], 20).unwrap(), 0.0);
    assert!(recall_at_k(&[a, b], &[a, a], 20).is_err());
    assert!(recall_at_k(&[], &[a], 20).is_err());
    assert_eq!(recall_at_k(&[], &[], 20).unwrap(), 1.0);
}
#[test]
fn ze77_stream_copy_rejects_digest_drift() {
    let status = std::process::Command::new("python3")
        .args(["scripts/graph-workloads.py", "self-test-copy"])
        .current_dir(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap(),
        )
        .status()
        .unwrap();
    assert!(status.success());
}
#[test]
fn ze77_public_workers_complete_small_fixture() {
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_graph-workload"))
        .arg("self-test")
        .status()
        .unwrap();
    assert!(status.success());
}
#[test]
fn ze77_complete_evidence_retains_target_failure_and_rejects_lies() {
    let manifest = json!({"samples":3,"warmups":1});
    let mut counters = json!({});
    for key in COUNTERS.into_iter().chain(RESOURCE_COUNTERS) {
        counters[key] = json!(0);
    }
    counters["engine_peak_bytes"] = json!(300 << 20);
    let check = json!({"expected_rows":[[{"node":"1267650600228229401496703205383"},{"f64_bits":"3ff0000000000000"}]],"observed_rows":[[{"node":"1267650600228229401496703205383"},{"f64_bits":"3ff0000000000000"}]],"admitted_generation":8,"truth_generation":8,"truth_ids":["1267650600228229401496703205383"],"hit_ids":["1267650600228229401496703205383"]});
    let mut good = json!({"tainted":false,"correct":true,"warmups":1,"samples_ns":[10,20,400_000_000],"disposal_ns":[1,1,1],"counters":[counters.clone(),counters.clone(),counters],"oracle_checks":[check.clone(),check.clone(),check],"duration_ms":2000,"digests_before":{"fixture":"abc"},"digests_after":{"fixture":"abc"},"monitor":[{"elapsed_ms":0,"thermal":"nominal","power":"normal","ac":true,"qos":"default","background_cpu_fraction":0.0},{"elapsed_ms":1000,"thermal":"nominal","power":"normal","ac":true,"qos":"default","background_cpu_fraction":0.0},{"elapsed_ms":2000,"thermal":"nominal","power":"normal","ac":true,"qos":"default","background_cpu_fraction":0.0}]});
    validate_repetition(&manifest, &good).unwrap();
    good["oracle_checks"][0]["recall_at_20"] = json!(0.0);
    assert!(validate_repetition(&manifest, &good).is_err());
    good["oracle_checks"][0]["recall_at_20"] = json!(1.0);
    good["oracle_checks"][0]["observed_rows"][0][0]["node"] = json!("7");
    assert!(validate_repetition(&manifest, &good).is_err());
    good["oracle_checks"][0]["observed_rows"][0][0]["node"] =
        json!("1267650600228229401496703205383");
    good["counters"][1]
        .as_object_mut()
        .unwrap()
        .remove("directory_pages_decoded");
    assert!(validate_repetition(&manifest, &good).is_err());
    good["counters"][1]["directory_pages_decoded"] = json!(0);
    good["monitor"][1]["elapsed_ms"] = json!(1700);
    assert!(validate_repetition(&manifest, &good).is_err());
}

#[test]
fn ze77_smoke_is_rejected_by_acceptance_and_keeps_taint_in_summary() {
    assert!(validate_manifest(&json!({"smoke":true})).is_err());
    let record = json!({"cell":"baseline/A/rust/structured/project-evidence","samples_ns":[10,20],"warmups":5,"correct":true,"tainted":true,"counters":[{"engine_peak_bytes":100},{"engine_peak_bytes":200}]});
    let summary = summarize_smoke(&[record]).unwrap();
    assert_eq!(summary["qualification"], false);
    assert_eq!(summary["acceptance_environment_tainted"], true);
    assert_eq!(summary["p95_ns"], 20);
}
