#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
#![recursion_limit = "256"]

mod adversarial;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs::File, io::Write as _};

use adversarial::campaign::{
    CampaignKind, CampaignSpec, FaultPlan, InvariantId, Qualification, RunConfig,
};
use adversarial::coverage::CoverageRegistry;
use adversarial::fault_vfs::{FaultEvent, FaultMode, FaultSite, ScheduledVfs};
use adversarial::profiles::FaultProfile;
use adversarial::program::{Op, PredicateKind, Program};
use adversarial::runner::{Invariant, SelfTestBug};
use zeppelin_embed::lifecycle::{ManualMonotonicClock, OpenOptions, Store, StoreTestDependencies};
use zeppelin_embed::meta::Schema;
use zeppelin_embed::vfs::StdVfs;

#[test]
fn campaign_registry_is_complete_unique_and_smoke_bounded() {
    let specs = CampaignSpec::catalog();
    let keys = specs
        .iter()
        .map(|spec| spec.kind.key())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), specs.len(), "duplicate campaign key");
    assert_eq!(
        keys,
        [
            "diagnostics-health",
            "ffi-bindings",
            "fts",
            "hybrid-fusion",
            "ingest-retention",
            "lifecycle-accounting",
            "metadata-filter-planner",
            "overall",
            "storage-durability",
            "tiering-maintenance",
            "vamana-graph",
            "vector-execution",
        ]
        .into_iter()
        .collect()
    );
    for spec in specs {
        assert!(!spec.required_coverage.is_empty(), "{}", spec.kind.key());
        assert!(
            spec.feature_faults.len() < 12,
            "{} has no clean slot in its 12-seed smoke cycle",
            spec.kind.key()
        );
        for fault in spec.feature_faults {
            assert_eq!(fault.campaign(), spec.kind, "typed fault crossed campaigns");
            assert!(
                spec.required_operations.contains(&fault.operation().key()),
                "{} fault {} targets undeclared operation {}",
                spec.kind.key(),
                fault.key(),
                fault.operation().key()
            );
        }
        if spec.kind != CampaignKind::Overall {
            let bound_ids = spec
                .invariant_specs
                .iter()
                .map(|binding| binding.invariant)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                bound_ids,
                spec.owned_invariants.iter().copied().collect(),
                "{} invariant bindings drifted",
                spec.kind.key()
            );
            for operation in adversarial::campaign::feature_operations(spec.kind) {
                assert!(
                    spec.invariant_specs
                        .iter()
                        .any(|binding| binding.operation == *operation),
                    "{} operation {} has no exact checker binding",
                    spec.kind.key(),
                    operation.key()
                );
            }
        }
    }
}

#[test]
fn invariant_registry_assigns_active_ids_once_and_reserves_epoch_ranges() {
    let specs = CampaignSpec::catalog();
    let mut assignments = BTreeMap::<InvariantId, &str>::new();
    for spec in specs {
        for invariant in spec.owned_invariants {
            assert!(
                assignments.insert(*invariant, spec.kind.key()).is_none(),
                "duplicate assignment for {}",
                invariant.key()
            );
            assert!(
                !invariant.label().is_empty(),
                "{} has no label",
                invariant.key()
            );
        }
    }
    assert_eq!(assignments.len(), 64);
    assert_eq!(
        assignments.keys().next().copied(),
        Some(InvariantId::new(1))
    );
    assert_eq!(
        assignments.keys().next_back().copied(),
        Some(InvariantId::new(70))
    );
    let reserved = adversarial::campaign::RESERVED_INVARIANTS
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        reserved,
        [12_u8, 14, 59, 60, 61, 62, 71, 72, 73, 74]
            .into_iter()
            .map(InvariantId::new)
            .collect()
    );
    assert!(
        assignments
            .keys()
            .all(|invariant| !reserved.contains(invariant))
    );
}

#[test]
fn generic_oracle_checker_kinds_have_planted_counterexamples() {
    for spec in CampaignSpec::catalog() {
        for binding in spec.invariant_specs {
            let clean = adversarial::oracle::PrimitiveObservation::clean(
                vec![1, 2, 3],
                vec![1.0_f64.to_bits()],
                3,
            );
            let planted = adversarial::oracle::planted(clean, binding.checker);
            let record = adversarial::oracle::compare(
                binding.invariant.number(),
                binding.checker_id,
                binding.operation.key(),
                binding.checker,
                &planted,
                "deterministic CAN-FIRE plant",
            );
            assert!(
                !record.passed,
                "generic {} checker plant did not fire",
                binding.checker_id
            );
            assert_eq!(record.invariant, binding.invariant.number());
            assert!(record.detail.contains(&binding.invariant.key()));
        }
    }
}

#[test]
fn independent_oracle_source_rejects_production_imports() {
    let source = include_str!("adversarial/oracle.rs");
    for forbidden in [
        "zeppelin_embed::",
        "crate::fts",
        "crate::fusion",
        "crate::planner",
    ] {
        assert!(
            !source.contains(forbidden),
            "independent oracle imported production helper {forbidden}"
        );
    }
}

#[test]
fn overall_campaign_dispatch_preserves_the_original_program_bytes() {
    for seed in [0, 1, 7, 11, 90_004] {
        assert_eq!(
            Program::generate_for(CampaignKind::Overall, seed).jsonl(),
            Program::generate(seed).jsonl(),
            "overall program drifted for seed {seed}"
        );
    }
}

#[test]
fn feature_programs_are_namespaced_and_emit_the_declared_operations() {
    for campaign in CampaignKind::FEATURES {
        let spec = CampaignSpec::for_kind(campaign);
        let program = Program::generate_for(campaign, 7);
        let emitted = program
            .ops
            .iter()
            .filter_map(|operation| match operation {
                Op::Feature(operation) if operation.campaign() == campaign => Some(operation.key()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            emitted,
            spec.required_operations.iter().copied().collect(),
            "{} dedicated operation grammar drifted",
            campaign.key()
        );
        assert_ne!(
            program.jsonl(),
            Program::generate(7).jsonl(),
            "{} reused overall bytes instead of its RNG namespace",
            campaign.key()
        );
    }
}

#[test]
fn feature_programs_exclude_every_legacy_epoch_operation() {
    for campaign in CampaignKind::FEATURES {
        for seed in 0..12 {
            let program = Program::generate_for(campaign, seed);
            assert!(
                program.ops.iter().all(|operation| !matches!(
                    operation,
                    Op::EpochMismatchProbe { .. }
                        | Op::PrepareEpochB
                        | Op::SwitchAliasToB
                        | Op::RollbackToA
                        | Op::DropEpochA
                        | Op::RollbackDroppedAProbe
                )),
                "{} seed={seed} retained an epoch operation",
                campaign.key()
            );
        }
    }
}

#[test]
fn feature_fault_plan_has_a_clean_slot_and_full_has_two_distinct_faults() {
    for campaign in CampaignKind::FEATURES {
        let spec = CampaignSpec::for_kind(campaign);
        assert!(
            spec.feature_faults.iter().all(|fault| {
                !fault.key().contains("epoch") && !fault.label().contains("epoch")
            }),
            "{} retained an epoch-specific feature fault",
            campaign.key()
        );
        let mut selected = std::collections::BTreeSet::new();
        let mut clean = 0;
        for seed in 0..12 {
            let program = Program::generate_for(campaign, seed);
            let plan = FaultPlan::for_program(campaign, seed, FaultProfile::None, &program, None);
            assert!(plan.feature.len() <= 1, "{} seed={seed}", campaign.key());
            if let Some(fault) = plan.feature.first() {
                selected.insert(fault.fault.key());
            } else {
                clean += 1;
            }

            let full = FaultPlan::for_program(campaign, seed, FaultProfile::Full, &program, None);
            assert_eq!(full.feature.len(), 2, "{} seed={seed}", campaign.key());
            assert_ne!(full.feature[0].fault, full.feature[1].fault);
        }
        assert!(
            clean > 0,
            "{} has no clean feature-fault seed",
            campaign.key()
        );
        assert_eq!(
            selected,
            spec.feature_faults
                .iter()
                .map(|fault| fault.key())
                .collect(),
            "{} smoke cannot select its complete feature-fault vocabulary",
            campaign.key()
        );
    }
}

#[test]
fn isolated_vfs_probes_cannot_issue_feature_fault_receipts() {
    let source = include_str!("adversarial/fault_vfs.rs");
    assert!(!source.contains("fire_feature_fault"));
    assert!(!source.contains("/feature.bin"));
    assert!(!source.contains("FeatureFaultReceipt"));
}

#[test]
fn selected_feature_fault_fires_once_at_its_declared_operation() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            !FaultPlan::for_program(campaign, *seed, FaultProfile::None, &program, None)
                .feature
                .is_empty()
        })
        .expect("storage campaign has a selected fault seed");
    let artifacts = tempfile::tempdir().expect("feature episode artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("the episode retains its failure evidence");
    assert_eq!(outcome.feature_faults_scheduled, 1);
    assert_eq!(outcome.feature_faults_fired, 1);
    assert!(outcome.missing_feature_faults.is_empty());
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
}

