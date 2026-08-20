use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use zeppelin_embed::kernels::InstructionTier;
use zeppelin_embed_bench::frontier::attestation::{
    AttestationSource, AttestationWriteError, CampaignPreflightOutcome, FileAttestationSource,
    MachineIdentifierProbe, MachineStateProvenance, default_attestation_path,
    machine_identifier_with_probe, preflight_with_attestation, write_operator_attestation,
};
use zeppelin_embed_bench::frontier::calibration::{
    CalibrationArtifact, CalibrationError, CalibrationMachineContext, CalibrationRun,
    CalibrationTier, CalibrationWritePolicy, default_calibration_path, load_calibration,
    persist_calibration,
};
use zeppelin_embed_bench::frontier::cli::{
    CliError, Command as FrontierCommand, DEFAULT_SEED, DenominatorCommand, TuneCampaign,
    TuneCommand, parse_command,
};
use zeppelin_embed_bench::frontier::ledger::{
    Ledger, LedgerAttribution, LedgerDecision, LedgerError, LedgerRow, LedgerStatus, LedgerSummary,
};
use zeppelin_embed_bench::frontier::measure::{
    MachineProbe, MeasurementConfig, MeasurementError, PreflightOutcome, ProbeOutput, SampleSource,
    StridedI8Workload, SyntheticI8Workload, Workload, WorkloadDescriptor, WorkloadError,
    WorkloadObservation, WorkloadSampler, measure_source, measure_source_with_provenance,
    preflight,
};
use zeppelin_embed_bench::frontier::pmu::{
    AttributionClass, CounterReading, PmuError, PmuOutcome, PmuReport, capture_cpu_counters,
    parse_counter_export,
};
use zeppelin_embed_bench::frontier::roofline::{
    BindingBound, ComputeCalibrationConfig, ComputeCalibrationError, ComputeCalibrationOutcome,
    ComputeCeiling, ComputeTier, DenominatorProvenance, RooflineDiagnostic, RooflineError,
    RooflineInput, RooflineLoadError, RooflineModel, WIDE_LOAD_SINGLE_CORE_GBPS,
    calibrate_compute_tiers,
};
use zeppelin_embed_bench::frontier::tune::{
    CampaignDecision, CampaignSignals, CampaignStop, CandidateDisposition, CandidateEvaluation,
    SearchConfig, SearchSpace, TuneError, load_search_state, rank_candidates, run_search,
    save_search_state,
};
use zeppelin_embed_bench::frontier::variants::{KernelPoint, VariantRegistry};

#[test]
fn frontier_roofline_math_matches_hand_computed_memory_and_compute_cases() {
    let provenance =
        DenominatorProvenance::measured("worked example", "frontier test", "2026-08-20")
            .expect("the worked provenance is complete");
    let mut model = RooflineModel::new();
    model
        .revise_memory_ceiling(1, 2.0, provenance.clone())
        .expect("the first measured ceiling is accepted");
    model
        .revise_compute_ceiling(
            ComputeCeiling::new(ComputeTier::NeonFma, 3.0e9, provenance)
                .expect("the compute ceiling is valid"),
        )
        .expect("the first compute ceiling is accepted");

    let memory = model
        .score(RooflineInput {
            bytes_touched: 4_000_000_000,
            operation_count: 6_000_000_000,
            elapsed_seconds: 4.0,
            cores: 1,
            compute_tier: ComputeTier::NeonFma,
            binding: BindingBound::Memory,
        })
        .expect("the hand-computed memory case scores");
    assert_eq!(memory.memory_bound_seconds, 2.0);
    assert_eq!(memory.compute_bound_seconds, 2.0);
    assert_eq!(memory.achieved_percent, 50.0);
    assert_eq!(memory.binding, BindingBound::Memory);

    let compute = model
        .score(RooflineInput {
            bytes_touched: 1_000_000_000,
            operation_count: 6_000_000_000,
            elapsed_seconds: 4.0,
            cores: 1,
            compute_tier: ComputeTier::NeonFma,
            binding: BindingBound::Compute,
        })
        .expect("the hand-computed compute case scores");
    assert_eq!(compute.achieved_percent, 50.0);
    assert_eq!(compute.binding, BindingBound::Compute);
}

#[test]
fn frontier_roofline_above_one_hundred_is_loud_and_denominators_only_move_up() {
    assert_eq!(WIDE_LOAD_SINGLE_CORE_GBPS, 80.689_179);
    let provenance = DenominatorProvenance::measured(
        "BL-013 wide-load",
        "cargo run --release -p zeppelin-embed-bench --bin platform-truth -- bandwidth-compare",
        "2026-08-20",
    )
    .expect("the adopted provenance is complete");
    let mut model = RooflineModel::new();
    model
        .revise_memory_ceiling(1, WIDE_LOAD_SINGLE_CORE_GBPS, provenance.clone())
        .expect("the adopted ceiling is accepted");
    assert!(
        model.revise_memory_ceiling(1, 70.0, provenance).is_err(),
        "a denominator revision may never move downward"
    );
    model
        .revise_compute_ceiling(
            ComputeCeiling::new(
                ComputeTier::NeonFma,
                1.0,
                DenominatorProvenance::measured(
                    "worked compute companion",
                    "frontier test",
                    "2026-08-20",
                )
                .expect("the companion provenance is complete"),
            )
            .expect("the companion compute ceiling is valid"),
        )
        .expect("the companion compute ceiling is accepted");
    let score = model
        .score(RooflineInput {
            bytes_touched: 80_689_179_000,
            operation_count: 1,
            elapsed_seconds: 0.5,
            cores: 1,
            compute_tier: ComputeTier::NeonFma,
            binding: BindingBound::Memory,
        })
        .expect("the stale-denominator case scores");
    assert_eq!(score.achieved_percent, 200.0);
    assert!(matches!(
        score.diagnostic,
        RooflineDiagnostic::DenominatorStale { .. }
    ));
    assert!(score.loud_message().contains("DENOMINATOR STALE"));
}

#[test]
fn frontier_roofline_selects_declared_bound_and_exactly_one_hundred_is_not_stale() {
    let provenance =
        DenominatorProvenance::measured("worked boundary", "frontier test", "2026-08-20")
            .expect("the boundary provenance is complete");
    let mut model = RooflineModel::new();
    model
        .revise_memory_ceiling(1, 2.0, provenance.clone())
        .expect("the memory ceiling installs");
    model
        .revise_compute_ceiling(
            ComputeCeiling::new(ComputeTier::NeonFma, 4.0e9, provenance)
                .expect("the compute ceiling is valid"),
        )
        .expect("the compute ceiling installs");
    let input = RooflineInput {
        bytes_touched: 4_000_000_000,
        operation_count: 4_000_000_000,
        elapsed_seconds: 2.0,
        cores: 1,
        compute_tier: ComputeTier::NeonFma,
        binding: BindingBound::Memory,
    };
    let memory = model.score(input).expect("the memory-bound case scores");
    assert_eq!(memory.memory_bound_seconds, 2.0);
    assert_eq!(memory.compute_bound_seconds, 1.0);
    assert_eq!(memory.achieved_percent, 100.0);
    assert_eq!(memory.diagnostic, RooflineDiagnostic::WithinDenominator);
    assert_eq!(memory.loud_message(), "roofline 100.000% (Memory binding)");

    let compute = model
        .score(RooflineInput {
            binding: BindingBound::Compute,
            ..input
        })
        .expect("the compute-bound case scores");
    assert_eq!(compute.achieved_percent, 50.0);
    assert_eq!(compute.binding, BindingBound::Compute);
}

#[test]
fn frontier_roofline_rejects_missing_invalid_and_downward_denominators() {
    let provenance =
        DenominatorProvenance::measured("error fixture", "frontier test", "2026-08-20")
            .expect("the error fixture provenance is complete");
    for blank in [
        DenominatorProvenance::measured("", "command", "date"),
        DenominatorProvenance::measured("method", "", "date"),
        DenominatorProvenance::measured("method", "command", ""),
    ] {
        assert!(matches!(blank, Err(RooflineError::MissingProvenance)));
    }
    for invalid in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        assert!(matches!(
            ComputeCeiling::new(ComputeTier::NeonFma, invalid, provenance.clone()),
            Err(RooflineError::InvalidPositiveValue(value)) if value.to_bits() == invalid.to_bits()
        ));
    }

    let input = RooflineInput {
        bytes_touched: 1,
        operation_count: 1,
        elapsed_seconds: 1.0,
        cores: 1,
        compute_tier: ComputeTier::NeonFma,
        binding: BindingBound::Memory,
    };
    let mut model = RooflineModel::new();
    assert!(matches!(
        model.score(input),
        Err(RooflineError::MissingMemoryCeiling { cores: 1 })
    ));
    assert!(matches!(
        model.revise_memory_ceiling(0, 1.0, provenance.clone()),
        Err(RooflineError::ZeroCoreCount)
    ));
    model
        .revise_memory_ceiling(1, 1.0, provenance.clone())
        .expect("a valid memory ceiling installs");
    assert!(matches!(
        model.score(input),
        Err(RooflineError::MissingComputeCeiling {
            tier: ComputeTier::NeonFma
        })
    ));
    model
        .revise_compute_ceiling(
            ComputeCeiling::new(ComputeTier::NeonFma, 10.0, provenance.clone())
                .expect("the initial compute ceiling is valid"),
        )
        .expect("the initial compute ceiling installs");
    let downward = model
        .revise_compute_ceiling(
            ComputeCeiling::new(ComputeTier::NeonFma, 9.0, provenance.clone())
                .expect("the lower positive ceiling is structurally valid"),
        )
        .expect_err("compute ceilings must never silently move downward");
    assert_eq!(
        downward,
        RooflineError::DownwardRevision {
            previous: 10.0,
            proposed: 9.0,
        }
    );
    assert!(downward.to_string().contains("upward-only"));
    assert!(matches!(
        model.score(RooflineInput {
            elapsed_seconds: 0.0,
            ..input
        }),
        Err(RooflineError::InvalidPositiveValue(0.0))
    ));
}

#[test]
fn frontier_compute_calibration_obeys_preflight_and_strict_statistics() {
    let evidence = ComputeCalibrationConfig::evidence();
    assert_eq!(evidence.measurement.repetitions_per_run, 30);
    assert_eq!(evidence.measurement.maximum_rsd_percent, 2.0);
    let invalid = calibrate_compute_tiers(
        &MockProbe {
            power: ProbeOutput::success("Now drawing from 'AC Power'"),
            thermal: ProbeOutput::success(
                "No thermal warning level has been recorded\nNo performance warning level has been recorded",
            ),
        },
        ComputeCalibrationConfig {
            iterations_per_sample: 0,
            measurement: MeasurementConfig::strict(),
        },
    )
    .expect_err("zero saturation work cannot produce a denominator");
    assert!(matches!(
        &invalid,
        ComputeCalibrationError::InvalidIterations
    ));
    assert!(invalid.to_string().contains("nonzero and bounded"));

    let outcome = calibrate_compute_tiers(
        &MockProbe {
            power: ProbeOutput::success("Now drawing from 'AC Power'"),
            thermal: ProbeOutput::success(
                "No thermal warning level has been recorded\nNo performance warning level has been recorded",
            ),
        },
        ComputeCalibrationConfig::evidence(),
    )
    .expect("a ready machine reaches the platform-specific calibration result");
    let ComputeCalibrationOutcome::Measured {
        calibrations,
        not_measured,
    } = outcome
    else {
        panic!("a ready preflight cannot return Idle");
    };
    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!(calibrations.len() + not_measured.len(), 3);
        assert!(!calibrations.is_empty());
        for calibration in calibrations {
            assert!(calibration.operations_per_second.is_finite());
            assert!(calibration.operations_per_second > 0.0);
            assert_eq!(calibration.measurement.accepted_run_medians_ns.len(), 3);
            assert!(
                calibration
                    .measurement
                    .accepted_run_rsd_percent
                    .iter()
                    .all(|rsd| *rsd <= 2.0)
            );
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        assert!(calibrations.is_empty());
        assert_eq!(not_measured.len(), 3);
        assert!(
            not_measured
                .iter()
                .all(|(_, reason)| reason.contains("requires AArch64"))
        );
    }
}

