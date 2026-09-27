//! Local ZE-57 extensions, not original TCK scenarios.
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
use zeppelin_embed_cypher::StatementError;

/// Computed mixed types fail after staging; literal null is refused statically.
#[test]
fn ze57_local_set_runtime_mixed_list_rejects_atomically() {
    for list in ["[n.a, n.b]", "[n.a, null]"] {
        let mut graph = graph::Graph::new("ze57-runtime-list");
        graph.setup("CREATE ({a: 1, b: 'x'})");
        let before = graph.snapshot().unwrap();
        let generation = graph.generation().unwrap();
        let query = format!("MATCH (n) SET n.staged = 1, n.l = {list}");
        let result = graph.run(&query, &[]);
        assert_eq!(graph.generation().unwrap(), generation, "{query}");
        assert_eq!(graph.snapshot().unwrap(), before, "{query}");
        graph.reopen();
        assert_eq!(graph.snapshot().unwrap(), before, "{query}");
        assert_eq!(graph.generation().unwrap(), generation, "{query}");
        match result {
            Err(StatementError::Compile(error)) if list == "[n.a, null]" => {
                assert_eq!(error.kind, zeppelin_embed_cypher::ErrorKind::Type);
                assert_eq!(
                    error.message,
                    "property list requires homogeneous nonnull scalars"
                );
            }
            Err(StatementError::Query(error)) if list != "[n.a, null]" => {
                assert_eq!(error.kind(), GraphQueryErrorKind::Expression, "{query}");
                assert!(error.nothing_committed(), "{query}");
            }
            Err(error) => panic!("{query}: expected runtime Expression refusal, got {error:?}"),
            Ok(_) => panic!("{query}: expected runtime Expression refusal, executed"),
        }
    }
}

/// Local pin: ZE-190 owns enabling fence-only publication; flip this when it lands.
#[test]
fn ze57_fence_only_statement_is_refused_until_ze190() {
    let graph = graph::Graph::new("ze57-fence-only");
    let before = graph.generation().unwrap();
    match graph.run("CREATE (m)-[r:R]->(k) DELETE m, k, r", &[]) {
        Err(StatementError::Query(error)) => {
            assert_eq!(error.kind(), GraphQueryErrorKind::InvalidPlan);
            assert!(error.nothing_committed());
        }
        Err(error) => panic!("expected query refusal, got {error}"),
        Ok(_) => panic!("expected fence-only refusal, executed"),
    }
    assert_eq!(graph.generation().unwrap(), before);
    graph.setup("CREATE ()");
    assert!(graph.generation().unwrap() > before);
}

use graph::Graph;
use tck::V;
use zeppelin_embed::property_graph::query::completed::Outcome;

fn local_write(setup: &str, query: &str, expected: &str, effects: &[(&str, u64)]) {
    let mut graph = Graph::new("ze57-local");
    graph.setup(setup);
    let before = graph.snapshot().unwrap();
    let generation = graph.generation().unwrap();
    let result = graph.run(query, &[]).unwrap();
    assert_eq!(
        tck::actual_table(&result).1,
        vec![vec![tck::parse_value(expected)]]
    );
    if effects.is_empty() {
        assert_eq!(result.metadata().outcome, Outcome::NoOp);
        assert_eq!(graph.generation().unwrap(), generation);
    } else {
        assert!(matches!(
            result.metadata().outcome,
            Outcome::Committed { .. }
        ));
    }
    let after = graph.snapshot().unwrap();
    assert_eq!(
        graph::Snapshot::diff(&before, &after),
        effects.iter().copied().collect()
    );
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), after);
}

#[test]
fn ze57_local_remove_missing_property_is_noop() {
    local_write(
        "CREATE ({p: 1})",
        "MATCH (n) REMOVE n.missing RETURN n",
        "({p: 1})",
        &[],
    );
}

#[test]
fn ze57_local_remove_multiple_properties_with_projection() {
    for remove in ["REMOVE n.num, n.name", "REMOVE n.num REMOVE n.name"] {
        let mut graph = Graph::new("ze57-remove");
        graph.setup("CREATE ({num: 1, name: 'x', name2: 'keep'})");
        let before = graph.snapshot().unwrap();
        let result = graph
            .run(
                &format!(
                    "MATCH (n) {remove} RETURN n.num IS NULL AS a, n.name IS NULL AS b, n.name2"
                ),
                &[],
            )
            .unwrap();
        assert!(matches!(
            result.metadata().outcome,
            Outcome::Committed { .. }
        ));
        assert_eq!(
            tck::actual_table(&result).1,
            vec![vec![V::Bool(true), V::Bool(true), V::Str("keep".into())]]
        );
        let after = graph.snapshot().unwrap();
        assert_eq!(
            graph::Snapshot::diff(&before, &after),
            [("-properties", 2)].into()
        );
        graph.reopen();
        assert_eq!(graph.snapshot().unwrap(), after);
    }
}

