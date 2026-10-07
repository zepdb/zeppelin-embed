//! ZE-169 differential tests: the ZE-50 pattern bags must survive a native
//! storage maintenance generation and a close/reopen, not just a freshly
//! written graph.
//!
//! ZE-50 proved `NativePattern` against the independent tiny-graph oracle on a
//! graph whose records had never moved. ZE-46 then added bounded consolidation,
//! which relocates node records into new packs and merges pending adjacency
//! deltas into bounded bases. Nothing executed a pattern across that seam. Each
//! case below reuses the exact ZE-50 fixture, pattern and oracle, computes the
//! expected bag once, and then requires that bag to stay byte-identical while
//! every underlying physical reference is replaced underneath it.
//!
//! The DETACH case is scoped to query invisibility. ZE-166's physical sweep has
//! not landed, so a detached node's incident relationship bytes are still on
//! disk; the contract proved here is that no expansion and no lookup can reach
//! them. A post-sweep physical-absence case belongs to a follow-up ticket.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use super::oracle_tests::oracle::{Cell, Direction as TinyDirection, Predicate, TinyPattern};
use super::oracle_tests::{Fixture, agreed, bounded, expand, fixture, lookup, oracle, row, scan};
use crate::lifecycle::native_graph::NativeGraphError;
use crate::lifecycle::native_graph::tests::consolidation::node_directory_value;
use crate::lifecycle::{CancelToken, QueryControl, Store};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite};
use crate::property_graph::{
    ApplicationKey, EntityId, EntityKind, GraphDeleteMode, GraphRevision, NodeId,
};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Maintenance driver
// ---------------------------------------------------------------------------

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

/// The number of consolidation rounds after which every live node record has been
/// relocated and every pending adjacency range in each direction has been
/// merged at least once.
///
/// One consolidating `commit_native_graph_maintenance` call relocates exactly
/// one live node record (`storage/consolidation.rs::select_live_node` takes the
/// lowest `(pack serial, node id)` among the oldest packs, and the moved record
/// lands in the newest pack, so selection rotates) and merges at most one OUT
/// range and one IN range
/// (`adjacency/prepare/ranges.rs::consolidate_pending_range`
/// takes the range with the highest pending count). An adjacency range is keyed
/// by `(node, relationship type, lower bound)`, so a fixture this small has one
/// range per distinct `(endpoint, type)` pair per direction.
///
/// The arithmetic is only the starting point: `prove_across_maintenance`
/// separately proves every node's directory value actually changed.
fn maintenance_rounds(graph: &oracle::Graph) -> usize {
    let mut outgoing = BTreeSet::new();
    let mut incoming = BTreeSet::new();
    for edge in &graph.edges {
        outgoing.insert((edge.source, edge.relationship_type.as_str()));
        incoming.insert((edge.target, edge.relationship_type.as_str()));
    }
    graph
        .nodes
        .len()
        .max(outgoing.len())
        .max(incoming.len())
        .max(1)
}

/// Requires one consolidation round, allowing the two reclaim-only phases
/// (drain, retire) that may precede it. ZE-380's fold before capture can arm
/// reclaim before the fixture's explicit halfway checkpoint.
/// A stale admission is retried once; every other refusal is a failure.
fn consolidate_once(store: &Store, round: usize) -> u64 {
    for phase in 0..3 {
        for attempt in 0..2 {
            let admission = store
                .admit_native_graph_maintenance()
                .expect("maintenance admission");
            match store.commit_native_graph_maintenance(&admission, &control()) {
                Ok(report) if report.replaced_physical_refs > 0 => {
                    return report.replaced_physical_refs;
                }
                Ok(_) => break,
                Err(NativeGraphError::StalePreparation) => {
                    assert!(
                        attempt == 0,
                        "round {round}, phase {phase} stayed stale across two attempts"
                    );
                }
                Err(error) => panic!("round {round}, phase {phase} failed: {error:?}"),
            }
        }
    }
    panic!("round {round} replaced no physical reference within three maintenance phases")
}

/// The raw node-directory value (the physical record reference) of every id.
fn node_records(fixture: &Fixture, nodes: &[u128]) -> Vec<Vec<u8>> {
    let lease = fixture
        .store
        .admit_native_read()
        .expect("node record reader");
    nodes
        .iter()
        .map(|id| {
            node_directory_value(
                &fixture.store,
                &lease,
                NodeId::new(*id).expect("node identity"),
            )
        })
        .collect()
}

