//! ZE-68 Slice A: the FFI's coordinator/conversion machinery wired to a
//! real `Store`, not a synthetic `ResultSource`. Every test here opens
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
use zeppelin_embed::lifecycle::Store;
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

// Unified creation publishes generation 1; each fixture batch advances it once.
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
        Store::create_graph(dir.path().join("native"), store_options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let requests = [create_request("alpha", &image)];

    let guarded = apply_and_settle(&REGISTRY, &store, &requests, &control());
    assert_eq!(
        guarded.outcome,
        OperationOutcome::Success(SuccessfulOutcome::Committed(
            std::num::NonZeroU64::new(2).unwrap()
        ))
    );
    let mut response = guarded.value.expect("no panic").expect("committed");
    assert_eq!(
        response.disposition,
        ZeGraphDisposition::ZeGraphDispositionCommitted as u32
    );
    assert_eq!(response.has_changed_generation, 1);
    assert_eq!(response.changed_generation, 2);
    assert_eq!(response.admitted_generation, 1);
    assert_eq!(response.receipt_count, 1);
    let receipts = unsafe { std::slice::from_raw_parts(response.receipts, response.receipt_count) };
    assert_eq!(receipts[0].item, 0);
    assert_eq!(
        receipts[0].entity_kind,
        ZeGraphEntityKind::ZeGraphEntityNode as u32
    );
    assert_eq!(receipts[0].deleted, 0);
    assert_eq!(receipts[0].generation, 2);
    REGISTRY.free(&mut response).expect("free");
    store.close_graph().expect("close graph store");
}

#[test]
fn apply_and_settle_replays_an_exact_retry() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().expect("temporary directory");
    let store =
        Store::create_graph(dir.path().join("native"), store_options(), None).expect("graph store");
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
    assert_eq!(receipts[0].generation, 2);
    REGISTRY.free(&mut second_response).expect("free");
    store.close_graph().expect("close graph store");
}

#[test]
fn apply_and_settle_reports_an_empty_batch_as_no_op() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().expect("temporary directory");
    let store =
        Store::create_graph(dir.path().join("native"), store_options(), None).expect("graph store");

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
    store.close_graph().expect("close graph store");
}

#[test]
fn apply_and_settle_reports_nothing_committed_on_a_real_constraint_refusal() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().expect("temporary directory");
    let store =
        Store::create_graph(dir.path().join("native"), store_options(), None).expect("graph store");
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
    store.close_graph().expect("close graph store");
}