#[test]
fn ze57_local_set_empty_list_stores_canonical_empty_list() {
    let mut graph = Graph::new("ze57-empty-list");
    graph.setup("CREATE ()");
    let result = graph.run("MATCH (n) SET n.l = [] RETURN n", &[]).unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));
    assert_eq!(
        tck::actual_table(&result).1,
        vec![vec![tck::parse_value("({l: []})")]]
    );
    let after = graph.snapshot().unwrap();
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), after);
    assert_eq!(
        tck::actual_table(&graph.run("MATCH (n) RETURN n.l", &[]).unwrap()).1,
        vec![vec![V::List(vec![])]]
    );
    let generation = graph.generation().unwrap();
    assert_eq!(
        graph
            .run("MATCH (n) SET n.l = []", &[])
            .unwrap()
            .metadata()
            .outcome,
        Outcome::NoOp
    );
    assert_eq!(graph.generation().unwrap(), generation);
}

fn compile_list_refusal(
    graph: &mut Graph,
    result: Result<
        zeppelin_embed::property_graph::query::completed::CompletedGraphResult,
        StatementError,
    >,
    before: graph::Snapshot,
    generation: zeppelin_embed::property_graph::GraphGeneration,
    message: &str,
) {
    match result {
        Err(StatementError::Compile(error)) => {
            assert_eq!(error.kind, zeppelin_embed_cypher::ErrorKind::Type);
            assert_eq!(error.message, message);
        }
        Err(error) => panic!("expected compile Type, got {error:?}"),
        Ok(_) => panic!("expected compile refusal"),
    }
    assert_eq!(graph.generation().unwrap(), generation);
    assert_eq!(graph.snapshot().unwrap(), before);
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), before);
    assert_eq!(graph.generation().unwrap(), generation);
}

#[test]
fn ze57_local_set_mixed_literal_list_is_a_compile_type_error() {
    let mut graph = Graph::new("ze57-literal");
    graph.setup("CREATE ()");
    let before = graph.snapshot().unwrap();
    let generation = graph.generation().unwrap();
    let result = graph.run("MATCH (n) SET n.l = [1, 'a']", &[]);
    compile_list_refusal(
        &mut graph,
        result,
        before,
        generation,
        "property list requires homogeneous nonnull scalars",
    );
}

#[test]
fn ze57_local_set_mixed_parameter_list_is_a_compile_type_error() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
    use zeppelin_embed::property_graph::query::plan::ParameterBinding;
    use zeppelin_embed::property_graph::query::{QueryList, QueryValue, QueryView, ValueContext};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    let mut graph = Graph::new("ze57-parameter");
    graph.setup("CREATE ()");
    let before = graph.snapshot().unwrap();
    let generation = graph.generation().unwrap();
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 1_000_000).unwrap();
    let items = [QueryValue::I64(1), QueryValue::String("a")];
    let parameters = [ParameterBinding {
        name: "l",
        value: QueryValue::List(QueryList::new(&items, &mut context).unwrap()),
    }];
    let result = graph.run("MATCH (n) SET n.l = $l", &parameters);
    compile_list_refusal(
        &mut graph,
        result,
        before,
        generation,
        "mixed property parameter list",
    );
}

#[test]
fn ze57_local_set_list_property_then_index_reads_back() {
    let mut graph = Graph::new("ze57-index");
    graph.setup("CREATE ()");
    let result = graph
        .run("MATCH (n) SET n.l = [1,2,3] RETURN n.l[1], n.l[-1]", &[])
        .unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));
    assert_eq!(
        tck::actual_table(&result).1,
        vec![vec![V::Int(2), V::Int(3)]]
    );
    let after = graph.snapshot().unwrap();
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), after);
    assert_eq!(
        tck::actual_table(&graph.run("MATCH (n) RETURN n.l[1], n.l[-1]", &[]).unwrap()).1,
        vec![vec![V::Int(2), V::Int(3)]]
    );
}

