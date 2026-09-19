use zeppelin_embed_bench::user_bench::{
    ColdCell, Components, QualityCell, Results, SteadyCell, Summary, percentiles, recall_at_k,
    render_tables, shuffled_order,
};

#[test]
fn populated_user_report_keeps_backend_tier_and_missing_measurements_distinct() {
    let results = Results {
        chunk_rows: Some(72_000),
        scan_rows: Some(72_000),
        graph_rows: Some(72_000),
        promote_seconds: Some(2.5),
        cold: vec![
            ColdCell {
                backend: "MLX GPU".into(),
                launch: "first-ever".into(),
                open_ms: 900.0,
                first_query_ms: 100.0,
                total_ms: 1_000.0,
            },
            ColdCell {
                backend: "CoreML CPU_AND_NE requested".into(),
                launch: "first-ever (fresh model digest)".into(),
                open_ms: 900.0,
                first_query_ms: 101.0,
                total_ms: 1_001.0,
            },
        ],
        steady: vec![
            SteadyCell {
                leg: "Dense".into(),
                backend: "MLX GPU".into(),
                store_tier: "scan".into(),
                summary: Summary {
                    p50: 12.0,
                    p95: 15.0,
                    p99: 18.0,
                    mean: 13.0,
                },
            },
            SteadyCell {
                leg: "Dense".into(),
                backend: "MLX GPU".into(),
                store_tier: "graph".into(),
                summary: Summary {
                    p50: 3.0,
                    p95: 5.0,
                    p99: 6.0,
                    mean: 4.0,
                },
            },
        ],
        quality: vec![
            QualityCell {
                leg: "Dense".into(),
                tier: "graph".into(),
                ndcg_at_10: 0.75,
                recall_mlx: Some(0.9),
                recall_ane: None,
            },
            QualityCell {
                leg: "Lexical".into(),
                tier: "n/a".into(),
                ndcg_at_10: 0.625,
                recall_mlx: None,
                recall_ane: None,
            },
        ],
        components: Some(Components {
            bundle_open_ms: 1.0,
            document_tower_ms: 2.0,
            mlx_query_ms: 3.0,
            ane_query_ms: Some(4.0),
            store_open_ms: 5.0,
        }),
        dense_graph_mlx_repetitions: vec![99.0, 3.0, 3.125, 3.25],
        truncated_queries: Some(7),
    };
    let rendered = render_tables(&results);
    for expected in [
        "57,638 documents, 72000 chunk rows",
        "`SealedScan` (72000 rows)",
        "`SealedGraph` (72000 rows), promoted in 2.500000 s",
        "| MLX GPU | first-ever | 900.000 | 100.000 | 1000.000 | met |",
        "| CoreML CPU_AND_NE requested | first-ever (fresh model digest) | 900.000 | 101.000 | 1001.000 | missed |",
        "| MLX GPU | relaunch (median of 10) | | | | |",
        "| Dense | MLX GPU | 12.000 | 3.000 | 9.000 | 4.000 | 15.000 | 5.000 | 6.000 |",
        "| Dense | CoreML CPU_AND_NE requested | | | | | | | |",
        "bundle open 1.000 / document tower (MLX)\n2.000 / query runtime (MLX 3.000, CoreML 4.000) / core store open 5.000",
        "p50 3.000 / 3.125 / 3.250 ms",
        "| Dense | graph | 0.750000 | 0.900000 |  |",
        "| Lexical | n/a | 0.625000 | exact by construction | exact by construction |",
        "| Dense | scan | | | |",
        "| MLX GPU | not measured | not measured | not measured | not measured |",
        "Queries truncated by `max_tokens` on FiQA: 7 of 648.",
    ] {
        assert!(
            rendered.contains(expected),
            "missing report contract {expected:?}\n{rendered}"
        );
    }
    assert!(!rendered.contains("99.000"));
}

#[test]
fn user_statistics_handle_empty_sets_duplicates_and_nearest_rank_boundaries() {
    assert_eq!(percentiles(&[]), None);
    assert_eq!(recall_at_k(&[1, 1, 2], &[]), 0.0);
    assert_eq!(recall_at_k(&[1, 1, 2, 3], &[1, 1, 4, 5, 6]), 0.25);
    let samples: Vec<_> = (1..=100).rev().map(f64::from).collect();
    assert_eq!(
        percentiles(&samples),
        Some(Summary {
            p50: 50.0,
            p95: 95.0,
            p99: 99.0,
            mean: 50.5,
        })
    );
}