#[test]
fn frontier_calibration_rejects_lower_without_override_and_records_override() {
    let path = unique_temp_path("compute-calibration");
    let initial = calibration_artifact(100.0, "2026-08-20");
    persist_calibration(&path, &initial, CalibrationWritePolicy::UpwardOnly)
        .expect("the initial measured calibration persists");
    let initial_bytes = fs::read(&path).expect("the initial calibration bytes are readable");

    let lower = calibration_artifact(90.0, "2026-08-21");
    let error = persist_calibration(&path, &lower, CalibrationWritePolicy::UpwardOnly)
        .expect_err("a lower ceiling must be rejected without an explicit override");
    assert!(matches!(
        error,
        CalibrationError::DownwardRevision {
            tier: ComputeTier::NeonFma,
            persisted_gmac_per_second: 100.0,
            proposed_gmac_per_second: 90.0,
        }
    ));
    assert!(error.to_string().contains("degraded machine state"));
    assert_eq!(
        fs::read(&path).expect("the rejected artifact remains readable"),
        initial_bytes,
        "rejection must not rewrite the persisted calibration"
    );

    persist_calibration(
        &path,
        &lower,
        CalibrationWritePolicy::AllowLower {
            reason: "operator invalidated the earlier measurement".to_owned(),
        },
    )
    .expect("the explicit calibration-only override accepts the lower value");
    let accepted = load_calibration(&path).expect("the overridden artifact reloads");
    assert_eq!(
        accepted
            .tier(ComputeTier::NeonFma)
            .expect("the FMA tier remains present")
            .adopted_gmac_per_second,
        90.0
    );
    let revision = accepted
        .revision_history
        .last()
        .expect("the override creates a revision record");
    assert!(revision.override_used);
    assert_eq!(
        revision.reason,
        "operator invalidated the earlier measurement"
    );
    assert!(revision.changes.iter().any(|change| {
        change.tier == ComputeTier::NeonFma
            && change.previous_gmac_per_second == Some(100.0)
            && change.adopted_gmac_per_second == 90.0
    }));
    let _ = fs::remove_file(path);
}

#[test]
fn frontier_calibration_rejects_incoherent_raw_run_provenance() {
    let valid = calibration_artifact(100.0, "2026-08-20");
    let base = valid
        .tier(ComputeTier::NeonFma)
        .expect("the FMA fixture exists")
        .clone();
    let mut cases = Vec::new();
    cases.push((0.0, base.runs.clone(), "positive and finite"));
    cases.push((100.0, Vec::new(), "has no raw runs"));
    cases.push((
        100.0,
        vec![CalibrationRun {
            run: 0,
            ..base.runs[0].clone()
        }],
        "invalid run numbers",
    ));
    cases.push((
        100.0,
        vec![base.runs[0].clone(), base.runs[0].clone()],
        "invalid run numbers",
    ));
    cases.push((
        100.0,
        vec![CalibrationRun {
            raw_medians_ns: Vec::new(),
            ..base.runs[0].clone()
        }],
        "has no raw medians",
    ));
    cases.push((
        100.0,
        vec![CalibrationRun {
            raw_medians_ns: vec![0.0],
            ..base.runs[0].clone()
        }],
        "must be positive and finite",
    ));
    cases.push((
        100.0,
        vec![
            base.runs[0].clone(),
            CalibrationRun {
                run: 2,
                checksum: 99,
                ..base.runs[0].clone()
            },
        ],
        "checksums differ across runs",
    ));
    cases.push((
        100.0,
        vec![CalibrationRun {
            gmac_per_second: 101.0,
            ..base.runs[0].clone()
        }],
        "adopted ceiling is below a raw measurement",
    ));
    cases.push((
        101.0,
        base.runs.clone(),
        "adopted ceiling does not match a raw measurement",
    ));
    for (adopted, runs, expected) in cases {
        let error = CalibrationTier::new(ComputeTier::NeonFma, adopted, runs)
            .expect_err("incoherent raw provenance must not produce a ceiling");
        assert!(
            error.to_string().contains(expected),
            "unexpected calibration rejection: {error}"
        );
    }
}

#[test]
fn frontier_calibration_rejects_incomplete_artifacts_and_blank_override_reason() {
    let valid = calibration_artifact(100.0, "2026-08-20");
    let missing = CalibrationArtifact::new(
        valid.machine.clone(),
        valid.measured_date.clone(),
        valid.command.clone(),
        valid.tiers[..2].to_vec(),
    )
    .expect_err("every compute tier is required");
    assert!(missing.to_string().contains("missing tier"));

    let duplicate = CalibrationArtifact::new(
        valid.machine.clone(),
        valid.measured_date.clone(),
        valid.command.clone(),
        vec![
            valid.tiers[0].clone(),
            valid.tiers[0].clone(),
            valid.tiers[1].clone(),
            valid.tiers[2].clone(),
        ],
    )
    .expect_err("duplicate tier provenance is ambiguous");
    assert!(duplicate.to_string().contains("duplicate tier"));

    for (date, command, model_name, expected) in [
        ("", "command", "MacBook Pro", "measured_date"),
        ("2026-08-20", "", "MacBook Pro", "command"),
        ("2026-08-20", "command", "", "machine.model_name"),
    ] {
        let mut machine = valid.machine.clone();
        machine.model_name = model_name.to_owned();
        let error = CalibrationArtifact::new(machine, date, command, valid.tiers.clone())
            .expect_err("blank provenance fields must be rejected");
        assert!(error.to_string().contains(expected));
    }

    let path = unique_temp_path("blank-calibration-override");
    let error = persist_calibration(
        &path,
        &valid,
        CalibrationWritePolicy::AllowLower {
            reason: " ".to_owned(),
        },
    )
    .expect_err("an override without an audit reason is invalid");
    assert!(
        error
            .to_string()
            .contains("override reason must not be blank")
    );
    assert!(!path.exists());
}

#[test]
fn frontier_calibration_loader_rejects_schema_and_tier_corruption() {
    let base: serde_json::Value = serde_json::from_slice(
        &fs::read(default_calibration_path()).expect("the seeded calibration is readable"),
    )
    .expect("the seeded calibration JSON parses");
    let corruptions = [
        (
            {
                let mut value = base.clone();
                value["schema"] = json!("future-calibration-schema");
                value
            },
            "unsupported schema",
        ),
        (
            {
                let mut value = base.clone();
                value["tiers"][0]["tier"] = json!("neon-impossible");
                value
            },
            "unknown compute tier neon-impossible",
        ),
        (
            {
                let mut value = base.clone();
                value["tiers"][0]["runs"][0]["raw_medians_ns"] = json!(["fast"]);
                value
            },
            "raw median must be a number",
        ),
    ];
    for (index, (value, expected)) in corruptions.into_iter().enumerate() {
        let path = unique_temp_path(&format!("corrupt-calibration-{index}"));
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("the corrupt calibration serializes"),
        )
        .expect("the corrupt calibration is writable");
        let error = load_calibration(&path).expect_err("corrupt calibration must not load");
        assert!(
            error.to_string().contains(expected),
            "unexpected calibration parse reason: {error}"
        );
        let _ = fs::remove_file(path);
    }
}

#[test]
fn frontier_roofline_loads_persisted_compute_calibration() {
    let path = unique_temp_path("roofline-calibration");
    persist_calibration(
        &path,
        &calibration_artifact(100.0, "2026-08-20"),
        CalibrationWritePolicy::UpwardOnly,
    )
    .expect("the roofline fixture persists");
    let model = RooflineModel::from_persisted_calibration(&path)
        .expect("roofline loads compute ceilings from the artifact");
    let score = model
        .score(RooflineInput {
            bytes_touched: 1,
            operation_count: 100_000_000_000,
            elapsed_seconds: 2.0,
            cores: 1,
            compute_tier: ComputeTier::NeonFma,
            binding: BindingBound::Compute,
        })
        .expect("the persisted FMA ceiling scores a compute-bound workload");
    assert_eq!(score.compute_bound_seconds, 1.0);
    assert_eq!(score.achieved_percent, 50.0);
    assert_eq!(
        score.compute_provenance.command,
        "frontier calibration test"
    );
    assert!(score.compute_provenance.method.contains("Mac15,9"));
    let _ = fs::remove_file(path);
}

#[test]
fn frontier_default_roofline_loads_tracked_compute_provenance() {
    let model = RooflineModel::from_default_calibration()
        .expect("the tracked machine calibration must remain loadable");
    let score = model
        .score(RooflineInput {
            bytes_touched: 1,
            operation_count: 33_294_029_000,
            elapsed_seconds: 1.0,
            cores: 1,
            compute_tier: ComputeTier::NeonFma,
            binding: BindingBound::Compute,
        })
        .expect("the tracked FMA denominator scores without recalibration");
    assert_eq!(score.achieved_percent, 100.0);
    assert_eq!(score.compute_provenance.measured_date, "2026-08-20");

    let missing = RooflineModel::from_persisted_calibration(unique_temp_path("missing-roofline"))
        .expect_err("a missing artifact must not produce an empty scoring model");
    assert!(matches!(&missing, RooflineLoadError::Calibration(_)));
    assert!(missing.to_string().contains("persisted calibration failed"));
}

#[test]
fn frontier_default_calibration_contains_two_supplied_operator_runs() {
    let artifact = load_calibration(default_calibration_path())
        .expect("the tracked M3 Max calibration artifact loads");
    let expected = [
        (ComputeTier::NeonFma, 33.294_029, 1_293_457_031),
        (ComputeTier::NeonSdot, 170.672_356, 640_000_112),
        (ComputeTier::NeonFp16ConvertFma, 21.569_397, 1_285_068_430),
    ];
    for (tier, adopted, checksum) in expected {
        let calibration = artifact.tier(tier).expect("the supplied tier is present");
        assert_eq!(calibration.adopted_gmac_per_second, adopted);
        assert_eq!(calibration.runs.len(), 2);
        assert!(calibration.runs.iter().all(|run| run.checksum == checksum));
    }
    assert_eq!(artifact.machine.power_state, "AC Power");
    assert_eq!(
        artifact.machine.thermal_state,
        "No thermal warning level has been recorded"
    );
}

struct ScriptedSamples {
    samples: VecDeque<f64>,
    warmups: usize,
}

impl SampleSource for ScriptedSamples {
    type Error = io::Error;

    fn warm_up(&mut self) -> Result<(), Self::Error> {
        self.warmups += 1;
        Ok(())
    }

    fn sample_ns(&mut self) -> Result<f64, Self::Error> {
        self.samples
            .pop_front()
            .ok_or_else(|| io::Error::other("scripted sample stream exhausted"))
    }
}

#[test]
fn frontier_variance_cap_discards_noisy_run_and_retries_without_averaging_it() {
    let noisy = (0..30).map(|index| if index % 2 == 0 { 80.0 } else { 120.0 });
    let stable = std::iter::repeat_n(100.0, 30);
    let mut source = ScriptedSamples {
        samples: noisy.chain(stable).collect(),
        warmups: 0,
    };
    let result = measure_source(
        &mut source,
        MeasurementConfig {
            warmup_repetitions: 1,
            repetitions_per_run: 30,
            accepted_runs: 1,
            maximum_attempts: 2,
            maximum_rsd_percent: 2.0,
        },
    )
    .expect("the stable retry must be accepted");
    assert_eq!(source.warmups, 1);
    assert_eq!(result.discarded_runs, 1);
    assert_eq!(result.accepted_run_medians_ns, vec![100.0]);
    assert_eq!(result.min_of_medians_ns, 100.0);
}

#[test]
fn frontier_measurement_rejects_weakened_policies_invalid_samples_and_source_failures() {
    let invalid_configs = [
        (
            MeasurementConfig {
                repetitions_per_run: 29,
                ..MeasurementConfig::strict()
            },
            "at least 30 repetitions",
        ),
        (
            MeasurementConfig {
                accepted_runs: 0,
                ..MeasurementConfig::strict()
            },
            "accepted runs and enough attempts",
        ),
        (
            MeasurementConfig {
                accepted_runs: 3,
                maximum_attempts: 2,
                ..MeasurementConfig::strict()
            },
            "accepted runs and enough attempts",
        ),
        (
            MeasurementConfig {
                maximum_rsd_percent: 0.0,
                ..MeasurementConfig::strict()
            },
            "positive and no greater than 2%",
        ),
        (
            MeasurementConfig {
                maximum_rsd_percent: 2.01,
                ..MeasurementConfig::strict()
            },
            "positive and no greater than 2%",
        ),
        (
            MeasurementConfig {
                maximum_rsd_percent: f64::NAN,
                ..MeasurementConfig::strict()
            },
            "positive and no greater than 2%",
        ),
    ];
    for (config, expected) in invalid_configs {
        let mut source = ScriptedSamples {
            samples: VecDeque::new(),
            warmups: 0,
        };
        let error = measure_source(&mut source, config)
            .expect_err("a weakened statistical policy must be rejected before sampling");
        let MeasurementError::InvalidConfiguration(reason) = error else {
            panic!("invalid policy must return InvalidConfiguration");
        };
        assert!(reason.contains(expected));
        assert_eq!(source.warmups, 0);
    }

    for sample in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        let mut source = ScriptedSamples {
            samples: std::iter::once(sample)
                .chain(std::iter::repeat_n(1.0, 29))
                .collect(),
            warmups: 0,
        };
        let error = measure_source(
            &mut source,
            MeasurementConfig {
                warmup_repetitions: 0,
                accepted_runs: 1,
                maximum_attempts: 1,
                ..MeasurementConfig::strict()
            },
        )
        .expect_err("non-positive and non-finite timings must never enter a median");
        assert!(
            matches!(error, MeasurementError::InvalidSample(value) if value.to_bits() == sample.to_bits())
        );
    }

    let mut exhausted = ScriptedSamples {
        samples: VecDeque::new(),
        warmups: 0,
    };
    let source_error = measure_source(
        &mut exhausted,
        MeasurementConfig {
            warmup_repetitions: 0,
            accepted_runs: 1,
            maximum_attempts: 1,
            ..MeasurementConfig::strict()
        },
    )
    .expect_err("source failures must retain their typed cause");
    assert!(matches!(source_error, MeasurementError::Source(_)));
    assert!(
        source_error
            .to_string()
            .contains("scripted sample stream exhausted")
    );
}