/// Asserts production still agrees with the oracle and still returns the exact
/// bag captured before any maintenance ran.
fn still_exact(fixture: &Fixture, patterns: &[TinyPattern], expected: &[oracle::Bag], stage: &str) {
    for (index, (pattern, bag)) in patterns.iter().zip(expected).enumerate() {
        assert_eq!(
            &agreed(fixture, pattern),
            bag,
            "pattern {index} changed its bag {stage}"
        );
    }
}

/// The shared ZE-169 contract: the bag of every pattern is exactly the bag the
/// fresh graph produced, after `2K` consolidation rounds with a checkpoint at the
/// halfway point, and again after a close/reopen.
///
/// `relocated` names the node ids whose physical record must have moved within
/// the first `K` rounds. A tombstoned node is never selected for relocation, so
/// it is deliberately excluded by the caller.
fn prove_across_maintenance(
    fixture: &mut Fixture,
    relocated: &[u128],
    rounds: usize,
    patterns: &[TinyPattern],
) {
    assert!(!patterns.is_empty(), "a case proves at least one pattern");
    assert!(rounds > 0, "a case runs at least one maintenance call");
    let expected = patterns
        .iter()
        .map(|pattern| agreed(fixture, pattern))
        .collect::<Vec<oracle::Bag>>();
    let original = node_records(fixture, relocated);

    // Require K actual consolidations, including when reclaim is armed early.
    for call in 0..rounds {
        assert!(
            consolidate_once(&fixture.store, call) > 0,
            "consolidation round {call} replaced no physical reference"
        );
    }
    for (index, (now, before)) in node_records(fixture, relocated)
        .iter()
        .zip(&original)
        .enumerate()
    {
        assert_ne!(
            now, before,
            "node {index} kept its physical record through {rounds} consolidation rounds"
        );
    }
    still_exact(fixture, patterns, &expected, "after the first half");

    // Run another K consolidations after an explicit checkpoint, then verify
    // close/reopen against the same expected bag.
    fixture
        .store
        .checkpoint_native_graph(&control())
        .expect("checkpoint between maintenance halves");
    let mut replaced_after_checkpoint = 0_u64;
    for call in rounds..rounds.saturating_mul(2) {
        replaced_after_checkpoint =
            replaced_after_checkpoint.saturating_add(consolidate_once(&fixture.store, call));
    }
    assert!(
        replaced_after_checkpoint > 0,
        "the {rounds} consolidation rounds after the checkpoint replaced no physical reference"
    );
    still_exact(fixture, patterns, &expected, "after maintenance");

    fixture.reopen();
    still_exact(fixture, patterns, &expected, "after reopen");
}

// ---------------------------------------------------------------------------
// The ZE-50 case list, across a maintenance generation
// ---------------------------------------------------------------------------

/// Every node id the fixture committed, in fixture order.
fn every_node(fixture: &Fixture) -> Vec<u128> {
    fixture.graph.nodes.iter().map(|node| node.id).collect()
}

#[test]
fn ze169_parallel_and_self_edges_survive_maintenance() {
    let mut fixture = fixture(
        "generation-parallel",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS"), (0, 0, "LINKS")],
    );
    let nodes = every_node(&fixture);
    let patterns = [
        expand(scan(0), 0, 1, 2, TinyDirection::Out, 0),
        expand(lookup(0, fixture.node(0)), 0, 1, 2, TinyDirection::Out, 0),
    ];
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    fixture.close();
}

#[test]
fn ze169_undirected_self_loop_survives_maintenance() {
    let mut fixture = fixture(
        "generation-undirected",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS"), (0, 0, "LINKS")],
    );
    let nodes = every_node(&fixture);
    let self_loop = fixture.relationship(2);
    let patterns = [expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0)];
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    let bag = agreed(&fixture, &patterns[0]);
    let visits = bag
        .iter()
        .filter(|row| {
            row.iter()
                .any(|(_, cell)| *cell == Cell::Relationship(self_loop))
        })
        .count();
    assert_eq!(
        visits, 1,
        "a consolidated undirected self-loop is still visited exactly once"
    );
    assert_eq!(bag.len(), 5, "two parallel edges seen from both endpoints");
    fixture.close();
}