#[test]
fn ze57_local_deleted_results_are_refused_statically() {
    for (query, span) in [
        ("MATCH (n) DELETE n RETURN n", "n"),
        ("MATCH (n) DELETE n RETURN n.p", "n.p"),
        ("MATCH ()-[r]->() DELETE r RETURN collect(r)", "collect(r)"),
        ("MATCH (n) DELETE n SET n.p = 1", "n.p = 1"),
    ] {
        let mut graph = Graph::new("ze57-deleted-static");
        graph.setup("CREATE ({p: 7})-[:R]->()");
        let before = graph.snapshot().unwrap();
        let generation = graph.generation().unwrap();
        match graph.run(query, &[]) {
            Err(StatementError::Compile(error)) => {
                assert_eq!(
                    error.kind,
                    zeppelin_embed_cypher::ErrorKind::DeletedEntity,
                    "{query}"
                );
                assert_eq!(&query[error.span.start..error.span.end], span, "{query}");
            }
            Err(error) => panic!("{query}: unexpected {error:?}"),
            Ok(_) => panic!("{query}: executed"),
        }
        assert_eq!(graph.generation().unwrap(), generation);
        assert_eq!(graph.snapshot().unwrap(), before);
        graph.reopen();
        assert_eq!(graph.snapshot().unwrap(), before);
        assert_eq!(graph.generation().unwrap(), generation);
    }
}

#[test]
fn ze57_local_deleted_results_are_refused_dynamically() {
    for delete in ["DELETE", "DETACH DELETE"] {
        for query in [
            format!("MATCH (n), (m) {delete} n RETURN m"),
            format!("OPTIONAL MATCH (n) {delete} n RETURN n"),
            format!("MATCH (n) WITH [n] AS ns, n {delete} n RETURN ns[0]"),
        ] {
            let mut graph = Graph::new("ze57-deleted-dynamic");
            graph.setup("CREATE ({p: 7})");
            let before = graph.snapshot().unwrap();
            let generation = graph.generation().unwrap();
            match graph.run(&query, &[]) {
                Err(StatementError::Query(error)) => {
                    assert_eq!(
                        error.kind(),
                        GraphQueryErrorKind::Constraint,
                        "{query}: {error}"
                    );
                    assert!(error.nothing_committed(), "{query}");
                }
                Err(error) => panic!("{query}: unexpected {error:?}"),
                Ok(_) => panic!("{query}: copied stale node"),
            }
            assert_eq!(graph.generation().unwrap(), generation);
            assert_eq!(graph.snapshot().unwrap(), before);
            graph.reopen();
            assert_eq!(graph.snapshot().unwrap(), before);
            assert_eq!(graph.generation().unwrap(), generation);
        }
    }
}

#[test]
fn ze57_local_delete_permits_captured_scalars_constants_and_null() {
    for (query, expected) in [
        (
            "MATCH (n) WITH n, n.p AS old DELETE n RETURN old",
            V::Int(7),
        ),
        ("MATCH (n) DELETE n RETURN 1", V::Int(1)),
    ] {
        let mut graph = Graph::new("ze57-delete-scalar");
        graph.setup("CREATE ({p: 7})");
        let result = graph.run(query, &[]).unwrap();
        assert!(matches!(
            result.metadata().outcome,
            Outcome::Committed { .. }
        ));
        assert_eq!(tck::actual_table(&result).1, vec![vec![expected]]);
        let after = graph.snapshot().unwrap();
        assert!(after.nodes.is_empty());
        graph.reopen();
        assert_eq!(graph.snapshot().unwrap(), after);
    }
    let mut graph = Graph::new("ze57-delete-null");
    let before = graph.snapshot().unwrap();
    let generation = graph.generation().unwrap();
    let result = graph
        .run("OPTIONAL MATCH (n) DELETE n RETURN n", &[])
        .unwrap();
    assert_eq!(result.metadata().outcome, Outcome::NoOp);
    assert_eq!(tck::actual_table(&result).1, vec![vec![V::Null]]);
    assert_eq!(graph.generation().unwrap(), generation);
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), before);
    assert_eq!(graph.generation().unwrap(), generation);
}

#[path = "support/faults.rs"]
mod faults;

#[test]
fn ze57_local_limit_zero_and_skip_keep_writes() {
    let mut graph = Graph::new("ze57-limit");
    let before = graph.snapshot().unwrap();
    let result = graph.run("CREATE (:X) RETURN 1 LIMIT 0", &[]).unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));
    assert_eq!(result.metadata().rows, 0);
    let created = graph.snapshot().unwrap();
    assert_eq!(
        graph::Snapshot::diff(&before, &created),
        [("+nodes", 1), ("+labels", 1)].into()
    );
    let result = graph
        .run("MATCH (n:X) SET n.v = 1 RETURN n SKIP 1", &[])
        .unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));
    assert_eq!(result.metadata().rows, 0);
    let after = graph.snapshot().unwrap();
    assert_eq!(
        graph::Snapshot::diff(&created, &after),
        [("+properties", 1)].into()
    );
    assert_eq!(
        tck::actual_table(&graph.run("MATCH (n:X) RETURN n.v", &[]).unwrap()).1,
        vec![vec![V::Int(1)]]
    );
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), after);
}

