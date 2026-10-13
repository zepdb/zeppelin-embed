//! ZE-51 S1: relational semantics through Cypher on a persisted native store.
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
use tck::V;
use zeppelin_embed::property_graph::query::{
    QueryValue,
    plan::ParameterBinding,
    runtime::{RuntimeLimits, WorkKind},
};
use zeppelin_embed_cypher::{ErrorKind, StatementError};

fn rows(graph: &Graph, text: &str) -> Vec<Vec<V>> {
    let started = std::time::Instant::now();
    let result = graph.run(text, &[]).unwrap();
    eprintln!("ZE51 execute {text:?} elapsed={:?}", started.elapsed());
    tck::actual_table(&result).1
}

fn bag(mut rows: Vec<Vec<V>>) -> Vec<String> {
    for row in &mut rows {
        row.iter_mut().for_each(V::sort_lists);
    }
    let mut values: Vec<_> = rows.iter().map(|row| format!("{row:?}")).collect();
    values.sort();
    values
}

#[test]
fn ze432_coalesce_returns_first_nonnull_and_skips_unused_arguments() {
    let graph = Graph::new("ze432-coalesce");
    assert_eq!(
        rows(
            &graph,
            "RETURN coalesce(null, null, 3, 1 / 0), coalesce(null), coalesce(null, null), coalesce(false, true), coalesce([], [1])"
        ),
        vec![vec![
            V::Int(3),
            V::Null,
            V::Null,
            V::Bool(false),
            V::List(vec![])
        ]]
    );
    let result = graph.run("RETURN coalesce(null, 3, 1 / 0)", &[]).unwrap();
    assert_eq!(result.metadata().counters.get(WorkKind::Expressions), 3);
    expression_limit(&graph, "RETURN coalesce(null, 3, 1 / 0)", 2);
    assert!(
        matches!(graph.run("RETURN coalesce()", &[]), Err(StatementError::Compile(error)) if error.kind == ErrorKind::Syntax)
    );
}

#[test]
fn ze432_case_simple_uses_equality_and_never_matches_null() {
    let graph = Graph::new("ze432-case-simple");
    assert_eq!(
        rows(
            &graph,
            "RETURN CASE 2 WHEN 1 THEN 1 / 0 WHEN 2.0 THEN 'yes' WHEN 2 THEN 'later' ELSE 1 / 0 END, CASE null WHEN null THEN 1 END, CASE 9 WHEN 1 THEN 1 ELSE 2 END"
        ),
        vec![vec![V::Str("yes".into()), V::Null, V::Int(2)]]
    );
    let result = graph
        .run("RETURN CASE 2 WHEN 1 THEN 9 WHEN 2 THEN 8 ELSE 7 END", &[])
        .unwrap();
    assert_eq!(result.metadata().counters.get(WorkKind::Expressions), 5);
    expression_limit(
        &graph,
        "RETURN CASE 2 WHEN 1 THEN 9 WHEN 2 THEN 8 ELSE 7 END",
        4,
    );
    graph.setup("CREATE (:Case {id: 1, v: 2}), (:Case {id: 2, v: 1})");
    assert_eq!(
        rows(
            &graph,
            "MATCH (n:Case) RETURN n.id ORDER BY CASE n.v WHEN 1 THEN 0 ELSE 1 END"
        ),
        vec![vec![V::Int(2)], vec![V::Int(1)]]
    );
}

#[test]
fn ze432_case_searched_selects_first_true_and_defaults_to_null() {
    let graph = Graph::new("ze432-case-searched");
    assert_eq!(
        rows(
            &graph,
            "RETURN CASE WHEN null THEN 1 / 0 WHEN false THEN 1 / 0 WHEN true THEN CASE WHEN true THEN 3 END ELSE 1 / 0 END + 2, CASE WHEN false THEN 1 END, CASE WHEN null THEN 1 ELSE 2 END"
        ),
        vec![vec![V::Int(5), V::Null, V::Int(2)]]
    );
    assert!(
        matches!(graph.run("RETURN CASE WHEN 1 THEN 2 END", &[]), Err(StatementError::Compile(error)) if error.kind == ErrorKind::Type)
    );
    let result = graph
        .run(
            "RETURN CASE WHEN false THEN 9 WHEN true THEN 8 ELSE 7 END",
            &[],
        )
        .unwrap();
    assert_eq!(result.metadata().counters.get(WorkKind::Expressions), 4);
    expression_limit(
        &graph,
        "RETURN CASE WHEN false THEN 9 WHEN true THEN 8 ELSE 7 END",
        3,
    );
    for malformed in [
        "RETURN CASE END",
        "RETURN CASE WHEN true 1 END",
        "RETURN CASE WHEN true THEN 1",
        "RETURN CASE 1 ELSE 2 END",
    ] {
        assert!(graph.run(malformed, &[]).is_err(), "{malformed}");
    }
}

