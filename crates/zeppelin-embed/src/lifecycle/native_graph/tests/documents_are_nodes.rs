use super::recovery::{disable_generation_fixture_maintenance, native_options};
use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::lifecycle::{CancelToken, QueryControl, Store};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphRevision, NodeId,
};

fn document(id: u128) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(1)),
        vec![1.0, 0.0],
    )
    .with_text("legacy orchard")
}

#[test]
fn allocator_skips_an_id_that_is_a_live_document() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![document(1), document(2)]))
        .unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let result = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "shared", "allocated").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert_eq!(result[0].entity, EntityId::Node(NodeId::new(3).unwrap()));
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(
        reopened
            .admit_native_read()
            .unwrap()
            .bundle()
            .high_waters()
            .node,
        3
    );
}

#[test]
fn a_legacy_document_matches_as_a_document_node() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.ingest(IngestBatch::new(vec![document(91)])).unwrap();
    store.seal().unwrap();
    store.ingest(IngestBatch::new(vec![document(92)])).unwrap();
    store.enable_graph().unwrap();
    let before = std::fs::read(directory.path().join("wal.ze")).unwrap();
    let result = document_nodes(&store);
    assert_eq!(
        result.cell(0, 0),
        Some(&crate::property_graph::query::completed::Value::I64(0))
    );
    assert_eq!(
        result.cell(1, 0),
        Some(&crate::property_graph::query::completed::Value::I64(0))
    );
    assert_eq!(
        result.cell(0, 3),
        Some(&crate::property_graph::query::completed::Value::Bool(true))
    );
    assert_eq!(
        result.cell(1, 3),
        Some(&crate::property_graph::query::completed::Value::Bool(true))
    );
    assert_eq!(
        result.metadata().rows,
        2,
        "MATCH (d:Document) must include active and sealed documents"
    );
    assert_eq!(
        std::fs::read(directory.path().join("wal.ze")).unwrap(),
        before
    );
}

#[test]
fn a_relationship_to_a_document_creates_its_node_record() {
    use super::recovery::observe_node;
    use crate::property_graph::{GraphName, NodeRef};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![document(91), document(92)]))
        .unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let source = NodeId::new(91).unwrap();
    let target = NodeId::new(92).unwrap();
    let result = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "shared", "link").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    relationship_type: GraphName::new("LINK").unwrap(),
                    properties: &[],
                    source: NodeRef::Existing(source),
                    target: NodeRef::Existing(target),
                }),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("a live document is a valid relationship endpoint");
    assert_eq!(result.len(), 1);
    assert!(observe_node(&store, source).is_some());
    assert!(observe_node(&store, target).is_some());
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    assert!(observe_node(&reopened, source).is_some());
    assert_eq!(reopened.count_documents(None, None).unwrap().count, 2);
}

fn link_documents(store: &Store) -> crate::property_graph::RelId {
    use crate::property_graph::{GraphName, NodeRef};
    let result = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "shared", "link").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    relationship_type: GraphName::new("LINK").unwrap(),
                    properties: &[],
                    source: NodeRef::Existing(NodeId::new(91).unwrap()),
                    target: NodeRef::Existing(NodeId::new(92).unwrap()),
                }),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let EntityId::Relationship(id) = result[0].entity else {
        panic!("relationship receipt");
    };
    id
}

#[test]
fn a_document_node_replay_reports_the_current_store_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![document(91), document(92)]))
        .unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    link_documents(&store);
    let current = store.ingest(IngestBatch::new(vec![document(93)])).unwrap();
    let before = std::fs::read(directory.path().join("wal.ze")).unwrap();
    let replay = store.ingest(IngestBatch::new(vec![document(92)])).unwrap();
    assert_eq!(replay.generation(), current.generation());
    assert_eq!(
        std::fs::read(directory.path().join("wal.ze")).unwrap(),
        before
    );
    store.ingest(IngestBatch::new(vec![document(94)])).unwrap();
}

#[test]
fn deleting_a_document_hides_its_edges() {
    use super::recovery::{observe_node, relationship_is_visible};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![document(91), document(92)]))
        .unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let relationship = link_documents(&store);
    assert!(relationship_is_visible(&store, relationship));
    store
        .delete(crate::ingest::DeleteBatch::new(vec![DocId::new(92)]))
        .unwrap();
    assert!(
        !relationship_is_visible(&store, relationship),
        "a deleted document must hide its incoming edges"
    );
    assert!(observe_node(&store, NodeId::new(92).unwrap()).is_none());
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    assert!(!relationship_is_visible(&reopened, relationship));
}

