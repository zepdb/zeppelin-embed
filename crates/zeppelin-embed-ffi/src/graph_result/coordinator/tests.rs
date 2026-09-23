#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use super::super::audit;
use super::super::tests::with_context;
use super::*;
use std::num::NonZeroU64;
use zeppelin_embed::property_graph::{EntityId, GraphGeneration, GraphRevision, NodeId, RelId};

static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);

fn c_node(high: u64, low: u64, revision: u64, generation: u64) -> ZeGraphNode {
    // Scalar/range/pointer-only C struct; all-zero is a valid value.
    let mut node: ZeGraphNode = unsafe { std::mem::zeroed() };
    node.id = ZeNodeId { high, low };
    node.revision = revision;
    node.last_change_generation = generation;
    node
}
fn c_relationship(high: u64, low: u64, revision: u64, generation: u64) -> ZeGraphRelationship {
    let mut relationship: ZeGraphRelationship = unsafe { std::mem::zeroed() };
    relationship.id = ZeRelId { high, low };
    relationship.revision = revision;
    relationship.last_change_generation = generation;
    relationship
}
fn node_receipt(high: u64, low: u64, revision: u64, generation: u64) -> ItemReceipt {
    ItemReceipt {
        entity: EntityId::Node(NodeId::new((u128::from(high) << 64) | u128::from(low)).unwrap()),
        revision: GraphRevision::new(revision).unwrap(),
        generation: GraphGeneration::new(generation),
        replayed: false,
    }
}
fn rel_receipt(high: u64, low: u64, revision: u64, generation: u64) -> ItemReceipt {
    ItemReceipt {
        entity: EntityId::Relationship(
            RelId::new((u128::from(high) << 64) | u128::from(low)).unwrap(),
        ),
        revision: GraphRevision::new(revision).unwrap(),
        generation: GraphGeneration::new(generation),
        replayed: false,
    }
}
unsafe fn slice<'a, T>(pointer: *const T, count: usize) -> &'a [T] {
    if count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(pointer, count) }
    }
}

/// Ascending-id pools at admitted generation 10: three nodes and two
/// relationships whose provisional stamps are the admitted records.
fn pending_fixture(context: &mut RuntimeContext<'_, '_, '_>) -> PendingResponse {
    let nodes = [c_node(0, 5, 1, 4), c_node(1, 2, 2, 10), c_node(1, 9, 3, 10)];
    let relationships = [c_relationship(0, 3, 1, 9), c_relationship(2, 1, 5, 10)];
    let parts = ResponseParts {
        nodes: &nodes,
        relationships: &relationships,
        ..ResponseParts::default()
    };
    REGISTRY
        .prepare(context, parts, ResponseMetadata::new(0, Some(10)))
        .unwrap()
        .detach()
}

#[test]
fn graph_result_pending_detach_releases_charge_and_free_rejects_until_settled() {
    with_context(|context| {
        let baseline = context.memory().reserved_bytes();
        let prepared = REGISTRY
            .prepare(
                context,
                ResponseParts {
                    bytes: b"payload",
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, Some(3)),
            )
            .unwrap();
        assert!(context.memory().reserved_bytes() > baseline);
        let allocation_bytes = prepared.allocation_bytes();
        let (pending, detach) = audit::run(0, true, || prepared.detach());
        assert_eq!((detach.attempts, detach.frees), (0, 0));
        assert_eq!(context.memory().reserved_bytes(), baseline);
        assert_eq!(pending.allocation_bytes(), allocation_bytes);

        let mut private = pending.descriptor();
        assert!(matches!(
            REGISTRY.free(&mut private),
            Err(OwnerError::InvalidOwner)
        ));
        assert_eq!(private.owner_token, pending.descriptor().owner_token);
        let stale = pending.descriptor();

        let guarded =
            run_potential_write(|attempt| attempt.settle(pending, &[], WriteSettlement::NoOp));
        assert_eq!(
            guarded.outcome,
            OperationOutcome::Success(SuccessfulOutcome::NoOp)
        );
        let mut published = guarded.value.unwrap();
        assert_eq!(
            published.disposition,
            ZeGraphDisposition::ZeGraphDispositionNoOp as u32
        );
        assert_eq!(
            unsafe { slice(published.pool.bytes, published.pool.byte_count) },
            b"payload"
        );
        // A pre-settle copy is stale after publication: its outcome fields
        // differ, so it cannot steal the published backing.
        let mut stale_copy = stale;
        assert!(matches!(
            REGISTRY.free(&mut stale_copy),
            Err(OwnerError::InvalidOwner)
        ));
        let copied = published;
        let report = REGISTRY.free(&mut published).unwrap();
        assert_eq!(report.released_bytes, allocation_bytes);
        assert_eq!(published.owner_token, 0);
        assert_eq!(REGISTRY.free(&mut published).unwrap().released_bytes, 0);
        let mut second = copied;
        assert!(matches!(
            REGISTRY.free(&mut second),
            Err(OwnerError::InvalidOwner)
        ));
    });
}