#[test]
fn every_feature_fault_can_fire_once_at_its_declared_operation() {
    for campaign in CampaignKind::FEATURES {
        let mut fired = BTreeSet::new();
        for seed in 0..12 {
            let program = Program::generate_for(campaign, seed);
            let plan = FaultPlan::for_program(campaign, seed, FaultProfile::None, &program, None);
            if plan.feature.is_empty() {
                continue;
            }
            let artifacts = tempfile::tempdir().expect("feature fault artifacts");
            let outcome = adversarial::runner::run_program_for(
                campaign,
                seed,
                FaultProfile::None,
                artifacts.path(),
            )
            .expect("feature fault episode");
            assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
            assert_eq!(
                outcome.feature_faults_scheduled, outcome.feature_faults_fired,
                "{campaign} seed {seed} did not fire its selected fault",
            );
            assert!(outcome.missing_feature_faults.is_empty());
            fired.extend(plan.feature.into_iter().map(|event| event.fault.key()));
        }
        assert_eq!(
            fired,
            CampaignSpec::for_kind(campaign)
                .feature_faults
                .iter()
                .map(|fault| fault.key())
                .collect(),
            "{campaign} did not CAN-FIRE its full vocabulary",
        );
    }
}

#[test]
fn clean_feature_episode_executes_every_bound_checker() {
    let campaign = CampaignKind::StorageDurability;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(campaign, *seed, FaultProfile::None, &program, None)
                .feature
                .is_empty()
        })
        .expect("storage campaign has a clean feature-fault slot");
    let artifacts = tempfile::tempdir().expect("clean feature artifacts");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, artifacts.path())
            .expect("clean feature episode");

    assert_eq!(outcome.campaign, campaign);
    assert_eq!(outcome.feature_faults_scheduled, 0);
    assert_eq!(outcome.feature_faults_fired, 0);
    assert!(outcome.missing_feature_faults.is_empty());
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    let oracle = String::from_utf8(outcome.oracle_bytes).expect("oracle JSON is UTF-8");
    for binding in CampaignSpec::for_kind(campaign).invariant_specs {
        assert!(
            oracle.contains(&format!("\"checker_id\":\"{}\"", binding.checker_id)),
            "{} did not emit its exact checker record: {oracle}",
            binding.checker_id,
        );
        assert!(
            oracle.contains(&format!("\"invariant\":\"{}\"", binding.invariant.key())),
            "{} did not name its invariant: {oracle}",
            binding.checker_id,
        );
    }
    for invariant in CampaignSpec::for_kind(campaign).owned_invariants {
        assert!(outcome.coverage.count(&invariant.checked_coverage_key()) > 0);
    }
}

#[test]
fn overall_dispatch_preserves_all_four_compatibility_artifact_streams() {
    for (seed, profile) in [(0, FaultProfile::None), (7, FaultProfile::Content)] {
        let old_root = tempfile::tempdir().expect("old overall artifacts");
        let new_root = tempfile::tempdir().expect("dispatched overall artifacts");
        let old = adversarial::runner::run_program(seed, profile, old_root.path())
            .expect("legacy overall run");
        let new = adversarial::runner::run_program_for(
            CampaignKind::Overall,
            seed,
            profile,
            new_root.path(),
        )
        .expect("dispatched overall run");
        assert_eq!(old.program_bytes, new.program_bytes);
        assert_eq!(old.faults_bytes, new.faults_bytes);
        assert_eq!(old.violations_bytes, new.violations_bytes);
        assert_eq!(old.coverage_bytes, new.coverage_bytes);
    }
}

#[test]
fn feature_episode_writes_schema_v3_replay_metadata() {
    let root = tempfile::tempdir().expect("feature metadata root");
    let campaign = CampaignKind::Fts;
    let seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(campaign, *seed);
            FaultPlan::for_program(campaign, *seed, FaultProfile::None, &program, None)
                .feature
                .is_empty()
        })
        .expect("FTS campaign has a clean feature-fault slot");
    let outcome =
        adversarial::runner::run_program_for(campaign, seed, FaultProfile::None, root.path())
            .expect("feature metadata episode");
    assert_eq!(outcome.campaign, campaign);
    let directory = root.path().join(format!("fts/seed-{seed}-none"));
    let metadata: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(directory.join("episode.json")).expect("episode metadata"),
        )
        .expect("parse episode metadata");
    assert_eq!(metadata["version"], 3);
    assert_eq!(metadata["campaign"], "fts");
    assert_eq!(
        adversarial::campaign::campaign_from_replay_metadata(&directory)
            .expect("feature replay campaign"),
        CampaignKind::Fts
    );
}

#[test]
fn legacy_replay_artifacts_are_implicit_overall() {
    for fixture in [
        zeppelin_embed_bench::harness_json::json!({"version": 2}),
        zeppelin_embed_bench::harness_json::json!({"schema": "zeppelin-embed-adversarial-failure", "version": 1}),
    ] {
        let directory = tempfile::tempdir().expect("legacy replay directory");
        std::fs::write(
            directory.path().join("legacy.json"),
            zeppelin_embed_bench::harness_json::to_vec(&fixture).expect("legacy fixture JSON"),
        )
        .expect("write legacy fixture");
        assert_eq!(
            adversarial::campaign::campaign_from_replay_metadata(directory.path())
                .expect("legacy replay campaign"),
            CampaignKind::Overall
        );
    }
}

#[test]
fn injected_store_vfs_reaches_open_and_wal_creation() {
    let directory = tempfile::tempdir().expect("injected VFS store");
    let scheduled = Arc::new(ScheduledVfs::new(
        StdVfs,
        Some(FaultEvent {
            id: "open-write".to_owned(),
            op_index: 0,
            site: FaultSite::Write,
            mode: FaultMode::Eio,
            nth_match: 1,
            path_contains: None,
            fired: false,
            path: None,
        }),
    ));
    scheduled.set_operation(0);
    let dependencies =
        StoreTestDependencies::new(scheduled.clone(), Arc::new(ManualMonotonicClock::new()));
    let result = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_schema(Schema::timestamp_only()),
        dependencies,
    );
    if let Ok(store) = result {
        store.close().expect("close unexpectedly opened store");
    }
    assert!(
        scheduled.event().is_some_and(|event| event.fired),
        "the Store-owned VFS did not observe initial manifest publication"
    );
}

#[test]
fn self_test_dropped_acknowledged_write_trips_i1() {
    let violation = adversarial::runner::run_self_test(SelfTestBug::DropAcknowledgedWrite);
    assert_eq!(violation.invariant, Invariant::I1);
    println!("{}", violation.report());
}