/// `MATCH (n) RETURN n`, through `run_query`.
#[allow(
    clippy::result_large_err,
    reason = "ProducerError retains the allocation-free core GraphStoreError"
)]
fn read_all(
    registry: &'static GraphResultRegistry,
    store: &Store,
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
        Store::create_graph(dir.path().join("native"), store_options(), None).expect("graph store");
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
    assert_eq!(response.admitted_generation, 2);
    assert_eq!(response.pool.node_count, 1);
    REGISTRY.free(&mut response).expect("free");
    store.close_graph().expect("close graph store");
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
fn ze241_receipt_response_failure_leaves_batch_not_committed() {
    static FULL: GraphResultRegistry = GraphResultRegistry::new(0);
    static AVAILABLE: GraphResultRegistry = GraphResultRegistry::new(16);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::create_graph(dir.path().join("native"), store_options(), None).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let requests = [create_request("alpha", &image)];
    let guarded = apply_and_settle(&FULL, &store, &requests, &control());
    assert!(matches!(
        guarded.value,
        Ok(Err(ProducerError::Conversion(_)))
    ));
    assert_eq!(guarded.outcome, OperationOutcome::NotCommitted);
    store.close_graph().unwrap();
    let store = Store::open_graph(dir.path().join("native"), store_options(), None).unwrap();
    let retry = apply_and_settle(&AVAILABLE, &store, &requests, &control());
    assert_eq!(
        retry.outcome,
        OperationOutcome::Success(SuccessfulOutcome::Committed(
            std::num::NonZeroU64::new(2).unwrap()
        ))
    );
    let mut response = retry.value.unwrap().unwrap();
    AVAILABLE.free(&mut response).unwrap();
    store.close_graph().unwrap();
}

#[cfg(feature = "graph-result-test-support")]
#[test]
fn ze241_receipt_allocation_failures_leave_batch_not_committed() {
    use super::super::test_support::AllocationFaultScope;
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    for ordinal in [1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create_graph(dir.path().join("native"), store_options(), None).unwrap();
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let requests = [create_request("alpha", &image)];
        let fault = AllocationFaultScope::arm(ordinal);
        let guarded = apply_and_settle(&REGISTRY, &store, &requests, &control());
        assert_eq!(fault.receipt().fires, 1);
        drop(fault);
        assert!(matches!(
            guarded.value,
            Ok(Err(ProducerError::Conversion(ConversionError::Owner(
                OwnerError::Allocation
            ))))
        ));
        assert_eq!(guarded.outcome, OperationOutcome::NotCommitted);
        store.close_graph().unwrap();
        let store = Store::open_graph(dir.path().join("native"), store_options(), None).unwrap();
        let retry = apply_and_settle(&REGISTRY, &store, &requests, &control());
        assert_eq!(
            retry.outcome,
            OperationOutcome::Success(SuccessfulOutcome::Committed(
                std::num::NonZeroU64::new(1).unwrap()
            ))
        );
        let mut response = retry.value.unwrap().unwrap();
        REGISTRY.free(&mut response).unwrap();
        store.close_graph().unwrap();
    }
}

fn ze211_fixture() -> (
    tempfile::TempDir,
    Store,
    [NodeId; 2],
    zeppelin_embed::property_graph::RelId,
) {
    use zeppelin_embed::property_graph::{
        GraphName, GraphProperty, NodeRef, PropertyData, PropertyValue,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Store::create_graph(dir.path().join("get"), store_options(), None).unwrap();
    let absent = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let mut properties = [
        GraphProperty::new(
            GraphName::new("sentinel").unwrap(),
            PropertyValue::new(PropertyData::EmptyList { count: 0 }).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("typed").unwrap(),
            PropertyValue::new(PropertyData::Strings(&[])).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("text").unwrap(),
            PropertyValue::new(PropertyData::String("a\0λ")).unwrap(),
        ),
    ];
    let mut labels = [GraphName::new("Label").unwrap()];
    let empty = CanonicalContents::node(&mut labels, &mut properties, Some(""), None).unwrap();
    let result = store
        .graph_apply(
            &[create_request("a", &absent), create_request("b", &empty)],
            &control(),
        )
        .unwrap();
    let ids = std::array::from_fn(|i| match result.receipts()[i].entity {
        EntityId::Node(id) => id,
        _ => panic!("node receipt"),
    });
    let result = store
        .graph_apply(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze211", "edge").unwrap(),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Existing(ids[0]),
                    target: NodeRef::Existing(ids[1]),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            }],
            &control(),
        )
        .unwrap();
    let edge = match result.receipts()[0].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("edge receipt"),
    };
    (dir, store, ids, edge)
}

fn ze211_check_rows(response: &ZeGraphResponse, tags: &[u32], generation: u64) {
    assert_eq!(response.row_count, tags.len());
    assert_eq!(response.column_count, 1);
    assert_eq!(response.cell_count, tags.len());
    let cells = if tags.is_empty() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(response.cells, response.cell_count) }
    };
    let values = if response.pool.value_count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(response.pool.values, response.pool.value_count) }
    };
    for (cell, tag) in cells.iter().zip(tags) {
        assert_eq!(values[*cell as usize].tag, *tag);
    }
    assert_eq!(response.has_admitted_generation, 1);
    assert_eq!(response.admitted_generation, generation);
}