#[test]
fn frontier_variance_budget_exhaustion_never_promotes_noisy_runs() {
    let noisy = (0..60).map(|index| if index % 2 == 0 { 80.0 } else { 120.0 });
    let mut source = ScriptedSamples {
        samples: noisy.collect(),
        warmups: 0,
    };
    let error = measure_source(
        &mut source,
        MeasurementConfig {
            warmup_repetitions: 0,
            repetitions_per_run: 30,
            accepted_runs: 1,
            maximum_attempts: 2,
            maximum_rsd_percent: 2.0,
        },
    )
    .expect_err("two discarded attempts cannot be averaged into an accepted run");
    assert!(matches!(
        error,
        MeasurementError::VarianceBudgetExhausted {
            discarded_runs: 2,
            accepted_runs_required: 1,
        }
    ));
    assert!(error.to_string().contains("discarded 2 noisy runs"));
}

#[test]
fn frontier_measurement_and_ledger_row_preserve_attested_provenance() {
    let provenance = MachineStateProvenance::OperatorAttestation {
        timestamp: "2026-08-20T12:00:00Z".to_owned(),
        machine_identifier: "Mac15,9".to_owned(),
    };
    let mut source = ScriptedSamples {
        samples: std::iter::repeat_n(100.0, 30).collect(),
        warmups: 0,
    };
    let measurement = measure_source_with_provenance(
        &mut source,
        MeasurementConfig {
            warmup_repetitions: 0,
            accepted_runs: 1,
            maximum_attempts: 1,
            ..MeasurementConfig::strict()
        },
        provenance.clone(),
    )
    .expect("the attested measurement is statistically valid");
    assert_eq!(measurement.machine_state, provenance);

    let path = unique_temp_path("attested-ledger-row");
    let mut ledger = Ledger::open(&path).expect("the attested ledger opens");
    let mut row = frontier_row("attested measurement", 1.0);
    row.machine_state = measurement.machine_state;
    ledger.append(&row).expect("the attested row appends");
    let rows = ledger.rows().expect("the attested row reloads");
    assert_eq!(rows[0].machine_state, provenance);
    let raw = fs::read_to_string(&path).expect("the attested ledger bytes are readable");
    assert!(raw.contains("operator-attestation"));
    assert!(raw.contains("2026-08-20T12:00:00Z"));
    let _ = fs::remove_file(path);
}

#[test]
fn frontier_ledger_is_append_only_and_detects_prior_byte_rewrites() {
    let path = unique_temp_path("ledger");
    let mut ledger = Ledger::open(&path).expect("a new ledger opens");
    ledger
        .append(&frontier_row("first", 0.0))
        .expect("the first row appends");
    let prefix = fs::read(&path).expect("the first ledger prefix is readable");
    ledger
        .append(&frontier_row("second", 1.25))
        .expect("the second row appends");
    let appended = fs::read(&path).expect("the appended ledger is readable");
    assert!(appended.starts_with(&prefix));
    assert_eq!(ledger.rows().expect("rows parse").len(), 2);

    let mut external = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("the test can simulate an external rewrite");
    external
        .seek(SeekFrom::Start(0))
        .expect("the test seeks to the first byte");
    external
        .write_all(b"X")
        .expect("the test rewrites one prior byte");
    drop(external);
    let error = ledger
        .append(&frontier_row("third", 2.0))
        .expect_err("rewritten history must block append");
    assert!(matches!(error, LedgerError::PriorContentChanged));
    let _ = fs::remove_file(path);
}

#[test]
fn frontier_ledger_rejects_malformed_history_with_line_context() {
    let invalid_utf8 = unique_temp_path("ledger-invalid-utf8");
    fs::write(&invalid_utf8, [0xff, b'\n']).expect("the malformed fixture is writable");
    let error = Ledger::open(&invalid_utf8).expect_err("invalid UTF-8 cannot become ledger data");
    let LedgerError::CorruptHistory(reason) = error else {
        panic!("invalid UTF-8 must be classified as corrupt history");
    };
    assert!(reason.contains("invalid utf-8"));

    let invalid_json = unique_temp_path("ledger-invalid-json");
    fs::write(&invalid_json, "\n{not-json}\n").expect("the malformed JSON fixture is writable");
    let error = Ledger::open(&invalid_json).expect_err("invalid JSON cannot become ledger data");
    let LedgerError::CorruptHistory(reason) = error else {
        panic!("invalid JSON must be classified as corrupt history");
    };
    assert!(reason.contains("line 2"));
    assert!(reason.contains("key must be a string"));
    let _ = fs::remove_file(invalid_utf8);
    let _ = fs::remove_file(invalid_json);
}

#[test]
fn frontier_ledger_rejects_unknown_decisions_statuses_and_missing_attribution() {
    let base = json!({
        "date": "2026-08-20",
        "hypothesis": "schema fixture",
        "mechanism": "schema validation",
        "delta": 1.0,
        "roofline-%": 80.0,
        "keep/revert": "keep",
        "workload": "real-fixture",
        "provisional?": false,
        "evidence path": "fixture.md",
        "status": {
            "kind": "frontier-open",
            "reason": "more work remains",
            "hypotheses": ["next mechanism"]
        }
    });
    let corruptions = [
        (
            "unknown-decision",
            {
                let mut value = base.clone();
                value["keep/revert"] = json!("ship-anyway");
                value
            },
            "unknown decision ship-anyway",
        ),
        (
            "unknown-status",
            {
                let mut value = base.clone();
                value["status"]["kind"] = json!("good-enough");
                value
            },
            "unknown status good-enough",
        ),
        (
            "missing-attribution",
            {
                let mut value = base.clone();
                value["status"] = json!({"kind": "complete"});
                value
            },
            "missing attribution",
        ),
        (
            "non-string-hypothesis",
            {
                let mut value = base.clone();
                value["status"]["hypotheses"] = json!([7]);
                value
            },
            "non-string item in hypotheses",
        ),
    ];
    for (label, value, expected) in corruptions {
        let path = unique_temp_path(label);
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string(&value).expect("the malformed schema fixture serializes")
            ),
        )
        .expect("the malformed schema fixture is writable");
        let error = Ledger::open(&path).expect_err("schema corruption must block ledger open");
        let LedgerError::CorruptHistory(reason) = error else {
            panic!("schema corruption must be classified as corrupt history");
        };
        assert!(reason.contains("line 1"));
        assert!(reason.contains(expected), "unexpected reason: {reason}");
        let _ = fs::remove_file(path);
    }
}

#[test]
fn frontier_ledger_rejects_incomplete_rows_before_writing_bytes() {
    let invalid_rows = [
        {
            let mut row = frontier_row("blank-date", 1.0);
            row.date = " ".to_owned();
            row
        },
        {
            let mut row = frontier_row("nonfinite", 1.0);
            row.delta_percent = f64::NAN;
            row
        },
        {
            let mut row = frontier_row("open-without-work", 1.0);
            row.status = LedgerStatus::FrontierOpen {
                reason: String::new(),
                hypotheses: Vec::new(),
            };
            row
        },
        {
            let mut row = frontier_row("complete-without-proof", 1.0);
            row.status = LedgerStatus::Complete {
                attribution: LedgerAttribution {
                    counters: Vec::new(),
                    evidence_path: String::new(),
                    conclusion: String::new(),
                },
            };
            row
        },
    ];
    for row in invalid_rows {
        let path = unique_temp_path("invalid-ledger-row");
        let mut ledger = Ledger::open(&path).expect("an empty ledger opens");
        let error = ledger
            .append(&row)
            .expect_err("invalid rows must be rejected before append");
        assert!(matches!(error, LedgerError::InvalidRow(_)));
        assert_eq!(fs::read(&path).expect("the ledger remains readable"), b"");
        let _ = fs::remove_file(path);
    }
}

#[test]
fn frontier_ledger_preserves_append_order_and_summarizes_authority() {
    let path = unique_temp_path("ledger-order-summary");
    let mut ledger = Ledger::open(&path).expect("the ordered ledger opens");
    assert_eq!(ledger.path(), path.as_path());
    let first = frontier_row("first-open", -1.0);
    let mut second = frontier_row("second-complete", 2.0);
    second.decision = LedgerDecision::Keep;
    second.provisional = false;
    let decision = CampaignDecision::evaluate(CampaignSignals {
        achieved_percent: 100.0,
        percentage_tripwire: 80.0,
        recent_improvements_percent: vec![2.0],
        pmu: irreducible_pmu("ledger-order.trace"),
        workload_name: "real-ledger-order".to_owned(),
        workload_is_provisional: false,
        follow_up_hypotheses: vec!["unused after attribution".to_owned()],
    });
    second.status = LedgerStatus::from_campaign_stop(&decision.stop);
    ledger.append(&first).expect("the first row appends");
    ledger.append(&second).expect("the second row appends");
    let rows = ledger.rows().expect("the ordered rows reload");
    assert_eq!(rows[0].hypothesis, "first-open");
    assert_eq!(rows[1].hypothesis, "second-complete");
    let summary = LedgerSummary::from_rows(&rows);
    assert_eq!(summary.rows, 2);
    assert_eq!(summary.keeps, 1);
    assert_eq!(summary.reverts, 1);
    assert_eq!(summary.complete, 1);
    assert_eq!(summary.frontier_open, 1);
    assert_eq!(summary.provisional, 1);

    fs::write(
        &path,
        [
            fs::read(&path).expect("ledger bytes are readable"),
            b"\n".to_vec(),
        ]
        .concat(),
    )
    .expect("the test can simulate external growth");
    assert!(matches!(
        ledger.rows(),
        Err(LedgerError::PriorContentChanged)
    ));
    let _ = fs::remove_file(path);
}

#[derive(Clone)]
struct MockProbe {
    power: ProbeOutput,
    thermal: ProbeOutput,
}

struct ErrorProbe;

struct MissingAttestationSource;

struct IdentifierFallbackProbe {
    calls: RefCell<Vec<(String, Vec<String>)>>,
}

struct IdentifierPrimaryProbe;

struct IdentifierEmptySysctlProbe;

struct IdentifierFailureProbe;

struct IdentifierErrorAttestationSource {
    bytes: Vec<u8>,
    now: SystemTime,
}

struct StaticAttestationSource {
    bytes: Vec<u8>,
    machine_identifier: String,
    now: SystemTime,
}

impl AttestationSource for MissingAttestationSource {
    fn read_attestation(&self) -> io::Result<Vec<u8>> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "operator attestation is absent",
        ))
    }

    fn current_machine_identifier(&self) -> io::Result<String> {
        Ok("Mac15,9".to_owned())
    }

    fn now(&self) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(1_776_364_400)
    }
}

impl AttestationSource for StaticAttestationSource {
    fn read_attestation(&self) -> io::Result<Vec<u8>> {
        Ok(self.bytes.clone())
    }

    fn current_machine_identifier(&self) -> io::Result<String> {
        Ok(self.machine_identifier.clone())
    }

    fn now(&self) -> SystemTime {
        self.now
    }
}

impl AttestationSource for IdentifierErrorAttestationSource {
    fn read_attestation(&self) -> io::Result<Vec<u8>> {
        Ok(self.bytes.clone())
    }

    fn current_machine_identifier(&self) -> io::Result<String> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "model identifier probe denied",
        ))
    }

    fn now(&self) -> SystemTime {
        self.now
    }
}

impl MachineProbe for ErrorProbe {
    fn pmset(&self, arguments: &[&str]) -> io::Result<ProbeOutput> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("denied {}", arguments.join(" ")),
        ))
    }
}

impl MachineProbe for MockProbe {
    fn pmset(&self, arguments: &[&str]) -> io::Result<ProbeOutput> {
        match arguments {
            ["-g", "ps"] => Ok(self.power.clone()),
            ["-g", "therm"] => Ok(self.thermal.clone()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected pmset arguments",
            )),
        }
    }
}

impl MachineIdentifierProbe for IdentifierFallbackProbe {
    fn command(&self, program: &str, arguments: &[&str]) -> io::Result<ProbeOutput> {
        self.calls.borrow_mut().push((
            program.to_owned(),
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
        ));
        match program {
            "sysctl" => Ok(ProbeOutput::failure("sysctl unavailable")),
            "system_profiler" => Ok(ProbeOutput::success(
                "Hardware:\n\n    Model Name: MacBook Pro\n    Model Identifier: Mac15,9\n",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unexpected identifier command",
            )),
        }
    }
}

impl MachineIdentifierProbe for IdentifierPrimaryProbe {
    fn command(&self, program: &str, _arguments: &[&str]) -> io::Result<ProbeOutput> {
        match program {
            "sysctl" => Ok(ProbeOutput::success("  Mac15,9\n")),
            "system_profiler" => panic!("a valid sysctl identity must not invoke the fallback"),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unexpected identifier command",
            )),
        }
    }
}