#[test]
fn seeded_query_order_is_repeatable_and_contains_every_row_once() {
    assert_eq!(shuffled_order(0, 17), Vec::<usize>::new());
    assert_eq!(shuffled_order(1, 17), vec![0]);
    let order = shuffled_order(257, 17);
    assert_eq!(order, shuffled_order(257, 17));
    assert_ne!(order, shuffled_order(257, 18));
    assert_ne!(order, (0..257).collect::<Vec<_>>());
    let mut sorted = order;
    sorted.sort_unstable();
    assert_eq!(sorted, (0..257).collect::<Vec<_>>());
}

#[test]
fn kernel_gate_reports_all_invalid_measurements_before_computing_ratios() {
    use zeppelin_embed_bench::kernel_gate::{
        GateFailure, KernelMeasurements, evaluate_kernel_measurements,
    };
    let failures = evaluate_kernel_measurements(KernelMeasurements {
        i8_ns: 0.0,
        u1_ns: -1.0,
        f16_ns: f64::INFINITY,
        f32_ns: f64::NAN,
    })
    .expect_err("invalid measurements cannot produce a performance report");
    assert_eq!(failures.len(), 4);
    for (failure, (kernel, expected)) in failures.iter().zip([
        ("i8", 0.0_f64),
        ("u1", -1.0),
        ("f16", f64::INFINITY),
        ("f32", f64::NAN),
    ]) {
        assert!(
            matches!(failure, GateFailure::InvalidMeasurement { kernel: actual, ns }
            if *actual == kernel && ns.to_bits() == expected.to_bits())
        );
        assert_eq!(
            failure.to_string(),
            format!("{kernel} returned invalid latency {expected} ns/vector")
        );
    }
    let failures = evaluate_kernel_measurements(KernelMeasurements {
        i8_ns: 10.0,
        u1_ns: 3.0,
        f16_ns: 1_000.0,
        f32_ns: 1_000.0,
    })
    .expect_err("both slow wide kernels must report their floors and ratios");
    assert_eq!(failures.len(), 4);
    for failure in failures {
        match failure {
            GateFailure::RooflineFloor { kernel, .. } => {
                assert!(matches!(kernel, "f16" | "f32"));
                assert!(failure.to_string().contains("is below floor"));
            }
            GateFailure::RatioCeiling {
                numerator,
                actual,
                ceiling,
                ..
            } => {
                assert_eq!(actual, 100.0);
                assert_eq!(ceiling, if numerator == "f16" { 6.0 } else { 8.0 });
                assert_eq!(
                    failure.to_string(),
                    format!(
                        "{numerator}/i8 latency ratio 100.000000x exceeds ceiling {ceiling:.6}x"
                    )
                );
            }
            other => panic!("valid positive measurements cannot be invalid: {other:?}"),
        }
    }
}

#[test]
fn process_summary_rejects_invalid_counts_and_values_with_actionable_diagnostics() {
    use zeppelin_embed_bench::process_median::{ProcessMedian, ProcessMedianError};
    for count in [0, 1, 2, 4] {
        let error = ProcessMedian::new(vec![1.0; count]).expect_err("odd N>=3 is mandatory");
        assert_eq!(error, ProcessMedianError::ProcessCount(count));
        assert_eq!(
            error.to_string(),
            format!("across-process median requires an odd count of at least three, got {count}")
        );
    }
    for value in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let error = ProcessMedian::new(vec![1.0, value, 3.0])
            .expect_err("invalid observations cannot enter the summary");
        assert_eq!(error, ProcessMedianError::InvalidObservation);
        assert_eq!(
            error.to_string(),
            "process observations must be positive and finite"
        );
    }
}

struct CalibrationProbe {
    load_checks: std::cell::Cell<usize>,
    allow_checks: usize,
}

