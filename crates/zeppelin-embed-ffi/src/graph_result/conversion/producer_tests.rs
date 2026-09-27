//! ZE-68 Slice A: the FFI's coordinator/conversion machinery wired to a
//! real `GraphStore`, not a synthetic `ResultSource`. Every test here opens
//! an actual native graph store and drives it through `apply_and_settle`/
//! `run_query`.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests fail loudly on the first broken contract"
)]

use super::*;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions};
use zeppelin_embed::property_graph::query::plan::{
    ExprId, Expression, Operator, OperatorKind, Parameter, PlanNodeId, Projection, SlotId,
};
use zeppelin_embed::property_graph::staging::WriteImage;
use zeppelin_embed::property_graph::{
    ApplicationKey, CanonicalContents, EntityKind, GraphDeleteMode, GraphPlanBacking,
    GraphRevision, NodeId,
};

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn store_options() -> OpenOptions {
    OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024)
}

fn node_key(key: &str) -> ApplicationKey<'_> {
    ApplicationKey::new(EntityKind::Node, "ze68-a", key).expect("node key")
}

fn revision(value: u64) -> GraphRevision {
    GraphRevision::new(value).expect("positive revision")
}

fn create_request<'a>(key: &'a str, image: &'a CanonicalContents<'a>) -> StructuredWrite<'a, 'a> {
    let _ = key;
    StructuredWrite {
        key: node_key(key),
        revision: revision(1),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(image)),
    }
}

fn first_receipt_node(response: &ZeGraphResponse) -> ZeNodeId {
    let receipts = unsafe { std::slice::from_raw_parts(response.receipts, response.receipt_count) };
    receipts[0].node
}

#[test]
fn apply_and_settle_commits_a_real_write_and_publishes_its_receipt() {
    // Each test gets its own registry: the gate is a non-blocking spinlock
    // ("contention is a typed precommit/free error"), and a shared static
    // would flake under real parallel test threads.
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().expect("temporary directory");
    let store =
        GraphStore::create(dir.path().join("native"), store_options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let requests = [create_request("alpha", &image)];

    let guarded = apply_and_settle(&REGISTRY, &store, &requests, &control());
    assert_eq!(
        guarded.outcome,
        OperationOutcome::Success(SuccessfulOutcome::Committed(
            std::num::NonZeroU64::new(1).unwrap()
        ))
    );
    let mut response = guarded.value.expect("no panic").expect("committed");
    assert_eq!(
        response.disposition,
        ZeGraphDisposition::ZeGraphDispositionCommitted as u32
    );
    assert_eq!(response.has_changed_generation, 1);
    assert_eq!(response.changed_generation, 1);
    assert_eq!(response.admitted_generation, 0);
    assert_eq!(response.receipt_count, 1);
    let receipts = unsafe { std::slice::from_raw_parts(response.receipts, response.receipt_count) };
    assert_eq!(receipts[0].item, 0);
    assert_eq!(
        receipts[0].entity_kind,
        ZeGraphEntityKind::ZeGraphEntityNode as u32
    );
    assert_eq!(receipts[0].deleted, 0);
    assert_eq!(receipts[0].generation, 1);
    REGISTRY.free(&mut response).expect("free");
    store.close().expect("close graph store");
}

#[test]
fn apply_and_settle_replays_an_exact_retry() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().expect("temporary directory");
    let store =
        GraphStore::create(dir.path().join("native"), store_options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let requests = [create_request("alpha", &image)];

    let first = apply_and_settle(&REGISTRY, &store, &requests, &control());
    let mut first_response = first.value.expect("no panic").expect("committed");
    let installed = first_receipt_node(&first_response);
    REGISTRY.free(&mut first_response).expect("free");

    // The exact same keyed request: nothing new is written.
    let second = apply_and_settle(&REGISTRY, &store, &requests, &control());
    assert_eq!(
        second.outcome,
        OperationOutcome::Success(SuccessfulOutcome::Replayed)
    );
    let mut second_response = second.value.expect("no panic").expect("replayed");
    assert_eq!(
        second_response.disposition,
        ZeGraphDisposition::ZeGraphDispositionReplayed as u32
    );
    assert_eq!(second_response.has_changed_generation, 0);
    let receipts = unsafe {
        std::slice::from_raw_parts(second_response.receipts, second_response.receipt_count)
    };
    assert_eq!(receipts[0].node, installed);
    assert_eq!(receipts[0].generation, 1);
    REGISTRY.free(&mut second_response).expect("free");
    store.close().expect("close graph store");
}

#[test]
fn apply_and_settle_reports_an_empty_batch_as_no_op() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().expect("temporary directory");
    let store =
        GraphStore::create(dir.path().join("native"), store_options(), None).expect("graph store");

    let guarded = apply_and_settle(&REGISTRY, &store, &[], &control());
    assert_eq!(
        guarded.outcome,
        OperationOutcome::Success(SuccessfulOutcome::NoOp)
    );
    let mut response = guarded.value.expect("no panic").expect("no-op");
    assert_eq!(
        response.disposition,
        ZeGraphDisposition::ZeGraphDispositionNoOp as u32
    );
    assert_eq!(response.receipt_count, 0);
    REGISTRY.free(&mut response).expect("free");
    store.close().expect("close graph store");
}

