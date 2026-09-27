//! ZE-57 S1-S5: original write TCK, public execution and persisted snapshots.
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

use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind;
use zeppelin_embed_cypher::{ErrorKind, StatementError};

const FIXTURE: &str = include_str!("fixtures/write-tck-execution.txt");

type Group = &'static [(&'static str, &'static [u8])];
const CREATE: Group = &[("create/Create1", &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])];
const SET: Group = &[
    ("set/Set2", &[1, 2, 3]),
    ("set/Set3", &[1, 2, 3, 4, 5, 6, 7, 8]),
    ("set/Set6", &[5]),
];
const REMOVE: Group = &[
    ("remove/Remove1", &[1, 3, 5, 6]),
    ("remove/Remove2", &[1, 2, 3, 4, 5]),
    ("remove/Remove3", &[1, 15]),
];
const PLAIN_DELETE: Group = &[("delete/Delete1", &[1, 4, 5, 7])];
const DETACH_DELETE: Group = &[("delete/Delete1", &[2, 3, 6])];
const REJECTED: Group = &[("remove/Remove1", &[2, 4, 7])];

fn claims(group: Group, coordinate: &str) -> bool {
    group.iter().any(|(feature, numbers)| {
        numbers
            .iter()
            .any(|n| coordinate == format!("clauses/{feature}.feature [{n}]"))
    })
}

fn selected(group: Group, count: usize) -> Vec<tck::Scenario> {
    let scenarios: Vec<_> = tck::scenarios(FIXTURE)
        .into_iter()
        .filter(|s| claims(group, &s.coordinate))
        .collect();
    assert_eq!(scenarios.len(), count);
    scenarios
}

#[test]
fn ze57_write_fixture_has_forty_two_blocks_and_every_coordinate_is_claimed() {
    let scenarios = tck::scenarios(FIXTURE);
    assert_eq!(scenarios.len(), 45);
    let coordinates: std::collections::BTreeSet<_> =
        scenarios.iter().map(|s| &s.coordinate).collect();
    assert_eq!(coordinates.len(), 45);
    let mut rejected = 0;
    for scenario in &scenarios {
        let owners = [CREATE, SET, REMOVE, PLAIN_DELETE, DETACH_DELETE, REJECTED]
            .into_iter()
            .filter(|group| claims(group, &scenario.coordinate))
            .count();
        assert_eq!(
            owners, 1,
            "{} must have exactly one execution owner",
            scenario.coordinate
        );
        let is_rejected = matches!(scenario.expect, tck::Expect::RejectProfile);
        assert_eq!(is_rejected, claims(REJECTED, &scenario.coordinate));
        rejected += usize::from(is_rejected);
    }
    assert_eq!(rejected, 3);
    assert_eq!(coordinates.len() - rejected, 42);
}

#[test]
fn ze57_original_create1_scenarios_execute() {
    let selected = selected(CREATE, 12);
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

#[test]
fn ze57_original_set_scenarios_execute() {
    let selected = selected(SET, 12);
    for scenario in selected {
        graph::check_write(&scenario).unwrap();
    }
}

#[test]
fn ze57_original_remove_scenarios_execute() {
    let selected = selected(REMOVE, 11);
    for scenario in selected {
        graph::check_write(&scenario).unwrap();
    }
}

#[test]
fn ze57_profile_rejected_remove_scenarios_stay_refused() {
    let mut seen = 0;
    for scenario in selected(REJECTED, 3) {
        assert!(matches!(scenario.expect, tck::Expect::RejectProfile));
        let graph = graph::Graph::new("ze57-remove-reject");
        let before = graph.generation().unwrap();
        match graph.run(&scenario.query, &[]) {
            Err(StatementError::Compile(error)) => {
                assert_eq!(
                    (error.kind, error.message),
                    (ErrorKind::Unsupported, "function outside profile"),
                    "{}",
                    scenario.coordinate
                );
            }
            Err(error) => panic!(
                "{}: expected compile refusal, got {error}",
                scenario.coordinate
            ),
            Ok(_) => panic!(
                "{}: expected compile refusal, executed",
                scenario.coordinate
            ),
        }
        assert_eq!(
            graph.generation().unwrap(),
            before,
            "{}",
            scenario.coordinate
        );
        seen += 1;
    }
    assert_eq!(seen, 3);
}

#[test]
fn ze57_original_plain_delete_scenarios_execute() {
    let selected = selected(PLAIN_DELETE, 4);
    for scenario in selected {
        graph::check_write(&scenario).unwrap();
        if let tck::Expect::RuntimeError { category, detail } = &scenario.expect {
            assert_eq!(
                (category.as_str(), detail.as_str()),
                ("ConstraintVerificationFailed", "DeleteConnectedNode")
            );
            let mut graph = graph::Graph::new("ze57-delete-connected");
            for setup in &scenario.setup {
                graph.setup(setup);
            }
            let before = graph.snapshot().unwrap();
            let generation = graph.generation().unwrap();
            assert_eq!(before.nodes.len(), 4);
            assert_eq!(before.rels.len(), 3);
            assert_eq!(
                before
                    .nodes
                    .values()
                    .filter(|(labels, _)| labels.iter().any(|label| label == "X"))
                    .count(),
                1
            );
            match graph.run(&scenario.query, &[]) {
                Err(StatementError::Query(error)) => {
                    assert_eq!(error.kind(), GraphQueryErrorKind::Constraint);
                    assert!(error.nothing_committed());
                }
                Err(error) => panic!(
                    "{}: expected query refusal, got {error}",
                    scenario.coordinate
                ),
                Ok(_) => panic!("{}: expected query refusal, executed", scenario.coordinate),
            }
            assert!(scenario.side_effects.is_empty());
            assert_eq!(graph.snapshot().unwrap(), before);
            assert_eq!(graph.generation().unwrap(), generation);
            graph.reopen();
            assert_eq!(graph.snapshot().unwrap(), before);
            assert_eq!(graph.generation().unwrap(), generation);
        }
    }
}

#[test]
fn ze57_original_detach_delete_scenarios_execute() {
    let selected = selected(DETACH_DELETE, 3);
    for scenario in selected {
        graph::check_write(&scenario).unwrap();
        if scenario.coordinate == "clauses/delete/Delete1.feature [3]" {
            // Pin surviving identities as well as the shared harness's exact side-effect diff.
            let mut graph = graph::Graph::new("ze57-detach-neighbours");
            for setup in &scenario.setup {
                graph.setup(setup);
            }
            let mut expected = graph.snapshot().unwrap();
            assert_eq!(expected.nodes.len(), 4);
            assert_eq!(expected.rels.len(), 3);
            expected
                .nodes
                .retain(|_, (labels, _)| !labels.iter().any(|label| label == "X"));
            assert_eq!(expected.nodes.len(), 3);
            expected.rels.clear();
            graph.setup(&scenario.query);
            assert_eq!(graph.snapshot().unwrap(), expected);
            graph.reopen();
            assert_eq!(graph.snapshot().unwrap(), expected);
        }
    }
}