fn expression_limit(graph: &Graph, text: &str, limit: u64) {
    let options = zeppelin_embed::property_graph::query::completed::GraphQueryOptions::default()
        .with_limits(
            24 * 1024 * 1024,
            RuntimeLimits::default()
                .with_limit(WorkKind::Expressions, limit)
                .unwrap(),
        )
        .unwrap();
    let error = zeppelin_embed_cypher::execute(
        graph.store(),
        &zeppelin_embed::lifecycle::QueryControl::Cancel(
            zeppelin_embed::lifecycle::CancelToken::new(),
        ),
        &options,
        text,
        &[],
        zeppelin_embed_cypher::CompileLimits::default(),
    )
    .map(|_| ())
    .unwrap_err();
    assert!(
        matches!(error, StatementError::Query(ref error) if error.kind() == zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind::Limit),
        "{error}"
    );
}

fn scalar_rows(text: &str, expected: Vec<V>, function: &str, invalid: &str) {
    let graph = Graph::new("ze432-scalar");
    assert_eq!(rows(&graph, text), vec![expected]);
    expression_limit(&graph, &format!("RETURN {function}(null)"), 1);
    for query in [
        format!("RETURN {function}()"),
        format!("RETURN {function}(null, null)"),
        format!("RETURN {function}(DISTINCT null)"),
    ] {
        assert!(
            matches!(graph.run(&query, &[]), Err(StatementError::Compile(_))),
            "{query}"
        );
    }
    graph.setup(&format!("CREATE (:Bad {{v: {invalid}}})"));
    let error = graph
        .run(&format!("MATCH (n:Bad) RETURN {function}(n.v)"), &[])
        .map(|_| ())
        .unwrap_err();
    assert!(
        matches!(error, StatementError::Query(ref error) if error.kind() == zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind::Expression),
        "{error}"
    );
    assert!(error.to_string().contains("wrong type"), "{error}");
}

#[test]
fn ze432_to_integer_truncates_numbers_and_strings() {
    // openCypher TCK TypeConversion2.feature, scenarios 1-5:
    // https://github.com/opencypher/openCypher/blob/main/tck/features/expressions/typeConversion/TypeConversion2.feature
    scalar_rows(
        "RETURN toInteger(82.9), toInteger(-2.9), toInteger('1.7'), toInteger('-2.9'), toInteger('foo'), toInteger(''), toInteger(null), toInteger(9223372036854775807), toInteger('9223372036854775807')",
        vec![
            V::Int(82),
            V::Int(-2),
            V::Int(1),
            V::Int(-2),
            V::Null,
            V::Null,
            V::Null,
            V::Int(i64::MAX),
            V::Int(i64::MAX),
        ],
        "toInteger",
        "[]",
    );
}

#[test]
fn ze432_to_integer_keeps_decimal_string_precision() {
    let graph = Graph::new("ze432-integer-precision");
    assert_eq!(
        rows(
            &graph,
            "RETURN toInteger('9007199254740993.9'), toInteger('-9007199254740993.9'), toInteger('9223372036854775807.0'), toInteger('9.007199254740993e15'), toInteger('-.9')"
        ),
        vec![vec![
            V::Int(9007199254740993),
            V::Int(-9007199254740993),
            V::Int(i64::MAX),
            V::Int(9007199254740993),
            V::Int(0)
        ]]
    );
}

#[test]
fn ze432_to_float_converts_numbers_and_strings() {
    // openCypher TCK expressions/typeConversion/TypeConversion3.feature.
    scalar_rows(
        "RETURN toFloat(3), toFloat(3.4), toFloat('5.2'), toFloat('foo'), toFloat(''), toFloat(null)",
        vec![
            V::Float(3.0),
            V::Float(3.4),
            V::Float(5.2),
            V::Null,
            V::Null,
            V::Null,
        ],
        "toFloat",
        "true",
    );
}