#[test]
fn self_test_wrong_document_trips_i3() {
    let violation = adversarial::runner::run_self_test(SelfTestBug::WrongDocument);
    assert_eq!(violation.invariant, Invariant::I3);
    println!("{}", violation.report());
}

#[test]
fn self_test_tombstone_leak_trips_i2() {
    let violation = adversarial::runner::run_self_test(SelfTestBug::LeakTombstone);
    assert_eq!(violation.invariant, Invariant::I2);
    println!("{}", violation.report());
}

#[test]
fn self_test_generation_lie_trips_i9() {
    let violation = adversarial::runner::run_self_test(SelfTestBug::MisreportGeneration);
    assert_eq!(violation.invariant, Invariant::I9);
    println!("{}", violation.report());
}

#[test]
fn self_test_filtered_result_outside_predicate_trips_i5() {
    let violation = adversarial::runner::planted_counterexample(Invariant::I5);
    assert_eq!(violation.invariant, Invariant::I5);
    println!("{}", violation.report());
}

#[test]
fn self_test_diagnostics_lie_trips_i13() {
    let violation = adversarial::runner::planted_counterexample(Invariant::I13);
    assert_eq!(violation.invariant, Invariant::I13);
    println!("{}", violation.report());
}

#[test]
fn self_test_mixed_alias_segments_trip_i14() {
    let violation = adversarial::runner::planted_counterexample(Invariant::I14);
    assert_eq!(violation.invariant, Invariant::I14);
    println!("{}", violation.report());
}

#[test]
fn every_implemented_invariant_has_a_counterexample_that_trips_it() {
    for invariant in [
        Invariant::I1,
        Invariant::I2,
        Invariant::I3,
        Invariant::I4,
        Invariant::I5,
        Invariant::I6,
        Invariant::I7,
        Invariant::I8,
        Invariant::I9,
        Invariant::I10,
        Invariant::I11,
        Invariant::I12,
        Invariant::I13,
        Invariant::I14,
        Invariant::I54,
    ] {
        let violation = adversarial::runner::planted_counterexample(invariant);
        assert_eq!(violation.invariant, invariant);
    }
}

#[test]
fn same_seed_is_byte_identical_and_has_identical_outcome() {
    let first = tempfile::tempdir().expect("first artifact root");
    let second = tempfile::tempdir().expect("second artifact root");
    let left = adversarial::runner::run_program(11, FaultProfile::None, first.path())
        .expect("first deterministic run");
    let right = adversarial::runner::run_program(11, FaultProfile::None, second.path())
        .expect("second deterministic run");
    assert_eq!(left.program_bytes, right.program_bytes);
    assert_eq!(left.faults_bytes, right.faults_bytes);
    assert_eq!(left.violations_bytes, right.violations_bytes);
    assert_eq!(left.coverage_bytes, right.coverage_bytes);
    assert_eq!(left.violations, right.violations);
}

#[test]
fn adversarial_program_can_delete_a_pre_seal_id() {
    let program = Program::generate(11);
    let (first_id, count) = program
        .ops
        .iter()
        .find_map(|op| match op {
            Op::Ingest {
                first_id, count, ..
            } => Some((*first_id, *count)),
            _ => None,
        })
        .expect("generated program starts with an ingest range");
    let first_seal = program
        .ops
        .iter()
        .position(|op| matches!(op, Op::Seal))
        .expect("generated program seals its initial range");
    let was_in_initial_range =
        |doc_id: u32| doc_id >= first_id && doc_id < first_id.saturating_add(count);
    let after_first_seal = &program.ops[first_seal.saturating_add(1)..];

    assert!(
        after_first_seal
            .iter()
            .any(|op| matches!(op, Op::Delete { doc_id } if was_in_initial_range(*doc_id)))
    );
    assert!(
        program.ops[..first_seal]
            .iter()
            .any(|op| matches!(op, Op::FilteredSearch { .. }))
    );
    assert!(
        after_first_seal
            .iter()
            .any(|op| matches!(op, Op::Revise { doc_id, .. } if was_in_initial_range(*doc_id)))
    );
    assert!(
        after_first_seal
            .iter()
            .any(|op| matches!(op, Op::Upsert { doc_id, .. } if was_in_initial_range(*doc_id)))
    );
    assert!(
        program
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Seal))
            .count()
            > 1
    );
}

#[test]
fn the_emitted_sweep_program_interleaves_an_epoch_mismatch_probe_with_real_writes() {
    let root = tempfile::tempdir().expect("epoch probe artifact root");
    let outcome = adversarial::runner::run_program(11, FaultProfile::None, root.path())
        .expect("emit adversarial program");
    let emitted = std::str::from_utf8(&outcome.program_bytes).expect("program artifact is UTF-8");
    let lines = emitted.lines().collect::<Vec<_>>();
    let probe = lines
        .iter()
        .position(|line| line.contains("\"kind\":\"epoch_mismatch_probe\""))
        .expect("emitted program contains the epoch probe");

    assert!(
        lines[..probe]
            .iter()
            .any(|line| line.contains("\"kind\":\"ingest\""))
    );
    assert!(lines[probe.saturating_add(1)..].iter().any(|line| {
        line.contains("\"kind\":\"upsert\"") || line.contains("\"kind\":\"revise\"")
    }));
}

#[test]
fn an_emitted_program_executes_alias_switch_rollback_drop_and_typed_rejection() {
    let root = tempfile::tempdir().expect("epoch-transition artifact root");
    let outcome = adversarial::runner::run_program(11, FaultProfile::None, root.path())
        .expect("execute emitted epoch-transition program");
    assert!(
        outcome.violations.is_empty(),
        "epoch-transition program found violations: {:?}",
        outcome.violations
    );
    assert_eq!(outcome.epoch_preparations, 1);
    assert_eq!(outcome.epoch_alias_switches, 2);
    assert_eq!(outcome.epoch_rollbacks, 1);
    assert_eq!(outcome.epoch_drops, 1);
    assert_eq!(outcome.rejected_dropped_epoch_rollbacks, 1);

    let emitted = std::str::from_utf8(&outcome.program_bytes).expect("program artifact is UTF-8");
    for operation in [
        "prepare_epoch_b",
        "switch_alias_to_b",
        "rollback_to_a",
        "drop_epoch_a",
        "rollback_dropped_a_probe",
    ] {
        assert!(
            emitted
                .lines()
                .any(|line| line.contains(&format!("\"kind\":\"{operation}\""))),
            "executed artifact omitted {operation}"
        );
    }
}