#[test]
fn ze169_zero_hops_survive_maintenance() {
    let mut fixture = fixture("generation-zero-hops", &[&[], &[]], &[(0, 1, "LINKS")]);
    let nodes = every_node(&fixture);
    let patterns = [bounded(scan(0), 0, 1, 2, 0, 0, TinyDirection::Out, 0)];
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    let mut expected = oracle::bag(
        nodes
            .iter()
            .map(|node| {
                row(&[
                    (0, Cell::Node(*node)),
                    (1, Cell::Node(*node)),
                    (2, Cell::Relationships(Vec::new())),
                ])
            })
            .collect(),
    );
    expected.sort();
    assert_eq!(
        agreed(&fixture, &patterns[0]),
        expected,
        "depth zero still emits [start, start, []]"
    );
    fixture.close();
}

#[test]
fn ze169_variable_relationship_lists_survive_maintenance() {
    let mut fixture = fixture(
        "generation-variable",
        &[&[], &[], &[]],
        &[
            (0, 1, "LINKS"),
            (1, 2, "LINKS"),
            (2, 0, "LINKS"),
            (0, 1, "LINKS"),
        ],
    );
    let nodes = every_node(&fixture);
    let patterns = [(0_u8, 2_u8), (1, 1), (2, 3)]
        .map(|(min, max)| bounded(scan(0), 0, 1, 2, min, max, TinyDirection::Out, 0));
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    for (pattern, (min, max)) in patterns.iter().zip([(0_u8, 2_u8), (1, 1), (2, 3)]) {
        let bag = agreed(&fixture, pattern);
        assert!(!bag.is_empty(), "variable-length bag for [{min}, {max}]");
        for row in &bag {
            let length = row
                .iter()
                .find_map(|(slot, cell)| match (slot, cell) {
                    (2, Cell::Relationships(path)) => Some(path.len()),
                    _ => None,
                })
                .expect("path cell");
            assert!(
                length >= usize::from(min) && length <= usize::from(max),
                "path length {length} outside [{min}, {max}]"
            );
        }
    }
    fixture.close();
}

#[test]
fn ze169_repeated_nodes_no_repeated_relationship_survives_maintenance() {
    let mut fixture = fixture(
        "generation-repeated",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS")],
    );
    let nodes = every_node(&fixture);
    let two_step = expand(
        expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0),
        1,
        3,
        4,
        TinyDirection::Undirected,
        0,
    );
    let distinct = TinyPattern::Filter {
        input: Box::new(two_step.clone()),
        predicate: Predicate::Not(Box::new(Predicate::Same { left: 2, right: 4 })),
    };
    let patterns = [two_step, distinct];
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    let bag = agreed(&fixture, &patterns[0]);
    assert!(!bag.is_empty(), "two-step bag is populated");
    for row in &bag {
        let first = row
            .iter()
            .find_map(|(slot, cell)| (*slot == 2).then(|| cell.clone()))
            .expect("first relationship");
        let second = row
            .iter()
            .find_map(|(slot, cell)| (*slot == 4).then(|| cell.clone()))
            .expect("second relationship");
        assert_ne!(
            first, second,
            "one consolidated pattern never reuses a relationship"
        );
    }
    assert_eq!(
        agreed(&fixture, &patterns[1]),
        bag,
        "an explicit distinct-relationship filter still removes nothing"
    );
    fixture.close();
}

