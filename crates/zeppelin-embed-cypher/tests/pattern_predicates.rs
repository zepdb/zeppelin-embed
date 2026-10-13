//! ZE-443: existential patterns through the public compiler and native store.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[path = "support/conformance.rs"]
mod conformance;
#[path = "support/graph.rs"]
mod graph;
mod support;
#[path = "support/tck.rs"]
mod tck;

use graph::Graph;
use tck::V;
use zeppelin_embed_cypher::{ErrorKind, StatementError};

fn rows(graph: &Graph, query: &str) -> Vec<Vec<V>> {
    tck::actual_table(&graph.run(query, &[]).unwrap()).1
}

#[test]
fn ze443_pattern_predicates_correlate_without_multiplying_rows() {
    let graph = Graph::new("ze443-patterns");
    graph.setup("CREATE (a:A {id:1}), (b:B {id:2}), (c:B {id:3}), (a)-[:KNOWS]->(b), (a)-[:KNOWS]->(b), (b)-[:T]->(c)");
    for query in [
        "MATCH (a:A), (b:B) WHERE (a)-[:KNOWS]-(b) RETURN b.id",
        "MATCH (a:A), (b:B) WHERE exists((a)-[:KNOWS]->(b)) RETURN b.id",
        "MATCH (a:A), (b:B) WHERE NOT NOT (a)-[:KNOWS]-(b) RETURN b.id",
    ] {
        assert_eq!(rows(&graph, query), vec![vec![V::Int(2)]], "{query}");
    }
    assert_eq!(
        rows(
            &graph,
            "MATCH (a:A), (b:B) WHERE NOT (a)-[:KNOWS]-(b) RETURN b.id"
        ),
        vec![vec![V::Int(3)]]
    );
    assert_eq!(
        rows(
            &graph,
            "MATCH (a:A), (b:B) RETURN b.id, (a)-[:KNOWS]-(b) AS known, NOT exists((a)-[:KNOWS]-(b)) AS fresh ORDER BY b.id"
        ),
        vec![
            vec![V::Int(2), V::Bool(true), V::Bool(false)],
            vec![V::Int(3), V::Bool(false), V::Bool(true)]
        ]
    );
    assert_eq!(
        rows(
            &graph,
            "MATCH (a:A) RETURN exists((a)-[:KNOWS]->()-[:T]->()), exists((a)<-[:KNOWS]-()), exists((a)-[:MISSING]->())"
        ),
        vec![vec![V::Bool(true), V::Bool(false), V::Bool(false)]]
    );
    assert_eq!(
        rows(
            &graph,
            "MATCH (a:A)-[r:KNOWS]->(b) WHERE (a)-[r]->(b) RETURN count(*)"
        ),
        vec![vec![V::Int(2)]]
    );
}

#[test]
fn ze443_null_bound_endpoints_never_become_scans() {
    let graph = Graph::new("ze443-null");
    graph.setup("CREATE (:A)-[:T]->(:B)");
    for query in [
        "OPTIONAL MATCH (a:Missing) RETURN exists((a)-[:T]->()), NOT (a)-[:T]->()",
        "MATCH (a:A) OPTIONAL MATCH (b:Missing) RETURN exists((a)-[:T]->(b)), NOT (a)-[:T]->(b)",
        "WITH null AS a RETURN exists((a)-[:T]->()), NOT (a)-[:T]->()",
    ] {
        assert_eq!(
            rows(&graph, query),
            vec![vec![V::Bool(false), V::Bool(true)]],
            "{query}"
        );
    }
    assert_eq!(
        rows(
            &graph,
            "OPTIONAL MATCH (a:Missing) WITH a WHERE NOT (a)-[:T]->() RETURN count(*)"
        ),
        vec![vec![V::Int(1)]]
    );
    assert_eq!(
        rows(
            &graph,
            "MATCH (a:A) OPTIONAL MATCH (a)-[:T]->(b) WHERE NOT (b)-[:T]->() RETURN b:B"
        ),
        vec![vec![V::Bool(true)]]
    );
}

#[test]
fn ze447_unresolved_predicate_variables_are_scope_errors() {
    let graph = Graph::new("ze443-scope");
    for query in [
        "MATCH (a) WHERE (a)-[:T]->(new) RETURN a",
        "MATCH (a) WHERE exists((a)-[new:T]->()) RETURN a",
        "MATCH (a) RETURN (new)-[:T]->(a)",
        "MATCH (a), (b) WITH a WHERE (a)-[:T]->(b) RETURN a",
        "MATCH (a)-[r:T]->(b) WITH a RETURN exists((a)-[r]->())",
    ] {
        let error = graph.run(query, &[]).err().unwrap();
        assert!(
            matches!(error, StatementError::Compile(ref error) if error.kind == ErrorKind::UnknownVariable),
            "{query}: {error}"
        );
    }
}

#[test]
fn ze447_reversed_bound_endpoint_has_linear_adjacency_work() {
    use zeppelin_embed::property_graph::query::runtime::WorkKind;
    let mut work = Vec::new();
    for edges in [50, 100] {
        let graph = Graph::new("ze447-reversed-work");
        for _ in 0..edges / 25 {
            graph.setup(&format!(
                "CREATE {}",
                vec!["(:Source)-[:U {ok:1}]->(:X {ok:1})"; 25].join(", ")
            ));
        }
        for query in [
            "MATCH (x:X) WHERE ()-[:U]->(x) RETURN count(*)",
            "MATCH (x:X) WHERE (x)<-[:U]-() RETURN count(*)",
            "MATCH (x:X) MATCH ()-[:U]->(x) RETURN count(*)",
        ] {
            let result = graph.run(query, &[]).unwrap();
            assert_eq!(tck::actual_table(&result).1, vec![vec![V::Int(edges)]]);
            work.push(result.metadata().counters.get(WorkKind::AdjacencyEntries));
        }
    }
    assert_eq!(work, [200, 200, 200, 400, 400, 400]);
}