#[test]
fn an_emitted_program_executes_a_filtered_graph_query() {
    let program = Program::generate(0);
    assert!(
        program
            .ops
            .iter()
            .any(|op| matches!(op, Op::FilteredSearch { .. })),
        "the emitted program must contain a filtered operation"
    );
    let artifacts = tempfile::tempdir().expect("filtered graph artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("filtered graph reachability program");
    assert!(
        outcome.violations.is_empty(),
        "filtered graph reachability found violations: {:?}",
        outcome.violations
    );
    assert!(
        outcome.filtered_graph_searches > 0,
        "the emitted program ran no in-traversal filtered graph branch"
    );
}

#[test]
fn an_emitted_hybrid_operation_fuses_sealed_vectors_and_lexical_content() {
    let program = Program::generate(0);
    let hybrid = program
        .ops
        .iter()
        .position(|op| matches!(op, Op::HybridSearch { .. }))
        .expect("the emitted program must contain a hybrid operation");
    assert!(
        program.ops[..hybrid]
            .iter()
            .any(|op| matches!(op, Op::Seal)),
        "hybrid reachability must cross a seal boundary"
    );
    let artifacts = tempfile::tempdir().expect("hybrid artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("hybrid reachability program");
    assert!(
        outcome.violations.is_empty(),
        "hybrid reachability found violations: {:?}",
        outcome.violations
    );
    assert!(outcome.hybrid_searches > 0, "no hybrid operation executed");
    assert!(
        outcome.text_documents_ingested > 0,
        "hybrid reachability ingested no text through Store"
    );
    assert!(
        outcome.store_lexical_searches > 0,
        "hybrid reachability bypassed Store::search_lexical"
    );
    assert!(
        outcome.store_hybrid_searches > 0,
        "hybrid reachability bypassed Store::search_hybrid"
    );
    assert!(
        outcome.hybrid_sealed_vector_documents > 0,
        "hybrid operation reached no sealed vector documents"
    );
    assert!(
        outcome.hybrid_lexical_documents > 0,
        "hybrid operation reached no lexical content"
    );
}

#[test]
fn an_emitted_fts_probe_reaches_every_shipped_lexical_extra() {
    let artifacts = tempfile::tempdir().expect("FTS extras artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("FTS extras reachability program");
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    assert!(outcome.phrase_searches > 0, "phrase path was unreachable");
    assert!(outcome.prefix_searches > 0, "prefix path was unreachable");
    assert!(outcome.fuzzy_searches > 0, "fuzzy path was unreachable");
    assert!(
        outcome.phonetic_encodes > 0,
        "phonetic path was unreachable"
    );
    assert!(outcome.snippets_built > 0, "snippet path was unreachable");
}

#[test]
fn one_episode_records_successful_public_path_coverage() {
    let artifacts = tempfile::tempdir().expect("coverage artifacts");
    let outcome = adversarial::runner::run_program(0, FaultProfile::None, artifacts.path())
        .expect("coverage episode");
    assert!(
        outcome.coverage.count("op.open") > 0,
        "open was not registered"
    );
    assert!(
        outcome.coverage.count("store.hybrid_search") > 0,
        "Store hybrid path was not registered"
    );
    assert!(
        !outcome.coverage_bytes.is_empty(),
        "coverage artifact is empty"
    );
}

#[test]
fn twelve_seed_sweep_emits_every_typed_predicate() {
    let mut seen = std::collections::BTreeSet::new();
    for seed in 0..12 {
        for operation in Program::generate(seed).ops {
            if let Op::PredicateSearch { predicate, .. } = operation {
                seen.insert(predicate.key());
            }
        }
    }
    let expected = PredicateKind::ALL
        .into_iter()
        .map(PredicateKind::key)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(seen, expected);
}

#[test]
fn twelve_seed_sweep_emits_every_process_crash_boundary() {
    let seen = (0..12)
        .flat_map(|seed| Program::generate(seed).ops)
        .filter_map(|operation| match operation {
            Op::Crash { boundary, .. } => Some(boundary.key()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let expected = adversarial::program::CrashBoundary::ALL
        .into_iter()
        .map(adversarial::program::CrashBoundary::key)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(seen, expected);
}

#[test]
fn smoke() {
    let config = RunConfig::from_env().expect("valid adversarial run configuration");
    let root = config.artifacts.clone();
    let mut failures = Vec::new();
    let mut unfired = Vec::new();
    let mut coverage = CoverageRegistry::default();
    for profile in FaultProfile::DEFAULTS {
        for offset in 0..12 {
            let seed = config
                .start_seed
                .checked_add(offset)
                .expect("smoke seed range fits u64");
            let outcome =
                adversarial::runner::run_program_for(config.campaign, seed, profile, &root)
                    .unwrap_or_else(|error| {
                        panic!(
                            "campaign={} seed={seed} profile={}: {error}",
                            config.campaign.key(),
                            profile.key()
                        )
                    });
            println!(
                "ADV campaign={} seed={seed} profile={} ops={} faults={} scheduled_faults={} feature_faults={}/{} graph_searches={} filtered_searches={} filtered_graph_searches={} predicate_searches={} hybrid_searches={} hybrid_sealed_vector_documents={} hybrid_lexical_documents={} violations={}",
                config.campaign.key(),
                profile.key(),
                outcome.operations,
                outcome.faults_fired,
                outcome.scheduled_faults_fired,
                outcome.feature_faults_fired,
                outcome.feature_faults_scheduled,
                outcome.graph_searches,
                outcome.filtered_searches,
                outcome.filtered_graph_searches,
                outcome.predicate_searches,
                outcome.hybrid_searches,
                outcome.hybrid_sealed_vector_documents,
                outcome.hybrid_lexical_documents,
                outcome.violations.len()
            );
            if !matches!(
                profile,
                FaultProfile::None | FaultProfile::Crash | FaultProfile::Clock
            ) && outcome.scheduled_faults_fired != 1
            {
                unfired.push(format!("seed={seed} profile={}", profile.key()));
            }
            if !outcome.missing_feature_faults.is_empty() {
                unfired.push(format!(
                    "seed={seed} profile={} feature={:?}",
                    profile.key(),
                    outcome.missing_feature_faults
                ));
            }
            coverage.merge(&outcome.coverage);
            for violation in outcome.violations {
                println!("{}", violation.report_for(config.campaign));
                failures.push(violation);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "adversarial smoke found {} invariant violations",
        failures.len()
    );
    assert!(
        unfired.is_empty(),
        "adversarial smoke selected faults that did not fire: {unfired:?}"
    );
    let missing = missing_campaign_coverage(config.campaign, &coverage);
    assert!(
        missing.is_empty(),
        "adversarial smoke missed required coverage: {missing:?}"
    );
}

#[test]
#[ignore = "explicit seeded adversarial replay entry point"]
fn run() {
    let config = RunConfig::from_env().expect("valid adversarial run configuration");
    let outcome = adversarial::runner::run_program_for(
        config.campaign,
        config.seed,
        config.profile,
        &config.artifacts,
    )
    .expect("seeded adversarial run");
    for violation in &outcome.violations {
        println!("{}", violation.report_for(config.campaign));
    }
    assert!(outcome.violations.is_empty());
    assert!(
        outcome.missing_feature_faults.is_empty(),
        "scheduled feature faults did not fire: {:?}",
        outcome.missing_feature_faults
    );
}

#[test]
#[ignore = "explicit byte-identical artifact replay entry point"]
fn replay() {
    let config = RunConfig::from_env().expect("valid adversarial replay configuration");
    let expected = config
        .replay_directory
        .as_ref()
        .expect("ZE_ADV_REPLAY_DIR must name one run directory");
    let campaign = if std::env::var_os("ZE_ADV_CAMPAIGN").is_some() {
        config.campaign
    } else {
        adversarial::campaign::campaign_from_replay_metadata(expected)
            .expect("campaign from replay metadata")
    };
    let actual_root = tempfile::tempdir().expect("replay output root");
    let outcome = adversarial::runner::run_program_for(
        campaign,
        config.seed,
        config.profile,
        actual_root.path(),
    )
    .expect("replayed adversarial run");
    for (name, actual) in [
        ("program.jsonl", outcome.program_bytes),
        ("faults.jsonl", outcome.faults_bytes),
        ("violations.json", outcome.violations_bytes),
        ("coverage.json", outcome.coverage_bytes),
    ] {
        let expected_bytes = std::fs::read(expected.join(name))
            .unwrap_or_else(|error| panic!("read replay artifact {name}: {error}"));
        assert_eq!(actual, expected_bytes, "replay drifted for {name}");
    }
}

#[test]
#[ignore = "manual release campaign: at least eight hours and 10,000 episodes"]
fn campaign() {
    let test_mode = std::env::var("ZE_ADV_CAMPAIGN_TEST_MODE").as_deref() == Ok("1");
    let config = RunConfig::from_env().expect("valid adversarial campaign configuration");
    config
        .validate_campaign_thresholds(test_mode)
        .expect("valid adversarial campaign thresholds");
    let root = config.artifacts.clone();
    std::fs::create_dir_all(&root).expect("campaign artifact root");
    let started = Instant::now();
    let started_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_secs();
    let injected_failures = campaign_test_failures(test_mode);
    let mut seed = config.start_seed;
    let mut episodes = 0_u64;
    let mut operations = 0_u64;
    let mut faults_fired = 0_u64;
    let mut violations = 0_u64;
    let mut execution_errors = 0_u64;
    let mut panics = 0_u64;
    let mut unfired_scheduled_faults = 0_u64;
    let mut failures = Vec::<CampaignFailure>::new();
    let mut successful_artifacts = VecDeque::<PathBuf>::new();
    let mut coverage = CoverageRegistry::default();
    let required_duration = Duration::from_secs(config.minimum_seconds);

    while episodes < config.minimum_episodes || started.elapsed() < required_duration {
        let profile = campaign_profile(episodes);
        let injected = injected_failures.get(&seed).copied();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                injected != Some(CampaignInjectedFailure::Panic),
                "test-mode injected episode panic"
            );
            adversarial::runner::run_program_for(config.campaign, seed, profile, &root)
        }));
        let result = match result {
            Ok(result) => result,
            Err(payload) => {
                panics = panics.saturating_add(1);
                Err(format!("runner panicked: {}", panic_detail(&payload)))
            }
        };
        episodes = episodes.saturating_add(1);
        let failure = match result {
            Ok(_) if injected == Some(CampaignInjectedFailure::ExecutionError) => {
                execution_errors = execution_errors.saturating_add(1);
                Some((
                    CampaignFailureKind::ExecutionError,
                    "test-mode injected runner error after artifact production".to_owned(),
                ))
            }
            Ok(outcome) => {
                operations = operations.saturating_add(outcome.operations as u64);
                faults_fired = faults_fired.saturating_add(outcome.faults_fired as u64);
                coverage.merge(&outcome.coverage);
                let episode_violations = (outcome.violations.len() as u64).saturating_add(
                    u64::from(injected == Some(CampaignInjectedFailure::Violation)),
                );
                violations = violations.saturating_add(episode_violations);
                let scheduled_missing = (!matches!(
                    profile,
                    FaultProfile::None | FaultProfile::Crash | FaultProfile::Clock
                ) && outcome.scheduled_faults_fired != 1)
                    || !outcome.missing_feature_faults.is_empty()
                    || injected == Some(CampaignInjectedFailure::UnfiredScheduledFault);
                unfired_scheduled_faults =
                    unfired_scheduled_faults.saturating_add(u64::from(scheduled_missing));
                match (episode_violations > 0, scheduled_missing) {
                    (false, false) => None,
                    (true, false) => Some((
                        CampaignFailureKind::InvariantViolation,
                        format!("{episode_violations} invariant violation(s)"),
                    )),
                    (false, true) => Some((
                        CampaignFailureKind::UnfiredScheduledFault,
                        "selected scheduled fault did not fire".to_owned(),
                    )),
                    (true, true) => Some((
                        CampaignFailureKind::ViolationAndUnfiredFault,
                        format!(
                            "{episode_violations} invariant violation(s) and the selected scheduled fault did not fire"
                        ),
                    )),
                }
            }
            Err(error) => {
                execution_errors = execution_errors.saturating_add(1);
                Some((CampaignFailureKind::ExecutionError, error))
            }
        };

        if let Some((kind, detail)) = failure {
            let artifact_directory =
                preserve_campaign_failure(&root, config.campaign, seed, profile, kind, &detail);
            println!(
                "ADV_CAMPAIGN_FAILURE seed={seed} profile={} campaign={} kind={} artifacts={}",
                profile.key(),
                config.campaign.key(),
                kind.key(),
                artifact_directory.display()
            );
            failures.push(CampaignFailure {
                seed,
                profile,
                kind,
                detail,
                artifact_directory,
            });
        } else {
            successful_artifacts.push_back(episode_artifact_directory(
                &root,
                config.campaign,
                seed,
                profile,
            ));
            while successful_artifacts.len() > config.retain_successful {
                let old_directory = successful_artifacts
                    .pop_front()
                    .expect("successful artifact queue is nonempty");
                if old_directory.is_dir() {
                    std::fs::remove_dir_all(&old_directory).unwrap_or_else(|error| {
                        panic!("rotate {}: {error}", old_directory.display())
                    });
                }
            }
        }

        write_campaign_summary(
            &root,
            &config,
            false,
            false,
            started_unix,
            started.elapsed(),
            episodes,
            operations,
            faults_fired,
            seed,
            &coverage,
            violations,
            execution_errors,
            panics,
            unfired_scheduled_faults,
            &failures,
        );
        if episodes.is_multiple_of(100) {
            println!(
                "ADV_CAMPAIGN episodes={episodes} elapsed_seconds={:.1} seed={seed} profile={} operations={operations} faults={faults_fired} failed_episodes={}",
                started.elapsed().as_secs_f64(),
                profile.key(),
                failures.len()
            );
        }
        seed = seed.checked_add(1).expect("campaign seed exhausted u64");
    }

    let missing = missing_campaign_coverage(config.campaign, &coverage);
    let run_passed = failures.is_empty();
    let qualification_passed = run_passed
        && (config.campaign == CampaignKind::Overall
            && config.qualification == Qualification::Exploratory
            || missing.is_empty());
    write_campaign_summary(
        &root,
        &config,
        true,
        qualification_passed,
        started_unix,
        started.elapsed(),
        episodes,
        operations,
        faults_fired,
        seed.saturating_sub(1),
        &coverage,
        violations,
        execution_errors,
        panics,
        unfired_scheduled_faults,
        &failures,
    );
    validate_completed_campaign_summary(&root, &config);
    println!(
        "ADV_CAMPAIGN_COMPLETE episodes={episodes} qualification={} campaign={} elapsed_seconds={:.1} operations={operations} faults={faults_fired} failed_episodes={}",
        if qualification_passed {
            "passed"
        } else {
            "failed"
        },
        config.campaign.key(),
        started.elapsed().as_secs_f64(),
        failures.len()
    );
    assert!(
        qualification_passed,
        "campaign completed both thresholds but failed qualification: mode={} failed_episodes={} missing_coverage={missing:?}",
        config.qualification.key(),
        failures.len()
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CampaignInjectedFailure {
    Violation,
    ExecutionError,
    UnfiredScheduledFault,
    Panic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CampaignFailureKind {
    InvariantViolation,
    ExecutionError,
    UnfiredScheduledFault,
    ViolationAndUnfiredFault,
}

impl CampaignFailureKind {
    const fn key(self) -> &'static str {
        match self {
            Self::InvariantViolation => "invariant_violation",
            Self::ExecutionError => "execution_error",
            Self::UnfiredScheduledFault => "unfired_scheduled_fault",
            Self::ViolationAndUnfiredFault => "invariant_violation_and_unfired_scheduled_fault",
        }
    }
}

#[derive(Debug)]
struct CampaignFailure {
    seed: u64,
    profile: FaultProfile,
    kind: CampaignFailureKind,
    detail: String,
    artifact_directory: PathBuf,
}

impl CampaignFailure {
    fn json(&self) -> zeppelin_embed_bench::harness_json::Value {
        zeppelin_embed_bench::harness_json::json!({
            "seed": self.seed,
            "profile": self.profile.key(),
            "kind": self.kind.key(),
            "detail": self.detail,
            "artifact_directory": self.artifact_directory.display().to_string(),
        })
    }
}

#[test]
fn campaign_records_failed_seeds_continues_and_fails_qualification_at_the_end() {
    let artifacts = tempfile::tempdir().expect("campaign failure artifacts");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "89")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", "0")
        .env(
            "ZE_ADV_CAMPAIGN_TEST_FAILURES",
            "84:violation,85:error,86:unfired,87:panic",
        )
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run campaign failure probe");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "a campaign with failed seeds must fail qualification after it finishes: {transcript}"
    );

    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json"))
                .expect("completed campaign summary"),
        )
        .expect("valid campaign summary JSON");
    assert_eq!(summary["complete"], true, "{transcript}");
    assert_eq!(summary["qualification_passed"], false, "{transcript}");
    assert_eq!(
        summary["episodes"], 89,
        "campaign stopped at the first failure"
    );
    assert_eq!(summary["failed_episodes"], 4);
    assert_eq!(summary["execution_errors"], 2);
    assert_eq!(summary["unfired_scheduled_faults"], 1);
    assert_eq!(summary["violations"], 1);
    assert_eq!(
        summary["failures"]
            .as_array()
            .expect("failure ledger")
            .iter()
            .map(|failure| failure["seed"].as_u64().expect("failure seed"))
            .collect::<Vec<_>>(),
        vec![84, 85, 86, 87]
    );

    for seed in 84..=87 {
        let directory = artifacts
            .path()
            .join("failures")
            .join(format!("seed-{seed}-none"));
        for name in [
            "program.jsonl",
            "faults.jsonl",
            "violations.json",
            "coverage.json",
            "repro.txt",
            "failure.json",
        ] {
            assert!(
                directory.join(name).is_file(),
                "failed seed {seed} lost {name}: {transcript}"
            );
        }
    }
    assert!(
        artifacts.path().join("seed-88-none").is_dir(),
        "the campaign did not continue through the canonical matrix: {transcript}"
    );
    assert!(transcript.contains("ADV_CAMPAIGN_FAILURE seed=84 profile=none"));
    assert!(transcript.contains("ADV_CAMPAIGN_FAILURE seed=85 profile=none"));
    assert!(transcript.contains("ADV_CAMPAIGN_FAILURE seed=86 profile=none"));
    assert!(transcript.contains("ADV_CAMPAIGN_FAILURE seed=87 profile=none"));
    assert!(transcript.contains("ADV_CAMPAIGN_COMPLETE episodes=89 qualification=failed"));
}

#[test]
fn exploratory_feature_campaign_reports_missing_coverage_and_fails_qualification() {
    let artifacts = tempfile::tempdir().expect("exploratory campaign artifacts");
    let clean_seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(CampaignKind::Fts, *seed);
            FaultPlan::for_program(CampaignKind::Fts, *seed, FaultProfile::None, &program, None)
                .feature
                .is_empty()
        })
        .expect("FTS campaign clean slot");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", "fts")
        .env("ZE_ADV_QUALIFICATION", "exploratory")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "1")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", clean_seed.to_string())
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run exploratory campaign probe");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{transcript}");
    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json"))
                .expect("exploratory summary"),
        )
        .expect("valid exploratory summary");
    assert_eq!(summary["version"], 3);
    assert_eq!(summary["campaign"], "fts");
    assert_eq!(summary["qualification"], "exploratory");
    assert_eq!(summary["verdict"], "failed");
    assert_eq!(summary["run_verdict"], "failed");
    assert_eq!(summary["qualification_passed"], false);
    assert!(summary["violations"].as_u64().unwrap() > 0);
    assert!(summary["host"]["os"].is_string());
    assert!(summary["host"]["arch"].is_string());
    assert!(summary["required_operations"].is_array());
    assert!(summary["executed_operations"].is_array());
    assert!(summary["missing_operations"].is_array());
    assert!(summary["required_profiles"].is_array());
    assert!(summary["executed_profiles"].is_array());
    assert!(summary["missing_profiles"].is_array());
    assert!(summary["required_generic_faults"].is_array());
    assert!(summary["fired_generic_faults"].is_array());
    assert!(summary["missing_generic_faults"].is_array());
    assert!(summary["languages"]["required"].is_array());
    assert!(summary["backends"]["required"].is_array());
    assert_eq!(summary["panics"], 0);
    assert_eq!(summary["counters"]["panics"], 0);
    assert!(!summary["missing_coverage"].as_array().unwrap().is_empty());
    assert!(
        !summary["required_invariants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!summary["checked_invariants"].as_array().unwrap().is_empty());
    assert!(summary["missing_invariants"].is_array());
    assert!(
        !summary["required_feature_faults"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !summary["missing_feature_faults"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn release_feature_campaign_refuses_unimplemented_oracles() {
    let artifacts = tempfile::tempdir().expect("release campaign artifacts");
    let clean_seed = (0..12)
        .find(|seed| {
            let program = Program::generate_for(CampaignKind::Fts, *seed);
            FaultPlan::for_program(CampaignKind::Fts, *seed, FaultProfile::None, &program, None)
                .feature
                .is_empty()
        })
        .expect("FTS campaign clean slot");
    let output = std::process::Command::new(std::env::current_exe().expect("campaign test binary"))
        .args(["campaign", "--ignored", "--exact", "--nocapture"])
        .env("ZE_ADV_CAMPAIGN_TEST_MODE", "1")
        .env("ZE_ADV_CAMPAIGN", "fts")
        .env("ZE_ADV_QUALIFICATION", "release")
        .env("ZE_ADV_MIN_SECONDS", "0")
        .env("ZE_ADV_MIN_EPISODES", "1")
        .env("ZE_ADV_RETAIN_SUCCESSFUL", "1")
        .env("ZE_ADV_CAMPAIGN_START_SEED", clean_seed.to_string())
        .env("ZE_ADV_ARTIFACTS", artifacts.path())
        .output()
        .expect("run release coverage probe");
    assert!(!output.status.success());
    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(artifacts.path().join("campaign-summary.json"))
                .expect("release summary"),
        )
        .expect("valid release summary");
    assert_eq!(summary["run_verdict"], "failed");
    assert_eq!(summary["qualification_passed"], false);
    assert!(summary["violations"].as_u64().unwrap() > 0);
    assert!(!summary["missing_coverage"].as_array().unwrap().is_empty());
}

#[test]
#[ignore = "helper subprocess intentionally aborts after a durable acknowledgement"]
fn crash_child() {
    if let Err(error) = adversarial::runner::crash_child_from_env() {
        panic!("crash child setup failed: {error}");
    }
}

fn missing_campaign_coverage(campaign: CampaignKind, coverage: &CoverageRegistry) -> Vec<String> {
    if campaign == CampaignKind::Overall {
        coverage
            .missing_required_smoke()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        let mut required = CampaignSpec::for_kind(campaign)
            .all_required_coverage()
            .into_iter();
        let generic = adversarial::coverage::REQUIRED_SMOKE_COVERAGE
            .iter()
            .filter(|key| {
                key.starts_with("fault.profile.")
                    || key.starts_with("fault.site.")
                    || key.starts_with("fault.mode.")
            })
            .map(|key| (*key).to_owned());
        let mut missing = required
            .by_ref()
            .chain(generic)
            .filter(|key| coverage.count(key) == 0)
            .collect::<Vec<_>>();
        if campaign == CampaignKind::FfiBindings {
            missing.extend(
                ["rust", "c", "python", "swift"]
                    .into_iter()
                    .map(|language| format!("binding.language.{language}"))
                    .filter(|key| coverage.count(key) == 0),
            );
        }
        if campaign == CampaignKind::VectorExecution {
            missing.extend(
                zeppelin_embed::kernels::KernelVariant::available()
                    .map(|variant| {
                        format!(
                            "kernel.backend.{}",
                            format!("{:?}", variant.arm()).to_ascii_lowercase()
                        )
                    })
                    .filter(|key| coverage.count(key) == 0),
            );
        }
        missing.sort();
        missing.dedup();
        missing
    }
}

fn campaign_profile(episode: u64) -> FaultProfile {
    FaultProfile::DEFAULTS[((episode / 12) as usize) % FaultProfile::DEFAULTS.len()]
}

fn campaign_test_failures(test_mode: bool) -> BTreeMap<u64, CampaignInjectedFailure> {
    let Ok(specification) = std::env::var("ZE_ADV_CAMPAIGN_TEST_FAILURES") else {
        return BTreeMap::new();
    };
    assert!(
        test_mode,
        "ZE_ADV_CAMPAIGN_TEST_FAILURES is available only in explicit campaign test mode"
    );
    let mut failures = BTreeMap::new();
    for entry in specification.split(',').filter(|entry| !entry.is_empty()) {
        let (seed, kind) = entry
            .split_once(':')
            .unwrap_or_else(|| panic!("invalid campaign test failure: {entry}"));
        let seed = seed
            .parse::<u64>()
            .unwrap_or_else(|error| panic!("invalid campaign test seed {seed}: {error}"));
        let kind = match kind {
            "violation" => CampaignInjectedFailure::Violation,
            "error" => CampaignInjectedFailure::ExecutionError,
            "unfired" => CampaignInjectedFailure::UnfiredScheduledFault,
            "panic" => CampaignInjectedFailure::Panic,
            _ => panic!("invalid campaign test failure kind: {kind}"),
        };
        assert!(
            failures.insert(seed, kind).is_none(),
            "duplicate campaign test failure seed: {seed}"
        );
    }
    failures
}

fn panic_detail(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string panic payload".to_owned())
        },
        |message| (*message).to_owned(),
    )
}

fn episode_artifact_directory(
    root: &Path,
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
) -> PathBuf {
    let root = if campaign == CampaignKind::Overall {
        root.to_path_buf()
    } else {
        root.join(campaign.key())
    };
    root.join(format!("seed-{seed}-{}", profile.key()))
}

fn preserve_campaign_failure(
    root: &Path,
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    kind: CampaignFailureKind,
    detail: &str,
) -> PathBuf {
    let source = episode_artifact_directory(root, campaign, seed, profile);
    let failures_root = if campaign == CampaignKind::Overall {
        root.join("failures")
    } else {
        root.join("failures").join(campaign.key())
    };
    std::fs::create_dir_all(&failures_root).expect("create campaign failures directory");
    let destination = failures_root.join(format!("seed-{seed}-{}", profile.key()));
    assert!(
        !destination.exists(),
        "campaign failure artifact already exists: {}",
        destination.display()
    );
    if source.is_dir() {
        std::fs::rename(&source, &destination).unwrap_or_else(|error| {
            panic!(
                "preserve campaign failure {} as {}: {error}",
                source.display(),
                destination.display()
            )
        });
    } else {
        std::fs::create_dir_all(&destination).expect("create failed episode artifact directory");
    }

    let mut partial_artifacts = false;
    partial_artifacts |= ensure_failure_artifact(
        &destination.join("program.jsonl"),
        &Program::generate_for(campaign, seed).jsonl(),
    );
    partial_artifacts |= ensure_failure_artifact(&destination.join("faults.jsonl"), b"");
    partial_artifacts |= ensure_failure_artifact(&destination.join("violations.json"), b"[]\n");
    partial_artifacts |= ensure_failure_artifact(&destination.join("coverage.json"), b"{}\n");
    partial_artifacts |= ensure_failure_artifact(
        &destination.join("repro.txt"),
        format!(
            "{}\n",
            adversarial::runner::reproduction_for(campaign, seed, profile)
        )
        .as_bytes(),
    );
    partial_artifacts |= ensure_failure_artifact(
        &destination.join("episode.json"),
        &zeppelin_embed_bench::harness_json::to_vec_pretty(
            &zeppelin_embed_bench::harness_json::json!({
                "schema": "zeppelin-embed-adversarial-episode",
                "version": 3,
                "campaign": campaign.key(),
                "seed": seed,
                "profile": profile.key(),
            }),
        )
        .expect("serialize fallback episode metadata"),
    );
    let failure = zeppelin_embed_bench::harness_json::json!({
        "schema": "zeppelin-embed-adversarial-episode-failure",
        "version": 2,
        "campaign": campaign.key(),
        "seed": seed,
        "profile": profile.key(),
        "kind": kind.key(),
        "detail": detail,
        "partial_artifacts": partial_artifacts,
        "reproduce": adversarial::runner::reproduction_for(campaign, seed, profile),
    });
    write_file_synced(
        &destination.join("failure.json"),
        &zeppelin_embed_bench::harness_json::to_vec_pretty(&failure)
            .expect("serialize campaign failure"),
    );
    File::open(&failures_root)
        .and_then(|directory| directory.sync_all())
        .expect("sync campaign failures directory");
    destination
}

fn ensure_failure_artifact(path: &Path, fallback: &[u8]) -> bool {
    if path.is_file() {
        false
    } else {
        write_file_synced(path, fallback);
        true
    }
}

fn write_file_synced(path: &Path, bytes: &[u8]) {
    let mut file = File::create(path)
        .unwrap_or_else(|error| panic!("create campaign artifact {}: {error}", path.display()));
    file.write_all(bytes)
        .unwrap_or_else(|error| panic!("write campaign artifact {}: {error}", path.display()));
    file.sync_all()
        .unwrap_or_else(|error| panic!("sync campaign artifact {}: {error}", path.display()));
}

#[allow(clippy::too_many_arguments)]
fn write_campaign_summary(
    root: &Path,
    config: &RunConfig,
    complete: bool,
    qualification_passed: bool,
    started_unix: u64,
    elapsed: Duration,
    episodes: u64,
    operations: u64,
    faults_fired: u64,
    last_seed: u64,
    coverage: &CoverageRegistry,
    violations: u64,
    execution_errors: u64,
    panics: u64,
    unfired_scheduled_faults: u64,
    failures: &[CampaignFailure],
) {
    let spec = CampaignSpec::for_kind(config.campaign);
    let required_invariants = spec
        .required_invariants()
        .into_iter()
        .map(|invariant| invariant.key())
        .collect::<Vec<_>>();
    let checked_invariants = spec
        .required_invariants()
        .into_iter()
        .filter(|invariant| campaign_invariant_checked(config.campaign, *invariant, coverage))
        .map(|invariant| invariant.key())
        .collect::<Vec<_>>();
    let missing_invariants = required_invariants
        .iter()
        .filter(|invariant| !checked_invariants.contains(invariant))
        .cloned()
        .collect::<Vec<_>>();
    let required_feature_faults = spec
        .feature_faults
        .iter()
        .map(|fault| fault.key())
        .collect::<Vec<_>>();
    let fired_feature_faults = spec
        .feature_faults
        .iter()
        .filter(|fault| coverage.count(&fault.coverage_key()) > 0)
        .map(|fault| fault.key())
        .collect::<Vec<_>>();
    let missing_feature_faults = required_feature_faults
        .iter()
        .filter(|fault| !fired_feature_faults.contains(fault))
        .copied()
        .collect::<Vec<_>>();
    let required_operations = spec.required_operations.to_vec();
    let executed_operations = spec
        .required_operations
        .iter()
        .copied()
        .filter(|operation| {
            coverage.count(&format!(
                "campaign.op.{}.{}",
                config.campaign.key(),
                operation
            )) > 0
        })
        .collect::<Vec<_>>();
    let missing_operations = required_operations
        .iter()
        .copied()
        .filter(|operation| !executed_operations.contains(operation))
        .collect::<Vec<_>>();
    let required_profiles = spec
        .fault_profiles
        .iter()
        .map(|profile| profile.key())
        .collect::<Vec<_>>();
    let executed_profiles = spec
        .fault_profiles
        .iter()
        .filter(|profile| coverage.count(&format!("fault.profile.{}", profile.key())) > 0)
        .map(|profile| profile.key())
        .collect::<Vec<_>>();
    let missing_profiles = required_profiles
        .iter()
        .copied()
        .filter(|profile| !executed_profiles.contains(profile))
        .collect::<Vec<_>>();
    let required_generic_faults = adversarial::coverage::REQUIRED_SMOKE_COVERAGE
        .iter()
        .copied()
        .filter(|key| key.starts_with("fault.site.") || key.starts_with("fault.mode."))
        .collect::<Vec<_>>();
    let fired_generic_faults = required_generic_faults
        .iter()
        .copied()
        .filter(|key| coverage.count(key) > 0)
        .collect::<Vec<_>>();
    let missing_generic_faults = required_generic_faults
        .iter()
        .copied()
        .filter(|key| !fired_generic_faults.contains(key))
        .collect::<Vec<_>>();
    let required_languages = if config.campaign == CampaignKind::FfiBindings {
        vec!["rust", "c", "python", "swift"]
    } else {
        Vec::new()
    };
    let observed_languages = required_languages
        .iter()
        .copied()
        .filter(|language| coverage.count(&format!("binding.language.{language}")) > 0)
        .collect::<Vec<_>>();
    let missing_languages = required_languages
        .iter()
        .copied()
        .filter(|language| !observed_languages.contains(language))
        .collect::<Vec<_>>();
    let required_backends = if config.campaign == CampaignKind::VectorExecution {
        let mut backends = zeppelin_embed::kernels::KernelVariant::available()
            .map(|variant| format!("{:?}", variant.arm()).to_ascii_lowercase())
            .collect::<Vec<_>>();
        backends.sort();
        backends.dedup();
        backends
    } else {
        Vec::new()
    };
    let observed_backends = required_backends
        .iter()
        .filter(|backend| coverage.count(&format!("kernel.backend.{backend}")) > 0)
        .cloned()
        .collect::<Vec<_>>();
    let missing_backends = required_backends
        .iter()
        .filter(|backend| !observed_backends.contains(backend))
        .cloned()
        .collect::<Vec<_>>();
    let missing_coverage = missing_campaign_coverage(config.campaign, coverage);
    let coverage_json: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_str(coverage.json().trim())
            .expect("coverage registry JSON");
    let run_verdict = if complete {
        if failures.is_empty() {
            "passed"
        } else {
            "failed"
        }
    } else {
        "running"
    };
    let summary = zeppelin_embed_bench::harness_json::json!({
        "schema": "zeppelin-embed-adversarial-campaign",
        "version": 3,
        "campaign": config.campaign.key(),
        "qualification": config.qualification.key(),
        "verdict": run_verdict,
        "run_verdict": run_verdict,
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "name": std::env::var("HOSTNAME").ok(),
        },
        "complete": complete,
        "qualification_passed": qualification_passed,
        "started_unix": started_unix,
        "elapsed_seconds": elapsed.as_secs_f64(),
        "required_seconds": config.minimum_seconds,
        "required_episodes": config.minimum_episodes,
        "episodes": episodes,
        "operations": operations,
        "faults_fired": faults_fired,
        "failed_episodes": failures.len(),
        "violations": violations,
        "execution_errors": execution_errors,
        "panics": panics,
        "unfired_scheduled_faults": unfired_scheduled_faults,
        "last_seed": last_seed,
        "required_invariants": required_invariants,
        "checked_invariants": checked_invariants,
        "missing_invariants": missing_invariants,
        "required_feature_faults": required_feature_faults,
        "fired_feature_faults": fired_feature_faults,
        "missing_feature_faults": missing_feature_faults,
        "required_operations": required_operations,
        "executed_operations": executed_operations,
        "missing_operations": missing_operations,
        "required_profiles": required_profiles,
        "executed_profiles": executed_profiles,
        "missing_profiles": missing_profiles,
        "required_generic_faults": required_generic_faults,
        "fired_generic_faults": fired_generic_faults,
        "missing_generic_faults": missing_generic_faults,
        "languages": {
            "required": required_languages,
            "observed": observed_languages,
            "missing": missing_languages,
        },
        "backends": {
            "required": required_backends,
            "observed": observed_backends,
            "missing": missing_backends,
        },
        "counters": {
            "episodes": episodes,
            "operations": operations,
            "faults_fired": faults_fired,
            "violations": violations,
            "execution_errors": execution_errors,
            "panics": panics,
            "unfired_scheduled_faults": unfired_scheduled_faults,
        },
        "missing_coverage": missing_coverage,
        "failures": failures.iter().map(CampaignFailure::json).collect::<Vec<_>>(),
        "coverage": coverage_json,
    });
    let mut summary = zeppelin_embed_bench::harness_json::to_vec(&summary)
        .expect("serialize campaign summary v3");
    summary.push(b'\n');
    let final_path = root.join("campaign-summary.json");
    let temporary_path = root.join(".campaign-summary.json.tmp");
    let mut temporary = File::create(&temporary_path).expect("create campaign checkpoint");
    temporary
        .write_all(&summary)
        .expect("write campaign checkpoint");
    temporary.sync_all().expect("sync campaign checkpoint");
    std::fs::rename(&temporary_path, &final_path).expect("publish campaign checkpoint");
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .expect("sync campaign checkpoint directory");
}

fn validate_completed_campaign_summary(root: &Path, config: &RunConfig) {
    let bytes =
        std::fs::read(root.join("campaign-summary.json")).expect("read final campaign summary");
    let summary: zeppelin_embed_bench::harness_json::Value =
        zeppelin_embed_bench::harness_json::from_slice(&bytes)
            .expect("parse final campaign summary");
    assert_eq!(summary["version"], 3, "campaign summary schema version");
    assert_eq!(summary["campaign"], config.campaign.key());
    assert_eq!(summary["qualification"], config.qualification.key());
    assert_eq!(summary["complete"], true, "campaign summary is incomplete");
    let failures = summary["failures"]
        .as_array()
        .expect("campaign summary failure ledger");
    assert_eq!(
        summary["failed_episodes"].as_u64(),
        Some(failures.len() as u64),
        "campaign failure count drifted from its ledger"
    );
    let run_passed = failures.is_empty()
        && summary["violations"].as_u64() == Some(0)
        && summary["execution_errors"].as_u64() == Some(0)
        && summary["unfired_scheduled_faults"].as_u64() == Some(0);
    let coverage_complete = summary["missing_coverage"]
        == zeppelin_embed_bench::harness_json::json!([])
        && summary["missing_invariants"] == zeppelin_embed_bench::harness_json::json!([])
        && summary["missing_feature_faults"] == zeppelin_embed_bench::harness_json::json!([]);
    let expected_qualification = run_passed
        && (config.campaign == CampaignKind::Overall
            && config.qualification == Qualification::Exploratory
            || coverage_complete);
    assert_eq!(
        summary["qualification_passed"].as_bool(),
        Some(expected_qualification),
        "campaign qualification disagrees with its evidence"
    );
    assert!(
        summary["elapsed_seconds"].as_f64().unwrap_or_default() >= config.minimum_seconds as f64,
        "campaign summary did not meet its duration"
    );
    assert!(
        summary["episodes"].as_u64().unwrap_or_default() >= config.minimum_episodes,
        "campaign summary did not meet its episode count"
    );
}

fn campaign_invariant_checked(
    campaign: CampaignKind,
    invariant: InvariantId,
    coverage: &CoverageRegistry,
) -> bool {
    if coverage.count(&invariant.checked_coverage_key()) > 0 {
        return true;
    }
    if campaign != CampaignKind::Overall {
        return false;
    }
    let evidence = match invariant.number() {
        1 | 2 | 3 | 11 => "op.search",
        4 => "op.crash",
        5 => "op.filtered_search",
        6 => "op.stats",
        7 => "fault.profile.content",
        8 => "op.reopen",
        9 => "op.ingest",
        10 => "op.purge",
        12 => "op.epoch_mismatch_probe",
        13 => "op.predicate_search",
        14 => "op.switch_alias_to_b",
        _ => return false,
    };
    coverage.count(evidence) > 0
}
