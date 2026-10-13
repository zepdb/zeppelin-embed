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
use tck::{V, actual_table, parse_value};

fn check(query: &str, expected: &str) {
    let graph = Graph::new("ze452");
    graph.setup("CREATE (:N {v:1}), (:N {v:2.0}), (:N), (:N {v:6}), (:N {v:1})");
    let result = graph.run(query, &[]).unwrap();
    assert_eq!(
        actual_table(&result).1,
        vec![vec![parse_value(expected)]],
        "{query}"
    );
}
#[test]
fn ze452_avg_numeric_null_empty_and_type_error() {
    check("MATCH (n:N) RETURN avg(n.v)", "2.5");
    check("MATCH (n:N) RETURN avg(DISTINCT n.v)", "3.0");
    check("MATCH (n:Missing) RETURN avg(n.v)", "null");
    check("MATCH (n:N) RETURN avg(n.missing)", "null");
    let graph = Graph::new("ze452-type");
    let error = graph
        .run("RETURN avg('bad')", &[])
        .err()
        .expect("typed rejection");
    assert!(
        matches!(error, zeppelin_embed_cypher::StatementError::Query(ref e)
        if e.kind() == zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind::Expression)
    );
}
#[test]
fn ze452_comprehension_forms_null_and_lexical_scope() {
    for (query, expected) in [
        ("RETURN [x IN [1,2,3,null] WHERE x > 1 | x * 2]", "[4,6]"),
        ("RETURN [x IN [1,2,3,null] WHERE x > 1]", "[2,3]"),
        ("RETURN [x IN [1,null,3] | x * 2]", "[2,null,6]"),
        ("RETURN [x IN [] | x]", "[]"),
        ("RETURN [x IN null | x]", "null"),
        (
            "WITH 10 AS x RETURN [x IN [1,2] | [x IN [x,3] | x]]",
            "[[1,3],[2,3]]",
        ),
        ("WITH 10 AS x RETURN [y IN [1,2] | x+y]", "[11,12]"),
    ] {
        check(query, expected);
    }
    let graph = Graph::new("ze452-scope");
    assert!(graph.run("RETURN [x IN [1] | x], x", &[]).is_err());
}
#[test]
fn ze452_map_literals_nested_access_and_grouping() {
    // Decode directly until the shared TCK model supports map cells.
    let graph = Graph::new("ze452-map");
    let result = graph
        .run("RETURN {k: 1, nested: {v: [2,null]}, empty: {}} AS m", &[])
        .unwrap();
    assert_eq!(result.metadata().rows, 1);
    check("RETURN {k: 1}.k", "1");
    check("RETURN {k: 1}.missing", "null");
    check("RETURN {nested: {v: 2}}.nested.v", "2");
    check(
        "MATCH (n:N) WITH CASE WHEN n.v = 1 THEN {a:1,b:2} ELSE {b:2,a:1} END AS x RETURN count(DISTINCT x)",
        "1",
    );
    check("RETURN [x IN [1,2] | {v:x}][1].v", "2");
    assert_eq!(
        actual_table(&graph.run("RETURN 1", &[]).unwrap()).1,
        vec![vec![V::Int(1)]]
    );
}

#[test]
fn ze452_comprehension_iterations_and_map_values_are_accounted() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
    use zeppelin_embed::property_graph::query::completed::{
        GraphQueryErrorKind, GraphQueryOptions,
    };
    use zeppelin_embed::property_graph::query::plan::ParameterBinding;
    use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind};
    use zeppelin_embed::property_graph::query::{QueryList, QueryValue, QueryView, ValueContext};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    use zeppelin_embed_cypher::{CompileLimits, StatementError, execute};
    let graph = Graph::new("ze452-budgets");
    let control = QueryControl::Cancel(CancelToken::new());
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let mut context = ValueContext::new(&view, &control, 1_000_000).unwrap();
    let elements = [QueryValue::I64(1); 8];
    let list = QueryList::new(&elements, &mut context).unwrap();
    let bindings = [ParameterBinding {
        name: "xs",
        value: QueryValue::List(list),
    }];
    let query = "RETURN [x IN $xs WHERE false]";
    let result = graph.run(query, &bindings).unwrap();
    assert_eq!(actual_table(&result).1, vec![vec![V::List(vec![])]]);
    assert_eq!(result.metadata().counters.get(WorkKind::Expressions), 18);
    let options = GraphQueryOptions::default()
        .with_limits(
            24 * 1024 * 1024,
            RuntimeLimits::default()
                .with_limit(WorkKind::Expressions, 17)
                .unwrap(),
        )
        .unwrap();
    let error = execute(
        graph.store(),
        &control,
        &options,
        query,
        &bindings,
        CompileLimits::default(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(error, StatementError::Query(ref e) if e.kind() == GraphQueryErrorKind::Limit)
    );
    let scalar = graph.run("RETURN 1", &[]).unwrap();
    let map = graph
        .run("RETURN {a:{b:[1,2,null]}, text:'payload'}", &[])
        .unwrap();
    assert!(map.metadata().peak_query_bytes > scalar.metadata().peak_query_bytes);
    assert!(
        map.metadata().counters.get(WorkKind::CopiedBytes)
            > scalar.metadata().counters.get(WorkKind::CopiedBytes)
    );
    let peak = map.metadata().peak_query_bytes;
    eprintln!(
        "ZE452 accounting: iterations=8 expressions={} scalar_peak={} map_peak={} scalar_copied={} map_copied={}",
        result.metadata().counters.get(WorkKind::Expressions),
        scalar.metadata().peak_query_bytes,
        peak,
        scalar.metadata().counters.get(WorkKind::CopiedBytes),
        map.metadata().counters.get(WorkKind::CopiedBytes)
    );
    let options = GraphQueryOptions::default()
        .with_limits(peak - 1, RuntimeLimits::default())
        .unwrap();
    let error = execute(
        graph.store(),
        &control,
        &options,
        "RETURN {a:{b:[1,2,null]}, text:'payload'}",
        &[],
        CompileLimits::default(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(error, StatementError::Query(ref e) if e.kind() == GraphQueryErrorKind::Limit)
    );
    // A refusal must release its reservation and allow the same query again.
    assert_eq!(
        actual_table(&graph.run(query, &bindings).unwrap()).1,
        vec![vec![V::List(vec![])]]
    );
}

#[test]
fn ze452_avg_does_not_overflow_a_finite_mean() {
    let graph = Graph::new("ze452-avg-finite");
    graph.setup("CREATE (:Large {v:1.0e308}), (:Large {v:1.0e308})");
    let result = graph.run("MATCH (n:Large) RETURN avg(n.v)", &[]).unwrap();
    assert_eq!(actual_table(&result).1, vec![vec![V::Float(1.0e308)]]);
    graph.setup("CREATE (:Signed {v:1.0e308}), (:Signed {v:-1.0e308})");
    let result = graph.run("MATCH (n:Signed) RETURN avg(n.v)", &[]).unwrap();
    assert_eq!(actual_table(&result).1, vec![vec![V::Float(0.0)]]);
}