#[test]
fn apply_and_settle_reports_nothing_committed_on_a_real_constraint_refusal() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().expect("temporary directory");
    let store =
        GraphStore::create(dir.path().join("native"), store_options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let created = apply_and_settle(
        &REGISTRY,
        &store,
        &[create_request("fenced", &image)],
        &control(),
    );
    let mut created_response = created.value.expect("no panic").expect("committed");
    let installed_high_low = first_receipt_node(&created_response);
    REGISTRY.free(&mut created_response).expect("free");
    let installed = NodeId::new(
        (u128::from(installed_high_low.high) << 64) | u128::from(installed_high_low.low),
    )
    .expect("nonzero node id");

    let delete = [StructuredWrite {
        key: node_key("fenced"),
        revision: revision(2),
        operation: StructuredOperation::Delete(
            EntityId::Node(installed),
            GraphDeleteMode::Restrict,
        ),
        image: None,
    }];
    let deleted = apply_and_settle(&REGISTRY, &store, &delete, &control());
    let mut deleted_response = deleted.value.expect("no panic").expect("committed");
    REGISTRY.free(&mut deleted_response).expect("free");

    // Putting the now-tombstoned incarnation again is a real refusal with
    // nothing committed, driven entirely through the same guarded path.
    let stale = [StructuredWrite {
        key: node_key("fenced"),
        revision: revision(3),
        operation: StructuredOperation::Put(EntityId::Node(installed)),
        image: Some(WriteImage::Node(&image)),
    }];
    let guarded = apply_and_settle(&REGISTRY, &store, &stale, &control());
    assert_eq!(guarded.outcome, OperationOutcome::NotCommitted);
    let error = guarded
        .value
        .expect("no panic")
        .expect_err("the tombstoned incarnation is refused");
    match error {
        ProducerError::Store(store_error) => assert!(store_error.nothing_committed()),
        ProducerError::Conversion(_) => panic!("expected a real store refusal"),
    }
    store.close().expect("close graph store");
}