impl MachineIdentifierProbe for IdentifierEmptySysctlProbe {
    fn command(&self, program: &str, _arguments: &[&str]) -> io::Result<ProbeOutput> {
        match program {
            "sysctl" => Ok(ProbeOutput::success(" \n")),
            "system_profiler" => Ok(ProbeOutput::success(
                "Hardware:\n    Model Identifier: Mac15,9\n",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unexpected identifier command",
            )),
        }
    }
}

impl MachineIdentifierProbe for IdentifierFailureProbe {
    fn command(&self, program: &str, _arguments: &[&str]) -> io::Result<ProbeOutput> {
        match program {
            "sysctl" => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "sysctl denied",
            )),
            "system_profiler" => Ok(ProbeOutput::failure("profiler omitted model identifier")),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unexpected identifier command",
            )),
        }
    }
}

#[test]
fn frontier_machine_identifier_falls_back_to_system_profiler_after_sysctl_failure() {
    let probe = IdentifierFallbackProbe {
        calls: RefCell::new(Vec::new()),
    };

    assert_eq!(
        machine_identifier_with_probe(&probe).expect("the profiler fallback supplies the model"),
        "Mac15,9"
    );
    assert_eq!(
        *probe.calls.borrow(),
        vec![
            (
                "sysctl".to_owned(),
                vec!["-n".to_owned(), "hw.model".to_owned()],
            ),
            (
                "system_profiler".to_owned(),
                vec!["SPHardwareDataType".to_owned()],
            ),
        ]
    );
}

#[test]
fn frontier_machine_identifier_prefers_sysctl_and_reports_both_command_failures() {
    assert_eq!(
        machine_identifier_with_probe(&IdentifierPrimaryProbe)
            .expect("the primary sysctl output supplies the model"),
        "Mac15,9"
    );

    let failure = machine_identifier_with_probe(&IdentifierFailureProbe)
        .expect_err("two failed identifier commands cannot establish machine identity");
    let message = failure.to_string();
    assert!(message.contains("sysctl -n hw.model could not start: sysctl denied"));
    assert!(message.contains(
        "system_profiler SPHardwareDataType returned no identifier: profiler omitted model identifier"
    ));
}

#[test]
fn frontier_machine_identifier_falls_back_when_sysctl_returns_a_blank_identifier() {
    assert_eq!(
        machine_identifier_with_probe(&IdentifierEmptySysctlProbe)
            .expect("blank sysctl output must fall back to system_profiler"),
        "Mac15,9"
    );
}

#[test]
fn frontier_preflight_fails_closed_on_mocked_battery_state() {
    let probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'Battery Power'"),
        thermal: ProbeOutput::success(
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
    };
    let outcome = preflight(&probe);
    let PreflightOutcome::Idle { reasons } = outcome else {
        panic!("battery power must idle the measurement loop");
    };
    assert!(reasons.iter().any(|reason| reason.contains("AC power")));
}

#[test]
fn frontier_preflight_favorable_direct_probe_is_marked_direct() {
    let power = "Now drawing from 'AC Power'";
    let thermal = "No thermal warning level has been recorded\nNo performance warning level has been recorded";
    let outcome = preflight_with_attestation(
        &MockProbe {
            power: ProbeOutput::success(power),
            thermal: ProbeOutput::success(thermal),
        },
        &MissingAttestationSource,
    );
    assert_eq!(
        outcome,
        CampaignPreflightOutcome::Ready {
            power_evidence: power.to_owned(),
            thermal_evidence: thermal.to_owned(),
            provenance: MachineStateProvenance::DirectProbe,
        }
    );
}

#[test]
fn frontier_preflight_probe_errors_without_attestation_stays_idle() {
    let outcome = preflight_with_attestation(&ErrorProbe, &MissingAttestationSource);
    let CampaignPreflightOutcome::Idle { reasons } = outcome else {
        panic!("an unavailable probe without an attestation must idle");
    };
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("operator attestation is absent"))
    );
}

#[test]
fn frontier_preflight_probe_errors_with_fresh_valid_attestation_is_ready_and_marked() {
    let source = StaticAttestationSource {
        bytes: attestation_bytes(
            "2026-08-20T12:00:00Z",
            "Now drawing from 'AC Power'\nAC attached; not charging",
            "No thermal warning level has been recorded\nNo performance warning level has been recorded\nNo CPU power status has been recorded",
        ),
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_260),
    };
    let outcome = preflight_with_attestation(&ErrorProbe, &source);
    let CampaignPreflightOutcome::Ready {
        power_evidence,
        thermal_evidence,
        provenance,
    } = outcome
    else {
        panic!("a fresh, machine-matched, safe attestation must substitute for probe errors");
    };
    assert!(power_evidence.contains("AC Power"));
    assert!(thermal_evidence.contains("No thermal warning"));
    assert_eq!(
        provenance,
        MachineStateProvenance::OperatorAttestation {
            timestamp: "2026-08-20T12:00:00Z".to_owned(),
            machine_identifier: "Mac15,9".to_owned(),
        }
    );
}

#[test]
fn frontier_preflight_status_zero_iokit_error_can_use_fresh_attestation() {
    let probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'AC Power'"),
        thermal: ProbeOutput::success(
            "Error:Failed to get thermal warning level with error code 0xe00002bc\nError: Failed to get performance warning level with error code 0xe00002bc\nError: No CPU power status with error code 0xe00002bc",
        ),
    };
    let source = StaticAttestationSource {
        bytes: attestation_bytes(
            "2026-08-20T12:00:00Z",
            "Now drawing from 'AC Power'",
            "No thermal warning level has been recorded\nNo performance warning level has been recorded\nNo CPU power status has been recorded",
        ),
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_260),
    };
    let CampaignPreflightOutcome::Ready { provenance, .. } =
        preflight_with_attestation(&probe, &source)
    else {
        panic!("literal IOKit probe-error output must be eligible for attestation");
    };
    assert!(matches!(
        provenance,
        MachineStateProvenance::OperatorAttestation { .. }
    ));
}

#[test]
fn frontier_preflight_probe_errors_with_expired_attestation_stays_idle() {
    let source = StaticAttestationSource {
        bytes: attestation_bytes(
            "2026-08-20T12:00:00Z",
            "Now drawing from 'AC Power'",
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_787_229_001),
    };
    let outcome = preflight_with_attestation(&ErrorProbe, &source);
    let CampaignPreflightOutcome::Idle { reasons } = outcome else {
        panic!("an attestation older than the TTL must idle");
    };
    assert!(reasons.iter().any(|reason| reason.contains("expired")));
}

#[test]
fn frontier_preflight_probe_errors_with_battery_attestation_stays_idle() {
    let source = StaticAttestationSource {
        bytes: attestation_bytes(
            "2026-08-20T12:00:00Z",
            "Now drawing from 'Battery Power'",
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_260),
    };
    let outcome = preflight_with_attestation(&ErrorProbe, &source);
    let CampaignPreflightOutcome::Idle { reasons } = outcome else {
        panic!("attested raw battery output must idle");
    };
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("raw pmset -g ps"))
    );
}

#[test]
fn frontier_preflight_successful_battery_probe_cannot_be_overridden_by_attestation() {
    let probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'Battery Power'"),
        thermal: ProbeOutput::success(
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
    };
    let source = StaticAttestationSource {
        bytes: attestation_bytes(
            "2026-08-20T12:00:00Z",
            "Now drawing from 'AC Power'",
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_260),
    };
    let outcome = preflight_with_attestation(&probe, &source);
    let CampaignPreflightOutcome::Idle { reasons } = outcome else {
        panic!("a successful unfavorable probe must outrank every attestation");
    };
    assert!(reasons.iter().any(|reason| reason.contains("AC power")));
}

#[test]
fn frontier_attestation_write_refuses_live_battery_state() {
    let path = unique_temp_path("operator-attestation-battery");
    let probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'Battery Power'"),
        thermal: ProbeOutput::success(
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
    };
    let error = write_operator_attestation(
        &probe,
        "Mac15,9",
        UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200),
        &path,
    )
    .expect_err("the live attest command must refuse battery power");
    assert!(error.to_string().contains("AC power"));
    assert!(!path.exists(), "refusal must not create an attestation");
}

#[test]
fn frontier_attestation_writer_refuses_probe_failures_without_creating_a_file() {
    let path = unique_temp_path("operator-attestation-probe-failure");

    let error = write_operator_attestation(
        &ErrorProbe,
        "Mac15,9",
        UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200),
        &path,
    )
    .expect_err("unavailable live probes must refuse to attest");
    let AttestationWriteError::UnsafeMachineState(reasons) = error else {
        panic!("probe failures must be reported as unsafe machine state");
    };
    assert!(reasons.iter().any(|reason| reason.contains("denied -g ps")));
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("denied -g therm"))
    );
    assert!(
        !path.exists(),
        "probe failure must not create an attestation"
    );
}

#[test]
fn frontier_attestation_writer_refuses_warning_throttle_and_ambiguous_thermal_state() {
    for (label, thermal) in [
        (
            "warning",
            "thermal warning level = 1\nperformance warning level = 0\nCPU power status = 100",
        ),
        (
            "throttle",
            "thermal warning level = 0\nperformance warning level = 0\ncpu_speed_limit = 80",
        ),
        ("ambiguous", "No thermal state is available"),
    ] {
        let path = unique_temp_path(&format!("operator-attestation-{label}"));
        let error = match write_operator_attestation(
            &MockProbe {
                power: ProbeOutput::success("Now drawing from 'AC Power'"),
                thermal: ProbeOutput::success(thermal),
            },
            "Mac15,9",
            UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200),
            &path,
        ) {
            Ok(()) => panic!("{label} thermal evidence must refuse to attest"),
            Err(error) => error,
        };
        let AttestationWriteError::UnsafeMachineState(reasons) = error else {
            panic!("{label} thermal evidence must be an unsafe-state refusal");
        };
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("nominal thermal state")),
            "{label} refusal must name the failed thermal requirement"
        );
        assert!(!path.exists(), "{label} refusal must not create a file");
    }
}

#[test]
fn frontier_attestation_writer_output_round_trips_through_error_fallback() {
    let path = unique_temp_path("operator-attestation-safe");
    let power = "Now drawing from 'AC Power'\nAC attached; not charging";
    let thermal = "No thermal warning level has been recorded\nNo performance warning level has been recorded\nNo CPU power status has been recorded";
    let probe = MockProbe {
        power: ProbeOutput::success(power),
        thermal: ProbeOutput::success(thermal),
    };
    write_operator_attestation(
        &probe,
        "Mac15,9",
        UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200),
        &path,
    )
    .expect("safe live output writes an attestation");
    let raw = fs::read(&path).expect("the written attestation is readable");
    let source = StaticAttestationSource {
        bytes: raw,
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_260),
    };
    let outcome = preflight_with_attestation(&ErrorProbe, &source);
    let CampaignPreflightOutcome::Ready {
        power_evidence,
        thermal_evidence,
        provenance,
    } = outcome
    else {
        panic!("writer output must be accepted by the error-only fallback");
    };
    assert_eq!(power_evidence, power);
    assert_eq!(thermal_evidence, thermal);
    assert_eq!(
        provenance,
        MachineStateProvenance::OperatorAttestation {
            timestamp: "2026-08-20T12:00:00Z".to_owned(),
            machine_identifier: "Mac15,9".to_owned(),
        }
    );
    let _ = fs::remove_file(path);
}

#[test]
fn frontier_attestation_missing_schema_and_each_required_field_are_rejected() {
    let safe = json!({
        "schema": 1,
        "timestamp": "2026-08-20T12:00:00Z",
        "machine_identifier": "Mac15,9",
        "pmset_ps_raw": "Now drawing from 'AC Power'",
        "pmset_therm_raw": "No thermal warning level has been recorded\nNo performance warning level has been recorded",
    });

    for (field, expected) in [
        ("schema", "schema must be 1"),
        ("timestamp", "missing nonblank string field timestamp"),
        (
            "machine_identifier",
            "missing nonblank string field machine_identifier",
        ),
        ("pmset_ps_raw", "missing nonblank string field pmset_ps_raw"),
        (
            "pmset_therm_raw",
            "missing nonblank string field pmset_therm_raw",
        ),
    ] {
        let mut value = safe.clone();
        value
            .as_object_mut()
            .expect("the fixture is an object")
            .remove(field);
        let source = StaticAttestationSource {
            bytes: serde_json::to_vec(&value).expect("the missing-field fixture serializes"),
            machine_identifier: "Mac15,9".to_owned(),
            now: UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_260),
        };

        let reason = attestation_rejection_reason(&source);
        assert!(
            reason.contains(expected),
            "missing {field} produced unexpected rejection: {reason}"
        );
    }
}

#[test]
fn frontier_attestation_rejects_an_unavailable_current_machine_identifier() {
    let source = IdentifierErrorAttestationSource {
        bytes: attestation_bytes(
            "2026-08-20T12:00:00Z",
            "Now drawing from 'AC Power'",
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_260),
    };
    let CampaignPreflightOutcome::Idle { reasons } =
        preflight_with_attestation(&ErrorProbe, &source)
    else {
        panic!("an unavailable current identity must reject the attestation");
    };
    assert!(reasons.iter().any(|reason| {
        reason.contains("current machine identifier unavailable: model identifier probe denied")
    }));
}

