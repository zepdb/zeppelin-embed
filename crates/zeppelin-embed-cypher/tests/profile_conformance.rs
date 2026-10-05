//! ZE-59 focused public execution evidence; no compiler/core changes.
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
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::query::completed::{GraphQueryErrorKind, GraphQueryOptions};
use zeppelin_embed_cypher::{CompileLimits, StatementError, execute};

#[test]
fn ze59_original_corpus_emits_complete_receipts() {
    let mut count = 0;
    for (write, fixture) in [
        (false, include_str!("fixtures/read-tck-execution.txt")),
        (true, include_str!("fixtures/write-tck-execution.txt")),
    ] {
        for scenario in tck::scenarios(fixture) {
            conformance::run_scenario(&scenario, write);
            count += 1;
        }
    }
    assert_eq!(count, 130);
}

#[test]
fn ze59_malformed_and_budget_refusals_are_atomic() {
    let mut graph = Graph::new("ze59-atomic");
    graph.setup("CREATE (:A {v: 1}), (:A {v: 2})");
    let before = graph.snapshot().unwrap();
    let generation = graph.generation().unwrap();
    let normal = QueryControl::Cancel(CancelToken::new());
    for text in [
        "CREATE (:B) RETURN",
        "CREATE (:B) RETURN keys(null)",
        "CREATE (:B); RETURN 1",
    ] {
        let error = graph.run(text, &[]).map(|_| ()).unwrap_err();
        assert!(matches!(error, StatementError::Compile(_)), "{error}");
    }
    let error = execute(
        graph.store(),
        &normal,
        &GraphQueryOptions::default(),
        "CREATE (:B)",
        &[],
        CompileLimits {
            tokens: 1,
            ..CompileLimits::default()
        },
    )
    .map(|_| ())
    .unwrap_err();
    assert!(matches!(error, StatementError::Compile(_)), "{error}");
    let error = execute(
        graph.store(),
        &normal,
        &GraphQueryOptions::default()
            .with_result_row_limit(1)
            .unwrap(),
        "MATCH (a:A) CREATE (:B) RETURN a",
        &[],
        CompileLimits::default(),
    )
    .map(|_| ())
    .unwrap_err();
    let StatementError::Query(error) = error else {
        panic!("{error}")
    };
    assert_eq!(error.kind(), GraphQueryErrorKind::Limit);
    assert!(error.nothing_committed());
    assert_eq!(graph.snapshot().unwrap(), before);
    assert_eq!(graph.generation().unwrap(), generation);
    let retained = graph
        .run("MATCH (a:A) RETURN a.v ORDER BY a.v", &[])
        .unwrap();
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), before);
    assert_eq!(
        tck::actual_table(&retained).1,
        vec![vec![V::Int(1)], vec![V::Int(2)]]
    );
    graph.store().close().unwrap();
    let token = CancelToken::new();
    token.cancel();
    let error = execute(
        graph.store(),
        &QueryControl::Cancel(token),
        &GraphQueryOptions::default(),
        "CREATE (:B) RETURN",
        &[],
        CompileLimits::default(),
    )
    .map(|_| ())
    .unwrap_err();
    let StatementError::Query(error) = error else {
        panic!("{error}")
    };
    assert_eq!(error.kind(), GraphQueryErrorKind::Closed);
}

#[test]
fn ze59_small_mutation_model_matches_progressive_frozen_and_ordered_writes() {
    for (projection, assignment, expected) in [
        ("n, m", "n.p + 1", 2),
        ("n, m, n.p AS old", "old + 1", 1),
        ("n, m", "m.x", 9),
    ] {
        let mut model = 0;
        let old = model;
        for x in [3, 9] {
            model = match assignment {
                "n.p + 1" => model + 1,
                "old + 1" => old + 1,
                _ => x,
            };
        }
        assert_eq!(model, expected);
        let mut graph = Graph::new("ze59-small-model");
        graph.setup("CREATE (n:N {p: 0})-[:R]->(:M {x: 3}), (n)-[:R]->(:M {x: 9})");
        let query = format!(
            "MATCH (n:N)-[:R]->(m) WITH {projection} ORDER BY m.x SET n.p = {assignment} RETURN n.p AS p"
        );
        let result = graph.run(&query, &[]).unwrap();
        assert_eq!(
            tck::actual_table(&result).1,
            vec![vec![V::Int(model)], vec![V::Int(model)]]
        );
        graph.reopen();
        assert_eq!(
            tck::actual_table(&graph.run("MATCH (n:N) RETURN n.p", &[]).unwrap()).1,
            vec![vec![V::Int(model)]]
        );
    }
}

