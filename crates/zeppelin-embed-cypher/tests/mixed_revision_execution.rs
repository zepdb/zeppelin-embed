//! ZE-57 S9: structured and Cypher writes share revisions and deletion fences.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
mod support;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl};
use zeppelin_embed::property_graph::query::completed::{GraphQueryOptions, Outcome, Value};
use zeppelin_embed::property_graph::staging::{
    StageError, StructuredOperation, StructuredWrite, WriteImage,
};
use zeppelin_embed::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphGeneration, GraphGetOptions,
    GraphName, GraphProperty, GraphRevision, GraphStore, GraphStoreErrorKind, GraphWriteOutcome,
    KeyLifecycleError, NodeId, PropertyData, PropertyValue,
};
use zeppelin_embed_cypher::{CompileLimits, execute};

fn revision(n: u64) -> GraphRevision {
    GraphRevision::new(n).unwrap()
}
fn properties(v: i64) -> [GraphProperty<'static>; 2] {
    [
        GraphProperty::new(
            GraphName::new("key").unwrap(),
            PropertyValue::new(PropertyData::String("one")).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("v").unwrap(),
            PropertyValue::new(PropertyData::I64(v)).unwrap(),
        ),
    ]
}
fn assert_node(graph: &GraphStore, id: NodeId, rev: u64, v: i64, control: &QueryControl) {
    let result = graph
        .get_nodes(&[id], GraphGetOptions::default(), control)
        .unwrap();
    let node = result.nodes()[0].as_ref().unwrap();
    assert_eq!(node.id, id);
    assert_eq!(node.revision, revision(rev));
    let property = result
        .properties(node.properties)
        .iter()
        .find(|p| result.string(p.name) == Some("v"))
        .unwrap();
    assert_eq!(result.value(property.value), Some(&Value::I64(v)));
}

#[test]
fn ze57_local_mixed_structured_and_cypher_revisions_keep_deleted_key_fence() {
    let root = support::unique_temp_dir("ze57-mixed-revisions");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("graph");
    let options = || OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024);
    let graph = GraphStore::create(&path, options(), None).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let key = ApplicationKey::new(EntityKind::Node, "ze57", "one").unwrap();
    let mut original_properties = properties(1);
    let original = CanonicalContents::node(&mut [], &mut original_properties, None, None).unwrap();
    let create = StructuredWrite {
        key,
        revision: revision(1),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&original)),
    };
    let created = graph.apply_batch(&[create], &control).unwrap();
    assert_eq!(created.receipts().len(), 1);
    let receipt = created.receipts()[0];
    assert_eq!(receipt.revision, revision(1));
    // Creation 1 + structured create 1 = generation 2.
    assert_eq!(receipt.generation, GraphGeneration::new(2));
    assert!(!receipt.replayed);
    let EntityId::Node(id) = receipt.entity else {
        panic!("expected node receipt")
    };
    assert_node(&graph, id, 1, 1, &control);
    let run = |graph: &GraphStore, query: &str| {
        execute(
            graph.statement_store(),
            &control,
            &GraphQueryOptions::default(),
            query,
            &[],
            CompileLimits::default(),
        )
        .unwrap()
    };
    assert!(matches!(
        run(&graph, "MATCH (n {key: 'one'}) SET n.v = 2")
            .metadata()
            .outcome,
        Outcome::Committed { .. }
    ));
    assert_node(&graph, id, 2, 2, &control);
    let generation = run(&graph, "RETURN 1").metadata().generation;
    let mut changed_properties = properties(3);
    let changed = CanonicalContents::node(&mut [], &mut changed_properties, None, None).unwrap();
    // The public API takes the requested installed revision, not a separate
    // revision precondition. Pin stale 1, conflicting 2, then install 3.
    for requested in [1, 2] {
        let error = graph
            .apply_batch(
                &[StructuredWrite {
                    key,
                    revision: revision(requested),
                    operation: StructuredOperation::Put(receipt.entity),
                    image: Some(WriteImage::Node(&changed)),
                }],
                &control,
            )
            .unwrap_err();
        assert_eq!(error.kind(), GraphStoreErrorKind::Constraint);
        assert!(error.nothing_committed());
        if requested == 1 {
            assert!(
                matches!(error.stage_error(), Some(StageError::Lifecycle(KeyLifecycleError::Stale { current })) if *current == revision(2))
            );
        } else {
            assert!(matches!(
                error.stage_error(),
                Some(StageError::Lifecycle(KeyLifecycleError::RevisionConflict))
            ));
        }
        assert_eq!(run(&graph, "RETURN 1").metadata().generation, generation);
        assert_node(&graph, id, 2, 2, &control);
    }
    let put = graph
        .apply_batch(
            &[StructuredWrite {
                key,
                revision: revision(3),
                operation: StructuredOperation::Put(receipt.entity),
                image: Some(WriteImage::Node(&changed)),
            }],
            &control,
        )
        .unwrap();
    assert_eq!(put.receipts()[0].revision, revision(3));
    assert_eq!(put.receipts()[0].entity, receipt.entity);
    assert_eq!(
        put.outcome(),
        GraphWriteOutcome::Committed {
            // Creation + create + Cypher SET + structured PUT.
            generation: GraphGeneration::new(4)
        }
    );
    assert_node(&graph, id, 3, 3, &control);
    assert!(matches!(
        run(&graph, "MATCH (n {key: 'one'}) DELETE n")
            .metadata()
            .outcome,
        Outcome::Committed { .. }
    ));
    let generation = run(&graph, "RETURN 1").metadata().generation;
    assert_eq!(generation, GraphGeneration::new(5));
    let assert_fence = |graph: &GraphStore, generation: GraphGeneration| {
        assert_eq!(
            graph.statement_store().snapshot().unwrap().generation(),
            generation.get()
        );
        // Reads and refused writes must preserve the admitted store generation.
        assert_eq!(run(graph, "RETURN 1").metadata().generation, generation);
        let error = graph.apply_batch(&[create], &control).unwrap_err();
        assert_eq!(error.kind(), GraphStoreErrorKind::Constraint);
        assert!(error.nothing_committed());
        assert!(
            matches!(error.stage_error(), Some(StageError::Lifecycle(KeyLifecycleError::Stale { current })) if *current == revision(4))
        );
        assert_eq!(run(graph, "RETURN 1").metadata().generation, generation);
        assert!(
            graph
                .get_nodes(&[id], GraphGetOptions::default(), &control)
                .unwrap()
                .nodes()[0]
                .is_none()
        );
        // Even a newer ordinary create cannot bypass a retained deletion fence.
        let error = graph
            .apply_batch(
                &[StructuredWrite {
                    revision: revision(5),
                    ..create
                }],
                &control,
            )
            .unwrap_err();
        assert!(matches!(
            error.stage_error(),
            Some(StageError::Lifecycle(KeyLifecycleError::DeletedKey))
        ));
        assert!(error.nothing_committed());
        assert_eq!(run(graph, "RETURN 1").metadata().generation, generation);
    };
    assert_fence(&graph, generation);
    graph.close().unwrap();
    let reopened = GraphStore::open(&path, options(), None).unwrap();
    // Close checkpoints the manifest once; coherent reads now expose that bump.
    let generation = GraphGeneration::new(6);
    assert_fence(&reopened, generation);
    assert_eq!(
        reopened.apply_batch(&[], &control).unwrap().outcome(),
        GraphWriteOutcome::NoOp
    );
    assert_eq!(run(&reopened, "RETURN 1").metadata().generation, generation);
    reopened.close().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