#[test]
fn frontier_attestation_ttl_accepts_the_exact_edge_and_rejects_one_second_past() {
    let bytes = attestation_bytes(
        "2026-04-30T12:00:00Z",
        "Now drawing from 'AC Power'",
        "No thermal warning level has been recorded\nNo performance warning level has been recorded",
    );
    let at_edge = StaticAttestationSource {
        bytes: bytes.clone(),
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_777_551_600),
    };
    let CampaignPreflightOutcome::Ready { provenance, .. } =
        preflight_with_attestation(&ErrorProbe, &at_edge)
    else {
        panic!("an attestation exactly at the 1,200-second TTL remains valid");
    };
    assert!(matches!(
        provenance,
        MachineStateProvenance::OperatorAttestation { .. }
    ));

    let just_past = StaticAttestationSource {
        bytes,
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH + std::time::Duration::from_secs(1_777_551_601),
    };
    let reason = attestation_rejection_reason(&just_past);
    assert!(
        reason.contains("age 1201 seconds exceeds 1200 second TTL"),
        "one second past the TTL produced unexpected rejection: {reason}"
    );
}

#[test]
fn frontier_attestation_writer_refuses_clocks_outside_the_iso8601_range() {
    let safe_probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'AC Power'"),
        thermal: ProbeOutput::success(
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
    };
    for (label, time, expected) in [
        (
            "before-epoch",
            UNIX_EPOCH - std::time::Duration::from_secs(1),
            "time predates the Unix epoch",
        ),
        (
            "five-digit-year",
            UNIX_EPOCH + std::time::Duration::from_secs(253_402_300_800),
            "time exceeds four-digit ISO-8601 years",
        ),
    ] {
        let path = unique_temp_path(&format!("operator-attestation-clock-{label}"));
        let error = write_operator_attestation(&safe_probe, "Mac15,9", time, &path)
            .expect_err("an unrepresentable clock must refuse to attest");
        let AttestationWriteError::InvalidClock(reason) = error else {
            panic!("{label} must remain a typed invalid-clock refusal");
        };
        assert_eq!(reason, expected);
        assert!(!path.exists(), "{label} refusal must not create a file");
    }
}

#[test]
fn frontier_attestation_rejects_schema_identity_clock_and_thermal_corruption() {
    let safe = json!({
        "schema": 1,
        "timestamp": "2026-08-20T12:00:00Z",
        "machine_identifier": "Mac15,9",
        "pmset_ps_raw": "Now drawing from 'AC Power'",
        "pmset_therm_raw": "No thermal warning level has been recorded\nNo performance warning level has been recorded",
    });
    let mut cases = Vec::new();
    cases.push((
        b"{not-json}".to_vec(),
        "Mac15,9",
        1_787_227_260,
        "key must be a string",
    ));
    for (label, replacement, expected) in [
        ("schema", json!(2), "schema must be 1"),
        (
            "pmset_therm_raw",
            json!("thermal warning level = 1\nperformance warning level = 1"),
            "raw pmset -g therm",
        ),
        (
            "timestamp",
            json!("2026-08-20T12:02:00Z"),
            "timestamp is in the future",
        ),
        (
            "timestamp",
            json!("2026/08/20 12:00:00Z"),
            "must use YYYY-MM-DDTHH:MM:SSZ",
        ),
        ("timestamp", json!("202X-08-20T12:00:00Z"), "decimal digits"),
        (
            "timestamp",
            json!("2026-13-20T12:00:00Z"),
            "outside the supported UTC range",
        ),
        (
            "timestamp",
            json!("2026-02-30T12:00:00Z"),
            "invalid UTC date or time",
        ),
    ] {
        let mut value = safe.clone();
        value[label] = replacement;
        cases.push((
            serde_json::to_vec(&value).expect("the corrupt attestation serializes"),
            "Mac15,9",
            1_787_227_260,
            expected,
        ));
    }
    cases.push((
        serde_json::to_vec(&safe).expect("the machine mismatch serializes"),
        "Mac99,1",
        1_787_227_260,
        "machine identifier mismatch",
    ));

    for (bytes, machine_identifier, now, expected) in cases {
        let source = StaticAttestationSource {
            bytes,
            machine_identifier: machine_identifier.to_owned(),
            now: UNIX_EPOCH + std::time::Duration::from_secs(now),
        };
        let reason = attestation_rejection_reason(&source);
        assert!(reason.contains(expected), "unexpected rejection: {reason}");
    }

    let before_epoch = StaticAttestationSource {
        bytes: serde_json::to_vec(&json!({
            "schema": 1,
            "timestamp": "1970-01-01T00:00:00Z",
            "machine_identifier": "Mac15,9",
            "pmset_ps_raw": "Now drawing from 'AC Power'",
            "pmset_therm_raw": "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        }))
        .expect("the pre-epoch clock fixture serializes"),
        machine_identifier: "Mac15,9".to_owned(),
        now: UNIX_EPOCH - std::time::Duration::from_secs(1),
    };
    assert!(attestation_rejection_reason(&before_epoch).contains("before the Unix epoch"));
}

#[test]
fn frontier_attestation_atomic_replace_is_private_and_leaves_no_temporary_file() {
    let directory = unique_temp_path("attestation-atomic-directory");
    let path = directory.join("operator-attestation.json");
    let first_probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'AC Power'\nfirst observation"),
        thermal: ProbeOutput::success(
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
    };
    write_operator_attestation(
        &first_probe,
        "Mac15,9",
        UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200),
        &path,
    )
    .expect("the initial safe observation is persisted");

    let second_probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'AC Power'\nsecond observation"),
        thermal: first_probe.thermal.clone(),
    };
    write_operator_attestation(
        &second_probe,
        "Mac15,9",
        UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_201),
        &path,
    )
    .expect("a newer safe observation atomically replaces the first");

    let raw = fs::read_to_string(&path).expect("the replacement is readable");
    assert!(raw.contains("second observation"));
    assert!(!raw.contains("first observation"));
    assert!(raw.contains("2026-08-20T12:00:01Z"));
    assert!(
        !path
            .with_extension(format!("tmp-{}", std::process::id()))
            .exists(),
        "successful replacement must leave no temporary file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&path)
                .expect("replacement metadata is available")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "attestations must be owner-readable and owner-writable only"
        );
    }
    fs::remove_dir_all(directory).expect("the atomic-write fixture is removable");
}

#[test]
fn frontier_attestation_persistence_reports_typed_open_and_rename_failures() {
    let safe_probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'AC Power'"),
        thermal: ProbeOutput::success(
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
    };
    let now = UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200);

    let open_target = unique_temp_path("attestation-open-failure");
    let open_temporary = open_target.with_extension(format!("tmp-{}", std::process::id()));
    fs::create_dir(&open_temporary).expect("the temporary-path directory fixture is created");
    let open_error = write_operator_attestation(&safe_probe, "Mac15,9", now, &open_target)
        .expect_err("a directory at the temporary path must prevent opening the file");
    let AttestationWriteError::Io { path, .. } = open_error else {
        panic!("temporary-file open failure must remain a typed I/O refusal");
    };
    assert_eq!(path, open_temporary);
    assert!(!open_target.exists());
    fs::remove_dir(open_temporary).expect("the open-failure fixture is removable");

    let rename_target = unique_temp_path("attestation-rename-failure");
    fs::create_dir(&rename_target).expect("the destination-directory fixture is created");
    let rename_temporary = rename_target.with_extension(format!("tmp-{}", std::process::id()));
    let rename_error = write_operator_attestation(&safe_probe, "Mac15,9", now, &rename_target)
        .expect_err("a directory destination must prevent atomic file replacement");
    let AttestationWriteError::Io { path, .. } = rename_error else {
        panic!("rename failure must remain a typed I/O refusal");
    };
    assert_eq!(path, rename_target);
    assert!(
        rename_temporary.is_file(),
        "the fully written temporary remains inspectable"
    );
    fs::remove_file(rename_temporary).expect("the rename-failure temporary is removable");
    fs::remove_dir(rename_target).expect("the rename-failure destination is removable");
}

#[test]
fn frontier_attestation_writer_and_file_source_report_typed_failures() {
    let safe_probe = MockProbe {
        power: ProbeOutput::success("Now drawing from 'AC Power'"),
        thermal: ProbeOutput::success(
            "No thermal warning level has been recorded\nNo performance warning level has been recorded",
        ),
    };
    let blank_path = unique_temp_path("blank-machine-attestation");
    let blank = write_operator_attestation(
        &safe_probe,
        " ",
        UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200),
        &blank_path,
    )
    .expect_err("a blank machine identifier must be refused");
    assert!(matches!(
        blank,
        AttestationWriteError::InvalidMachineIdentifier
    ));
    assert_eq!(blank.to_string(), "live machine identifier is blank");

    let clock_path = unique_temp_path("invalid-clock-attestation");
    let clock = write_operator_attestation(
        &safe_probe,
        "Mac15,9",
        UNIX_EPOCH - std::time::Duration::from_secs(1),
        &clock_path,
    )
    .expect_err("a pre-epoch clock must be refused");
    assert!(matches!(clock, AttestationWriteError::InvalidClock(_)));
    assert!(clock.to_string().contains("attestation clock is invalid"));

    let parent_file = unique_temp_path("attestation-parent-file");
    fs::write(&parent_file, b"not a directory").expect("the parent-file fixture is writable");
    let io_path = parent_file.join("attestation.json");
    let persistence = write_operator_attestation(
        &safe_probe,
        "Mac15,9",
        UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200),
        &io_path,
    )
    .expect_err("a file cannot become the attestation directory");
    assert!(matches!(persistence, AttestationWriteError::Io { .. }));
    assert!(persistence.to_string().contains("attestation"));
    let _ = fs::remove_file(parent_file);

    let failed_probe_path = unique_temp_path("failed-probe-attestation");
    let probe_error = write_operator_attestation(
        &ErrorProbe,
        "Mac15,9",
        UNIX_EPOCH + std::time::Duration::from_secs(1_787_227_200),
        &failed_probe_path,
    )
    .expect_err("probe I/O errors must not write an attestation");
    assert!(probe_error.to_string().contains("denied -g ps"));

    let root = unique_temp_path("attestation-root");
    assert_eq!(
        default_attestation_path(&root),
        root.join("target/frontier/operator-attestation.json")
    );
    let file_path = unique_temp_path("file-attestation-source");
    fs::write(&file_path, b"attestation bytes").expect("the source fixture is writable");
    let file_source = FileAttestationSource::new(&file_path);
    assert_eq!(
        file_source
            .read_attestation()
            .expect("the file source reads bytes"),
        b"attestation bytes"
    );
    assert!(file_source.now() >= UNIX_EPOCH);
    #[cfg(target_os = "macos")]
    assert!(
        !file_source
            .current_machine_identifier()
            .expect("the file source resolves the live macOS model identifier")
            .is_empty()
    );
    #[cfg(not(target_os = "macos"))]
    let _platform_identifier_result = file_source.current_machine_identifier();
    let _ = fs::remove_file(file_path);
}

#[test]
fn frontier_preflight_requires_explicit_ac_and_unthrottled_thermal_evidence() {
    let ready = preflight(&MockProbe {
        power: ProbeOutput::success("AC attached; charging"),
        thermal: ProbeOutput::success(
            "thermal_warning_level = 0\nperformance_warning_level = 0\ncpu_speed_limit = 100",
        ),
    });
    let PreflightOutcome::Ready {
        power_evidence,
        thermal_evidence,
    } = ready
    else {
        panic!("explicit AC, zero warnings, and full CPU speed must be ready");
    };
    assert!(power_evidence.contains("AC attached"));
    assert!(thermal_evidence.contains("cpu_speed_limit = 100"));

    let ambiguous = preflight(&MockProbe {
        power: ProbeOutput::failure("power access denied"),
        thermal: ProbeOutput::success(
            "No thermal warning level has been recorded\nNo performance warning level has been recorded\ncpu_speed_limit = 80",
        ),
    });
    let PreflightOutcome::Idle { reasons } = ambiguous else {
        panic!("failed power output and throttled speed must idle");
    };
    assert_eq!(reasons.len(), 2);
    assert!(reasons[0].contains("power access denied"));
    assert!(reasons[1].contains("nominal thermal state not established"));

    let failed = preflight(&ErrorProbe);
    let PreflightOutcome::Idle { reasons } = failed else {
        panic!("probe I/O failures must fail closed");
    };
    assert_eq!(reasons.len(), 2);
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("AC power probe failed"))
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("thermal probe failed"))
    );
}