#[test]
fn graph_result_pending_outlives_query_and_every_path_is_heap_flat() {
    // The pending owner leaves the query, its memory and its store behind.
    let pending = with_context(pending_fixture);
    let receipts = [node_receipt(1, 2, 7, 11)];
    let committed = WriteSettlement::Committed(NonZeroU64::new(12).unwrap());
    let guarded = run_potential_write(|attempt| attempt.settle(pending, &receipts, committed));
    let mut published = guarded.value.unwrap();
    let nodes = unsafe { slice(published.pool.nodes, published.pool.node_count) };
    assert_eq!(
        (nodes[1].revision, nodes[1].last_change_generation),
        (7, 12)
    );
    REGISTRY.free(&mut published).unwrap();

    with_context(|context| {
        let baseline = context.memory().reserved_bytes();
        let (_, heap) = audit::run(0, false, || {
            for round in 0..32_u64 {
                let pending = pending_fixture(context);
                if round % 2 == 0 {
                    // Abort: a pending owner dropped before settle.
                    drop(pending);
                } else {
                    let guarded = run_potential_write(|attempt| {
                        attempt.settle(pending, &receipts, committed)
                    });
                    let mut published = guarded.value.unwrap();
                    REGISTRY.free(&mut published).unwrap();
                }
            }
        });
        assert_eq!(heap.bytes, 0);
        assert_eq!(heap.allocations, heap.frees);
        assert_eq!(heap.allocations, 64);
        assert_eq!(context.memory().reserved_bytes(), baseline);
    });
}

