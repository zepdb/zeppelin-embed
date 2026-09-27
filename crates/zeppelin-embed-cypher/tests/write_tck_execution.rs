//! ZE-57 S1: original write TCK, public execution and persisted snapshots.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[path = "support/graph.rs"]
mod graph;
mod support;
#[path = "support/tck.rs"]
mod tck;

const FIXTURE: &str = include_str!("fixtures/write-tck-execution.txt");

#[test]
fn ze57_write_fixture_has_forty_two_blocks_and_every_coordinate_is_claimed() {
    let scenarios = tck::scenarios(FIXTURE);
    assert_eq!(scenarios.len(), 42);
    let coordinates: std::collections::BTreeSet<_> =
        scenarios.iter().map(|s| &s.coordinate).collect();
    assert_eq!(coordinates.len(), 42);
    // S1 claims Create1 only. Full execution ownership is completed in S5.
    for (name, count) in [
        ("Create1", 12),
        ("Set2", 3),
        ("Set3", 8),
        ("Set6", 1),
        ("Delete1", 7),
        ("Remove1", 4),
        ("Remove2", 5),
        ("Remove3", 2),
    ] {
        assert_eq!(
            scenarios
                .iter()
                .filter(|s| s.coordinate.contains(&format!("/{name}.feature ")))
                .count(),
            count,
            "{name}"
        );
    }
}

#[test]
fn ze57_original_create1_scenarios_execute() {
    let selected: Vec<_> = tck::scenarios(FIXTURE)
        .into_iter()
        .filter(|s| s.coordinate.contains("/Create1.feature "))
        .collect();
    assert_eq!(selected.len(), 12);
    let mut failures = Vec::new();
    let mut passed = 0;
    for scenario in selected {
        match graph::check_write(&scenario) {
            Ok(()) => passed += 1,
            Err(error) => failures.push(error),
        }
    }
    eprintln!(
        "ze57 original Create1: {passed} passed, {} failed",
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(passed, 12);
}