#[test]
fn frontier_workload_construction_rejects_invalid_and_overflowing_shapes() {
    assert!(matches!(
        SyntheticI8Workload::new("", 1, 1, 7),
        Err(WorkloadError::InvalidShape)
    ));
    assert!(matches!(
        SyntheticI8Workload::new("rows", 0, 1, 7),
        Err(WorkloadError::InvalidShape)
    ));
    assert!(matches!(
        SyntheticI8Workload::new("overflow", usize::MAX, 2, 7),
        Err(WorkloadError::SizeOverflow)
    ));
    assert!(matches!(
        StridedI8Workload::new("stride", 1, 1, 1, 7),
        Err(WorkloadError::InvalidShape)
    ));
    assert!(matches!(
        StridedI8Workload::new("overflow", usize::MAX, 1, 2, 7),
        Err(WorkloadError::SizeOverflow)
    ));
    assert_eq!(
        WorkloadError::InvalidShape.to_string(),
        "workload shape and name must be nonzero"
    );
    assert_eq!(
        WorkloadError::SizeOverflow.to_string(),
        "workload fixture size overflowed"
    );
}

struct FailingWorkload {
    descriptor: WorkloadDescriptor,
}

impl Workload for FailingWorkload {
    type Error = io::Error;

    fn descriptor(&self) -> &WorkloadDescriptor {
        &self.descriptor
    }

    fn execute(
        &mut self,
        _variant: &zeppelin_embed_bench::frontier::variants::RegisteredVariant,
    ) -> Result<WorkloadObservation, Self::Error> {
        Err(io::Error::other("candidate access failed"))
    }
}

#[test]
fn frontier_workload_sampler_propagates_execution_failure_before_timing() {
    let registry = VariantRegistry::from_kernel_knob_space().expect("the registry is valid");
    let variant = registry
        .materialized()
        .first()
        .expect("at least the scalar variant is materialized");
    let mut workload = FailingWorkload {
        descriptor: WorkloadDescriptor {
            name: "failing-workload".to_owned(),
            provisional: true,
            binding: BindingBound::Memory,
            compute_tier: ComputeTier::NeonSdot,
            bytes_touched: 1,
            operation_count: 1,
        },
    };
    let mut sampler = WorkloadSampler::new(&mut workload, variant);
    let error = measure_source(&mut sampler, MeasurementConfig::strict())
        .expect_err("a workload failure during warmup must abort before samples exist");
    assert!(matches!(error, MeasurementError::Source(_)));
    assert!(error.to_string().contains("candidate access failed"));
}

#[test]
fn frontier_workload_abstraction_executes_contiguous_and_strided_synthetic_patterns() {
    let registry = VariantRegistry::from_kernel_knob_space()
        .expect("task-03 declarations form a valid registry");
    let scalar = registry
        .materialized()
        .iter()
        .find(|variant| variant.point().tier == InstructionTier::Scalar)
        .expect("the scalar oracle is always materialized");
    let mut contiguous = SyntheticI8Workload::new("synthetic-contiguous", 8, 16, 7)
        .expect("the contiguous workload is valid");
    let mut strided = StridedI8Workload::new("synthetic-strided", 8, 16, 3, 7)
        .expect("the strided workload is valid");
    let contiguous_observation = contiguous
        .execute(scalar)
        .expect("the contiguous workload executes");
    let strided_observation = strided
        .execute(scalar)
        .expect("the strided workload executes");
    let contiguous_timed = contiguous
        .execute_timed(scalar)
        .expect("the contiguous timed path executes without an oracle in-band");
    let strided_timed = strided
        .execute_timed(scalar)
        .expect("the strided timed path executes without an oracle in-band");
    assert_eq!(contiguous.descriptor().name, "synthetic-contiguous");
    assert_eq!(strided.descriptor().name, "synthetic-strided");
    assert!(contiguous.descriptor().provisional);
    assert!(strided.descriptor().provisional);
    assert_ne!(
        contiguous_observation.access_fingerprint,
        strided_observation.access_fingerprint
    );
    assert!(contiguous_observation.correct);
    assert!(strided_observation.correct);
    assert_eq!(contiguous_timed.checksum, contiguous_observation.checksum);
    assert_eq!(strided_timed.checksum, strided_observation.checksum);
    assert_eq!(
        contiguous_timed.access_fingerprint,
        contiguous_observation.access_fingerprint
    );
    assert_eq!(
        strided_timed.access_fingerprint,
        strided_observation.access_fingerprint
    );
}

#[test]
fn frontier_variant_registry_consumes_task03_knob_space_and_materializes_real_builds() {
    let registry = VariantRegistry::from_kernel_knob_space()
        .expect("task-03 declarations form a valid registry");
    assert_eq!(registry.declared_points().len(), 3 * 4 * 5 * 5 * 6);
    assert!(!registry.materialized().is_empty());
    assert!(
        registry
            .materialized()
            .iter()
            .all(|variant| variant.is_monomorphized())
    );
}

#[test]
fn frontier_variant_identity_and_callable_builds_preserve_tier_and_shape_provenance() {
    let expected = [
        (InstructionTier::Scalar, "scalar"),
        (InstructionTier::NeonWiden, "neon-widen"),
        (InstructionTier::NeonDotprod, "neon-dotprod"),
        (InstructionTier::Avx2, "avx2"),
        (InstructionTier::NeonI8mmReserved, "i8mm-reserved"),
        (InstructionTier::Sme2Reserved, "sme2-reserved"),
    ];
    for (tier, suffix) in expected {
        let point = KernelPoint {
            unroll: 4,
            accumulators: 8,
            rows_per_block: 2,
            prefetch_dist: 16,
            tier,
        };
        assert_eq!(point.stable_id(), format!("u4-a8-r2-p16-{suffix}"));
    }

    let registry = VariantRegistry::from_kernel_knob_space().expect("the registry is valid");
    for variant in registry.materialized() {
        assert_eq!(variant.dot_i8(&[1, 2], &[3, 4]), 11);
        assert!(variant.build_name().contains("Shape<4, 4, 1, 0>"));
        let debug = format!("{variant:?}");
        assert!(debug.contains("RegisteredVariant"));
        assert!(debug.contains(variant.build_name()));
        assert!(registry.declared_points().contains(&variant.point()));
    }
}

#[test]
fn frontier_pmu_counter_export_parses_ipc_stalls_and_bandwidth() {
    let report = parse_counter_export(
        "Instructions Per Cycle,2.75,ratio\nBackend Stall Cycles,18.5,percent\nMemory Bandwidth,63.2,GB/s\n",
        "counter.trace",
    )
    .expect("the three required counter families parse");
    assert!(
        report
            .counters()
            .iter()
            .any(|counter| counter.name == "ipc")
    );
    assert!(
        report
            .counters()
            .iter()
            .any(|counter| counter.name == "stall")
    );
    assert!(
        report
            .counters()
            .iter()
            .any(|counter| counter.name == "bandwidth")
    );
}

#[test]
fn frontier_pmu_rejects_incomplete_nonfinite_and_unattributed_evidence() {
    for reading in [
        CounterReading::new("", 1.0, "ratio", "observed"),
        CounterReading::new("ipc", f64::NAN, "ratio", "observed"),
        CounterReading::new("ipc", 1.0, "", "observed"),
        CounterReading::new("ipc", 1.0, "ratio", ""),
    ] {
        assert!(matches!(reading, Err(PmuError::InvalidCounter)));
    }
    let counter = CounterReading::new("ipc", 2.0, "ratio", "issue rate")
        .expect("the report fixture counter is valid");
    for report in [
        PmuReport::new(
            Vec::new(),
            "trace",
            AttributionClass::Unattributed {
                reason: "needs analysis".to_owned(),
            },
        ),
        PmuReport::new(
            vec![counter.clone()],
            "",
            AttributionClass::Unattributed {
                reason: "needs analysis".to_owned(),
            },
        ),
        PmuReport::new(
            vec![counter.clone()],
            "trace",
            AttributionClass::Reducible {
                reason: String::new(),
            },
        ),
        PmuReport::new(
            vec![counter.clone()],
            "trace",
            AttributionClass::Irreducible {
                cause: String::new(),
            },
        ),
    ] {
        assert!(matches!(report, Err(PmuError::MissingAttributionEvidence)));
    }
    assert_eq!(
        PmuError::InvalidCounter.to_string(),
        "PMU counter reading is incomplete"
    );
    assert!(
        PmuError::MissingAttributionEvidence
            .to_string()
            .contains("counters, evidence path, and conclusion")
    );
}

#[test]
fn frontier_pmu_parser_requires_every_counter_family_and_rejects_nan() {
    let missing = parse_counter_export(
        "Backend Stall Cycles,18.5,percent\nMemory Bandwidth,63.2,GB/s\n",
        "missing-ipc.trace",
    )
    .expect_err("completion attribution cannot omit IPC");
    assert_eq!(missing, PmuError::MissingCounterFamily("ipc"));
    assert!(missing.to_string().contains("required ipc counter"));

    let invalid = parse_counter_export(
        "IPC,NaN,ratio\nStall,1,percent\nBandwidth,2,GB/s\n",
        "nan.trace",
    )
    .expect_err("a non-finite counter must not enter attribution");
    assert_eq!(invalid, PmuError::InvalidCounter);

    let report = parse_counter_export(
        "ignored,row\nIPC,2.5\nFrontend Stall,4.0\nDRAM Bandwidth,70.0\n",
        "aliases.trace",
    )
    .expect("canonical aliases and omitted units remain explicit values");
    assert_eq!(report.evidence_path(), "aliases.trace");
    assert!(matches!(
        report.class(),
        AttributionClass::Unattributed { reason } if reason.contains("agent must relate")
    ));
    assert_eq!(report.counters().len(), 3);
    assert!(
        report
            .counters()
            .iter()
            .all(|counter| counter.unit == "value")
    );
}

#[test]
fn frontier_pmu_capture_degrades_explicitly_when_recording_is_unavailable() {
    let missing_parent = unique_temp_path("missing-xctrace-parent").join("counter.trace");
    let outcome = capture_cpu_counters("/definitely/not/a/frontier-program", &[], &missing_parent);
    let PmuOutcome::Unavailable { reason } = outcome else {
        panic!("an impossible record target must never produce measured counters");
    };
    assert!(reason.contains("xctrace"));
    assert!(
        reason.contains("could not start") || reason.contains("record failed"),
        "unexpected unavailable reason: {reason}"
    );

    let io_error = PmuError::from(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
    let PmuError::Io { path, reason } = io_error else {
        panic!("I/O conversion must retain a typed PMU error");
    };
    assert!(path.as_os_str().is_empty());
    assert_eq!(reason, "denied");
}

#[test]
fn frontier_planted_regression_is_ranked_below_baseline_and_reverted() {
    let ranking = rank_candidates(vec![
        CandidateEvaluation::correct("baseline", 100.0),
        CandidateEvaluation::correct("planted-regression", 135.0),
    ])
    .expect("the planted registry ranks");
    assert_eq!(ranking[0].variant, "baseline");
    assert_eq!(ranking[0].disposition, CandidateDisposition::Keep);
    let regression = ranking
        .iter()
        .find(|candidate| candidate.variant == "planted-regression")
        .expect("the planted regression remains in evidence");
    assert_eq!(
        regression.disposition,
        CandidateDisposition::RevertRegression
    );
}

#[test]
fn frontier_planted_broken_fast_is_discarded_before_speed_ranking() {
    let ranking = rank_candidates(vec![
        CandidateEvaluation::correct("baseline", 100.0),
        CandidateEvaluation::incorrect("planted-broken-fast", 40.0),
    ])
    .expect("the planted registry ranks");
    assert_eq!(ranking[0].variant, "baseline");
    let broken = ranking
        .iter()
        .find(|candidate| candidate.variant == "planted-broken-fast")
        .expect("the broken variant remains in evidence");
    assert_eq!(broken.disposition, CandidateDisposition::DiscardIncorrect);
}

#[test]
fn frontier_tuner_rejects_empty_duplicate_invalid_and_all_incorrect_candidates() {
    assert!(matches!(
        SearchSpace::new(Vec::new()),
        Err(TuneError::EmptySearchSpace)
    ));
    let duplicate = KernelPoint {
        unroll: 4,
        accumulators: 4,
        rows_per_block: 1,
        prefetch_dist: 0,
        tier: InstructionTier::Scalar,
    };
    let error = SearchSpace::new(vec![duplicate, duplicate])
        .expect_err("duplicate grid points make resumption ambiguous");
    assert_eq!(error, TuneError::DuplicatePoint(duplicate.stable_id()));
    assert!(error.to_string().contains("duplicate tuner point"));

    assert!(matches!(
        rank_candidates(Vec::new()),
        Err(TuneError::EmptySearchSpace)
    ));
    for invalid in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        assert!(matches!(
            rank_candidates(vec![CandidateEvaluation::correct("invalid", invalid)]),
            Err(TuneError::InvalidTiming)
        ));
    }
    assert!(matches!(
        rank_candidates(vec![CandidateEvaluation::incorrect("wrong", 1.0)]),
        Err(TuneError::NoCorrectCandidate)
    ));
}

