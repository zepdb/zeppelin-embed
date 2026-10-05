//! Shared public read execution and duplicate-preserving typed comparison.
#![allow(dead_code)]
use crate::graph::Graph;
use crate::tck;
use crate::tck::{Expect, V};
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::query::completed::{CompletedGraphResult, Outcome};
use zeppelin_embed::property_graph::query::plan::ParameterBinding;
use zeppelin_embed::property_graph::query::{QueryList, QueryValue, QueryView, ValueContext};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
use zeppelin_embed_cypher::{ErrorKind, StatementError};

/// Runs `query` with scalar or scalar-list parameters built from `V`.
pub(crate) fn run_with(
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

/// One positive scenario; `Err` describes the first mismatch.
pub(crate) fn check_positive(scenario: &tck::Scenario) -> Result<usize, String> {
    let Expect::Table { .. } = &scenario.expect else {
        unreachable!()
    };
    let mut graph = Graph::new("ze56-tck");
    for statement in &scenario.setup {
        graph.setup(statement);
    }
    let before = generation(&graph);
    let mut observation = String::new();
    for pass in 0..2 {
        if pass == 1 {
            graph.reopen();
        }
        let result = run_with(&graph, &scenario.query, &scenario.parameters)
            .map_err(|error| format!("query failed: {error}"))?;
        if result.metadata().outcome != Outcome::Read || result.metadata().generation != before {
            return Err(format!(
                "side effect: outcome {:?} at {:?}, before {before:?}",
                result.metadata().outcome,
                result.metadata().generation
            ));
        }
        compare_result(&scenario.expect, &result)?;
        observation = format!(
            "metadata={:?};table={:?};reopened={}",
            result.metadata(),
            tck::actual_table(&result),
            pass == 1
        );
        if generation(&graph) != before {
            return Err("generation moved after the read".to_owned());
        }
    }
    tck::emit_receipt(scenario, &observation);
    Ok(2)
}

pub(crate) use crate::tck::compare_result;

/// One unchanged original query, or an explicitly local profile refusal.
/// Rejections with no original setup in the execution fixture use a local
/// sentinel graph; the source verifier keeps the original expectation separate.
pub(crate) fn run_scenario(scenario: &tck::Scenario, write: bool) {
    match &scenario.expect {
        Expect::RejectProfile | Expect::CompileError(_) => {
            let mut graph = Graph::new("ze59-refusal");
            for setup in &scenario.setup {
                graph.setup(setup);
            }
            if matches!(scenario.expect, Expect::RejectProfile) && scenario.setup.is_empty() {
                graph.setup("CREATE (:Sentinel {v: 11})-[:R {v: 12}]->(:Sentinel {v: 13})");
            }
            let before = graph.snapshot().unwrap();
            let generation = graph.generation().unwrap();
            let error = run_with(&graph, &scenario.query, &scenario.parameters)
                .map(|_| ())
                .unwrap_err();
            let StatementError::Compile(error) = error else {
                panic!("{}: {error}", scenario.coordinate)
            };
            let expected = match &scenario.expect {
                Expect::CompileError(original) if original == "SyntaxError InvalidParameterUse" => {
                    ErrorKind::InvalidParameterUse
                }
                Expect::CompileError(original)
                    if original == "SyntaxError RelationshipUniquenessViolation" =>
                {
                    ErrorKind::RelationshipUniqueness
                }
                Expect::RejectProfile => {
                    let unbounded = [
                        "clauses/match/Match4.feature [2]",
                        "clauses/match/Match4.feature [5]",
                        "clauses/match/Match4.feature [8]",
                        "clauses/match/Match7.feature [12]",
                    ];
                    if unbounded.contains(&scenario.coordinate.as_str()) {
                        ErrorKind::InvalidRange
                    } else {
                        ErrorKind::Unsupported
                    }
                }
                other => panic!("unmapped error {other:?}"),
            };
            assert_eq!(error.kind, expected, "{}", scenario.coordinate);
            assert!(matches!(
                error.kind,
                ErrorKind::Unsupported
                    | ErrorKind::InvalidRange
                    | ErrorKind::InvalidParameterUse
                    | ErrorKind::RelationshipUniqueness
            ));
            assert_eq!(graph.snapshot().unwrap(), before);
            assert_eq!(graph.generation().unwrap(), generation);
            graph.reopen();
            assert_eq!(graph.snapshot().unwrap(), before);
            assert_eq!(graph.generation().unwrap(), generation);
            emit_receipt(
                scenario,
                &format!("compile;{:?};no-effects;reopened", error.kind),
            );
        }
        Expect::LocalExample(value) => {
            let graph = Graph::new("ze59-outline-observation");
            let result = graph.run(&scenario.query, &[]).unwrap();
            assert_eq!(
                tck::actual_table(&result).1,
                vec![vec![value.clone().unwrap()]]
            );
            emit_receipt(scenario, "local-observation;not-original-outline-pass");
        }
        _ if write => crate::graph::check_write(scenario).unwrap(),
        Expect::Table { .. } => {
            check_positive(scenario).unwrap();
        }
        other => panic!("unexpected read expectation {other:?}"),
    }
}

pub(crate) use crate::tck::emit_receipt;