#[test]
fn a_restrict_policy_refuses_the_document_delete() {
    use crate::property_graph::GraphName;
    use crate::property_graph::catalog::{OnDelete, RelationshipRule, RelationshipRules};
    use crate::property_graph::query::completed::GraphQueryErrorKind;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let rules = [RelationshipRule {
        relationship_type: GraphName::new("LINK").unwrap(),
        on_delete: OnDelete::Restrict,
    }];
    let store = Store::create_native_graph_with_relationship_types(
        &directory.path().join("store"),
        native_options(),
        None,
        RelationshipRules::new(&rules).unwrap(),
    )
    .unwrap();
    store
        .ingest(IngestBatch::new(vec![document(91), document(92)]))
        .unwrap();
    disable_generation_fixture_maintenance(&store);
    let relationship = link_documents(&store);
    let before = store.snapshot().unwrap().generation();
    let wal = std::fs::read(path.join("wal.ze")).unwrap();
    let error = store
        .delete(crate::ingest::DeleteBatch::new(vec![DocId::new(92)]))
        .unwrap_err();
    assert!(
        matches!(error, crate::ingest::IngestError::Graph(ref error) if error.kind() == GraphQueryErrorKind::Constraint)
    );
    assert_eq!(store.snapshot().unwrap().generation(), before);
    assert_eq!(std::fs::read(path.join("wal.ze")).unwrap(), wal);
    assert!(super::recovery::relationship_is_visible(
        &store,
        relationship
    ));
    store
        .ingest(IngestBatch::new(vec![document(93)]))
        .expect("definite graph refusal must leave the shared writer usable");
}

#[test]
fn document_endpoint_below_the_allocator_watermark_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    store
        .jump_native_graph_allocators_for_test(
            NodeId::new(101).unwrap(),
            crate::property_graph::RelId::new(1).unwrap(),
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    store
        .ingest(IngestBatch::new(vec![document(91), document(92)]))
        .unwrap();
    let relationship = link_documents(&store);
    drop(store);
    let reopened = Store::open(directory.path(), native_options())
        .expect("caller document identities do not have to exceed the graph allocator watermark");
    assert!(super::recovery::relationship_is_visible(
        &reopened,
        relationship
    ));
}

fn record_document_version(store: &Store, node: NodeId) -> Option<DocumentVersion> {
    use crate::property_graph::query::resources::QueryMemory;
    use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
    use crate::property_graph::storage::tree::directory::TreeResources;
    use crate::property_graph::storage::{
        GraphReadView, NativeCatalog, NativeQuerySource, NativeReadCapability,
    };
    let lease = store.admit_native_read().unwrap();
    let shared = crate::property_graph::resources::GraphResources::from_store(store).unwrap();
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime =
        RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
    let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
    let mut resources = TreeResources::for_query(&mut runtime).unwrap();
    let source = NativeQuerySource::new(capability, &resources, 32).unwrap();
    let catalog = NativeCatalog::open(&source, &mut resources).unwrap();
    let view = GraphReadView::new(&source, &catalog).unwrap();
    view.lookup_node(node, &mut resources)
        .unwrap()
        .and_then(|node| node.record().document_version())
}
#[test]
fn a_document_update_rebinds_the_existing_node_record() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![document(91), document(92)]))
        .unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let relationship = link_documents(&store);
    let version = DocumentVersion::new(DocId::new(92), Revision::new(2));
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(version, vec![0.0, 1.0]).with_text("corrected"),
        ]))
        .unwrap();
    assert_eq!(
        record_document_version(&store, NodeId::new(92).unwrap()),
        Some(version),
        "the node must refer to the published document version"
    );
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(
        record_document_version(&reopened, NodeId::new(92).unwrap()),
        Some(version)
    );
    assert!(super::recovery::relationship_is_visible(
        &reopened,
        relationship
    ));
}

#[test]
fn a_legacy_zero_document_id_matches_without_a_write() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.ingest(IngestBatch::new(vec![document(0)])).unwrap();
    store.enable_graph().unwrap();
    let result = document_nodes(&store);
    assert_eq!(result.metadata().rows, 1);
}