#[test]
fn frontier_tuner_single_point_and_exhausted_restarts_terminate_without_repeats() {
    let point = KernelPoint {
        unroll: 4,
        accumulators: 4,
        rows_per_block: 1,
        prefetch_dist: 0,
        tier: InstructionTier::Scalar,
    };
    let single = SearchSpace::new(vec![point]).expect("the single point is valid");
    assert_eq!(single.points(), &[point]);
    let mut evaluations = 0;
    let run = run_search(
        &single,
        SearchConfig {
            seed: 0,
            exhaustive_limit: 1,
            random_restarts: 8,
            maximum_evaluations: 10,
        },
        None,
        |candidate| {
            evaluations += 1;
            CandidateEvaluation::correct(candidate.stable_id(), 10.0)
        },
    )
    .expect("a single-point exhaustive search terminates");
    assert_eq!(evaluations, 1);
    assert_eq!(run.trajectory, vec![point.stable_id()]);
    assert_eq!(run.ranking[0].disposition, CandidateDisposition::Keep);

    let disconnected = SearchSpace::new(vec![
        point,
        KernelPoint {
            unroll: 8,
            accumulators: 8,
            rows_per_block: 2,
            prefetch_dist: 4,
            tier: InstructionTier::NeonDotprod,
        },
        KernelPoint {
            unroll: 16,
            accumulators: 16,
            rows_per_block: 4,
            prefetch_dist: 8,
            tier: InstructionTier::Sme2Reserved,
        },
    ])
    .expect("the disconnected large space is valid");
    let run = run_search(
        &disconnected,
        SearchConfig {
            seed: 7,
            exhaustive_limit: 1,
            random_restarts: 0,
            maximum_evaluations: 10,
        },
        None,
        |candidate| CandidateEvaluation::correct(candidate.stable_id(), 10.0),
    )
    .expect("an exhausted zero-restart search terminates");
    assert_eq!(run.trajectory.len(), 1);
    assert_eq!(run.state.starts, 1);
    assert_eq!(run.state.expanded.len(), 1);
}

#[test]
fn frontier_tuner_rejects_mismatched_and_out_of_range_resume_state() {
    let space = search_space(3);
    let config = SearchConfig {
        seed: 41,
        exhaustive_limit: 8,
        random_restarts: 0,
        maximum_evaluations: 1,
    };
    let partial = run_search(&space, config, None, deterministic_cost)
        .expect("the resume fixture search starts");

    let mut wrong_seed = partial.state.clone();
    wrong_seed.seed = 42;
    assert!(matches!(
        run_search(&space, config, Some(wrong_seed), deterministic_cost),
        Err(TuneError::ResumeMismatch)
    ));

    let mode_changed = SearchConfig {
        exhaustive_limit: 1,
        maximum_evaluations: 2,
        ..config
    };
    assert!(matches!(
        run_search(
            &space,
            mode_changed,
            Some(partial.state.clone()),
            deterministic_cost
        ),
        Err(TuneError::ResumeMismatch)
    ));

    let other_space = search_space(4);
    assert!(matches!(
        run_search(
            &other_space,
            SearchConfig {
                maximum_evaluations: 2,
                ..config
            },
            Some(partial.state.clone()),
            deterministic_cost
        ),
        Err(TuneError::ResumeMismatch)
    ));

    let mut out_of_range = partial.state;
    out_of_range.evaluated.clear();
    out_of_range.pending = vec![99];
    assert!(matches!(
        run_search(
            &space,
            SearchConfig {
                maximum_evaluations: 2,
                ..config
            },
            Some(out_of_range),
            deterministic_cost
        ),
        Err(TuneError::ResumeMismatch)
    ));
    assert!(matches!(
        run_search(
            &space,
            SearchConfig {
                maximum_evaluations: 0,
                ..config
            },
            None,
            deterministic_cost
        ),
        Err(TuneError::InvalidSearchConfig)
    ));
}

#[test]
fn frontier_tuner_state_loader_rejects_io_json_schema_and_field_corruption() {
    let missing = unique_temp_path("missing-search-state");
    let error = load_search_state(&missing).expect_err("missing state must be a typed I/O error");
    assert!(matches!(&error, TuneError::Io { path, .. } if path == &missing));
    assert!(error.to_string().contains("search state"));

    let malformed = unique_temp_path("malformed-search-state");
    fs::write(&malformed, "{not-json}").expect("the malformed state is writable");
    assert!(matches!(
        load_search_state(&malformed),
        Err(TuneError::StateFormat(_))
    ));

    let space = search_space(2);
    let run = run_search(
        &space,
        SearchConfig {
            seed: 9,
            exhaustive_limit: 8,
            random_restarts: 0,
            maximum_evaluations: 1,
        },
        None,
        deterministic_cost,
    )
    .expect("the valid state fixture is produced");
    let valid = unique_temp_path("valid-search-state");
    save_search_state(&valid, &run.state).expect("the valid state persists");
    let base: serde_json::Value =
        serde_json::from_slice(&fs::read(&valid).expect("the valid state bytes are readable"))
            .expect("the valid state JSON parses");
    let corruptions = [
        {
            let mut value = base.clone();
            value["schema"] = json!("future-schema");
            value
        },
        {
            let mut value = base.clone();
            value
                .as_object_mut()
                .expect("state fixture is an object")
                .remove("evaluated");
            value
        },
        {
            let mut value = base.clone();
            value["pending"] = json!(["not-an-index"]);
            value
        },
        {
            let mut value = base.clone();
            value["evaluated"][0]
                .as_object_mut()
                .expect("evaluation fixture is an object")
                .remove("correctness_green");
            value
        },
    ];
    for (index, value) in corruptions.into_iter().enumerate() {
        let path = unique_temp_path(&format!("corrupt-search-state-{index}"));
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("the corrupt state serializes"),
        )
        .expect("the corrupt state is writable");
        assert!(matches!(
            load_search_state(&path),
            Err(TuneError::StateFormat(_))
        ));
        let _ = fs::remove_file(path);
    }
    let _ = fs::remove_file(malformed);
    let _ = fs::remove_file(valid);
}

#[test]
fn frontier_planted_premature_stop_cannot_complete_on_percentage_or_stagnation() {
    let pmu = PmuReport::new(
        vec![
            CounterReading::new(
                "stall",
                31.0,
                "percent",
                "31% backend stalls leave a reducible issue-width gap",
            )
            .expect("the planted counter is complete"),
        ],
        "planted-premature-stop.trace",
        AttributionClass::Reducible {
            reason: "backend stalls remain reducible".to_owned(),
        },
    )
    .expect("the planted PMU report is complete");
    let decision = CampaignDecision::evaluate(CampaignSignals {
        achieved_percent: 95.0,
        percentage_tripwire: 80.0,
        recent_improvements_percent: vec![0.2, 0.1, 0.0, 0.3, 0.1],
        pmu: PmuOutcome::Measured(pmu),
        workload_name: "synthetic-premature-stop".to_owned(),
        workload_is_provisional: true,
        follow_up_hypotheses: vec!["increase independent accumulators".to_owned()],
    });
    assert!(matches!(decision.stop, CampaignStop::FrontierOpen { .. }));
    assert!(decision.provisional);
}

#[test]
fn frontier_below_tripwire_forces_continuation_despite_irreducible_pmu() {
    let decision = CampaignDecision::evaluate(CampaignSignals {
        achieved_percent: 60.0,
        percentage_tripwire: 80.0,
        recent_improvements_percent: vec![2.0],
        pmu: irreducible_pmu("below-tripwire.trace"),
        workload_name: "real-below-tripwire".to_owned(),
        workload_is_provisional: false,
        follow_up_hypotheses: vec!["find the missing throughput mechanism".to_owned()],
    });
    assert!(matches!(decision.stop, CampaignStop::FrontierOpen { .. }));
}

#[test]
fn frontier_stale_denominator_cannot_complete_despite_irreducible_pmu() {
    let decision = CampaignDecision::evaluate(CampaignSignals {
        achieved_percent: 101.0,
        percentage_tripwire: 80.0,
        recent_improvements_percent: vec![2.0],
        pmu: irreducible_pmu("stale-denominator.trace"),
        workload_name: "real-stale-denominator".to_owned(),
        workload_is_provisional: false,
        follow_up_hypotheses: vec!["remeasure the denominator".to_owned()],
    });
    let CampaignStop::FrontierOpen { reason, .. } = decision.stop else {
        panic!(">100% must invalidate completion authority");
    };
    assert!(reason.contains("DENOMINATOR STALE"));
}

#[test]
fn frontier_exactly_one_hundred_completes_only_through_irreducible_pmu() {
    let decision = CampaignDecision::evaluate(CampaignSignals {
        achieved_percent: 100.0,
        percentage_tripwire: 80.0,
        recent_improvements_percent: vec![0.0; 5],
        pmu: irreducible_pmu("exactly-100.trace"),
        workload_name: "real-exact-boundary".to_owned(),
        workload_is_provisional: false,
        follow_up_hypotheses: Vec::new(),
    });
    let CampaignStop::Complete { attribution } = decision.stop else {
        panic!("exactly 100% is valid but only irreducible PMU may complete");
    };
    assert_eq!(attribution.evidence_path(), "exactly-100.trace");
    assert_eq!(
        attribution.conclusion(),
        "the remaining gap is measured refresh overhead"
    );
    assert_eq!(attribution.counters()[0].name, "bandwidth");
    assert!(!decision.provisional);

    let unavailable = CampaignDecision::evaluate(CampaignSignals {
        achieved_percent: 100.0,
        percentage_tripwire: 80.0,
        recent_improvements_percent: vec![2.0],
        pmu: PmuOutcome::Unavailable {
            reason: "CI has no CPU Counters template".to_owned(),
        },
        workload_name: "real-no-pmu".to_owned(),
        workload_is_provisional: false,
        follow_up_hypotheses: Vec::new(),
    });
    let CampaignStop::FrontierOpen { reason, hypotheses } = unavailable.stop else {
        panic!("PMU unavailability cannot construct completion");
    };
    assert!(reason.contains("percentage tripwire cleared"));
    assert!(reason.contains("PMU unavailable"));
    assert_eq!(
        hypotheses,
        vec!["collect PMU attribution and form the next mechanism hypothesis"]
    );
}

#[test]
fn frontier_attributed_synthetic_completion_stays_provisional_in_ledger() {
    let decision = CampaignDecision::evaluate(CampaignSignals {
        achieved_percent: 90.0,
        percentage_tripwire: 80.0,
        recent_improvements_percent: vec![2.0],
        pmu: irreducible_pmu("synthetic-attribution.trace"),
        workload_name: "synthetic-attributed".to_owned(),
        workload_is_provisional: true,
        follow_up_hypotheses: Vec::new(),
    });
    let CampaignStop::Complete { ref attribution } = decision.stop else {
        panic!("irreducible PMU is the completion authority");
    };
    assert!(!attribution.counters().is_empty());
    assert!(decision.provisional);

    let path = unique_temp_path("provisional-complete-ledger");
    let mut ledger = Ledger::open(&path).expect("the provisional ledger opens");
    ledger
        .append(&LedgerRow {
            date: "2026-08-20".to_owned(),
            hypothesis: "attribute the remaining gap".to_owned(),
            mechanism: "PMU saturation evidence".to_owned(),
            delta_percent: 1.0,
            roofline_percent: 90.0,
            decision: LedgerDecision::Keep,
            workload: decision.workload_name,
            provisional: decision.provisional,
            evidence_path: "tasks/evidence/synthetic-attribution.md".to_owned(),
            machine_state: MachineStateProvenance::DirectProbe,
            status: LedgerStatus::from_campaign_stop(&decision.stop),
        })
        .expect("the attributed row appends");
    let rows = ledger.rows().expect("the attributed row reloads");
    assert!(rows[0].provisional);
    let LedgerStatus::Complete { attribution } = &rows[0].status else {
        panic!("completion status must retain attribution");
    };
    assert_eq!(attribution.counters[0].name, "bandwidth");
    assert_eq!(attribution.evidence_path, "synthetic-attribution.trace");
    let _ = fs::remove_file(path);
}

#[test]
fn frontier_determinism_same_seed_produces_same_search_trajectory() {
    let space = search_space(32);
    let config = SearchConfig {
        seed: 0x27_2026_0820,
        exhaustive_limit: 8,
        random_restarts: 3,
        maximum_evaluations: 20,
    };
    let left = run_search(&space, config, None, deterministic_cost)
        .expect("the first seeded search completes");
    let right = run_search(&space, config, None, deterministic_cost)
        .expect("the repeated seeded search completes");
    assert_eq!(left.trajectory, right.trajectory);
    assert_eq!(left.state, right.state);
}

#[test]
fn frontier_resumed_search_matches_uninterrupted_seeded_trajectory() {
    let space = search_space(32);
    let partial_config = SearchConfig {
        seed: 0x27_2026_0820,
        exhaustive_limit: 8,
        random_restarts: 3,
        maximum_evaluations: 9,
    };
    let partial = run_search(&space, partial_config, None, deterministic_cost)
        .expect("the partial seeded search completes");
    let full_config = SearchConfig {
        maximum_evaluations: 20,
        ..partial_config
    };
    let resumed = run_search(
        &space,
        full_config,
        Some(partial.state.clone()),
        deterministic_cost,
    )
    .expect("the saved state resumes");
    let uninterrupted = run_search(&space, full_config, None, deterministic_cost)
        .expect("the uninterrupted seeded search completes");
    assert_eq!(resumed.trajectory, uninterrupted.trajectory);
    assert_eq!(resumed.state, uninterrupted.state);
}