#[test]
fn graph_result_settle_stamps_changed_entities_and_allocates_nothing() {
    with_context(|context| {
        // Node (1,2) is the binary-search midpoint; (1,9) is the last node.
        // Receipt generations above admitted 10 were staged by this write.
        let receipts = [
            node_receipt(1, 2, 7, 11),
            node_receipt(1, 9, 4, 8),
            rel_receipt(2, 1, 6, 11),
            node_receipt(7, 7, 9, 11), // not in the result
            rel_receipt(0, 4, 9, 11),  // not in the result
        ];
        let committed = WriteSettlement::Committed(NonZeroU64::new(12).unwrap());
        let pending = pending_fixture(context);
        let (guarded, window) = audit::run(0, true, || {
            run_potential_write(|attempt| attempt.settle(pending, &receipts, committed))
        });
        assert_eq!(window.attempts, 0);
        assert_eq!(
            guarded.outcome,
            OperationOutcome::Success(SuccessfulOutcome::Committed(NonZeroU64::new(12).unwrap()))
        );
        let mut published = guarded.value.unwrap();
        assert_eq!(
            (
                published.disposition,
                published.has_changed_generation,
                published.changed_generation,
                published.admitted_generation
            ),
            (
                ZeGraphDisposition::ZeGraphDispositionCommitted as u32,
                1,
                12,
                10
            )
        );
        let nodes = unsafe { slice(published.pool.nodes, published.pool.node_count) };
        let stamps: Vec<_> = nodes
            .iter()
            .map(|node| (node.revision, node.last_change_generation))
            .collect();
        assert_eq!(stamps, [(1, 4), (7, 12), (4, 8)]);
        let relationships = unsafe {
            slice(
                published.pool.relationships,
                published.pool.relationship_count,
            )
        };
        let stamps: Vec<_> = relationships
            .iter()
            .map(|relationship| (relationship.revision, relationship.last_change_generation))
            .collect();
        assert_eq!(stamps, [(1, 9), (6, 12)]);
        REGISTRY.free(&mut published).unwrap();

        // Without a changed generation every receipt keeps its own.
        for (settlement, disposition, outcome) in [
            (
                WriteSettlement::Replayed,
                ZeGraphDisposition::ZeGraphDispositionReplayed,
                SuccessfulOutcome::Replayed,
            ),
            (
                WriteSettlement::NoOp,
                ZeGraphDisposition::ZeGraphDispositionNoOp,
                SuccessfulOutcome::NoOp,
            ),
        ] {
            let pending = pending_fixture(context);
            let guarded =
                run_potential_write(|attempt| attempt.settle(pending, &receipts, settlement));
            assert_eq!(guarded.outcome, OperationOutcome::Success(outcome));
            let mut published = guarded.value.unwrap();
            assert_eq!(
                (
                    published.disposition,
                    published.has_changed_generation,
                    published.changed_generation
                ),
                (disposition as u32, 0, 0)
            );
            let nodes = unsafe { slice(published.pool.nodes, published.pool.node_count) };
            assert_eq!(
                (nodes[1].revision, nodes[1].last_change_generation),
                (7, 11)
            );
            REGISTRY.free(&mut published).unwrap();
        }
    });
}

#[test]
fn graph_result_potential_write_guard_is_indeterminate_until_resolved() {
    // A caught panic before any resolution: unknown, never a stale
    // NotCommitted, and the pending owner is aborted during unwind.
    with_context(|context| {
        // Warm the panic runtime's one-time lazy state outside the audit.
        let warm = run_potential_write(|_| panic!("warm-up"));
        assert_eq!(warm.outcome, OperationOutcome::Indeterminate);
        let pending = pending_fixture(context);
        let bytes = pending.allocation_bytes();
        let (guarded, heap) = audit::run(0, false, || {
            run_potential_write(|_attempt| {
                let _pending = pending;
                panic!("injected interruption before commit");
            })
        });
        assert_eq!(guarded.outcome, OperationOutcome::Indeterminate);
        assert_eq!(guarded.value.unwrap_err(), WriteInterrupted::Panicked);
        assert_eq!(heap.bytes, -(bytes as isize));
    });

    // An error return that does not resolve the attempt is also unknown.
    let guarded = run_potential_write(|_attempt| {
        Err::<(), _>("commit failed after it may have been durable")
    });
    assert_eq!(guarded.outcome, OperationOutcome::Indeterminate);
    assert!(guarded.value.unwrap().is_err());

    // Only a proven no-effect result reports NotCommitted.
    let guarded = run_potential_write(|attempt| attempt.no_effect());
    assert_eq!(guarded.outcome, OperationOutcome::NotCommitted);
    assert!(guarded.value.is_ok());

    // A known outcome recorded by settle survives a later panic.
    with_context(|context| {
        let pending = pending_fixture(context);
        let committed = WriteSettlement::Committed(NonZeroU64::new(12).unwrap());
        let token = pending.descriptor().owner_token;
        let delivered = std::cell::Cell::new(None);
        let guarded = run_potential_write(|attempt| {
            delivered.set(Some(attempt.settle(pending, &[], committed)));
            panic!("injected delivery failure after commit");
        });
        assert_eq!(
            guarded.outcome,
            OperationOutcome::Success(SuccessfulOutcome::Committed(NonZeroU64::new(12).unwrap()))
        );
        assert_eq!(guarded.value.unwrap_err(), WriteInterrupted::Panicked);
        let mut published = delivered.take().unwrap();
        assert_eq!(published.owner_token, token);
        REGISTRY.free(&mut published).unwrap();
    });
}