fn document_nodes(store: &Store) -> crate::property_graph::query::completed::CompletedGraphResult {
    use crate::property_graph::GraphName;
    use crate::property_graph::query::completed::GraphQueryOptions;
    use crate::property_graph::query::entry_probe::{Backing, run_plan};
    use crate::property_graph::query::plan::{
        ExprId, Expression, Operator, OperatorKind, PlanNodeId, Projection, SlotId, UnaryExpression,
    };
    store
        .execute_graph_statement(
            &QueryControl::Cancel(CancelToken::new()),
            &GraphQueryOptions::default(),
            |runtime, executor| {
                let label = String::from("Document");
                let property = String::from("ts");
                let unit = vec![PlanNodeId(0)];
                let scan = vec![PlanNodeId(1)];
                let projections = vec![
                    Projection {
                        slot: SlotId(10),
                        expression: ExprId(1),
                    },
                    Projection {
                        slot: SlotId(11),
                        expression: ExprId(0),
                    },
                    Projection {
                        slot: SlotId(12),
                        expression: ExprId(2),
                    },
                    Projection {
                        slot: SlotId(13),
                        expression: ExprId(3),
                    },
                ];
                let expressions = vec![
                    Expression::Slot(SlotId(0)),
                    Expression::Property {
                        entity: ExprId(0),
                        name: GraphName::new(&property).unwrap(),
                    },
                    Expression::Unary {
                        operation: UnaryExpression::Labels,
                        operand: ExprId(0),
                    },
                    Expression::HasLabel {
                        entity: ExprId(0),
                        label: GraphName::new(&label).unwrap(),
                    },
                ];
                let operators = vec![
                    Operator {
                        inputs: &[],
                        kind: OperatorKind::Unit,
                    },
                    Operator {
                        inputs: &unit,
                        kind: OperatorKind::ScanNodes {
                            output: SlotId(0),
                            label: Some(GraphName::new(&label).unwrap()),
                        },
                    },
                    Operator {
                        inputs: &scan,
                        kind: OperatorKind::Project(&projections),
                    },
                ];
                let mut backing = Backing::default();
                backing.string(&label)?;
                backing.string(&property)?;
                backing.vec(&unit)?;
                backing.vec(&scan)?;
                backing.vec(&projections)?;
                run_plan(
                    runtime,
                    executor,
                    &operators,
                    &expressions,
                    &Vec::new(),
                    &backing,
                    &["ts", "d", "labels", "is_document"],
                )
            },
        )
        .unwrap()
}

struct LabelDocument;
impl super::super::mutate::NativeMutationConsumer<()> for LabelDocument {
    fn consume<'lease, 'm, 'g, 'w, 'i>(
        &mut self,
        _: &'w crate::property_graph::storage::GraphReadView<'w, 'lease, 'm, 'g>,
        _: &mut crate::property_graph::query::runtime::RuntimeContext<'lease, 'm, 'g>,
        mut overlay: crate::property_graph::staging::GraphBatchReadView<'w, 'static>,
        images: &'w crate::property_graph::staging::StatementImages<'i>,
        control: &mut crate::property_graph::staging::WriteControl<'_>,
    ) -> Result<
        (
            (),
            crate::property_graph::staging::GraphBatchReadView<'w, 'static>,
        ),
        crate::property_graph::query::runtime::NativeExecutionError,
    > {
        let label = crate::property_graph::GraphName::new("Archive").unwrap();
        let mut budget = crate::property_graph::staging::NodeImageBudget::default();
        budget.label(label)?;
        let mut image = images.node(budget, control)?;
        image.label(label, control)?;
        overlay.replace(
            crate::property_graph::staging::BatchEntityRef::Node(
                crate::property_graph::NodeRef::Existing(NodeId::new(91).unwrap()),
            ),
            WriteImage::Node(image.finish(control)?),
            control,
        )?;
        Ok(((), overlay))
    }
}
#[test]
fn a_label_write_adopts_an_implicit_document() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.ingest(IngestBatch::new(vec![document(91)])).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    store
        .with_native_mutation(
            &QueryControl::Cancel(CancelToken::new()),
            crate::property_graph::query::runtime::RuntimeLimits::default(),
            24 * 1024 * 1024,
            crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            1,
            1,
            1,
            LabelDocument,
        )
        .expect("a label write must adopt the legacy document node");
    assert_eq!(
        record_document_version(&store, NodeId::new(91).unwrap()),
        Some(DocumentVersion::new(DocId::new(91), Revision::new(1)))
    );
    let result = document_nodes(&store);
    assert_eq!(result.metadata().rows, 1);
    let pools = result.pools();
    let node = pools.nodes.first().unwrap();
    let labels = pools.names
        [node.labels.start as usize..(node.labels.start + node.labels.len) as usize]
        .iter()
        .map(|span| result.string(*span).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        ["Archive", "Document"],
        "adoption must preserve the implicit Document label"
    );
    let properties = &pools.properties
        [node.properties.start as usize..(node.properties.start + node.properties.len) as usize];
    let timestamp = properties
        .iter()
        .find(|property| result.string(property.name) == Some("ts"))
        .expect("the adopted node retains its document columns");
    assert_eq!(
        pools.values[timestamp.value.0 as usize],
        crate::property_graph::query::completed::Value::I64(0)
    );
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    assert!(super::recovery::observe_node(&reopened, NodeId::new(91).unwrap()).is_some());
}