#[test]
fn ze447_reversed_pattern_preserves_directions_constraints_and_reuse() {
    let graph = Graph::new("ze447-reversed-semantics");
    graph.setup("CREATE (a:A {ok:1})-[r:U {ok:1}]->(b:B {ok:1}), (b)-[:V]->(x:X), (a)-[:U]->(b)");
    for predicate in [
        "(:A {ok:1})-[:U {ok:1}]->(:B {ok:1})-[:V]->(x)",
        "(:A)-[:U]-(:B)-[:V]-(x)",
        "(:A)<-[:U]-(:B)-[:V]->(x)",
        "(:A {ok:2})-[:U]->(:B)-[:V]->(x)",
    ] {
        let expected = !predicate.contains("<-") && !predicate.contains("ok:2");
        assert_eq!(
            rows(&graph, &format!("MATCH (x:X) RETURN exists({predicate})")),
            vec![vec![V::Bool(expected)]],
            "{predicate}"
        );
    }
    assert_eq!(
        rows(
            &graph,
            "MATCH (a:A)-[r:U]->(b:B) MATCH (x:X) RETURN exists(()-[r]->(b)-[:V]->(x))"
        ),
        vec![vec![V::Bool(true)], vec![V::Bool(true)]]
    );
    assert_eq!(
        rows(
            &graph,
            "MATCH (a:A) RETURN exists(()<-[:U]-(a)), exists(()-[:U]-(a))"
        ),
        vec![vec![V::Bool(true), V::Bool(true)]]
    );
}

#[test]
fn ze447_reversed_match_keeps_property_dependencies_in_scope() {
    let graph = Graph::new("ze447-property-scope");
    graph.setup("CREATE (:A {id:1})-[:U {id:1}]->(:X {id:1})");
    assert_eq!(
        rows(
            &graph,
            "MATCH (x:X) MATCH (a:A)-[:U {id:a.id}]->(x {id:a.id}) RETURN a.id"
        ),
        vec![vec![V::Int(1)]]
    );
}

#[test]
fn ze447_null_last_endpoint_reads_no_adjacency() {
    use zeppelin_embed::property_graph::query::runtime::WorkKind;
    let graph = Graph::new("ze447-null-last");
    graph.setup("CREATE (:Source)-[:U]->(:X)");
    for query in [
        "WITH null AS x RETURN exists(()-[:U]->(x))",
        "OPTIONAL MATCH (x:Missing) RETURN exists(()-[:U]->(x))",
    ] {
        let result = graph.run(query, &[]).unwrap();
        assert_eq!(tck::actual_table(&result).1, vec![vec![V::Bool(false)]]);
        assert_eq!(
            result.metadata().counters.get(WorkKind::AdjacencyEntries),
            0,
            "{query}"
        );
    }
}

#[test]
fn ze443_tck_style_pattern_fixtures_execute() {
    let scenarios = tck::scenarios(include_str!("fixtures/pattern-predicates.txt"));
    assert_eq!(scenarios.len(), 2);
    for scenario in scenarios {
        let mut graph = Graph::new("ze443-fixture");
        for statement in &scenario.setup {
            graph.setup(statement);
        }
        let before = graph.generation().unwrap();
        for pass in 0..2 {
            if pass == 1 {
                graph.reopen();
            }
            let result = graph.run(&scenario.query, &[]).unwrap();
            conformance::compare_result(&scenario.expect, &result).unwrap();
            assert_eq!(result.metadata().generation, before);
            assert_eq!(
                result.metadata().outcome,
                zeppelin_embed::property_graph::query::completed::Outcome::Read
            );
        }
    }
}

#[test]
fn ze443_pattern_work_limits_remain_charged() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
    use zeppelin_embed::property_graph::query::{
        completed::{GraphQueryErrorKind, GraphQueryOptions},
        runtime::{RuntimeLimits, WorkKind},
    };
    let graph = Graph::new("ze443-work");
    graph.setup("CREATE (:A)-[:T]->(:B)");
    let query = "MATCH (a:A) RETURN exists((a)-[:T]->())";
    let result = graph.run(query, &[]).unwrap();
    assert_eq!(tck::actual_table(&result).1, vec![vec![V::Bool(true)]]);
    for kind in [
        WorkKind::AdjacencyEntries,
        WorkKind::Expressions,
        WorkKind::OperatorRows,
    ] {
        assert!(result.metadata().counters.get(kind) > 0, "{kind:?}");
        let options = GraphQueryOptions::default()
            .with_limits(
                24 * 1024 * 1024,
                RuntimeLimits::default().with_limit(kind, 0).unwrap(),
            )
            .unwrap();
        let error = zeppelin_embed_cypher::execute(
            graph.store(),
            &QueryControl::Cancel(CancelToken::new()),
            &options,
            query,
            &[],
            zeppelin_embed_cypher::CompileLimits::default(),
        )
        .err()
        .unwrap();
        assert!(
            matches!(error, StatementError::Query(ref error) if error.kind() == GraphQueryErrorKind::Limit),
            "{kind:?}: {error}"
        );
    }
}