#[test]
fn ze438_numeric_conversions_trim_whitespace() {
    // Pinned TCK TypeConversion2.feature / TypeConversion3.feature cover numeric
    // strings and invalid text, but not whitespace. Match existing toFloat trim.
    // https://github.com/opencypher/openCypher/blob/007895aff5f33097d67b2e48a0a2babd6bd18590/tck/features/expressions/typeConversion/TypeConversion2.feature
    // https://github.com/opencypher/openCypher/blob/007895aff5f33097d67b2e48a0a2babd6bd18590/tck/features/expressions/typeConversion/TypeConversion3.feature
    let graph = Graph::new("ze438-whitespace");
    assert_eq!(
        rows(
            &graph,
            "RETURN toInteger(' 42'), toFloat(' 42'), toInteger(' -2.9 '), toFloat(' -2.9 '), toInteger(' 9007199254740993.9 '), toInteger('   '), toFloat('   '), toInteger('4 2'), toFloat('4 2')"
        ),
        vec![vec![
            V::Int(42),
            V::Float(42.0),
            V::Int(-2),
            V::Float(-2.9),
            V::Int(9007199254740993),
            V::Null,
            V::Null,
            V::Null,
            V::Null
        ]]
    );
}

#[test]
fn ze438_to_float_rejects_nonfinite_strings() {
    // TCK TypeConversion3.feature has no NaN/infinity/overflow-string scenario.
    // Keep the engine's fail-loudly ArithmeticDomain rule for parsed nonfinite.
    // https://github.com/opencypher/openCypher/blob/007895aff5f33097d67b2e48a0a2babd6bd18590/tck/features/expressions/typeConversion/TypeConversion3.feature
    let graph = Graph::new("ze438-nonfinite");
    for text in ["NaN", "inf", "-inf", "Infinity", "1e999", " NaN "] {
        let error = graph
            .run(&format!("RETURN toFloat('{text}')"), &[])
            .map(|_| ())
            .unwrap_err();
        assert!(
            matches!(error, StatementError::Query(ref error) if error.kind() == zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind::Expression),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("nonfinite graph arithmetic operand"),
            "{error}"
        );
    }
}

#[test]
fn ze438_to_integer_boolean_remains_type_error() {
    // TCK TypeConversion2.feature [8] omits booleans (unlike float [6]).
    // The pinned profile is silent: retain Type, rather than Neo4j 5 coercion.
    // https://github.com/opencypher/openCypher/blob/007895aff5f33097d67b2e48a0a2babd6bd18590/tck/features/expressions/typeConversion/TypeConversion2.feature
    let graph = Graph::new("ze438-boolean");
    for value in ["true", "false"] {
        assert!(
            matches!(graph.run(&format!("RETURN toInteger({value})"), &[]), Err(StatementError::Compile(error)) if error.kind == ErrorKind::Type)
        );
        graph.setup(&format!("CREATE (:Boolean {{v: {value}}})"));
        let error = graph
            .run(
                &format!("MATCH (n:Boolean) WHERE n.v = {value} RETURN toInteger(n.v)"),
                &[],
            )
            .map(|_| ())
            .unwrap_err();
        assert!(
            matches!(error, StatementError::Query(ref error) if error.kind() == zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind::Expression),
            "{error}"
        );
        assert!(error.to_string().contains("wrong type"), "{error}");
    }
}

#[test]
fn ze432_floor_returns_float_and_propagates_null() {
    scalar_rows(
        "RETURN floor(2), floor(2.9), floor(-2.1), floor(null)",
        vec![V::Float(2.0), V::Float(2.0), V::Float(-3.0), V::Null],
        "floor",
        "'2'",
    );
}

#[test]
fn ze432_round_ties_toward_positive_infinity() {
    // TCK Mathematical12.feature (Rounding numbers) has no scenarios:
    // https://github.com/opencypher/openCypher/blob/main/tck/features/expressions/mathematical/Mathematical12.feature
    // The one-argument reference uses Math.round, ties toward +infinity:
    // https://github.com/neo4j/neo4j/blob/3.5/community/cypher/runtime-util/src/main/java/org/neo4j/cypher/operations/CypherFunctions.java
    scalar_rows(
        "RETURN round(2.5), round(-2.5), round(-2.6), round(2), round(null), round(0.49999999999999994)",
        vec![
            V::Float(3.0),
            V::Float(-2.0),
            V::Float(-3.0),
            V::Float(2.0),
            V::Null,
            V::Float(0.0),
        ],
        "round",
        "'2'",
    );
}