#[test]
fn frontier_search_state_round_trips_for_process_resumption() {
    let space = search_space(32);
    let config = SearchConfig {
        seed: 0x27_2026_0820,
        exhaustive_limit: 8,
        random_restarts: 3,
        maximum_evaluations: 9,
    };
    let partial = run_search(&space, config, None, deterministic_cost)
        .expect("the partial seeded search completes");
    let path = unique_temp_path("search-state");
    save_search_state(&path, &partial.state).expect("state persists atomically");
    let loaded = load_search_state(&path).expect("state reloads");
    assert_eq!(loaded, partial.state);
    let _ = fs::remove_file(path);
}

#[test]
fn frontier_cli_no_arguments_returns_the_existing_usage_error() {
    let error = parse_cli(&[]).expect_err("the frontier command is required");
    assert_eq!(error, CliError::Usage);
    assert_eq!(
        error.to_string(),
        "usage: frontier attest | tune --campaign kernels-i8 --smoke [--seed N] | report | denominators [--persist --date YYYY-MM-DD [--allow-lower-ceiling]]"
    );
}

#[test]
fn frontier_cli_unknown_subcommand_is_a_typed_usage_error() {
    let error = parse_cli(&["unknown"]).expect_err("unknown commands must not select a default");
    assert_eq!(
        error,
        CliError::UnknownSubcommand {
            command: "unknown".to_owned(),
        }
    );
    assert!(error.to_string().starts_with("usage: frontier attest"));
}

#[test]
fn frontier_cli_attest_is_an_exact_arity_command() {
    assert_eq!(
        parse_cli(&["attest"]).expect("attest has no caller-supplied evidence"),
        FrontierCommand::Attest
    );
    assert_eq!(
        parse_cli(&["attest", "quoted-output"])
            .expect_err("attest must collect live output rather than accept arguments"),
        CliError::Usage
    );
}

#[test]
fn frontier_cli_tune_requires_an_explicit_campaign() {
    let error = parse_cli(&["tune", "--smoke"])
        .expect_err("smoke without a campaign must not run a default campaign");
    assert_eq!(error, CliError::TuneCampaignRequired);
    assert_eq!(
        error.to_string(),
        "27-H exposes only the non-campaign smoke: tune --campaign kernels-i8 --smoke"
    );
}

#[test]
fn frontier_cli_unknown_campaign_is_typed_and_not_silently_defaulted() {
    let error = parse_cli(&["tune", "--campaign", "unknown", "--smoke"])
        .expect_err("unknown campaigns must not run kernels-i8");
    assert_eq!(
        error,
        CliError::UnknownCampaign {
            campaign: "unknown".to_owned(),
        }
    );
    assert!(error.to_string().contains("only the non-campaign smoke"));
}

#[test]
fn frontier_cli_seed_requires_a_numeric_value() {
    let missing = parse_cli(&["tune", "--campaign", "kernels-i8", "--smoke", "--seed"])
        .expect_err("--seed without a value must be rejected");
    assert_eq!(missing, CliError::SeedValueRequired);
    assert_eq!(missing.to_string(), "--seed requires a value");

    let invalid = parse_cli(&[
        "tune",
        "--campaign",
        "kernels-i8",
        "--smoke",
        "--seed",
        "not-a-seed",
    ])
    .expect_err("a non-numeric seed must be rejected");
    let CliError::InvalidSeed { value, reason } = &invalid else {
        panic!("a non-numeric seed must return InvalidSeed");
    };
    assert_eq!(value, "not-a-seed");
    assert!(reason.contains("invalid digit"));
    assert!(invalid.to_string().starts_with("invalid seed:"));
}

#[test]
fn frontier_cli_valid_seed_is_parsed_into_the_tune_command() {
    let command = parse_cli(&[
        "tune",
        "--campaign",
        "kernels-i8",
        "--smoke",
        "--seed",
        "424242",
    ])
    .expect("the supported seeded smoke command parses");
    assert_eq!(
        command,
        FrontierCommand::Tune(TuneCommand {
            campaign: TuneCampaign::KernelsI8,
            smoke: true,
            seed: 424_242,
        })
    );
}

#[test]
fn frontier_cli_smoke_presence_is_recorded_and_absence_is_rejected() {
    let present = parse_cli(&["tune", "--campaign", "kernels-i8", "--smoke"])
        .expect("the supported smoke command parses");
    assert_eq!(
        present,
        FrontierCommand::Tune(TuneCommand {
            campaign: TuneCampaign::KernelsI8,
            smoke: true,
            seed: DEFAULT_SEED,
        })
    );

    let absent = parse_cli(&["tune", "--campaign", "kernels-i8"])
        .expect_err("the campaign command must remain smoke-only");
    assert_eq!(absent, CliError::SmokeRequired);
    assert!(absent.to_string().contains("--smoke"));
}

#[test]
fn frontier_cli_unknown_tune_flag_is_a_typed_error() {
    let error = parse_cli(&["tune", "--campaign", "kernels-i8", "--smoke", "--bogus"])
        .expect_err("unknown tune flags must not be ignored");
    assert_eq!(
        error,
        CliError::UnknownTuneArgument {
            argument: "--bogus".to_owned(),
        }
    );
    assert_eq!(error.to_string(), "unknown tune argument --bogus");
}

#[test]
fn frontier_cli_report_and_denominator_options_preserve_existing_validation() {
    assert_eq!(
        parse_cli(&["report"]).expect("report has no required arguments"),
        FrontierCommand::Report
    );
    assert_eq!(
        parse_cli(&["denominators"]).expect("denominators defaults to measurement only"),
        FrontierCommand::Denominators(DenominatorCommand::default())
    );
    assert_eq!(
        parse_cli(&[
            "denominators",
            "--persist",
            "--date",
            "2026-08-20",
            "--allow-lower-ceiling",
        ])
        .expect("the existing fully specified persistence command parses"),
        FrontierCommand::Denominators(DenominatorCommand {
            persist: true,
            date: Some("2026-08-20".to_owned()),
            allow_lower_ceiling: true,
        })
    );

    let cases = [
        (vec!["report", "extra"], CliError::Usage, "usage: frontier"),
        (
            vec!["denominators", "--date"],
            CliError::DateValueRequired,
            "--date requires YYYY-MM-DD",
        ),
        (
            vec!["denominators", "--bogus"],
            CliError::UnknownDenominatorsArgument {
                argument: "--bogus".to_owned(),
            },
            "unknown denominators argument --bogus",
        ),
        (
            vec!["denominators", "--date", "2026-08-20"],
            CliError::PersistenceRequired,
            "--date and --allow-lower-ceiling apply only with --persist",
        ),
        (
            vec!["denominators", "--persist"],
            CliError::PersistenceDateRequired,
            "--persist requires caller-supplied --date YYYY-MM-DD",
        ),
    ];
    for (arguments, expected, message) in cases {
        let error = parse_cli(&arguments).expect_err("invalid denominator options are rejected");
        assert_eq!(error, expected);
        assert!(error.to_string().contains(message));
    }
}

#[test]
fn frontier_cli_rejects_bad_subcommands_seeds_campaigns_and_persistence_flags() {
    let cases: &[(&[&str], &str)] = &[
        (&["not-a-command"], "usage: frontier"),
        (
            &[
                "tune",
                "--campaign",
                "kernels-i8",
                "--smoke",
                "--seed",
                "not-a-seed",
            ],
            "invalid seed",
        ),
        (
            &["tune", "--campaign", "unknown", "--smoke"],
            "27-H exposes only the non-campaign smoke",
        ),
        (
            &["tune", "--campaign", "kernels-i8", "--smoke", "--bogus"],
            "unknown tune argument --bogus",
        ),
        (&["report", "extra"], "usage: frontier"),
        (
            &["denominators", "--persist"],
            "--persist requires caller-supplied --date YYYY-MM-DD",
        ),
        (
            &["denominators", "--allow-lower-ceiling"],
            "--date and --allow-lower-ceiling apply only with --persist",
        ),
    ];
    for (arguments, expected) in cases {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_frontier"))
            .args(*arguments)
            .output()
            .expect("the frontier test binary launches");
        assert_eq!(output.status.code(), Some(1), "arguments: {arguments:?}");
        let stderr = String::from_utf8(output.stderr).expect("frontier stderr is UTF-8");
        assert!(
            stderr.contains(expected),
            "arguments {arguments:?} returned unexpected stderr: {stderr}"
        );
    }
}

fn parse_cli(arguments: &[&str]) -> Result<FrontierCommand, CliError> {
    parse_command(
        &arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>(),
    )
}

fn frontier_row(hypothesis: &str, delta_percent: f64) -> LedgerRow {
    LedgerRow {
        date: "2026-08-20".to_owned(),
        hypothesis: hypothesis.to_owned(),
        mechanism: "test mechanism".to_owned(),
        delta_percent,
        roofline_percent: 75.0,
        decision: LedgerDecision::Revert,
        workload: "synthetic-test".to_owned(),
        provisional: true,
        evidence_path: "tasks/evidence/test.md".to_owned(),
        machine_state: MachineStateProvenance::DirectProbe,
        status: LedgerStatus::FrontierOpen {
            reason: "test frontier remains open".to_owned(),
            hypotheses: vec!["next test hypothesis".to_owned()],
        },
    }
}

fn attestation_bytes(timestamp: &str, power_raw: &str, thermal_raw: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": 1,
        "timestamp": timestamp,
        "machine_identifier": "Mac15,9",
        "pmset_ps_raw": power_raw,
        "pmset_therm_raw": thermal_raw,
    }))
    .expect("the attestation fixture serializes")
}

fn attestation_rejection_reason(source: &StaticAttestationSource) -> String {
    let CampaignPreflightOutcome::Idle { reasons } =
        preflight_with_attestation(&ErrorProbe, source)
    else {
        panic!("the corrupt attestation must idle");
    };
    reasons
        .last()
        .cloned()
        .expect("attestation rejection includes a reason")
}

fn calibration_artifact(base_gmac_per_second: f64, measured_date: &str) -> CalibrationArtifact {
    CalibrationArtifact::new(
        CalibrationMachineContext {
            model_name: "MacBook Pro".to_owned(),
            model_identifier: "Mac15,9".to_owned(),
            chip: "Apple M3 Max".to_owned(),
            os_product_version: "27.0".to_owned(),
            os_build: "26A5388g".to_owned(),
            power_state: "AC Power".to_owned(),
            thermal_state: "No thermal warning level has been recorded".to_owned(),
        },
        measured_date,
        "frontier calibration test",
        vec![
            CalibrationTier::new(
                ComputeTier::NeonFma,
                base_gmac_per_second,
                vec![CalibrationRun {
                    run: 1,
                    gmac_per_second: base_gmac_per_second,
                    raw_medians_ns: vec![1.0, 1.0, 1.0],
                    checksum: 1,
                }],
            )
            .expect("the FMA fixture is valid"),
            CalibrationTier::new(
                ComputeTier::NeonSdot,
                base_gmac_per_second * 2.0,
                vec![CalibrationRun {
                    run: 1,
                    gmac_per_second: base_gmac_per_second * 2.0,
                    raw_medians_ns: vec![1.0, 1.0, 1.0],
                    checksum: 2,
                }],
            )
            .expect("the SDOT fixture is valid"),
            CalibrationTier::new(
                ComputeTier::NeonFp16ConvertFma,
                base_gmac_per_second / 2.0,
                vec![CalibrationRun {
                    run: 1,
                    gmac_per_second: base_gmac_per_second / 2.0,
                    raw_medians_ns: vec![1.0, 1.0, 1.0],
                    checksum: 3,
                }],
            )
            .expect("the FP16 fixture is valid"),
        ],
    )
    .expect("the complete calibration fixture is valid")
}

fn unique_temp_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock is after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "zeppelin-frontier-{label}-{}-{nonce}.jsonl",
        std::process::id()
    ))
}

fn search_space(point_count: usize) -> SearchSpace {
    let points = (0..point_count)
        .map(|index| KernelPoint {
            unroll: 2 + index,
            accumulators: 2,
            rows_per_block: 1,
            prefetch_dist: 0,
            tier: InstructionTier::Scalar,
        })
        .collect();
    SearchSpace::new(points).expect("the test search space is unique and nonempty")
}

fn deterministic_cost(point: KernelPoint) -> CandidateEvaluation {
    let distance = point.unroll.abs_diff(19) as f64;
    CandidateEvaluation::correct(point.stable_id(), 50.0 + distance)
}

fn irreducible_pmu(evidence_path: &str) -> PmuOutcome {
    PmuOutcome::Measured(
        PmuReport::new(
            vec![
                CounterReading::new(
                    "bandwidth",
                    80.0,
                    "GB/s",
                    "sustained bandwidth equals the measured ceiling",
                )
                .expect("the irreducible counter is complete"),
            ],
            evidence_path,
            AttributionClass::Irreducible {
                cause: "the remaining gap is measured refresh overhead".to_owned(),
            },
        )
        .expect("the irreducible report is complete"),
    )
}