#[test]
fn ze57_local_late_row_capacity_refuses_the_whole_write() {
    // Default-budget probe: 11x11 is the largest committing square;
    // 12..=32 hit read-work Limit, and 33 hits the query memory Limit
    // (before ZE-51's chunked blocking rows it was fixed-row InvalidPlan).
    for (count, refusal) in [
        (33, Some(GraphQueryErrorKind::Limit)),
        (12, Some(GraphQueryErrorKind::Limit)),
        (11, None),
    ] {
        let mut graph = Graph::new("ze57-capacity");
        graph.setup(&format!("CREATE {}", vec!["()"; count].join(", ")));
        let before = graph.snapshot().unwrap();
        let generation = graph.generation().unwrap();
        let result = graph.run("MATCH (a), (b) CREATE (:C)", &[]);
        let expected = if let Some(kind) = refusal {
            match result {
                Err(StatementError::Query(error)) => {
                    // ZE-208 owns fixed-row InvalidPlan -> Limit;
                    // ZE-210 item 1: driven rows count even without RETURN.
                    assert_eq!(error.kind(), kind, "{count}x{count}: {error}");
                    assert!(error.nothing_committed());
                }
                Err(error) => panic!("unexpected {error:?}"),
                Ok(_) => panic!("{count}x{count} must refuse"),
            }
            assert_eq!(graph.generation().unwrap(), generation);
            assert_eq!(graph.snapshot().unwrap(), before);
            0
        } else {
            assert!(matches!(
                result.unwrap().metadata().outcome,
                Outcome::Committed { .. }
            ));
            (count * count) as i64
        };
        graph.reopen();
        if refusal.is_some() {
            assert_eq!(graph.snapshot().unwrap(), before);
            assert_eq!(graph.generation().unwrap(), generation);
        }
        assert_eq!(
            tck::actual_table(&graph.run("MATCH (n:C) RETURN count(n)", &[]).unwrap()).1,
            vec![vec![V::Int(expected)]]
        );
    }
}

#[test]
fn ze57_local_indeterminate_commit_reopens_to_one_permissible_state() {
    use std::sync::Arc;
    use zeppelin_embed::lifecycle::SystemMonotonicClock;
    use zeppelin_embed::lifecycle::{Store, StoreTestDependencies};
    use zeppelin_embed::vfs::StdVfs;
    let mut graph = Graph::new("ze57-indeterminate");
    graph.store.take().unwrap().close().unwrap();
    let vfs = Arc::new(faults::FailingAppendVfs::new(StdVfs));
    let dependencies = || StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock));
    // Exercise the create helper on a separate directory, then reopen the
    // fixture through the injected open helper before arming its WAL handle.
    Store::create_graph_store_with_test_dependencies(
        graph.root.join("control"),
        graph::store_options(),
        dependencies(),
    )
    .unwrap()
    .close()
    .unwrap();
    graph.store = Some(
        Store::open_graph_store_with_test_dependencies(
            graph.root.join("graph"),
            graph::store_options(),
            dependencies(),
        )
        .unwrap(),
    );
    graph.setup("CREATE ({v: 1})");
    let before = graph.snapshot().unwrap();
    let mut after = graph.snapshot().unwrap();
    for (_, properties) in after.nodes.values_mut() {
        properties.insert("v".into(), V::Int(2));
    }
    vfs.arm(1);
    match graph.run("MATCH (n) SET n.v = 2", &[]) {
        Err(StatementError::Query(error)) => {
            assert_eq!(
                error.kind(),
                GraphQueryErrorKind::WriteIndeterminate,
                "{error}"
            );
            assert!(!error.nothing_committed());
        }
        Err(error) => panic!("unexpected {error:?}"),
        Ok(_) => panic!("armed append did not refuse"),
    }
    assert_eq!(vfs.fired(), 1);
    match graph.run("CREATE ()", &[]) {
        Err(StatementError::Query(error)) => {
            assert_eq!(error.kind(), GraphQueryErrorKind::Unavailable)
        }
        Err(error) => panic!("unexpected {error:?}"),
        Ok(_) => panic!("stopped store accepted statement"),
    }
    graph.reopen(); // Plain StdVfs.
    let recovered = graph.snapshot().unwrap();
    assert!(recovered == before || recovered == after);
    println!(
        "ZE-57 recovery chose {}",
        if recovered == before {
            "before"
        } else {
            "after"
        }
    );
    // Disarmed control runs through the same injected VFS and statement.
    graph.store.take().unwrap().close().unwrap();
    graph.store = Some(
        Store::open_graph_store_with_test_dependencies(
            graph.root.join("graph"),
            graph::store_options(),
            dependencies(),
        )
        .unwrap(),
    );
    graph.run("MATCH (n) SET n.v = 1", &[]).unwrap();
    graph.setup("MATCH (n) SET n.v = 2");
    assert_eq!(vfs.fired(), 1);
    let control = graph.snapshot().unwrap();
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), control);
}
