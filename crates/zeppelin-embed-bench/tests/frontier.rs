use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use zeppelin_embed::kernels::InstructionTier;
use zeppelin_embed_bench::frontier::calibration::{
    CalibrationArtifact, CalibrationError, CalibrationMachineContext, CalibrationRun,
    CalibrationTier, CalibrationWritePolicy, default_calibration_path, load_calibration,
    persist_calibration,
};
use zeppelin_embed_bench::frontier::ledger::{
    Ledger, LedgerDecision, LedgerError, LedgerRow, LedgerStatus,
};
use zeppelin_embed_bench::frontier::measure::{
    MachineProbe, MeasurementConfig, PreflightOutcome, ProbeOutput, SampleSource,
    StridedI8Workload, SyntheticI8Workload, Workload, measure_source, preflight,
};
use zeppelin_embed_bench::frontier::pmu::{
    AttributionClass, CounterReading, PmuOutcome, PmuReport, parse_counter_export,
};
use zeppelin_embed_bench::frontier::roofline::{
    BindingBound, ComputeCeiling, ComputeTier, DenominatorProvenance, RooflineDiagnostic,
    RooflineInput, RooflineModel, WIDE_LOAD_SINGLE_CORE_GBPS,
};
use zeppelin_embed_bench::frontier::tune::{
    CampaignDecision, CampaignSignals, CampaignStop, CandidateDisposition, CandidateEvaluation,
    SearchConfig, SearchSpace, load_search_state, rank_candidates, run_search, save_search_state,
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

#[derive(Clone)]
struct MockProbe {
    power: ProbeOutput,
    thermal: ProbeOutput,
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
        status: LedgerStatus::FrontierOpen {
            reason: "test frontier remains open".to_owned(),
            hypotheses: vec!["next test hypothesis".to_owned()],
        },
    }
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