#[test]
fn ze211_nodes_preserve_sparse_order_and_payloads() {
    use zeppelin_embed::property_graph::GraphGetOptions;
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let (_dir, store, ids, _) = ze211_fixture();
    let missing = NodeId::new(999999).unwrap();
    let mut response = run_get_nodes(
        &REGISTRY,
        &store,
        &[ids[1], missing, ids[0], ids[1]],
        GraphGetOptions {
            text: true,
            vector: false,
        },
        &control(),
    )
    .unwrap();
    let mut empty = run_get_nodes(
        &REGISTRY,
        &store,
        &[],
        GraphGetOptions::default(),
        &control(),
    )
    .unwrap();
    let mut absent = run_get_nodes(
        &REGISTRY,
        &store,
        &[missing],
        GraphGetOptions::default(),
        &control(),
    )
    .unwrap();
    store
        .graph_apply(
            &[StructuredWrite {
                key: node_key("a"),
                revision: revision(2),
                operation: StructuredOperation::Delete(
                    EntityId::Node(ids[0]),
                    GraphDeleteMode::Detach,
                ),
                image: None,
            }],
            &control(),
        )
        .unwrap();
    let mut deleted = run_get_nodes(
        &REGISTRY,
        &store,
        &[ids[0]],
        GraphGetOptions::default(),
        &control(),
    )
    .unwrap();
    store.close_graph().unwrap();
    ze211_check_rows(&deleted, &[0], 4);
    REGISTRY.free(&mut deleted).unwrap();
    ze211_check_rows(&response, &[5, 0, 5, 5], 3);
    ze211_check_rows(&empty, &[], 3);
    ze211_check_rows(&absent, &[0], 3);
    let nodes =
        unsafe { std::slice::from_raw_parts(response.pool.nodes, response.pool.node_count) };
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].id, node_id(ids[0].get()));
    assert_eq!(nodes[1].id, node_id(ids[1].get()));
    assert_eq!(nodes[0].has_text, 0);
    assert_eq!(nodes[1].has_text, 1);
    assert_eq!(nodes[1].text.count, 0);
    let cells = unsafe { std::slice::from_raw_parts(response.cells, response.cell_count) };
    let values =
        unsafe { std::slice::from_raw_parts(response.pool.values, response.pool.value_count) };
    assert_eq!(values[cells[0] as usize].entity_index, 1);
    assert_eq!(values[cells[2] as usize].entity_index, 0);
    assert_eq!(values[cells[3] as usize].entity_index, 1);
    let properties = unsafe {
        std::slice::from_raw_parts(response.pool.properties, response.pool.property_count)
    };
    let bytes =
        unsafe { std::slice::from_raw_parts(response.pool.bytes, response.pool.byte_count) };
    let slice =
        |span: ZeGraphRange| &bytes[span.start as usize..(span.start + span.count) as usize];
    let mut list_kinds = Vec::new();
    for property in &properties[nodes[1].properties.start as usize
        ..(nodes[1].properties.start + nodes[1].properties.count) as usize]
    {
        let value = &values[property.value as usize];
        match slice(property.name) {
            b"text" => assert_eq!(slice(value.range), "a\0λ".as_bytes()),
            b"sentinel" | b"typed" => {
                assert_eq!(value.tag, 7);
                assert_eq!(value.range.count, 0);
                list_kinds.push(value.list_kind);
            }
            _ => panic!("unexpected property"),
        }
    }
    assert_eq!(list_kinds.len(), 2);
    assert_ne!(list_kinds[0], list_kinds[1]);
    for owner in [&mut response, &mut empty, &mut absent] {
        REGISTRY.free(owner).unwrap();
    }
}