#[test]
fn ze432_head_returns_first_element_or_null() {
    scalar_rows(
        "RETURN head([null, 2]), head([[1], 2]), head([]), head(null)",
        vec![V::Null, V::List(vec![V::Int(1)]), V::Null, V::Null],
        "head",
        "'x'",
    );
}

#[test]
fn ze432_last_returns_last_element_or_null() {
    scalar_rows(
        "RETURN last([1, null]), last([1, [2]]), last([]), last(null)",
        vec![V::Null, V::List(vec![V::Int(2)]), V::Null, V::Null],
        "last",
        "'x'",
    );
}

#[test]
fn ze432_abs_preserves_numeric_kind_and_propagates_null() {
    scalar_rows(
        "RETURN abs(-2), abs(-2.5), abs(null), abs(0)",
        vec![V::Int(2), V::Float(2.5), V::Null, V::Int(0)],
        "abs",
        "'2'",
    );
}

#[test]
fn ze51_grouped_aggregate_over_empty_input_returns_no_rows() {
    let graph = Graph::new("ze51-empty");
    let result = graph
        .run("MATCH (n:Missing) RETURN n.k AS k, count(*) AS c", &[])
        .unwrap();
    let (columns, actual) = tck::actual_table(&result);
    assert_eq!(columns, ["k", "c"]);
    assert_eq!(actual, Vec::<Vec<V>>::new());
    // Global aggregation over the same empty MATCH still emits one row.
    assert_eq!(
        rows(&graph, "MATCH (n:Missing) RETURN count(*)"),
        vec![vec![V::Int(0)]]
    );
}

#[test]
fn ze51_null_group_key_and_null_operands_follow_equivalence() {
    let graph = Graph::new("ze51-null");
    graph.setup("CREATE (:N {num: 2}), (:N), (:N {num: 2}), (:N {num: 3}), (:N {name: 'named'})");
    let actual = rows(
        &graph,
        "MATCH (n:N) RETURN n.name AS name, count(n.num) AS c, collect(n.num) AS l",
    );
    assert_eq!(
        bag(actual),
        bag(vec![
            vec![
                V::Null,
                V::Int(3),
                V::List(vec![V::Int(2), V::Int(2), V::Int(3)])
            ],
            vec![V::Str("named".into()), V::Int(0), V::List(vec![])],
        ])
    );
}

#[test]
fn ze51_distinct_coalesces_numeric_null_and_nan_through_text() {
    let graph = Graph::new("ze51-distinct");
    graph.setup("CREATE (:P {v: 1}), (:P {v: 1.0}), (:P {v: 2}), (:P), (:P)");
    let actual = rows(&graph, "MATCH (p:P) RETURN DISTINCT p.v");
    // Numeric equivalence, without requiring the representative's storage tag.
    let actual: Vec<_> = actual
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|v| match v {
                    V::Float(1.0) => V::Int(1),
                    other => other,
                })
                .collect()
        })
        .collect();
    assert_eq!(
        bag(actual),
        bag(vec![vec![V::Int(1)], vec![V::Int(2)], vec![V::Null]])
    );
    let parameters = [ParameterBinding {
        name: "nan",
        value: QueryValue::F64(f64::NAN),
    }];
    graph
        .run("CREATE (:Nan {v: $nan}), (:Nan {v: $nan})", &parameters)
        .unwrap();
    for text in [
        "MATCH (p:Nan) RETURN DISTINCT p.v",
        "MATCH (p:Nan) WITH $nan AS v RETURN DISTINCT v",
    ] {
        let result = graph
            .run(
                text,
                if text.contains("$nan") {
                    &parameters
                } else {
                    &[]
                },
            )
            .unwrap();
        let actual = tck::actual_table(&result).1;
        assert_eq!(actual.len(), 1);
        assert!(matches!(&actual[0][0], V::Float(value) if value.is_nan()));
    }
}