fn restricted_document_fixture() -> (tempfile::TempDir, Store) {
    use crate::property_graph::catalog::{OnDelete, RelationshipRule, RelationshipRules};
    let directory = tempfile::tempdir().unwrap();
    let rules = [RelationshipRule {
        relationship_type: crate::property_graph::GraphName::new("LINK").unwrap(),
        on_delete: OnDelete::Restrict,
    }];
    let store = Store::create_native_graph_with_relationship_types(
        &directory.path().join("store"),
        native_options(),
        None,
        RelationshipRules::new(&rules).unwrap(),
    )
    .unwrap();
    store
        .ingest(IngestBatch::new(vec![document(92).with_timestamp(0)]))
        .unwrap();
    store.seal().unwrap();
    store
        .ingest(IngestBatch::new(vec![document(91).with_timestamp(1)]))
        .unwrap();
    disable_generation_fixture_maintenance(&store);
    link_documents(&store);
    (directory, store)
}

#[test]
fn purge_on_a_document_node_obeys_restrict() {
    let (directory, store) = restricted_document_fixture();
    let before = super::recovery::file_snapshot(&directory.path().join("store"));
    store
        .purge_with_available_space(&[DocId::new(92)], u64::MAX)
        .expect_err("Restrict must refuse purge before its durable intent");
    assert_eq!(
        super::recovery::file_snapshot(&directory.path().join("store")),
        before
    );
}

#[test]
fn delete_matching_on_a_document_node_obeys_restrict() {
    let (directory, store) = restricted_document_fixture();
    let before = super::recovery::file_snapshot(&directory.path().join("store"));
    store
        .delete_matching(&crate::meta::Predicate::Eq {
            column: crate::meta::TIMESTAMP_COLUMN,
            value: crate::meta::PredicateValue::I64(0),
        })
        .expect_err("Restrict must refuse delete_matching before its durable intent");
    assert_eq!(
        super::recovery::file_snapshot(&directory.path().join("store")),
        before
    );
}

#[test]
fn retention_on_a_document_node_obeys_restrict() {
    let (directory, store) = restricted_document_fixture();
    let before = super::recovery::file_snapshot(&directory.path().join("store"));
    store
        .drop_partition(0..1)
        .expect_err("Restrict must refuse retention before manifest publication");
    assert_eq!(
        super::recovery::file_snapshot(&directory.path().join("store")),
        before
    );
}

fn link_ids(store: &Store, source: u128, target: u128) {
    use crate::property_graph::{GraphName, NodeRef};
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "shared", "link").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    relationship_type: GraphName::new("LINK").unwrap(),
                    properties: &[],
                    source: NodeRef::Existing(NodeId::from(DocId::new(source))),
                    target: NodeRef::Existing(NodeId::from(DocId::new(target))),
                }),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
}

#[test]
fn a_uuid_document_endpoint_does_not_exhaust_the_allocator() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![document(u128::MAX), document(91)]))
        .unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    link_ids(&store, u128::MAX, 91);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let result = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "shared", "allocated").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("a caller UUID must not advance the allocator to exhaustion");
    assert_eq!(result[0].entity, EntityId::Node(NodeId::new(1).unwrap()));
    drop(store);
    Store::open(directory.path(), native_options()).unwrap();
}

#[test]
fn a_zero_document_can_be_a_relationship_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![document(0), document(91)]))
        .unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    link_ids(&store, 0, 91);
    store
        .delete(crate::ingest::DeleteBatch::new(vec![DocId::new(0)]))
        .unwrap();
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
}

