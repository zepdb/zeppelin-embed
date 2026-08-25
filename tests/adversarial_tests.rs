#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod adversarial;

use std::path::PathBuf;

use adversarial::profiles::FaultProfile;
use adversarial::program::{Op, Program};
use adversarial::runner::{Invariant, SelfTestBug};

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
        outcome.hybrid_sealed_vector_documents > 0,
        "hybrid operation reached no sealed vector documents"
    );
    assert!(
        outcome.hybrid_lexical_documents > 0,
        "hybrid operation reached no lexical content"
    );
}

#[test]
fn smoke() {
    let root = artifact_root();
    let mut failures = Vec::new();
    for profile in FaultProfile::DEFAULTS {
        for seed in 0..12 {
            let outcome = adversarial::runner::run_program(seed, profile, &root)
                .unwrap_or_else(|error| panic!("seed={seed} profile={}: {error}", profile.key()));
            println!(
                "ADV seed={seed} profile={} ops={} faults={} graph_searches={} filtered_searches={} filtered_graph_searches={} hybrid_searches={} hybrid_sealed_vector_documents={} hybrid_lexical_documents={} violations={}",
                profile.key(),
                outcome.operations,
                outcome.faults_fired,
                outcome.graph_searches,
                outcome.filtered_searches,
                outcome.filtered_graph_searches,
                outcome.hybrid_searches,
                outcome.hybrid_sealed_vector_documents,
                outcome.hybrid_lexical_documents,
                outcome.violations.len()
            );
            for violation in outcome.violations {
                println!("{}", violation.report());
                failures.push(violation);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "adversarial smoke found {} invariant violations",
        failures.len()
    );
}

#[test]
#[ignore = "explicit seeded adversarial replay entry point"]
fn run() {
    let seed = std::env::var("ZE_ADV_SEED")
        .unwrap_or_else(|_| "0".to_owned())
        .parse::<u64>()
        .expect("ZE_ADV_SEED must be a u64");
    let profile = std::env::var("ZE_ADV_PROFILE")
        .map(|value| FaultProfile::from_env(&value))
        .unwrap_or(FaultProfile::None);
    let outcome = adversarial::runner::run_program(seed, profile, &artifact_root())
        .expect("seeded adversarial run");
    for violation in &outcome.violations {
        println!("{}", violation.report());
    }
    assert!(outcome.violations.is_empty());
}

#[test]
#[ignore = "helper subprocess intentionally aborts after a durable acknowledgement"]
fn crash_child() {
    if let Err(error) = adversarial::runner::crash_child_from_env() {
        panic!("crash child setup failed: {error}");
    }
}

fn artifact_root() -> PathBuf {
    std::env::var("ZE_ADV_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target/adversarial"))
}