#[test]
fn ze51_order_by_ties_are_returned_as_a_bag() {
    let graph = Graph::new("ze51-ties");
    let keys = [2, 0, 1, 2, 1, 0, 2, 0, 1, 2];
    let nodes: Vec<_> = keys
        .iter()
        .enumerate()
        .map(|(id, key)| format!("(:N {{id: {id}, key: {key}}})"))
        .collect();
    graph.setup(&format!("CREATE {}", nodes.join(", ")));
    let actual = rows(
        &graph,
        "MATCH (n:N) RETURN n.key AS key, n.id AS id ORDER BY key",
    );
    let mut offset = 0;
    for key in 0..3 {
        let expected: Vec<_> = keys
            .iter()
            .enumerate()
            .filter(|(_, k)| **k == key)
            .map(|(id, _)| vec![V::Int(key), V::Int(id as i64)])
            .collect();
        let end = offset + expected.len();
        assert_eq!(bag(actual[offset..end].to_vec()), bag(expected));
        offset = end;
    }
    assert_eq!(actual.len(), offset);
}

#[test]
fn ze51_order_by_hidden_key_then_with_limit_then_match() {
    let graph = Graph::new("ze51-notes");
    // id, updatedAt, folder id, tag names; unique timestamps make paging exact.
    let notes = [
        (0, 20, 1, vec!["a", "b"]),
        (1, 40, 1, vec!["c"]),
        (2, 10, 1, vec!["a"]),
        (3, 30, 1, vec!["d", "d"]),
        (4, 50, 2, vec!["other"]),
    ];
    let mut parts = vec![
        "(f1:Folder {id: 1})".to_owned(),
        "(f2:Folder {id: 2})".to_owned(),
    ];
    for (id, updated, folder, tags) in &notes {
        parts.push(format!(
            "(n{id}:Note {{id: {id}, updatedAt: {updated}}})-[:IN]->(f{folder})"
        ));
        for tag in tags {
            parts.push(format!("(n{id})-[:TAGGED]->(:Tag {{name: '{tag}'}})"));
        }
    }
    graph.setup(&format!("CREATE {}", parts.join(", ")));
    let parameters = [
        ParameterBinding {
            name: "id",
            value: QueryValue::I64(1),
        },
        ParameterBinding {
            name: "s",
            value: QueryValue::I64(1),
        },
        ParameterBinding {
            name: "l",
            value: QueryValue::I64(2),
        },
    ];
    let mut selected: Vec<_> = notes.iter().filter(|n| n.2 == 1).collect();
    selected.sort_by_key(|n| std::cmp::Reverse(n.1));
    let result = graph.run("MATCH (n:Note)-[:IN]->(f:Folder {id: $id}) WITH n ORDER BY n.updatedAt DESC SKIP $s LIMIT $l MATCH (n)-[:TAGGED]->(t) RETURN n.id, collect(t.name)", &parameters).unwrap();
    let expected = selected
        .iter()
        .skip(1)
        .take(2)
        .map(|n| {
            vec![
                V::Int(n.0),
                V::List(n.3.iter().map(|t| V::Str((*t).into())).collect()),
            ]
        })
        .collect();
    assert_eq!(bag(tck::actual_table(&result).1), bag(expected));
    let result = graph
        .run(
            "MATCH (n:Note)-[:IN]->(f:Folder {id: $id}) RETURN n ORDER BY n.updatedAt DESC",
            &parameters[..1],
        )
        .unwrap();
    let expected: Vec<_> = selected
        .iter()
        .map(|n| {
            vec![tck::parse_value(&format!(
                "(:Note {{id: {}, updatedAt: {}}})",
                n.0, n.1
            ))]
        })
        .collect();
    assert_eq!(tck::actual_table(&result).1, expected);
}

fn speakers() -> Graph {
    let graph = Graph::new("ze51-speakers");
    graph.setup("CREATE (a:Speaker {name: 'Ada'}), (b:Speaker {name: 'Bea'}), (c:Speaker {name: 'Cy'}), (:Segment)-[:SPOKEN_BY]->(b), (:Segment)-[:SPOKEN_BY]->(a), (:Segment)-[:SPOKEN_BY]->(c), (:Segment)-[:SPOKEN_BY]->(b), (:Segment)-[:SPOKEN_BY]->(a)");
    graph
}

