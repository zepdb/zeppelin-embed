//! ZE-56: original openCypher TCK read coordinates, executed from Cypher text
//! through the compiler and the GraphStore's structured statement seam on a
//! real persisted native graph store. Expected tables come verbatim from the
//! pinned feature files (tests/fixtures/read-tck-execution.txt).
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

#[path = "support/conformance.rs"]
mod conformance;
use conformance::{check_positive, run_with};
use graph::Graph;
use tck::Expect;
use zeppelin_embed_cypher::{ErrorKind, StatementError};
fn generation(graph: &Graph) -> zeppelin_embed::property_graph::GraphGeneration {
    graph.generation().unwrap()
}

const FIXTURE: &str = include_str!("fixtures/read-tck-execution.txt");

fn scenarios() -> Vec<tck::Scenario> {
    let all = tck::scenarios(FIXTURE);
    // 69 positive (including map examples), 3 compile errors, 12 profile
    // rejections and the previously accepted scalar outline observation.
    assert_eq!(all.len(), 85);
    all
}

#[test]
fn ze56_original_read_tck_positive_scenarios_execute() {
    let mut passed = 0;
    let mut selected = 0;
    let mut failures = Vec::new();
    for scenario in scenarios() {
        if !matches!(scenario.expect, Expect::Table { .. }) {
            continue;
        }
        selected += 1;
        match check_positive(&scenario) {
            Ok(validations) => {
                passed += validations;
            }
            Err(reason) => failures.push(format!("{}: {reason}", scenario.coordinate)),
        }
    }
    eprintln!(
        "ze56 original read TCK: {passed} passed, {} failed",
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(selected, 69);
    assert_eq!(passed, 138);
}

/// The three original compile-time errors are refused by the compiler with
/// their own kind, before any admission can run or change the store.
#[test]
fn ze56_original_read_tck_compile_errors_are_refused_before_execution() {
    let mut seen = 0;
    for scenario in scenarios() {
        let Expect::CompileError(original) = &scenario.expect else {
            continue;
        };
        let graph = Graph::new("ze56-tck-error");
        for statement in &scenario.setup {
            graph.setup(statement);
        }
        let before = generation(&graph);
        let error = match run_with(&graph, &scenario.query, &scenario.parameters) {
            Err(StatementError::Compile(error)) => error,
            Err(other) => panic!(
                "{}: expected compile error, got {other}",
                scenario.coordinate
            ),
            Ok(_) => panic!("{}: expected compile error, executed", scenario.coordinate),
        };
        // The parser refuses a parameter in pattern-property position with
        // the TCK's own category, before binding; the binder refuses reuse.
        let expected = match original.as_str() {
            "SyntaxError InvalidParameterUse" => (
                ErrorKind::InvalidParameterUse,
                "whole-map pattern parameter",
            ),
            "SyntaxError RelationshipUniquenessViolation" => (
                ErrorKind::RelationshipUniqueness,
                "relationship reused in one MATCH",
            ),
            other => panic!("unmapped original error {other}"),
        };
        assert_eq!(
            (error.kind, error.message),
            expected,
            "{}",
            scenario.coordinate
        );
        assert_eq!(generation(&graph), before, "{}", scenario.coordinate);
        tck::emit_receipt(&scenario, &format!("compile;{:?};no-effects", error.kind));
        seen += 1;
    }
    assert_eq!(seen, 3);
}

const UNBOUNDED: (ErrorKind, &str) = (
    ErrorKind::InvalidRange,
    "finite literal path bound required",
);
const NAMED_PATH: (ErrorKind, &str) = (
    ErrorKind::Unsupported,
    "only unnamed node/relationship patterns are supported",
);
const COMPOUND_AGGREGATE: (ErrorKind, &str) = (
    ErrorKind::Unsupported,
    "aggregate must be a top-level projection",
);
const UNWIND: (ErrorKind, &str) = (ErrorKind::Unsupported, "statement form outside profile");

/// Each rejected coordinate's exact profile category.
const PROFILE_REJECTIONS: [(&str, ErrorKind, &str); 12] = [
    ("clauses/match/Match4.feature [2]", UNBOUNDED.0, UNBOUNDED.1),
    ("clauses/match/Match4.feature [5]", UNBOUNDED.0, UNBOUNDED.1),
    (
        "clauses/match/Match4.feature [7]",
        NAMED_PATH.0,
        NAMED_PATH.1,
    ),
    ("clauses/match/Match4.feature [8]", UNBOUNDED.0, UNBOUNDED.1),
    (
        "clauses/match/Match7.feature [12]",
        UNBOUNDED.0,
        UNBOUNDED.1,
    ),
    (
        "clauses/match/Match7.feature [20]",
        NAMED_PATH.0,
        NAMED_PATH.1,
    ),
    ("clauses/with/With6.feature [4]", NAMED_PATH.0, NAMED_PATH.1),
    (
        "clauses/with/With6.feature [5]",
        COMPOUND_AGGREGATE.0,
        COMPOUND_AGGREGATE.1,
    ),
    (
        "clauses/with/With6.feature [6]",
        COMPOUND_AGGREGATE.0,
        COMPOUND_AGGREGATE.1,
    ),
    (
        "clauses/with/With6.feature [7]",
        COMPOUND_AGGREGATE.0,
        COMPOUND_AGGREGATE.1,
    ),
    (
        "expressions/aggregation/Aggregation8.feature [3]",
        UNWIND.0,
        UNWIND.1,
    ),
    (
        "expressions/aggregation/Aggregation8.feature [4]",
        UNWIND.0,
        UNWIND.1,
    ),
];

/// Unbounded, named-path, UNWIND and compound-aggregate scenarios stay
/// outside the accepted profile: refused by the compiler, never executed.
#[test]
fn ze56_profile_rejected_read_scenarios_stay_refused() {
    let mut seen = 0;
    for scenario in scenarios() {
        if !matches!(scenario.expect, Expect::RejectProfile) {
            continue;
        }
        let graph = Graph::new("ze56-tck-reject");
        let before = generation(&graph);
        match graph.run(&scenario.query, &[]) {
            Err(StatementError::Compile(error)) => {
                let expected = PROFILE_REJECTIONS
                    .iter()
                    .find(|(coordinate, ..)| scenario.coordinate.starts_with(coordinate))
                    .map(|(_, kind, message)| (*kind, *message));
                assert_eq!(
                    Some((error.kind, error.message)),
                    expected,
                    "{}",
                    scenario.coordinate
                );
            }
            Err(other) => panic!(
                "{}: expected profile refusal, got {other}",
                scenario.coordinate
            ),
            Ok(_) => panic!(
                "{}: expected profile refusal, executed",
                scenario.coordinate
            ),
        }
        assert_eq!(generation(&graph), before);
        seen += 1;
    }
    assert_eq!(seen, 12, "remaining unsupported profile scenarios");
}

/// Null1 [5]'s one scalar example row (`WITH null AS map`) is inside the
/// profile. It runs and returns the example's own result; this is a local
/// observation of a rejected outline, not an original TCK pass.
#[test]
fn ze56_rejected_outline_scalar_example_is_a_local_observation() {
    let mut seen = 0;
    for scenario in scenarios() {
        let Expect::LocalExample(expected) = &scenario.expect else {
            continue;
        };
        let graph = Graph::new("ze56-tck-local");
        let result = graph.run(&scenario.query, &[]).unwrap();
        let (_, rows) = tck::actual_table(&result);
        assert_eq!(
            rows,
            vec![vec![expected.clone().unwrap()]],
            "{}",
            scenario.coordinate
        );
        seen += 1;
    }
    assert_eq!(seen, 1);
}