#[test]
fn a_revision_zero_document_can_be_a_relationship_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    let zero = IngestDocument::new(
        DocumentVersion::new(DocId::new(91), Revision::new(0)),
        vec![1.0, 0.0],
    );
    store
        .ingest(IngestBatch::new(vec![zero, document(92)]))
        .unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    link_documents(&store);
    drop(store);
    Store::open(directory.path(), native_options()).unwrap();
}

#[test]
fn a_mixed_relationship_resolves_documents_created_in_the_same_batch() {
    use crate::property_graph::{GraphName, NodeRef};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    store
        .apply_native_mixed(
            &IngestBatch::new(vec![document(91), document(92)]),
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "shared", "link").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    relationship_type: GraphName::new("LINK").unwrap(),
                    properties: &[],
                    source: NodeRef::Existing(NodeId::new(91).unwrap()),
                    target: NodeRef::Existing(NodeId::new(92).unwrap()),
                }),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("mixed endpoints must resolve the prepared document rows");
    assert_eq!(
        record_document_version(&store, NodeId::new(91).unwrap()),
        Some(DocumentVersion::new(DocId::new(91), Revision::new(1)))
    );
    drop(store);
    Store::open(directory.path(), native_options()).unwrap();
}

#[test]
fn bulk_document_removal_tombstones_its_node_and_edges() {
    for operation in ["purge", "delete_matching", "retention"] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), native_options()).unwrap();
        store
            .ingest(IngestBatch::new(vec![document(92).with_timestamp(0)]))
            .unwrap();
        store.seal().unwrap();
        store
            .ingest(IngestBatch::new(vec![document(91).with_timestamp(1)]))
            .unwrap();
        store.enable_graph().unwrap();
        disable_generation_fixture_maintenance(&store);
        let relationship = link_documents(&store);
        match operation {
            "purge" => {
                let token = store.purge(&[DocId::new(92)]).unwrap();
                store.await_physical_purge(token).unwrap();
            }
            "delete_matching" => {
                let report = store
                    .delete_matching(&crate::meta::Predicate::Eq {
                        column: crate::meta::TIMESTAMP_COLUMN,
                        value: crate::meta::PredicateValue::I64(0),
                    })
                    .unwrap();
                assert_eq!(report.deleted_ids(), &[DocId::new(92)]);
            }
            "retention" => {
                store.drop_partition(0..1).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            !super::recovery::relationship_is_visible(&store, relationship),
            "{operation}"
        );
        assert!(
            super::recovery::observe_node(&store, NodeId::new(92).unwrap()).is_none(),
            "{operation}"
        );
        assert!(
            store
                .get_documents(&[DocId::new(92)], crate::lifecycle::DocumentFields::ALL)
                .unwrap()
                .iter()
                .all(Option::is_none),
            "{operation}"
        );
        drop(store);
        let reopened = Store::open(directory.path(), native_options()).unwrap();
        assert!(
            !super::recovery::relationship_is_visible(&reopened, relationship),
            "{operation} reopen"
        );
        assert_eq!(
            document_nodes(&reopened).metadata().rows,
            1,
            "{operation} reopen"
        );
    }
}

#[test]
fn failed_document_node_deletion_fences_writers_and_recovers_both_halves() {
    shared_document_delete_faults();
}

pub(super) fn shared_document_delete_faults() {
    use super::publication::{FaultPoint, RecordingVfs};
    use std::sync::Arc;
    for point in [
        FaultPoint::Append,
        FaultPoint::PartialAppend,
        FaultPoint::ManifestWrite,
        FaultPoint::PostManifestRename,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let vfs = Arc::new(RecordingVfs::default());
        let store = Store::open_with_test_dependencies(
            directory.path(),
            native_options(),
            crate::lifecycle::StoreTestDependencies::new(
                vfs.clone(),
                Arc::new(crate::lifecycle::SystemMonotonicClock),
            ),
        )
        .unwrap();
        store
            .ingest(IngestBatch::new(vec![document(91), document(92)]))
            .unwrap();
        store.seal().unwrap();
        store.enable_graph().unwrap();
        disable_generation_fixture_maintenance(&store);
        let relationship = link_documents(&store);
        vfs.arm_fault(point);
        assert!(
            store
                .delete(crate::ingest::DeleteBatch::new(vec![DocId::new(92)]))
                .is_err(),
            "{point:?}"
        );
        vfs.assert_fired_once();
        assert!(
            store.ingest(IngestBatch::new(vec![document(93)])).is_err(),
            "{point:?} shared fence"
        );
        drop(store);
        let reopened = Store::open(directory.path(), native_options()).unwrap();
        let deleted = matches!(
            point,
            FaultPoint::ManifestWrite | FaultPoint::PostManifestRename
        );
        assert_eq!(
            reopened.count_documents(None, None).unwrap().count,
            if deleted { 1 } else { 2 },
            "{point:?}"
        );
        assert_eq!(
            super::recovery::relationship_is_visible(&reopened, relationship),
            !deleted,
            "{point:?}"
        );
        assert_eq!(
            super::recovery::observe_node(&reopened, NodeId::new(92).unwrap()).is_some(),
            !deleted,
            "{point:?}"
        );
        reopened
            .ingest(IngestBatch::new(vec![document(93)]))
            .unwrap();
    }
}