impl zeppelin_embed_bench::frontier::measure::MachineProbe for CalibrationProbe {
    fn pmset(
        &self,
        arguments: &[&str],
    ) -> std::io::Result<zeppelin_embed_bench::frontier::measure::ProbeOutput> {
        use zeppelin_embed_bench::frontier::measure::ProbeOutput;
        match arguments {
            ["-g", "ps"] => Ok(ProbeOutput::success("Now drawing from 'AC Power'")),
            ["-g", "therm"] => Ok(ProbeOutput::success(
                "No thermal warning level has been recorded\nNo performance warning level has been recorded",
            )),
            _ => panic!("unexpected probe request {arguments:?}"),
        }
    }
    fn concurrent_load_check(&self) -> Result<(), String> {
        let checks = self.load_checks.get();
        self.load_checks.set(checks + 1);
        if checks < self.allow_checks {
            Ok(())
        } else {
            Err("controlled competing build".into())
        }
    }
}

#[test]
fn compute_calibration_stays_idle_on_load_and_rejects_overflow_before_sampling() {
    use zeppelin_embed_bench::frontier::roofline::{
        ComputeCalibrationConfig, ComputeCalibrationOutcome, calibrate_compute_tiers,
    };
    let busy = CalibrationProbe {
        load_checks: 0.into(),
        allow_checks: 0,
    };
    let outcome = calibrate_compute_tiers(&busy, ComputeCalibrationConfig::evidence())
        .expect("busy preflight idles");
    assert_eq!(
        outcome,
        ComputeCalibrationOutcome::Idle {
            reasons: vec!["concurrent load not excluded: controlled competing build".into()]
        }
    );
    assert_eq!(busy.load_checks.get(), 1);
    #[cfg(target_arch = "aarch64")]
    {
        use zeppelin_embed_bench::frontier::measure::MeasurementConfig;
        use zeppelin_embed_bench::frontier::roofline::ComputeCalibrationError;

        let ready = CalibrationProbe {
            load_checks: 0.into(),
            allow_checks: usize::MAX,
        };
        let error = calibrate_compute_tiers(
            &ready,
            ComputeCalibrationConfig {
                iterations_per_sample: u64::MAX,
                measurement: MeasurementConfig::strict(),
            },
        )
        .expect_err("operation count overflow must fail before enormous warmup");
        assert!(matches!(error, ComputeCalibrationError::InvalidIterations));
        assert_eq!(ready.load_checks.get(), 1);
    }
}

#[test]
#[cfg(target_arch = "aarch64")]
fn compute_calibration_honors_a_load_veto_after_warmup_without_publishing_a_rate() {
    use zeppelin_embed_bench::frontier::measure::{MeasurementConfig, MeasurementError};
    use zeppelin_embed_bench::frontier::roofline::{
        ComputeCalibrationConfig, ComputeCalibrationError, calibrate_compute_tiers,
    };
    let probe = CalibrationProbe {
        load_checks: 0.into(),
        allow_checks: 1,
    };
    let error = calibrate_compute_tiers(
        &probe,
        ComputeCalibrationConfig {
            iterations_per_sample: 1,
            measurement: MeasurementConfig {
                warmup_repetitions: 1,
                ..MeasurementConfig::strict()
            },
        },
    )
    .expect_err("a ready preflight cannot override the post-warmup load veto");
    assert!(
        matches!(&error, ComputeCalibrationError::Measurement(MeasurementError::ConcurrentLoad(reason)) if reason == "controlled competing build")
    );
    assert_eq!(
        error.to_string(),
        "compute calibration failed: measurement refused concurrent load: controlled competing build"
    );
    assert_eq!(probe.load_checks.get(), 2);
}

#[test]
fn attested_compute_calibration_cannot_override_busy_preflight_or_zero_work() {
    use zeppelin_embed_bench::frontier::roofline::{
        ComputeCalibrationConfig, ComputeCalibrationError, ComputeCalibrationOutcome,
        calibrate_compute_tiers_with_attestation,
    };
    let busy = CalibrationProbe {
        load_checks: 0.into(),
        allow_checks: 0,
    };
    let outcome = calibrate_compute_tiers_with_attestation(
        &busy,
        &super::MissingAttestationSource,
        ComputeCalibrationConfig::evidence(),
    )
    .expect("unsafe machine idles");
    let ComputeCalibrationOutcome::Idle { reasons } = outcome else {
        panic!("busy machine must never calibrate")
    };
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("controlled competing build"))
    );
    let ready = CalibrationProbe {
        load_checks: 0.into(),
        allow_checks: usize::MAX,
    };
    let error = calibrate_compute_tiers_with_attestation(
        &ready,
        &super::MissingAttestationSource,
        ComputeCalibrationConfig {
            iterations_per_sample: 0,
            ..ComputeCalibrationConfig::evidence()
        },
    )
    .expect_err("zero work is invalid before preflight");
    assert!(matches!(error, ComputeCalibrationError::InvalidIterations));
    assert_eq!(ready.load_checks.get(), 0);
}