#[test]
fn ze169_subsequent_match_reuse_survives_maintenance() {
    let mut fixture = fixture(
        "generation-subsequent",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS")],
    );
    let nodes = every_node(&fixture);
    let same = expand(
        expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0),
        1,
        3,
        4,
        TinyDirection::Undirected,
        0,
    );
    let subsequent = expand(
        expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0),
        1,
        3,
        4,
        TinyDirection::Undirected,
        1,
    );
    let reused_rows = TinyPattern::Filter {
        input: Box::new(subsequent.clone()),
        predicate: Predicate::Same { left: 2, right: 4 },
    };
    let patterns = [same, subsequent, reused_rows];
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    let same_bag = agreed(&fixture, &patterns[0]);
    let subsequent_bag = agreed(&fixture, &patterns[1]);
    let reused = subsequent_bag
        .iter()
        .filter(|row| {
            let first = row
                .iter()
                .find(|(slot, _)| *slot == 2)
                .map(|(_, cell)| cell);
            let second = row
                .iter()
                .find(|(slot, _)| *slot == 4)
                .map(|(_, cell)| cell);
            first == second
        })
        .count();
    assert!(
        reused > 0,
        "a subsequent pattern match still gets a fresh uniqueness set"
    );
    assert!(
        subsequent_bag.len() > same_bag.len(),
        "the fresh set still admits rows the first pattern rejects"
    );
    assert_eq!(
        agreed(&fixture, &patterns[2]).len(),
        reused,
        "the reused-relationship rows are exactly the rows the filter keeps"
    );
    fixture.close();
}

#[test]
fn ze169_optional_attached_where_survives_maintenance() {
    let mut fixture = fixture(
        "generation-optional",
        &[&[], &["Tag"], &[]],
        &[(0, 1, "LINKS"), (0, 2, "LINKS")],
    );
    let nodes = every_node(&fixture);
    let tagged = TinyPattern::Optional {
        left: Box::new(scan(0)),
        right: Box::new(expand(TinyPattern::Anchor, 0, 1, 2, TinyDirection::Out, 1)),
        predicate: Some(Predicate::HasLabel {
            slot: 1,
            label: String::from("Tag"),
        }),
    };
    let negated = TinyPattern::Optional {
        left: Box::new(scan(0)),
        right: Box::new(expand(TinyPattern::Anchor, 0, 1, 2, TinyDirection::Out, 1)),
        predicate: Some(Predicate::Not(Box::new(Predicate::HasLabel {
            slot: 1,
            label: String::from("Tag"),
        }))),
    };
    let patterns = [tagged, negated];
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    let mut expected = oracle::bag(vec![
        row(&[
            (0, Cell::Node(fixture.node(0))),
            (1, Cell::Node(fixture.node(1))),
            (2, Cell::Relationship(fixture.relationship(0))),
        ]),
        row(&[
            (0, Cell::Node(fixture.node(1))),
            (1, Cell::Null),
            (2, Cell::Null),
        ]),
        row(&[
            (0, Cell::Node(fixture.node(2))),
            (1, Cell::Null),
            (2, Cell::Null),
        ]),
    ]);
    expected.sort();
    assert_eq!(
        agreed(&fixture, &patterns[0]),
        expected,
        "an unmatched left row still keeps one row with null new slots"
    );
    fixture.close();
}

#[test]
fn ze169_disconnected_patterns_survive_maintenance() {
    let mut fixture = fixture(
        "generation-disconnected",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS")],
    );
    let nodes = every_node(&fixture);
    let independent = TinyPattern::Join {
        left: Box::new(expand(scan(0), 0, 1, 2, TinyDirection::Out, 0)),
        right: Box::new(expand(scan(3), 3, 4, 5, TinyDirection::Out, 1)),
    };
    let shared_scope = TinyPattern::Join {
        left: Box::new(expand(scan(0), 0, 1, 2, TinyDirection::Out, 0)),
        right: Box::new(expand(scan(3), 3, 4, 5, TinyDirection::Out, 0)),
    };
    let patterns = [independent, shared_scope];
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    assert_eq!(
        agreed(&fixture, &patterns[0]).len(),
        4,
        "disjoint patterns still form the cross product"
    );
    assert_eq!(
        agreed(&fixture, &patterns[1]).len(),
        2,
        "uniqueness inside one pattern still survives the join"
    );
    fixture.close();
}

