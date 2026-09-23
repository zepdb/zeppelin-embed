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

use graph::Graph;
use tck::{Expect, V};
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::query::completed::{CompletedGraphResult, Outcome};
use zeppelin_embed::property_graph::query::plan::ParameterBinding;
use zeppelin_embed::property_graph::query::{QueryList, QueryValue, QueryView, ValueContext};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
use zeppelin_embed_cypher::{ErrorKind, StatementError};

const FIXTURE: &str = include_str!("fixtures/read-tck-execution.txt");

fn scenarios() -> Vec<tck::Scenario> {
    let all = tck::scenarios(FIXTURE);
    // 54 positive, 3 compile errors, 16 rejected scenarios and the 12
    // example rows of the rejected Null1 [5] outline.
    assert_eq!(all.len(), 85);
    all
}

/// Runs `query` with scalar or scalar-list parameters built from `V`.
fn run_with(
    graph: &Graph,
    query: &str,
    parameters: &[(String, V)],
) -> Result<CompletedGraphResult, StatementError> {
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 1_000_000).unwrap();
    let scalars: Vec<Vec<QueryValue<'_>>> = parameters
        .iter()
        .map(|(_, value)| match value {
            V::List(items) => items.iter().map(scalar).collect(),
            _ => Vec::new(),
        })
        .collect();
    let bindings: Vec<ParameterBinding<'_>> = parameters
        .iter()
        .zip(&scalars)
        .map(|((name, value), items)| ParameterBinding {
            name,
            value: match value {
                V::List(_) => QueryValue::List(QueryList::new(items, &mut context).unwrap()),
                other => scalar(other),
            },
        })
        .collect();
    graph.run(query, &bindings)
}

fn scalar(value: &V) -> QueryValue<'_> {
    match value {
        V::Null => QueryValue::Null,
        V::Bool(value) => QueryValue::Bool(*value),
        V::Int(value) => QueryValue::I64(*value),
        V::Float(value) => QueryValue::F64(*value),
        V::Str(value) => QueryValue::String(value),
        other => panic!("unsupported fixture parameter {other:?}"),
    }
}

/// The current generation, observed by a read that cannot change it.
fn generation(graph: &Graph) -> GraphGeneration {
    let probe = graph.run("RETURN 1 AS probe", &[]).unwrap();
    assert_eq!(probe.metadata().outcome, Outcome::Read);
    probe.metadata().generation
}

fn canonical(rows: &[Vec<V>], unordered_lists: bool) -> Vec<String> {
    let mut rendered: Vec<String> = rows
        .iter()
        .map(|row| {
            let mut row = row.clone();
            if unordered_lists {
                row.iter_mut().for_each(V::sort_lists);
            }
            format!("{row:?}")
        })
        .collect();
    rendered.sort();
    rendered
}

/// One positive scenario; `Err` describes the first mismatch.
fn check_positive(scenario: &tck::Scenario) -> Result<(), String> {
    let Expect::Table { mode, header, rows } = &scenario.expect else {
        unreachable!()
    };
    let graph = Graph::new("ze56-tck");
    for statement in &scenario.setup {
        graph.setup(statement);
    }
    let before = generation(&graph);
    let result = run_with(&graph, &scenario.query, &scenario.parameters)
        .map_err(|error| format!("query failed: {error}"))?;
    if result.metadata().outcome != Outcome::Read || result.metadata().generation != before {
        return Err(format!(
            "side effect: outcome {:?} at {:?}, before {before:?}",
            result.metadata().outcome,
            result.metadata().generation
        ));
    }
    let (columns, actual) = tck::actual_table(&result);
    if &columns != header {
        return Err(format!("columns {columns:?}, expected {header:?}"));
    }
    let (expected, actual) = match mode.as_str() {
        "ordered" => (
            rows.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>(),
            actual.iter().map(|r| format!("{r:?}")).collect(),
        ),
        "bag" => (canonical(rows, false), canonical(&actual, false)),
        "bag-lists-unordered" => (canonical(rows, true), canonical(&actual, true)),
        other => panic!("mode {other}"),
    };
    if expected != actual {
        return Err(format!("rows {actual:#?}\nexpected {expected:#?}"));
    }
    if generation(&graph) != before {
        return Err("generation moved after the read".to_owned());
    }
    Ok(())
}

#[test]
fn ze56_original_read_tck_positive_scenarios_execute() {
    let mut passed = 0;
    let mut failures = Vec::new();
    for scenario in scenarios() {
        if !matches!(scenario.expect, Expect::Table { .. }) {
            continue;
        }
        match check_positive(&scenario) {
            Ok(()) => passed += 1,
            Err(reason) => failures.push(format!("{}: {reason}", scenario.coordinate)),
        }
    }
    eprintln!(
        "ze56 original read TCK: {passed} passed, {} failed",
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(passed, 54);
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
const FUNCTION: (ErrorKind, &str) = (ErrorKind::Unsupported, "function outside profile");
const COMPOUND_AGGREGATE: (ErrorKind, &str) = (
    ErrorKind::Unsupported,
    "aggregate must be a top-level projection",
);
const MAP: (ErrorKind, &str) = (ErrorKind::Unsupported, "map expression outside profile");
const UNWIND: (ErrorKind, &str) = (ErrorKind::Unsupported, "statement form outside profile");

/// Each rejected coordinate's exact profile category.
const PROFILE_REJECTIONS: [(&str, ErrorKind, &str); 17] = [
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
    ("clauses/with/With6.feature [5]", FUNCTION.0, FUNCTION.1),
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
    ("clauses/return/Return5.feature [1]", MAP.0, MAP.1),
    ("clauses/return/Return5.feature [3]", MAP.0, MAP.1),
    ("clauses/return/Return5.feature [4]", MAP.0, MAP.1),
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
    ("expressions/list/List1.feature [5]", FUNCTION.0, FUNCTION.1),
    // Every map-valued example row: a map literal is not a profile expression.
    ("expressions/null/Null1.feature [5] example", MAP.0, MAP.1),
];

/// Unbounded, named-path, UNWIND, map and unsupported-function scenarios stay
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
    assert_eq!(
        seen,
        16 + 11,
        "16 scenarios plus 11 map-valued outline rows"
    );
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