/// `MATCH (n) RETURN n`, through `run_query`.
#[allow(
    clippy::result_large_err,
    reason = "ProducerError retains the allocation-free core GraphStoreError"
)]
fn read_all(
    registry: &'static GraphResultRegistry,
    store: &GraphStore,
    control: &QueryControl,
) -> Result<ZeGraphResponse, ProducerError> {
    let unit = vec![PlanNodeId(0)];
    let scan = vec![PlanNodeId(1)];
    let projections = vec![Projection {
        slot: SlotId(10),
        expression: ExprId(0),
    }];
    let operators = vec![
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &unit,
            kind: OperatorKind::ScanNodes {
                output: SlotId(0),
                label: None,
            },
        },
        Operator {
            inputs: &scan,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let expressions = vec![Expression::Slot(SlotId(0))];
    let eager: Vec<PlanNodeId> = Vec::new();
    let parameters: Vec<Parameter<'_>> = Vec::new();
    let mut backing = GraphPlanBacking::default();
    backing.vec(&unit).expect("backing unit");
    backing.vec(&scan).expect("backing scan");
    backing.vec(&projections).expect("backing projections");
    let plan = GraphQueryPlan {
        operators: &operators,
        expressions: &expressions,
        parameters: &parameters,
        eager_searches: &eager,
        root: PlanNodeId(2),
        backing: &backing,
        bindings: &[],
        columns: &["n"],
    };
    run_query(
        registry,
        store,
        control,
        &GraphQueryOptions::default(),
        &plan,
    )
}

#[test]
fn run_query_reads_real_committed_nodes() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().expect("temporary directory");
    let store =
        GraphStore::create(dir.path().join("native"), store_options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let write = apply_and_settle(
        &REGISTRY,
        &store,
        &[create_request("alpha", &image)],
        &control(),
    );
    REGISTRY
        .free(&mut write.value.expect("no panic").expect("committed"))
        .expect("free");

    let mut response = read_all(&REGISTRY, &store, &control()).expect("real read");
    assert_eq!(response.row_count, 1);
    assert_eq!(response.has_admitted_generation, 1);
    assert_eq!(response.admitted_generation, 1);
    assert_eq!(response.pool.node_count, 1);
    REGISTRY.free(&mut response).expect("free");
    store.close().expect("close graph store");
}

// ----- Unit coverage for the two ZE-68 open-question mappings -----

#[test]
fn write_settlement_maps_every_graph_write_outcome_variant() {
    assert!(matches!(
        write_settlement(GraphWriteOutcome::NoOp),
        Ok(WriteSettlement::NoOp)
    ));
    assert!(matches!(
        write_settlement(GraphWriteOutcome::Replayed),
        Ok(WriteSettlement::Replayed)
    ));
    let committed = write_settlement(GraphWriteOutcome::Committed {
        generation: GraphGeneration::new(7),
    });
    assert!(matches!(
        committed,
        Ok(WriteSettlement::Committed(g)) if g.get() == 7
    ));
}

#[test]
fn write_receipt_deleted_flag_comes_from_the_request_not_the_receipt() {
    // `ItemReceipt` has no `deleted` field; `write_receipt`'s `deleted`
    // parameter must come from the caller's own request classification,
    // independent of `replayed`. The two cases below deliberately disagree
    // (deleted=true with replayed=false, and the reverse) so a `deleted`
    // that was accidentally derived from `replayed` cannot pass both.
    let receipt = ItemReceipt {
        entity: EntityId::Node(NodeId::new(1).unwrap()),
        revision: GraphRevision::new(1).unwrap(),
        generation: GraphGeneration::new(1),
        replayed: false,
    };
    let c = write_receipt(0, &receipt, true);
    assert_eq!(c.deleted, 1);
    assert_eq!(
        c.disposition,
        ZeGraphDisposition::ZeGraphDispositionCommitted as u32
    );
    let receipt = ItemReceipt {
        replayed: true,
        ..receipt
    };
    let c = write_receipt(0, &receipt, false);
    assert_eq!(c.deleted, 0);
    assert_eq!(
        c.disposition,
        ZeGraphDisposition::ZeGraphDispositionReplayed as u32
    );
}

#[test]
fn apply_and_settle_keeps_the_known_commit_when_response_preparation_fails() {
    static FULL: GraphResultRegistry = GraphResultRegistry::new(0);
    static AVAILABLE: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().unwrap();
    let store = GraphStore::create(dir.path().join("native"), store_options(), None).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let requests = [create_request("alpha", &image)];
    let guarded = apply_and_settle(&FULL, &store, &requests, &control());
    assert!(matches!(
        guarded.value,
        Ok(Err(ProducerError::Conversion(_)))
    ));
    assert_eq!(
        guarded.outcome,
        OperationOutcome::Success(SuccessfulOutcome::Committed(
            std::num::NonZeroU64::new(1).unwrap()
        ))
    );
    let retry = apply_and_settle(&AVAILABLE, &store, &requests, &control());
    assert_eq!(
        retry.outcome,
        OperationOutcome::Success(SuccessfulOutcome::Replayed)
    );
    let mut response = retry.value.unwrap().unwrap();
    AVAILABLE.free(&mut response).unwrap();
    store.close().unwrap();
}