mod ze404_counts {
    use super::*;
    use crate::property_graph::query::resources::QueryMemory;
    use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
    use crate::property_graph::storage::tree::directory::TreeResources;
    use crate::property_graph::storage::{
        GraphReadView, LabelSelection, NativeCatalog, NativeQuerySource, NativeReadCapability,
    };

    fn counts(
        store: &Store,
        lease: &crate::lifecycle::native_graph::NativeReadLease,
    ) -> (i64, i64) {
        let shared = crate::property_graph::resources::GraphResources::from_store(store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(lease, &runtime).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &resources, 32).unwrap();
        let catalog = NativeCatalog::open(&source, &mut resources).unwrap();
        drop(resources);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let all = view.global_node_count(false, &mut runtime).unwrap();
        let documents = view.global_node_count(true, &mut runtime).unwrap();
        // The original node cursor is an independent visibility oracle.
        for (selection, expected) in [
            (LabelSelection::All, all),
            (
                LabelSelection::AllOf(&[
                    crate::property_graph::catalog::LabelId::new(u64::MAX).unwrap()
                ]),
                documents,
            ),
        ] {
            let mut cursor = view.node_cursor(selection, &mut runtime).unwrap();
            let mut count = 0;
            let mut row = [NodeId::new(1).unwrap()];
            loop {
                let (n, state) = view
                    .scan_nodes(&mut cursor, &mut row, &mut runtime)
                    .unwrap();
                count += n as i64;
                if state == crate::property_graph::storage::CursorState::Done {
                    break;
                }
            }
            assert_eq!(expected, count);
        }
        (all, documents)
    }

    #[test]
    fn ze404_counts_follow_retained_generation() {
        use super::super::publication::{FaultPoint, RecordingVfs};
        use std::sync::Arc;
        for point in [FaultPoint::PartialAppend, FaultPoint::PostManifestRename] {
            let directory = tempfile::tempdir().unwrap();
            let vfs = Arc::new(RecordingVfs::default());
            let store = Store::open_with_test_dependencies(
                directory.path(),
                native_options(),
                crate::lifecycle::StoreTestDependencies::new(
                    vfs.clone(),
                    Arc::new(crate::lifecycle::SystemMonotonicClock),
                ),
            )
            .unwrap();
            store
                .ingest(IngestBatch::new(vec![
                    document(0),
                    document(91),
                    document(92),
                ]))
                .unwrap();
            store.enable_graph().unwrap();
            disable_generation_fixture_maintenance(&store);
            let old = store.admit_native_read().unwrap();
            assert_eq!(counts(&store, &old), (3, 3));
            link_documents(&store);
            store.seal().unwrap();
            store.ingest(IngestBatch::new(vec![document(93)])).unwrap();
            let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "ze407", "extra").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&image)),
                    }],
                    &QueryControl::Cancel(CancelToken::new()),
                )
                .unwrap();
            let current = store.admit_native_read().unwrap();
            assert_eq!(counts(&store, &old), (3, 3));
            assert_eq!(counts(&store, &current), (5, 4));
            vfs.arm_fault(point);
            assert!(
                store
                    .delete(crate::ingest::DeleteBatch::new(vec![DocId::new(92)]))
                    .is_err()
            );
            vfs.assert_fired_once();
            assert_eq!(counts(&store, &old), (3, 3));
            assert_eq!(counts(&store, &current), (5, 4));
            drop(current);
            drop(old);
            drop(store);
            let reopened = Store::open(directory.path(), native_options()).unwrap();
            let expected = if matches!(point, FaultPoint::PostManifestRename) {
                (4, 3)
            } else {
                (5, 4)
            };
            assert_eq!(
                counts(&reopened, &reopened.admit_native_read().unwrap()),
                expected
            );
        }
    }
}
