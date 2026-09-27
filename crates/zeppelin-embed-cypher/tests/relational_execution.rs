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
    QueryValue, plan::ParameterBinding, runtime::WorkKind,
};
use zeppelin_embed_cypher::{ErrorKind, StatementError};

fn rows(graph: &Graph, text: &str) -> Vec<Vec<V>> {
    tck::actual_table(&graph.run(text, &[]).unwrap()).1
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