#[test]
fn ze51_count_per_group_with_node_key_and_alias_order() {
    let graph = speakers();
    for projection in ["p", "p.name"] {
        let actual = rows(
            &graph,
            &format!(
                "MATCH (s:Segment)-[:SPOKEN_BY]->(p:Speaker) RETURN {projection} AS speaker, count(s) AS n ORDER BY n DESC, p.name"
            ),
        );
        let expected: Vec<_> = [("Ada", 2), ("Bea", 2), ("Cy", 1)]
            .into_iter()
            .map(|(name, count)| {
                let key = if projection == "p" {
                    tck::parse_value(&format!("(:Speaker {{name: '{name}'}})"))
                } else {
                    V::Str(name.into())
                };
                vec![key, V::Int(count)]
            })
            .collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn ze51_unaliased_aggregate_in_order_by_is_a_profile_refusal() {
    let graph = speakers();
    let error = graph.run("MATCH (s:Segment)-[:SPOKEN_BY]->(p:Speaker) RETURN p AS speaker, count(s) AS n ORDER BY count(s) DESC", &[]).map(|_| ()).unwrap_err();
    let StatementError::Compile(error) = error else {
        panic!("expected compile refusal: {error}")
    };
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(error.message, "aggregate must be a top-level projection");
}

#[test]
fn ze51_with_scope_and_bound_refusals_through_execute() {
    let graph = Graph::new("ze51-refusals");
    graph.setup("CREATE (:N {name: 'Ada'})");
    let generation = || graph.run("RETURN 1", &[]).unwrap().metadata().generation;
    let before = generation();
    let parameters = [ParameterBinding {
        name: "f",
        value: QueryValue::F64(1.5),
    }];
    for (text, kind) in [
        (
            "MATCH (n:N) WITH n.name AS name RETURN n",
            ErrorKind::UnknownVariable,
        ),
        ("MATCH (n:N) RETURN n SKIP -1", ErrorKind::InvalidRange),
        ("MATCH (n:N) RETURN n LIMIT 1.5", ErrorKind::InvalidRange),
        ("MATCH (n:N) RETURN n LIMIT $f", ErrorKind::InvalidRange),
    ] {
        let error = graph
            .run(
                text,
                if text.contains("$f") {
                    &parameters
                } else {
                    &[]
                },
            )
            .map(|_| ())
            .unwrap_err();
        let StatementError::Compile(error) = error else {
            panic!("{text}: expected compile refusal: {error}")
        };
        assert_eq!(error.kind, kind, "{text}");
        assert_eq!(generation(), before, "{text}");
    }
}

#[test]
fn ze51_limit_does_not_reduce_charged_sort_work() {
    let graph = Graph::new("ze51-work");
    // Small setup batches stay within the parser's statement limits.
    for start in (0..200).step_by(20) {
        let nodes: Vec<_> = (start..start + 20)
            .map(|i| format!("(:N {{i: {i}}})"))
            .collect();
        graph.setup(&format!("CREATE {}", nodes.join(", ")));
    }
    let sorted = graph
        .run("MATCH (n:N) RETURN n ORDER BY n.i LIMIT 1", &[])
        .unwrap();
    let streaming = graph.run("MATCH (n:N) RETURN n LIMIT 1", &[]).unwrap();
    assert_eq!(sorted.metadata().rows, 1);
    assert_eq!(streaming.metadata().rows, 1);
    assert!(sorted.metadata().counters.get(WorkKind::OperatorRows) >= 200);
    assert!(
        streaming.metadata().counters.get(WorkKind::RowsIn)
            < sorted.metadata().counters.get(WorkKind::RowsIn)
    );
}

fn capacity_graph(count: i64) -> Graph {
    let graph = Graph::new("ze51-capacity");
    zeppelin_embed::property_graph::query::native_relational_test_support::seed_capacity_store(
        graph.store(),
        count as usize,
        true,
    )
    .unwrap();
    graph
}

#[test]
fn ze51_aggregate_over_more_than_one_chunk_of_rows() {
    let graph = capacity_graph(1025);
    assert_eq!(
        rows(&graph, "MATCH (s:Segment) RETURN count(s)"),
        vec![vec![V::Int(1025)]]
    );
}

#[test]
fn ze51_sort_then_limit_over_more_than_one_chunk() {
    let graph = capacity_graph(1025);
    assert_eq!(
        rows(
            &graph,
            "MATCH (n:Segment) RETURN n.i AS i ORDER BY i DESC LIMIT 3"
        ),
        vec![vec![V::Int(1024)], vec![V::Int(1023)], vec![V::Int(1022)]]
    );
}

#[test]
fn ze51_distinct_over_more_than_one_chunk() {
    let graph = capacity_graph(2050);
    assert_eq!(
        bag(rows(&graph, "MATCH (n:Segment) RETURN DISTINCT n.k")),
        bag((0..7).map(|i| vec![V::Int(i)]).collect())
    );
}

#[test]
fn ze51_blocking_operators_over_5000_nodes() {
    let graph = Graph::new("ze51-5000");
    zeppelin_embed::property_graph::query::native_relational_test_support::seed_capacity_store(
        graph.store(),
        5000,
        false,
    )
    .unwrap();
    let rows = |graph: &Graph, text: &str| {
        let started = std::time::Instant::now();
        let result = zeppelin_embed_cypher::execute(
            graph.store(),
            &zeppelin_embed::lifecycle::QueryControl::Cancel(
                zeppelin_embed::lifecycle::CancelToken::new(),
            ),
            &zeppelin_embed::property_graph::query::completed::GraphQueryOptions::default(),
            text,
            &[],
            zeppelin_embed_cypher::CompileLimits::default(),
        )
        .unwrap();
        eprintln!("ZE51 execute {text:?} elapsed={:?}", started.elapsed());
        tck::actual_table(&result).1
    };
    assert_eq!(
        rows(&graph, "MATCH (n) RETURN count(n)"),
        vec![vec![V::Int(5000)]]
    );
    // Constants keep the fixture minimal while Sort and DISTINCT must still
    // drain 5,000 input rows. Property ordering is covered by the 1,025-row test.
    assert_eq!(
        rows(&graph, "MATCH (n) RETURN 1 AS i ORDER BY i LIMIT 3"),
        vec![vec![V::Int(1)]; 3]
    );
    assert_eq!(
        rows(&graph, "MATCH (n) RETURN DISTINCT 1 AS i"),
        vec![vec![V::Int(1)]]
    );
}

#[test]
fn ze255_numeric_aggregates() {
    let graph = Graph::new("ze255-numeric");
    graph.setup("CREATE (:Number {v: 2}), (:Number {v: 3}), (:Number)");
    assert_eq!(
        rows(
            &graph,
            "MATCH (n:Number) RETURN sum(n.v), min(n.v), max(n.v)"
        ),
        vec![vec![V::Int(5), V::Int(2), V::Int(3)]]
    );
    assert_eq!(
        rows(
            &graph,
            "MATCH (n:Missing) RETURN sum(n.v), min(n.v), max(n.v)"
        ),
        vec![vec![V::Int(0), V::Null, V::Null]]
    );
}

#[test]
fn ze255_top_k_memory_bound() {
    let mut peaks = Vec::new();
    for count in [128, 5000] {
        let graph = capacity_graph(count);
        let result = graph
            .run(
                "MATCH (n:Segment) RETURN n.i AS i ORDER BY i DESC SKIP 2 LIMIT 3",
                &[],
            )
            .unwrap();
        assert_eq!(
            tck::actual_table(&result).1,
            (count - 5..count - 2)
                .rev()
                .map(|i| vec![V::Int(i)])
                .collect::<Vec<_>>()
        );
        let peak = result.metadata().peak_query_bytes;
        drop(result);
        // Both queries scan the same properties and retain the statement's
        // page-validation memo. Compare growth against a streaming aggregate
        // so the fixed-k operator cannot hide input-sized row storage behind
        // that shared bookkeeping.
        let baseline = graph
            .run("MATCH (n:Segment) RETURN sum(n.i) AS sum", &[])
            .unwrap();
        assert_eq!(
            tck::actual_table(&baseline).1,
            vec![vec![V::Int(count * (count - 1) / 2)]]
        );
        peaks.push((peak, baseline.metadata().peak_query_bytes));
    }
    assert_eq!(
        peaks[1].0.checked_sub(peaks[0].0).unwrap(),
        peaks[1].1.checked_sub(peaks[0].1).unwrap(),
        "fixed k must add no input-sized capacity beyond the streaming scan"
    );
}

#[test]
fn ze255_top_k_preserves_full_sort_ties_and_hidden_keys() {
    let graph = capacity_graph(128);
    let query =
        "MATCH (n:Segment) RETURN n.i AS i, 'retained payload' AS text ORDER BY n.k DESC, n.i % 3";
    let full = rows(&graph, query);
    let bounded = rows(&graph, &format!("{query} SKIP 5 LIMIT 17"));
    assert_eq!(bounded.as_slice(), full.get(5..22).unwrap());
}