#[test]
fn ze59_compaction_preserves_observations() {
    use zeppelin_embed::property_graph::{GraphMaintenancePolicy, GraphStore};
    let root = support::unique_temp_dir("ze59-compaction");
    std::fs::create_dir_all(&root).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut store = GraphStore::create(root.join("graph"), graph::store_options(), None).unwrap();
    store
        .set_maintenance_policy(GraphMaintenancePolicy {
            automatic: false,
            reclaim_after_bytes: 1024 * 1024,
        })
        .unwrap();
    let run = |store: &GraphStore, query: &str| {
        execute(
            store.statement_store(),
            &control,
            &GraphQueryOptions::default(),
            query,
            &[],
            CompileLimits::default(),
        )
        .unwrap()
    };
    run(&store, "CREATE (a:A {p: 0})-[:R {v: 7}]->(:B {p: 8})");
    for value in 1..=12 {
        run(&store, &format!("MATCH (a:A) SET a.p = {value}"));
    }
    let query = "MATCH (a)-[r]->(b) RETURN ze.node_id(a), a, ze.relationship_id(r), r, b ORDER BY ze.node_id(a)";
    let retained = run(&store, query);
    let before = tck::actual_table(&retained);
    let report = store.maintain_cycle(&control).unwrap();
    assert!(report.cycle_complete);
    assert!(
        report.replaced_physical_refs > 0
            || report.new_pack_bytes > 0
            || report.relocated_bytes > 0,
        "no physical maintenance work: {report:?}"
    );
    assert_eq!(tck::actual_table(&run(&store, query)), before);
    store.close().unwrap();
    store = GraphStore::open(root.join("graph"), graph::store_options(), None).unwrap();
    assert_eq!(tck::actual_table(&run(&store, query)), before);
    assert_eq!(tck::actual_table(&retained), before);
    eprintln!("ZE59-MAINTENANCE {report:?}");
    store.close().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// Public examples fill expression/token gaps left by the selected originals.
/// This is local profile evidence, not an adapted TCK pass.
#[test]
fn ze59_inventory_has_positive_and_boundary_evidence() {
    let graph = Graph::new("ze59-inventory");
    let cases = [
        (
            "RETURN -9223372036854775808 AS i, 1.25e1 AS f, true AS b, null AS n, 'λ' AS s, [1,[null],false] AS l",
            "|-9223372036854775808|12.5|true|null|'λ'|[1,[null],false]|",
        ),
        (
            "RETURN [4,5][-1] AS a, [4][9] AS b, null[0] AS c, size('λ🙂') AS d",
            "|5|null|null|2|",
        ),
        (
            "RETURN 1+2*3 AS a, 7/2 AS b, 7%2 AS c, -2+5.0 AS d",
            "|7|3|1|3.0|",
        ),
        (
            "RETURN NOT false AND true OR false AS a, true XOR false AS b, 2 IN [1,2] AS c, null IS NULL AS d, 1 IS NOT NULL AS e",
            "|true|true|true|true|true|",
        ),
        (
            "RETURN 1=1.0 AS a, 1<>2 AS b, 1<2 AS c, 2<=2 AS d, 3>2 AS e, 3>=3 AS f",
            "|true|true|true|true|true|true|",
        ),
        (
            "RETURN 'abc' STARTS WITH 'a' AS a, 'abc' ENDS WITH 'c' AS b, 'abc' CONTAINS 'b' AS c",
            "|true|true|true|",
        ),
        ("/* comment */ ReTuRn 7 AS `a``λ` // comment\n;", "|7|"),
    ];
    for (query, expected) in cases {
        let expected: Vec<V> = expected
            .trim_matches('|')
            .split('|')
            .map(tck::parse_value)
            .collect();
        let result = graph.run(query, &[]).unwrap();
        assert_eq!(tck::actual_table(&result).1, vec![expected], "{query}");
    }
    let before = graph.generation().unwrap();
    for query in [
        "RETURN λ",
        "RETURN `unterminated",
        "MATCH p = ()-->() RETURN p",
        "MATCH (n {x: 1, x: 2}) RETURN n",
        "MATCH ()-[r*1..17]->() RETURN r",
        "WHERE true RETURN 1",
        "RETURN DISTINCT 1 AS x ORDER BY missing",
        "CREATE ()-[:R]-()",
        "MATCH (n) SET n = {x:1}",
        "MATCH (n) REMOVE n[0]",
        "MATCH (n) DELETE n.p",
        "CALL unknown() YIELD node RETURN node",
        "RETURN 9223372036854775808",
        "RETURN {x: 1}",
        "RETURN [1][0..1]",
        "RETURN 1^2",
        "RETURN 'abc' =~ 'a'",
        "RETURN toInteger('1')",
        "RETURN sum(1)+1",
        "RETURN $missing",
        "RETURN 1 AS x, 2 AS x",
    ] {
        let error = graph.run(query, &[]).map(|_| ()).unwrap_err();
        assert!(
            matches!(error, StatementError::Compile(_)),
            "{query}: {error}"
        );
    }
    for query in ["RETURN 1/0", "RETURN 9223372036854775807+1"] {
        let error = graph.run(query, &[]).map(|_| ()).unwrap_err();
        let StatementError::Query(error) = error else {
            panic!("{query}: {error}")
        };
        assert_eq!(error.kind(), GraphQueryErrorKind::Expression);
        assert!(error.nothing_committed());
    }
    assert_eq!(graph.generation().unwrap(), before);
}