#[test]
fn ledger_io_failures_name_the_affected_path_and_preserve_observed_history() {
    use zeppelin_embed_bench::frontier::ledger::{Ledger, LedgerError};
    let directory = tempfile::tempdir().expect("ledger fixture directory");
    let parent_file = directory.path().join("file-not-directory");
    std::fs::write(&parent_file, "sentinel").expect("blocked parent");
    let error =
        Ledger::open(parent_file.join("ledger.jsonl")).expect_err("file parent cannot hold ledger");
    assert!(
        matches!(&error, LedgerError::Io { path, reason } if path == &parent_file && !reason.is_empty())
    );
    assert!(
        error
            .to_string()
            .contains(&parent_file.display().to_string())
    );
    let error = Ledger::open(directory.path()).expect_err("directory cannot be ledger file");
    assert!(matches!(&error, LedgerError::Io { path, .. } if path == directory.path()));

    let path = directory.path().join("ledger.jsonl");
    let mut ledger = Ledger::open(&path).expect("new ledger opens");
    ledger
        .append(&super::frontier_row("retained", 1.0))
        .expect("initial row appends");
    let original = std::fs::read(&path).expect("original bytes");
    std::fs::remove_file(&path).expect("simulate external removal");
    for result in [
        ledger.rows().map(|_| ()),
        ledger.append(&super::frontier_row("not appended", 2.0)),
    ] {
        let error = result.expect_err("removed ledger fails closed");
        assert!(matches!(&error, LedgerError::Io { path: actual, .. } if actual == &path));
        assert!(error.to_string().contains(&path.display().to_string()));
    }
    assert!(
        !path.exists(),
        "failed append must not recreate a lost ledger"
    );
    std::fs::write(&path, &original).expect("restore exact observed bytes");
    assert_eq!(ledger.rows().expect("restored bytes remain valid").len(), 1);
    std::fs::write(&path, b"changed\n").expect("simulate external rewrite");
    let error = ledger
        .rows()
        .expect_err("changed bytes are not silently adopted");
    assert_eq!(error, LedgerError::PriorContentChanged);
    assert!(
        error
            .to_string()
            .contains("previously observed ledger bytes were changed")
    );
}

#[test]
fn ledger_rejects_blank_attestation_and_corrupt_counter_evidence_before_adoption() {
    use serde_json::json;
    use zeppelin_embed_bench::frontier::attestation::MachineStateProvenance;
    use zeppelin_embed_bench::frontier::ledger::{Ledger, LedgerError, LedgerStatus};
    use zeppelin_embed_bench::frontier::tune::CampaignStop;
    let directory = tempfile::tempdir().expect("ledger fixture directory");
    let path = directory.path().join("ledger.jsonl");
    let mut ledger = Ledger::open(&path).expect("new ledger");
    let mut row = super::frontier_row("attestation required", 1.0);
    row.machine_state = MachineStateProvenance::OperatorAttestation {
        timestamp: " ".into(),
        machine_identifier: "Mac15,9".into(),
    };
    let error = ledger
        .append(&row)
        .expect_err("attestation timestamp cannot be blank");
    assert_eq!(
        error.to_string(),
        "ledger row is invalid: attested machine state requires timestamp and machine identifier"
    );
    assert_eq!(std::fs::read(&path).expect("unchanged ledger"), b"");
    let stop = CampaignStop::FrontierOpen {
        reason: "counter evidence absent".into(),
        hypotheses: vec!["measure stalls".into()],
    };
    row.status = LedgerStatus::from_campaign_stop(&stop);
    row.machine_state = MachineStateProvenance::DirectProbe;
    ledger
        .append(&row)
        .expect("open frontier is valid without completion authority");
    assert_eq!(ledger.rows().expect("roundtrip")[0], row);
    let valid: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("valid row")).expect("row json");
    let mut invalid = valid.clone();
    invalid["machine-state"]["kind"] = json!("assumed-safe");
    let mut invalid_counter = valid;
    invalid_counter["status"] = json!({
        "kind": "complete", "attribution": {
            "evidence_path": "trace", "conclusion": "irreducible",
            "counters": [{"name": "ipc", "value": 1.0, "unit": " ", "showed": "no issue slots"}]
        }
    });
    for (value, expected) in [
        (invalid, "unknown machine-state kind assumed-safe"),
        (invalid_counter, "PMU counter reading is incomplete"),
    ] {
        std::fs::write(&path, format!("{value}\n")).expect("corrupt fixture");
        let error = Ledger::open(&path).expect_err("invalid stored authority cannot be adopted");
        assert!(
            matches!(&error, LedgerError::CorruptHistory(reason) if reason.contains("line 1") && reason.contains(expected))
        );
        assert!(error.to_string().starts_with("ledger history is corrupt:"));
    }
}

