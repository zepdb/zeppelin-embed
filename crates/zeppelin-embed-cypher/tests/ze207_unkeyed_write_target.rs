//! ZE-207: a Cypher CREATE admits an entity with no application key. Before
//! this fix, `lifecycle/native_graph/base.rs`'s `cached_from_parts` (the
//! ZE-52 slice B lazy base-target loader) forced every lazily loaded target
//! to have a key and refused an unkeyed one as `Corruption`, so no later
//! Cypher SET, REMOVE, DELETE or property read could reach an entity Cypher
//! itself had created. These are the ticket's own reproduction scenarios,
//! run through the real `zeppelin_embed_cypher::execute` seam against a real
//! native graph store, with reopen checks proving the write actually
//! committed rather than merely returning `Ok`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::result_large_err
)]
#[path = "support/graph.rs"]
mod graph;
mod support;
#[path = "support/tck.rs"]
mod tck;

use graph::Graph;
use tck::V;
use zeppelin_embed::property_graph::query::QueryValue;
use zeppelin_embed::property_graph::query::completed::Outcome;
use zeppelin_embed::property_graph::query::plan::ParameterBinding;

/// Scenario 1: `CREATE (:A {v: 1})` then `MATCH (a:A) SET a.v = 2` must
/// commit, and a reopen must show the updated value, not the original one.
#[test]
fn ze207_set_on_a_cypher_created_unkeyed_node_commits_and_reopens() {
    let mut graph = Graph::new("ze207-set");
    graph.setup("CREATE (:A {v: 1})");

    let result = graph.run("MATCH (a:A) SET a.v = 2", &[]).unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));

    graph.reopen();
    let read = graph.run("MATCH (a:A) RETURN a.v AS v", &[]).unwrap();
    assert_eq!(tck::actual_table(&read).1, vec![vec![V::Int(2)]]);
}

/// Scenario 2: a SET on an unkeyed matched node must commit and RETURN the
/// value it just wrote, not merely avoid erroring.
#[test]
fn ze207_set_on_unkeyed_node_returns_the_new_value() {
    let graph = Graph::new("ze207-set-return");
    graph.setup("CREATE (:A {v: 1})");

    let result = graph
        .run("MATCH (a:A) SET a.w = 3 RETURN a.w AS w", &[])
        .unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));
    assert_eq!(tck::actual_table(&result).1, vec![vec![V::Int(3)]]);
}

/// Scenario 3: `MATCH (a:A) DELETE a` on an unkeyed node must commit, and a
/// reopen must show the node gone.
#[test]
fn ze207_delete_of_unkeyed_node_commits_and_reopens_gone() {
    let mut graph = Graph::new("ze207-delete");
    graph.setup("CREATE (:A {v: 1})");

    let result = graph.run("MATCH (a:A) DELETE a", &[]).unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));

    graph.reopen();
    let read = graph.run("MATCH (a:A) RETURN a.v AS v", &[]).unwrap();
    assert_eq!(tck::actual_table(&read).1, Vec::<Vec<V>>::new());
}

/// Scenario 4: `MATCH (a:A) CREATE (:B {v: a.v / $d})` reads a property of
/// the unkeyed matched node under the writer admission (not just a write to
/// it). The property read must see the value Cypher itself wrote.
#[test]
fn ze207_create_reads_a_property_of_the_unkeyed_matched_node() {
    let graph = Graph::new("ze207-create-read");
    graph.setup("CREATE (:A {v: 10})");

    let two = [ParameterBinding {
        name: "d",
        value: QueryValue::I64(2),
    }];
    let result = graph
        .run("MATCH (a:A) CREATE (:B {v: a.v / $d})", &two)
        .unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));

    let read = graph.run("MATCH (b:B) RETURN b.v AS v", &[]).unwrap();
    assert_eq!(tck::actual_table(&read).1, vec![vec![V::Int(5)]]);
}

/// Scenario 5 (regression): `MATCH (a:A) CREATE (:B)` never loads the
/// matched node as a target at all, so it must keep committing exactly as it
/// did before this fix.
#[test]
fn ze207_create_without_reading_matched_node_still_commits() {
    let graph = Graph::new("ze207-create-no-read");
    graph.setup("CREATE (:A {v: 1})");

    let result = graph.run("MATCH (a:A) CREATE (:B)", &[]).unwrap();
    assert!(matches!(
        result.metadata().outcome,
        Outcome::Committed { .. }
    ));

    let read = graph.run("MATCH (b:B) RETURN count(b) AS n", &[]).unwrap();
    assert_eq!(tck::actual_table(&read).1, vec![vec![V::Int(1)]]);
}

#[test]
fn ze214_keyed_structured_create_then_cypher_set_delete() {
    use zeppelin_embed::graph_structured_write_test_support::create_keyed_node;
    let mut graph = Graph::new("ze214-keyed");
    let (node, created) = create_keyed_node(graph.store()).unwrap();
    assert_eq!(graph.generation().unwrap(), created);
    assert_eq!(graph.snapshot().unwrap().nodes.len(), 1);
    let identity = graph.run("MATCH (a:A) RETURN a", &[]).unwrap();
    assert_eq!(identity.pools().nodes.len(), 1);
    assert_eq!(identity.pools().nodes.first().unwrap().id, node);
    let set = graph
        .run("MATCH (a:A) SET a.v = 2 RETURN a.v", &[])
        .unwrap();
    assert!(
        matches!(set.metadata().outcome, Outcome::Committed { changed } if changed.get() == created.get() + 1)
    );
    assert_eq!(tck::actual_table(&set).1, vec![vec![V::Int(2)]]);
    drop(set);
    drop(identity);
    let updated = graph.snapshot().unwrap();
    graph.reopen();
    assert_eq!(graph.snapshot().unwrap(), updated);
    assert_eq!(
        tck::actual_table(&graph.run("MATCH (a:A) RETURN a.v", &[]).unwrap()).1,
        vec![vec![V::Int(2)]]
    );
    let generation = graph.generation().unwrap();
    let deleted = graph.run("MATCH (a:A) DELETE a", &[]).unwrap();
    // This changing write may run maintenance before its final admission.
    let admitted = deleted.metadata().generation;
    assert!(admitted >= generation);
    assert!(
        matches!(deleted.metadata().outcome, Outcome::Committed { changed } if changed.get() == admitted.get() + 1)
    );
    drop(deleted);
    graph.reopen();
    assert!(graph.snapshot().unwrap().nodes.is_empty());
}