#[test]
fn ze169_start_join_permutations_survive_maintenance() {
    let mut fixture = fixture(
        "generation-permutations",
        &[&[], &[], &[]],
        &[
            (0, 1, "LINKS"),
            (1, 2, "LINKS"),
            (0, 1, "LINKS"),
            (2, 0, "LINKS"),
        ],
    );
    let nodes = every_node(&fixture);
    let forward_left = expand(scan(0), 0, 1, 2, TinyDirection::Out, 0);
    let reverse_left = expand(scan(1), 1, 0, 2, TinyDirection::In, 0);
    let forward_right = expand(scan(1), 1, 3, 4, TinyDirection::Out, 0);
    let reverse_right = expand(scan(3), 3, 1, 4, TinyDirection::In, 0);
    let patterns = [
        TinyPattern::Join {
            left: Box::new(forward_left.clone()),
            right: Box::new(forward_right.clone()),
        },
        TinyPattern::Join {
            left: Box::new(forward_right.clone()),
            right: Box::new(forward_left.clone()),
        },
        TinyPattern::Join {
            left: Box::new(reverse_left.clone()),
            right: Box::new(forward_right),
        },
        TinyPattern::Join {
            left: Box::new(forward_left),
            right: Box::new(reverse_right.clone()),
        },
        TinyPattern::Join {
            left: Box::new(reverse_right),
            right: Box::new(reverse_left),
        },
    ];
    let rounds = maintenance_rounds(&fixture.graph);
    prove_across_maintenance(&mut fixture, &nodes, rounds, &patterns);
    let bags = patterns
        .iter()
        .map(|pattern| agreed(&fixture, pattern))
        .collect::<Vec<oracle::Bag>>();
    let first = bags.first().expect("one permutation");
    assert!(!first.is_empty(), "permutation bag is populated");
    for bag in &bags {
        assert_eq!(
            bag, first,
            "every legal start and build-side choice still yields the identical bag"
        );
    }
    fixture.close();
}

// ---------------------------------------------------------------------------
// DETACH: hidden, not yet physically swept
// ---------------------------------------------------------------------------

/// A DETACH-deleted node and its incident relationships are unreachable from
/// every expansion direction and from a direct lookup, and stay unreachable
/// across a maintenance generation and a reopen.
///
/// ZE-166's physical sweep has not landed, so the detached node's relationship
/// bytes are still present on disk. This case proves query invisibility only;
/// it makes no claim about physical absence.
#[test]
fn ze169_detached_node_stays_invisible_across_maintenance() {
    let mut fixture = fixture(
        "generation-detach",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS"), (0, 0, "LINKS")],
    );
    let kept = fixture.node(0);
    let detached = fixture.node(1);
    // Derived from the whole written graph, before DETACH shrinks the oracle.
    let rounds = maintenance_rounds(&fixture.graph);

    let before = agreed(&fixture, &expand(scan(0), 0, 1, 2, TinyDirection::Out, 0));
    assert_eq!(
        before.len(),
        3,
        "both parallel relationships and the self-loop are reachable first"
    );

    fixture
        .store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "generation-detach", "n1")
                    .expect("detach key"),
                revision: GraphRevision::new(2).expect("detach revision"),
                operation: StructuredOperation::Delete(
                    EntityId::Node(NodeId::new(detached).expect("detached identity")),
                    GraphDeleteMode::Detach,
                ),
                image: None,
            }],
            &control(),
        )
        .expect("detach the second node");

    // The oracle describes the graph a query may see: the detached node and
    // every relationship incident to it are gone from it.
    fixture.graph.nodes.retain(|node| node.id != detached);
    fixture
        .graph
        .edges
        .retain(|edge| edge.source != detached && edge.target != detached);

    let patterns = [
        expand(scan(0), 0, 1, 2, TinyDirection::Out, 0),
        expand(scan(0), 0, 1, 2, TinyDirection::In, 0),
        expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0),
        lookup(0, detached),
        expand(lookup(0, detached), 0, 1, 2, TinyDirection::Undirected, 0),
    ];

    // A tombstoned record is never selected for relocation, so only the kept
    // node has to move. The call budget still comes from the whole written
    // graph, not from the smaller graph a query may now see.
    prove_across_maintenance(&mut fixture, &[kept], rounds, &patterns);

    let out = agreed(&fixture, &patterns[0]);
    assert_eq!(
        out.len(),
        1,
        "only the surviving self-loop remains reachable"
    );
    assert_eq!(
        out[0],
        row(&[
            (0, Cell::Node(kept)),
            (1, Cell::Node(kept)),
            (2, Cell::Relationship(fixture.relationship(2))),
        ]),
        "the kept node's self-loop is the exact surviving row"
    );
    assert!(
        agreed(&fixture, &patterns[3]).is_empty(),
        "a direct lookup of the detached node returns nothing"
    );
    fixture.close();
}