#[test]
fn calibration_loading_retains_io_and_json_causes_through_the_roofline_api() {
    use std::error::Error;
    use zeppelin_embed_bench::frontier::calibration::{CalibrationError, load_calibration};
    use zeppelin_embed_bench::frontier::roofline::{RooflineLoadError, RooflineModel};
    let directory = tempfile::tempdir().expect("calibration fixture");
    let path = directory.path().join("missing.json");
    let error = load_calibration(&path).expect_err("missing calibration is an I/O error");
    assert!(matches!(error, CalibrationError::Io(_)));
    assert!(error.source().is_some());
    std::fs::write(&path, b"{broken}").expect("bad JSON fixture");
    let error = RooflineModel::from_persisted_calibration(&path)
        .expect_err("invalid JSON is not replaced by default ceilings");
    assert!(matches!(
        error,
        RooflineLoadError::Calibration(CalibrationError::Json(_))
    ));
    assert!(
        error
            .to_string()
            .starts_with("persisted calibration failed: calibration JSON failed:")
    );
    assert!(
        error
            .source()
            .expect("calibration cause")
            .source()
            .is_some()
    );
    std::fs::write(&path, b"{}").expect("bad schema fixture");
    let error = load_calibration(&path).expect_err("missing schema is invalid artifact");
    assert!(matches!(error, CalibrationError::InvalidArtifact(_)));
    assert!(error.source().is_none());
}

#[test]
fn measurement_odd_runs_use_the_middle_observation() {
    use zeppelin_embed_bench::frontier::measure::{MeasurementConfig, measure_source};
    let config = MeasurementConfig {
        warmup_repetitions: 0,
        repetitions_per_run: 31,
        accepted_runs: 1,
        maximum_attempts: 1,
        maximum_rsd_percent: 2.0,
    };
    let mut source = super::ScriptedSamples {
        samples: std::iter::repeat_n(99.0, 15)
            .chain([100.0])
            .chain(std::iter::repeat_n(101.0, 15))
            .collect(),
        warmups: 0,
    };
    let result = measure_source(&mut source, config).expect("stable odd sample count");
    assert_eq!(result.accepted_run_medians_ns, [100.0]);
    assert_eq!(result.discarded_runs, 0);
}

#[test]
fn measurement_rejects_nonfinite_variance_from_finite_samples() {
    use zeppelin_embed_bench::frontier::measure::{
        MeasurementConfig, MeasurementError, measure_source,
    };
    let config = MeasurementConfig {
        warmup_repetitions: 0,
        repetitions_per_run: 31,
        accepted_runs: 1,
        maximum_attempts: 1,
        maximum_rsd_percent: 2.0,
    };
    let mut source = super::ScriptedSamples {
        samples: std::iter::repeat_n(f64::MAX, 31).collect(),
        warmups: 0,
    };
    let error = measure_source(&mut source, config)
        .expect_err("an overflowing sum cannot become an accepted zero-variance run");
    assert!(matches!(
        error,
        MeasurementError::VarianceBudgetExhausted {
            discarded_runs: 1,
            accepted_runs_required: 1
        }
    ));
}