#[test]
fn ze211_relationships_preserve_sparse_order_and_payloads() {
    use zeppelin_embed::property_graph::RelId;
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(16);
    let (_dir, store, ids, edge) = ze211_fixture();
    use zeppelin_embed::property_graph::{
        GraphName, GraphProperty, NodeRef, PropertyData, PropertyValue,
    };
    let created = store
        .graph_apply(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze211", "reverse").unwrap(),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Existing(ids[1]),
                    target: NodeRef::Existing(ids[0]),
                    relationship_type: GraphName::new("BACK").unwrap(),
                    properties: &[GraphProperty::new(
                        GraphName::new("weight").unwrap(),
                        PropertyValue::new(PropertyData::I64(42)).unwrap(),
                    )],
                }),
            }],
            &control(),
        )
        .unwrap();
    let reverse = match created.receipts()[0].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("edge receipt"),
    };
    let missing = RelId::new(999999).unwrap();
    let mut response = run_get_relationships(
        &REGISTRY,
        &store,
        &[reverse, missing, edge, reverse],
        &control(),
    )
    .unwrap();
    let mut empty = run_get_relationships(&REGISTRY, &store, &[], &control()).unwrap();
    let mut absent = run_get_relationships(&REGISTRY, &store, &[missing], &control()).unwrap();
    store
        .graph_apply(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze211", "edge").unwrap(),
                revision: revision(2),
                operation: StructuredOperation::Delete(
                    EntityId::Relationship(edge),
                    GraphDeleteMode::Restrict,
                ),
                image: None,
            }],
            &control(),
        )
        .unwrap();
    let mut deleted = run_get_relationships(&REGISTRY, &store, &[edge], &control()).unwrap();
    store.close_graph().unwrap();
    ze211_check_rows(&deleted, &[0], 5);
    REGISTRY.free(&mut deleted).unwrap();
    ze211_check_rows(&response, &[6, 0, 6, 6], 4);
    ze211_check_rows(&empty, &[], 4);
    ze211_check_rows(&absent, &[0], 4);
    assert_eq!(response.pool.relationship_count, 2);
    let relationships = unsafe { std::slice::from_raw_parts(response.pool.relationships, 2) };
    assert_eq!(relationships[1].source, node_id(ids[1].get()));
    assert_eq!(relationships[1].target, node_id(ids[0].get()));
    let cells = unsafe { std::slice::from_raw_parts(response.cells, response.cell_count) };
    let values =
        unsafe { std::slice::from_raw_parts(response.pool.values, response.pool.value_count) };
    assert_eq!(values[cells[0] as usize].entity_index, 1);
    assert_eq!(values[cells[2] as usize].entity_index, 0);
    assert_eq!(values[cells[3] as usize].entity_index, 1);
    let properties = unsafe {
        std::slice::from_raw_parts(response.pool.properties, response.pool.property_count)
    };
    let property = &properties[relationships[1].properties.start as usize];
    assert_eq!(values[property.value as usize].tag, 2);
    assert_eq!(values[property.value as usize].integer, 42);
    let relationship = unsafe { &*response.pool.relationships };
    assert_eq!(relationship.source, node_id(ids[0].get()));
    assert_eq!(relationship.target, node_id(ids[1].get()));
    let bytes =
        unsafe { std::slice::from_raw_parts(response.pool.bytes, response.pool.byte_count) };
    let span = relationship.relationship_type;
    assert_eq!(
        &bytes[span.start as usize..(span.start + span.count) as usize],
        b"LINKS"
    );
    for owner in [&mut response, &mut empty, &mut absent] {
        REGISTRY.free(owner).unwrap();
    }
}

#[cfg(feature = "graph-result-test-support")]
#[test]
fn ze211_get_conversion_refusal_cleans_owner() {
    use super::super::test_support::AllocationFaultScope;
    use zeppelin_embed::property_graph::GraphGetOptions;
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(1);
    let (_dir, store, ids, edge) = ze211_fixture();
    for ordinal in [1, 2] {
        let fault = AllocationFaultScope::arm(ordinal);
        assert!(
            run_get_nodes(
                &REGISTRY,
                &store,
                &ids,
                GraphGetOptions::default(),
                &control()
            )
            .is_err()
        );
        assert_eq!(fault.receipt().fires, 1);
        drop(fault);
        let mut response = run_get_nodes(
            &REGISTRY,
            &store,
            &ids,
            GraphGetOptions::default(),
            &control(),
        )
        .unwrap();
        REGISTRY.free(&mut response).unwrap();
        let fault = AllocationFaultScope::arm(ordinal);
        assert!(run_get_relationships(&REGISTRY, &store, &[edge], &control()).is_err());
        assert_eq!(fault.receipt().fires, 1);
        drop(fault);
        let mut response = run_get_relationships(&REGISTRY, &store, &[edge], &control()).unwrap();
        REGISTRY.free(&mut response).unwrap();
    }
    store.close_graph().unwrap();
}
