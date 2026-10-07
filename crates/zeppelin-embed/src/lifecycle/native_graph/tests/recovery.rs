use super::publication::DurabilityEvent;
use super::publication::{FaultPoint, RecordingVfs};
use super::tempfile;
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::catalog::{Symbol, SymbolKind};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::adjacency::RelationshipRow;
use crate::property_graph::storage::search::Modality;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::storage::{
    CursorState, DirectionSelection, GraphReadView, NativeCatalog, NativeQuerySource,
    NativeReadCapability, RelationshipTypeSelection,
};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphDeleteMode,
    GraphGeneration, GraphName, GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData,
    PropertyValue, RelId, StoreInstanceId,
};
use crate::vfs::{StdVfs, Vfs};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[cfg(test)]
#[path = "recovery_random.rs"]
mod random;

#[test]
fn random_operation_sequences_reopen_to_the_model_state() {
    random::run();
}

#[test]
fn a_graph_checkpoint_after_a_later_document_batch_reopens() {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-later-document");
    let acknowledged = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(91), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .unwrap()
        .generation();
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    assert_eq!(store.snapshot().unwrap().generation(), acknowledged + 1);
    drop(store);
    for access in [
        crate::lifecycle::AccessMode::ReadWrite,
        crate::lifecycle::AccessMode::ReadOnly,
        crate::lifecycle::AccessMode::ReadWrite,
    ] {
        let reopened =
            Store::open(directory.path(), native_options().with_access_mode(access)).unwrap();
        assert_eq!(reopened.snapshot().unwrap().generation(), acknowledged + 1);
        assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
        assert!(
            reopened
                .get_documents(&[DocId::new(91)], crate::lifecycle::DocumentFields::NONE)
                .unwrap()[0]
                .is_some()
        );
        assert!(observe_node(&reopened, node).is_some());
    }
    let store = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&store);
    let next = commit_tail_test_node(&store, "after-later-document");
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(reopened.snapshot().unwrap().generation(), acknowledged + 2);
    assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
    assert!(observe_node(&reopened, node).is_some());
    assert!(observe_node(&reopened, next).is_some());
}

fn purge_documents(ids: &[u128], revision: u64) -> crate::ingest::IngestBatch {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    IngestBatch::new(
        ids.iter()
            .map(|id| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(*id), Revision::new(revision)),
                    vec![1.0, 0.0],
                )
            })
            .collect(),
    )
}

fn assert_purge_reopens(
    path: &Path,
    generation: u64,
    alive: &[u128],
    dead: &[u128],
    nodes: &[NodeId],
) {
    use crate::lifecycle::{AccessMode, DocumentFields};
    for access in [
        AccessMode::ReadOnly,
        AccessMode::ReadWrite,
        AccessMode::ReadOnly,
        AccessMode::ReadWrite,
    ] {
        let store = Store::open(path, native_options().with_access_mode(access)).unwrap();
        assert_eq!(store.snapshot().unwrap().generation(), generation);
        assert_eq!(
            store.count_documents(None, None).unwrap().count,
            alive.len() as u64
        );
        for (ids, present) in [(alive, true), (dead, false)] {
            for id in ids {
                assert_eq!(
                    store
                        .get_documents(&[crate::ingest::DocId::new(*id)], DocumentFields::NONE)
                        .unwrap()[0]
                        .is_some(),
                    present
                );
            }
        }
        for node in nodes {
            assert!(observe_node(&store, *node).is_some());
        }
    }
}

const PURGE_PUBLIC_MARKER: &str = "ZE346PUBLICPURGEDDOCUMENT";

fn public_graph_write(path: &Path, key: &str) -> NodeId {
    use crate::property_graph::GraphStore;
    let graph = GraphStore::open(path, native_options(), None).unwrap();
    graph
        .set_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let result = graph
        .apply_batch(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "purge", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let EntityId::Node(node) = result.receipts()[0].entity else {
        panic!("node receipt")
    };
    // Drop models stopping before close's checkpoint.
    drop(graph);
    node
}

fn public_purge_fixture(path: &Path) -> NodeId {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    let store = Store::open(path, native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(91), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text(PURGE_PUBLIC_MARKER),
        ]))
        .unwrap();
    store.enable_graph().unwrap();
    drop(store);
    public_graph_write(path, "before-purge")
}

fn assert_public_purge_complete(path: &Path, first: NodeId) {
    use crate::property_graph::{GraphGetOptions, GraphStore};
    assert!(!path.join(crate::ingest::PURGE_INTENT_FILE).exists());
    assert!(file_snapshot(path).values().all(|bytes| {
        !bytes
            .windows(PURGE_PUBLIC_MARKER.len())
            .any(|window| window == PURGE_PUBLIC_MARKER.as_bytes())
    }));
    let store = Store::open(path, native_options()).unwrap();
    assert_eq!(store.count_documents(None, None).unwrap().count, 0);
    drop(store);
    let next = public_graph_write(path, "after-purge");
    let graph = GraphStore::open(path, native_options(), None).unwrap();
    let nodes = graph
        .get_nodes(
            &[first, next],
            GraphGetOptions::default(),
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert!(nodes.nodes().iter().all(Option::is_some));
}

#[test]
fn delete_matching_on_a_store_with_an_unabsorbed_graph_write_completes() {
    let directory = tempfile::tempdir().unwrap();
    let node = public_purge_fixture(directory.path());
    let store = Store::open(directory.path(), native_options()).unwrap();
    let report = store
        .delete_matching(&crate::meta::Predicate::And(Vec::new()))
        .unwrap();
    assert_eq!(report.deleted_ids(), &[crate::ingest::DocId::new(91)]);
    drop(store);
    assert_public_purge_complete(directory.path(), node);
}

#[test]
fn a_namespace_delete_on_a_store_with_an_unabsorbed_graph_write_completes() {
    use crate::lifecycle::{LiveNamespaceMutation, NamespaceMutation, namespace_batch_live};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("alpha");
    let node = public_purge_fixture(&path);
    let store = Store::open(&path, native_options()).unwrap();
    let other = Store::open(directory.path().join("beta"), native_options()).unwrap();
    namespace_batch_live(
        directory.path(),
        vec![
            LiveNamespaceMutation {
                store: &store,
                mutation: NamespaceMutation {
                    name: "alpha".into(),
                    options: native_options(),
                    upserts: Vec::new(),
                    deletes: vec![crate::ingest::DocId::new(91)],
                    delete_where: None,
                },
            },
            LiveNamespaceMutation {
                store: &other,
                mutation: NamespaceMutation {
                    name: "beta".into(),
                    options: native_options(),
                    upserts: Vec::new(),
                    deletes: Vec::new(),
                    delete_where: None,
                },
            },
        ],
    )
    .unwrap();
    // The other participant retains a usable WAL writer after completion.
    other.ingest(purge_documents(&[92], 1)).unwrap();
    drop(store);
    drop(other);
    assert_public_purge_complete(&path, node);
}

#[test]
fn a_failed_graph_checkpoint_during_purge_leaves_the_store_openable_and_the_purge_retryable() {
    for (point, live_retry) in [
        (FaultPoint::ManifestSync, true),
        (FaultPoint::ManifestSync, false),
        (FaultPoint::SelectorSync, false),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let node = public_purge_fixture(directory.path());
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
        let token = store.purge(&[crate::ingest::DocId::new(91)]).unwrap();
        vfs.arm_fault(point);
        let error = store.await_physical_purge(token.clone()).unwrap_err();
        vfs.assert_fired_once();
        assert!(error.to_string().contains("scheduled"), "{error}");
        assert!(
            directory
                .path()
                .join(crate::ingest::PURGE_INTENT_FILE)
                .exists()
        );
        assert_shared_writer_stopped(&store);
        if live_retry || point == FaultPoint::SelectorSync {
            // Retrying the failed publication on this handle must refuse.
            // Writable reopen below still completes the pending purge.
            assert!(store.await_physical_purge(token).is_err());
        }
        drop(store);
        // Writable open retries the checkpoint and pending purge automatically.
        let store = Store::open(directory.path(), native_options()).unwrap();
        drop(store);
        assert_public_purge_complete(directory.path(), node);
    }
}

#[test]
fn a_purge_with_an_unabsorbed_graph_write_reopens() {
    use crate::ingest::DocId;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    store.ingest(purge_documents(&[91, 92, 93], 1)).unwrap();
    let node = commit_tail_test_node(&store, "unabsorbed");
    let token = store.purge(&[DocId::new(91)]).unwrap();
    let generation = store.await_physical_purge(token).unwrap().generation();
    drop(store);
    assert_purge_reopens(directory.path(), generation, &[92, 93], &[91], &[node]);
    let store = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&store);
    let next = commit_tail_test_node(&store, "after-unabsorbed-purge");
    let next_generation = store.snapshot().unwrap().generation();
    assert!(next_generation > generation);
    drop(store);
    assert_purge_reopens(
        directory.path(),
        next_generation,
        &[92, 93],
        &[91],
        &[node, next],
    );
}

#[test]
fn a_purge_after_a_checkpoint_with_active_documents_reopens() {
    use crate::ingest::DocId;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    store.ingest(purge_documents(&[91, 92], 1)).unwrap();
    store.ingest(purge_documents(&[91], 2)).unwrap();
    let node = commit_tail_test_node(&store, "folded-with-active-documents");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let token = store.purge(&[DocId::new(91)]).unwrap();
    let generation = store.await_physical_purge(token).unwrap().generation();
    drop(store);
    assert_purge_reopens(directory.path(), generation, &[92], &[91], &[node]);
    let store = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&store);
    let next = commit_tail_test_node(&store, "after-folded-purge");
    let next_generation = store.snapshot().unwrap().generation();
    assert_eq!(next_generation, generation + 1);
    drop(store);
    assert_purge_reopens(
        directory.path(),
        next_generation,
        &[92],
        &[91],
        &[node, next],
    );
}

#[test]
fn a_wal_rename_that_succeeds_then_reports_failure_fences_the_writers() {
    use crate::ingest::DocId;
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
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-rename-error");
    store.ingest(purge_documents(&[91, 92, 93], 1)).unwrap();
    let token = store.purge(&[DocId::new(91)]).unwrap();
    vfs.take();
    vfs.arm_fault(FaultPoint::PostWalRename);
    let error = store.await_physical_purge(token).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("scheduled post-WAL-rename error"),
        "{error}"
    );
    vfs.assert_fired_once();
    assert!(vfs.take().iter().any(|event| matches!(event, DurabilityEvent::Rename(_, to) if to.file_name().is_some_and(|name| name == "wal.ze"))));
    let wal = std::fs::read(directory.path().join("wal.ze")).unwrap();
    assert!(
        store.ingest(purge_documents(&[999], 1)).is_err(),
        "a reported rename error must fence even if rename happened"
    );
    assert_eq!(std::fs::read(directory.path().join("wal.ze")).unwrap(), wal);
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&reopened);
    let generation = reopened.snapshot().unwrap().generation();
    let next = commit_tail_test_node(&reopened, "after-rename-error");
    drop(reopened);
    assert_purge_reopens(
        directory.path(),
        generation + 1,
        &[92, 93],
        &[91, 999],
        &[node, next],
    );
}

#[test]
fn a_budget_failure_after_wal_replacement_fences_the_writers() {
    use crate::ingest::{DeleteBatch, DocId};
    use crate::lifecycle::stats::{AccountedCounter, AllocationComponent};
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
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-budget-purge");
    // Individual batches have no op-11 overhead. The retained image grows
    // when purge wraps these small documents into one replacement batch.
    for id in 91..219 {
        store.ingest(purge_documents(&[id], 1)).unwrap();
    }
    let generation = store.snapshot().unwrap().generation();
    let token = store.purge(&[DocId::new(91)]).unwrap();
    let pressure = Arc::new(std::sync::Mutex::new(None));
    let held = pressure.clone();
    let accounting = store.accounting.clone();
    let path = directory.path().join("manifest.ze");
    // Existing post-manifest-sync seam: next_active is already allocated,
    // and the active purge generation is durable, but WAL replacement follows.
    vfs.after_selector_sync(move || {
        let manifest = crate::manifest::io::load_manifest(&StdVfs, &path, u64::MAX).unwrap();
        if manifest.generation == generation + 2 && held.lock().unwrap().is_none() {
            let current = accounting.graph_resource_bytes().unwrap().0;
            let mut reservation =
                AccountedCounter::new(&accounting, AllocationComponent::Cache).unwrap();
            reservation
                .set(usize::try_from(accounting.resident_limit() - current).unwrap())
                .unwrap();
            *held.lock().unwrap() = Some(reservation);
        }
        Ok(())
    });
    let original_wal_bytes = std::fs::metadata(directory.path().join("wal.ze"))
        .unwrap()
        .len();
    vfs.take();
    let error = store.await_physical_purge(token.clone()).unwrap_err();
    assert!(
        matches!(
            error,
            crate::ingest::PurgeError::Store(crate::lifecycle::StoreError::BudgetExceeded {
                component: "wal",
                ..
            })
        ),
        "{error:?}"
    );
    let events = vfs.take();
    assert!(events.iter().any(|event| matches!(event, DurabilityEvent::Rename(_, to) if to.file_name().is_some_and(|name| name == "wal.ze"))), "failure must be after WAL rename");
    assert!(
        std::fs::metadata(directory.path().join("wal.ze"))
            .unwrap()
            .len()
            > original_wal_bytes,
        "op-11 replacement must grow the WAL"
    );
    let rename = events.iter().position(|event| matches!(event, DurabilityEvent::Rename(_, to) if to.file_name().is_some_and(|name| name == "wal.ze"))).unwrap();
    assert!(
        !events[rename + 1..].iter().any(
            |event| matches!(event, DurabilityEvent::Sync(path, _) if path == directory.path())
        ),
        "accounting failure precedes directory sync"
    );
    assert!(pressure.lock().unwrap().take().is_some());
    let wal = std::fs::read(directory.path().join("wal.ze")).unwrap();
    assert!(
        store.ingest(purge_documents(&[999], 1)).is_err(),
        "document writers must be fenced"
    );
    assert!(
        store
            .delete(DeleteBatch::new(vec![DocId::new(92)]))
            .is_err(),
        "delete writers must be fenced"
    );
    assert!(
        store.await_physical_purge(token).is_err(),
        "purge retry must preserve the fence"
    );
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    assert!(
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "tail", "budget-fenced").unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new())
            )
            .is_err(),
        "graph writers must be fenced"
    );
    assert_eq!(std::fs::read(directory.path().join("wal.ze")).unwrap(), wal);
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&reopened);
    assert_eq!(reopened.count_documents(None, None).unwrap().count, 127);
    assert!(observe_node(&reopened, node).is_some());
    let recovered_generation = reopened.snapshot().unwrap().generation();
    let next = commit_tail_test_node(&reopened, "after-budget-purge");
    drop(reopened);
    assert_purge_reopens(
        directory.path(),
        recovered_generation + 1,
        &(92..219).collect::<Vec<_>>(),
        &[91, 999],
        &[node, next],
    );
}

#[test]
fn a_purge_retry_after_a_failed_directory_sync_keeps_the_fence() {
    use crate::ingest::DocId;
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
    store.ingest(purge_documents(&[91, 92], 1)).unwrap();
    store.seal().unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-failed-purge");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let generation = store.snapshot().unwrap().generation();
    assert_eq!(generation, 5);
    let token = store.purge(&[DocId::new(91)]).unwrap();
    vfs.take();
    vfs.arm_fault_after(FaultPoint::DirectorySync, 1);
    assert!(store.await_physical_purge(token.clone()).is_err());
    vfs.assert_fired_once();
    assert!(
        vfs.take()
            .iter()
            .any(|event| matches!(event, DurabilityEvent::Rename(_, path)
        if path.file_name().is_some_and(|name| name == "manifest.ze")))
    );
    assert_eq!(store.snapshot().unwrap().generation(), generation);
    assert_eq!(
        crate::manifest::io::load_manifest(
            &StdVfs,
            &directory.path().join("manifest.ze"),
            u64::MAX
        )
        .unwrap()
        .generation,
        6
    );
    let wal_before = std::fs::read(directory.path().join("wal.ze")).unwrap();
    let manifest_before = std::fs::read(directory.path().join("manifest.ze")).unwrap();
    assert!(
        store.await_physical_purge(token).is_err(),
        "retry must preserve the publication fence"
    );
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    assert!(
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "tail", "after-failed-purge")
                        .unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new())
            )
            .is_err()
    );
    assert_eq!(
        std::fs::read(directory.path().join("wal.ze")).unwrap(),
        wal_before
    );
    assert_eq!(
        std::fs::read(directory.path().join("manifest.ze")).unwrap(),
        manifest_before
    );
    assert_eq!(store.snapshot().unwrap().generation(), generation);
    drop(store);
    // Writable recovery finishes the already published replacement, at 6.
    let store = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(store.snapshot().unwrap().generation(), 6);
    drop(store);
    assert_purge_reopens(directory.path(), 6, &[92], &[91], &[node]);
}

#[test]
fn a_purge_after_retention_removed_the_segment_of_an_earlier_delete_reopens() {
    use crate::ingest::{
        DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(91), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_timestamp(91),
        ]))
        .unwrap();
    store.seal().unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    store.ingest(purge_documents(&[92, 93], 1)).unwrap();
    store
        .delete(DeleteBatch::new(vec![DocId::new(91)]))
        .unwrap();
    let node = commit_tail_test_node(&store, "before-retention-purge");
    store.drop_partition(91..92).unwrap();
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    assert!(store.snapshot().unwrap().segments().is_empty());
    let token = store.purge(&[DocId::new(92)]).unwrap();
    let generation = store.await_physical_purge(token).unwrap().generation();
    drop(store);
    assert_purge_reopens(directory.path(), generation, &[93], &[91, 92], &[node]);
    let store = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&store);
    let next = commit_tail_test_node(&store, "after-retention-purge");
    drop(store);
    assert_purge_reopens(
        directory.path(),
        generation + 1,
        &[93],
        &[91, 92],
        &[node, next],
    );
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MixedObservation {
    store: StoreInstanceId,
    generation: GraphGeneration,
    sequence: u64,
    first_revision: u64,
    second_revision: u64,
    rank_property: Vec<u8>,
    text: Vec<u8>,
    vector_bits: Vec<u32>,
    sparse_vector_bits: Vec<u32>,
    text_count: u64,
    vector_count: u64,
    text_membership: [bool; 2],
    vector_membership: [bool; 2],
    relationship: RelationshipRow,
    outgoing: Vec<RelationshipRow>,
    incoming: Vec<RelationshipRow>,
}

struct ObserveRecoveredMixed {
    first: NodeId,
    second: NodeId,
    relationship: RelId,
}

impl super::super::NativeReadConsumer<MixedObservation> for ObserveRecoveredMixed {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<MixedObservation, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        let first = view
            .lookup_node(self.first, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered first node"))?;
        let second = view
            .lookup_node(self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered second node"))?;
        let property = match view.expression_symbol(
            SymbolKind::Property,
            GraphName::new("rank").map_err(|_| TreeError::Invalid("property name"))?,
            &mut resources,
        )? {
            Some(Symbol::Property(property)) => property,
            _ => return Err(TreeError::Invalid("missing recovered property symbol")),
        };
        let rank = view
            .node_property(&first, property, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered property"))?;
        let mut rank_property =
            vec![0_u8; usize::try_from(rank.len()).map_err(|_| TreeError::Memory)?];
        let rank_bytes = rank_property.len();
        if rank.read_at(0, &mut rank_property, &mut resources)? != rank_bytes {
            return Err(TreeError::Invalid("short recovered property"));
        }
        let text = view
            .stored_text(self.first, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered text"))?;
        let mut text_bytes =
            vec![0_u8; usize::try_from(text.len()).map_err(|_| TreeError::Memory)?];
        let text_length = text_bytes.len();
        if text.read_at(0, &mut text_bytes, &mut resources)? != text_length {
            return Err(TreeError::Invalid("short recovered text"));
        }
        let vector = view
            .vector_payload(self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered vector"))?;
        let mut vector_bits = Vec::new();
        for index in 0..vector.dimensions() {
            vector_bits.push(vector.coordinate(index, &mut resources)?.to_bits());
        }
        let relationship = view
            .lookup_relationship(self.relationship, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered relationship"))?
            .row();
        let first_revision = first.record().revision().get();
        let second_revision = second.record().revision().get();
        drop(resources);

        let sparse = view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let text_membership = [
            sparse
                .lookup(Modality::Text, self.first, &mut resources)?
                .is_some(),
            sparse
                .lookup(Modality::Text, self.second, &mut resources)?
                .is_some(),
        ];
        let first_vector = sparse
            .lookup(Modality::Vector, self.first, &mut resources)?
            .is_some();
        let vector_member = sparse
            .lookup(Modality::Vector, self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered sparse vector"))?;
        let stored = vector_member.vector.ok_or(TreeError::Invalid(
            "missing recovered sparse vector payload",
        ))?;
        let mut sparse_vector_bits = Vec::new();
        for index in 0..stored.dimensions() {
            sparse_vector_bits.push(stored.coordinate(index, &mut resources)?.to_bits());
        }
        let text_count = sparse.text_count();
        let vector_count = sparse.vector_count();
        drop(resources);
        drop(sparse);

        let mut outgoing = Vec::new();
        let mut incoming = Vec::new();
        for (node, direction, output) in [
            (self.first, DirectionSelection::Out, &mut outgoing),
            (self.second, DirectionSelection::In, &mut incoming),
        ] {
            let mut cursor =
                view.expansion_cursor(node, direction, RelationshipTypeSelection::All, runtime)?;
            let mut rows = [relationship; 2];
            loop {
                let (count, state) = view.expand(&mut cursor, &mut rows, runtime)?;
                output.extend_from_slice(
                    rows.get(..count)
                        .ok_or(TreeError::Invalid("recovered expansion extent"))?,
                );
                if state == CursorState::Done {
                    break;
                }
            }
        }
        Ok(MixedObservation {
            store: view.store_instance_id(),
            generation: view.generation(),
            sequence: view.sequence(),
            first_revision,
            second_revision,
            rank_property,
            text: text_bytes,
            vector_bits,
            sparse_vector_bits,
            text_count,
            vector_count,
            text_membership,
            vector_membership: [first_vector, true],
            relationship,
            outgoing,
            incoming,
        })
    }
}

fn compare_mixed(
    expected: &MixedObservation,
    actual: &MixedObservation,
) -> Result<(), &'static str> {
    if expected != actual {
        Err("recovered mixed graph differs from committed observation")
    } else {
        Ok(())
    }
}

struct RecoveryPathReceipt {
    key: &'static str,
    fires: u64,
    clean_controls: u64,
}

fn begin_recovery_path() {
    super::publication::reset_verified_faults();
}

fn finish_recovery_path(key: &'static str) -> RecoveryPathReceipt {
    RecoveryPathReceipt {
        key,
        fires: super::publication::take_verified_faults(),
        // Returning proves the case's real clean control and assertions completed.
        clean_controls: 1,
    }
}

fn wal_path(directory: &Path) -> PathBuf {
    directory.join("wal.ze")
}

pub(super) fn file_snapshot(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(directory)
        .expect("read native graph directory")
        .map(|entry| {
            let path = entry.expect("directory entry").path();
            let bytes = std::fs::read(&path).expect("read native graph file");
            (path, bytes)
        })
        .collect()
}

fn assert_refused_without_vfs_mutation(
    path: &Path,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    vfs: &Arc<RecordingVfs>,
) {
    vfs.take();
    let before = file_snapshot(path);
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let result = Store::open_native_graph_with_infrastructure(
        path,
        options,
        document,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    );
    if let Ok(store) = result {
        store.close().expect("close unexpectedly admitted store");
        panic!("invalid native store was admitted");
    }
    assert_eq!(file_snapshot(path), before);
    let events = vfs.take();
    assert!(
        events
            .iter()
            .all(|event| matches!(event, DurabilityEvent::OpenAppend(_))),
        "refused recovery must issue no VFS mutation call: {events:?}"
    );
}

fn remove_required_and_assert_refused(
    directory: &Path,
    required: crate::property_graph::wal::RequiredRef,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    vfs: &Arc<RecordingVfs>,
) {
    let path = crate::property_graph::storage::allocation::artifact_path(
        directory,
        required.object.artifact,
    );
    let bytes = std::fs::read(&path).expect("required recovery artifact");
    std::fs::remove_file(&path).expect("remove required recovery artifact");
    assert_refused_without_vfs_mutation(directory, options, document, vfs);
    std::fs::write(path, bytes).expect("restore required recovery artifact");
}

pub(super) fn native_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

struct ObserveNode {
    node: NodeId,
}

impl super::super::NativeReadConsumer<Option<(u64, u64, u64)>> for ObserveNode {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Option<(u64, u64, u64)>, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        Ok(view.lookup_node(self.node, &mut resources)?.map(|node| {
            (
                view.generation().get(),
                view.sequence(),
                node.record().revision().get(),
            )
        }))
    }
}

pub(super) fn observe_node(store: &Store, node: NodeId) -> Option<(u64, u64, u64)> {
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveNode { node },
        )
        .expect("observe native node")
}

fn observe_node_with_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
    node: NodeId,
) -> Option<(u64, u64, u64)> {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared graph resources");
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).expect("query memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime = RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default())
        .expect("retained runtime");
    let capability = NativeReadCapability::admit(lease, &runtime).expect("retained capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("retained resources");
    let source = NativeQuerySource::new(capability, &resources, 16).expect("retained source");
    let catalog = NativeCatalog::open(&source, &mut resources).expect("retained catalog");
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).expect("retained graph view");
    let mut resources = TreeResources::for_query(&mut runtime).expect("retained node resources");
    view.lookup_node(node, &mut resources)
        .expect("retained node lookup")
        .map(|record| {
            (
                view.generation().get(),
                view.sequence(),
                record.record().revision().get(),
            )
        })
}

struct ObserveRelationship {
    relationship: RelId,
}

impl super::super::NativeReadConsumer<bool> for ObserveRelationship {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<bool, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        Ok(view
            .lookup_relationship(self.relationship, &mut resources)?
            .is_some())
    }
}

pub(super) fn relationship_is_visible(store: &Store, relationship: RelId) -> bool {
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveRelationship { relationship },
        )
        .expect("observe native relationship")
}

fn checkpoint_fault_reopens(parent: &Path, name: &str, point: FaultPoint) {
    let path = parent.join(name);
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh checkpoint fault store");
    let image =
        CanonicalContents::node(&mut [], &mut [], None, None).expect("checkpoint fault image");
    let receipt = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", name)
                    .expect("checkpoint fault key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("checkpoint fault commit");
    let node = match receipt[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("checkpoint fault node domain"),
    };
    vfs.take();
    vfs.arm_fault(point);
    assert!(
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .is_err()
    );
    vfs.assert_fired_once();
    let selected = checkpoint_from_selected(&path);
    store.close().expect("close checkpoint fault store");
    drop(store);

    vfs.take();
    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen surviving checkpoint authority");
    assert_eq!(observe_node(&reopened, node), Some((2, 1, 1)));
    let admission = reopened
        .admit_native_read()
        .expect("checkpoint fault admission");
    assert_eq!(
        admission.bundle().base().generation,
        selected.state.generation
    );
    drop(admission);
    assert!(
        vfs.take()
            .iter()
            .all(|event| matches!(event, super::publication::DurabilityEvent::OpenAppend(_)))
    );
    reopened
        .close()
        .expect("close recovered checkpoint fault store");
}

struct SelectedGraphCheckpoint<'a> {
    state: crate::property_graph::wal::CommitState<'a>,
    first_sequence: u64,
}

fn checkpoint_from_selected(directory: &Path) -> SelectedGraphCheckpoint<'static> {
    use crate::manifest::io::DurableLog as _;
    let reader = crate::wal::WalReader::open(&StdVfs, &directory.join("wal.ze")).unwrap();
    let manifest = Box::leak(Box::new(
        crate::manifest::io::load_manifest(
            &StdVfs,
            &directory.join("manifest.ze"),
            reader.durable_end(),
        )
        .unwrap(),
    ));
    let graph = manifest.graph.as_ref().unwrap();
    let state = graph.state().unwrap();
    SelectedGraphCheckpoint {
        first_sequence: graph.graph_absorbed_through + 1,
        state,
    }
}

fn first_graph_envelope(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap();
    let replay = crate::wal::replay::replay(&bytes);
    replay
        .records
        .iter()
        .find(|record| record.op == crate::ingest::wal_payload::GRAPH_COMMIT_V1)
        .unwrap()
        .payload
        .to_vec()
}

fn replace_first_graph_envelope(path: &Path, envelope: &[u8]) {
    let bytes = std::fs::read(path).unwrap();
    let replay = crate::wal::replay::replay(&bytes);
    assert_eq!(
        replay.terminator,
        crate::wal::replay::ReplayTerminator::CleanEnd
    );
    let mut output = bytes[..crate::wal::header::WAL_HEADER_LEN].to_vec();
    let mut replaced = false;
    for record in replay.records {
        let payload = if !replaced && record.op == crate::ingest::wal_payload::GRAPH_COMMIT_V1 {
            replaced = true;
            envelope
        } else {
            record.payload
        };
        crate::wal::record::append_record_into(
            crate::wal::record::WalRecord { payload, ..record },
            &mut output,
        )
        .unwrap();
    }
    assert!(replaced);
    std::fs::write(path, output).unwrap();
}

fn corrupt_first_membership_with_valid_checksums(path: &Path) {
    let mut bytes = first_graph_envelope(path);
    let envelope = 0;
    let payload_length = |at: usize, bytes: &[u8]| {
        u32::from_le_bytes(
            bytes[at + 8..at + 12]
                .try_into()
                .expect("WAL payload length"),
        ) as usize
    };
    let begin_length = payload_length(envelope, &bytes);
    let mutation = envelope + 72 + begin_length;
    assert_eq!(
        u16::from_le_bytes(bytes[mutation + 4..mutation + 6].try_into().unwrap()),
        2
    );
    let mutation_length = payload_length(mutation, &bytes);
    bytes[mutation + 65] ^= 1;
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[mutation..mutation + 64 + mutation_length]);
    bytes[mutation + 64 + mutation_length..mutation + 72 + mutation_length]
        .copy_from_slice(&checksum.to_le_bytes());

    let mut commit = mutation + 72 + mutation_length;
    while u16::from_le_bytes(bytes[commit + 4..commit + 6].try_into().unwrap()) != 6 {
        commit += 72 + payload_length(commit, &bytes);
    }
    let digest = xxhash_rust::xxh3::xxh3_64(&bytes[envelope..commit]);
    bytes[commit + 72..commit + 80].copy_from_slice(&digest.to_le_bytes());
    let commit_length = payload_length(commit, &bytes);
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[commit..commit + 64 + commit_length]);
    bytes[commit + 64 + commit_length..commit + 72 + commit_length]
        .copy_from_slice(&checksum.to_le_bytes());
    replace_first_graph_envelope(path, &bytes);
}

fn repair_wal_record(bytes: &mut [u8], offset: usize) {
    let payload_length = u32::from_le_bytes(
        bytes[offset + 8..offset + 12]
            .try_into()
            .expect("WAL payload length"),
    ) as usize;
    let header_checksum = xxhash_rust::xxh3::xxh3_64(&bytes[offset..offset + 56]);
    bytes[offset + 56..offset + 64].copy_from_slice(&header_checksum.to_le_bytes());
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[offset..offset + 64 + payload_length]);
    bytes[offset + 64 + payload_length..offset + 72 + payload_length]
        .copy_from_slice(&checksum.to_le_bytes());
}

fn omit_second_mutation_with_valid_framing(path: &Path) {
    let mut bytes = first_graph_envelope(path);
    let envelope = 0;
    let payload_length = |at: usize, bytes: &[u8]| {
        u32::from_le_bytes(
            bytes[at + 8..at + 12]
                .try_into()
                .expect("WAL payload length"),
        ) as usize
    };
    let mut offset = envelope;
    let mut mutations = Vec::new();
    loop {
        let kind = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
        if kind == 2 {
            mutations.push(offset);
        }
        if kind == 6 {
            break;
        }
        offset += 72 + payload_length(offset, &bytes);
    }
    assert_eq!(mutations.len(), 2, "two-node control has two mutations");
    let removed = mutations[1];
    let removed_bytes = 72 + payload_length(removed, &bytes);
    bytes.drain(removed..removed + removed_bytes);

    let begin_payload = envelope + 64;
    let old_count = u32::from_le_bytes(
        bytes[begin_payload + 40..begin_payload + 44]
            .try_into()
            .expect("begin change count"),
    );
    bytes[begin_payload + 40..begin_payload + 44].copy_from_slice(&(old_count - 1).to_le_bytes());
    let old_size = u64::from_le_bytes(
        bytes[begin_payload + 48..begin_payload + 56]
            .try_into()
            .expect("begin envelope size"),
    );
    bytes[begin_payload + 48..begin_payload + 56]
        .copy_from_slice(&(old_size - removed_bytes as u64).to_le_bytes());
    repair_wal_record(&mut bytes, envelope);

    offset = envelope + 72 + payload_length(envelope, &bytes);
    loop {
        let kind = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
        if offset >= removed {
            let index = u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap());
            bytes[offset + 12..offset + 16].copy_from_slice(&(index - 1).to_le_bytes());
        }
        repair_wal_record(&mut bytes, offset);
        if kind == 6 {
            break;
        }
        offset += 72 + payload_length(offset, &bytes);
    }
    let commit = offset;
    let commit_payload = commit + 64;
    let commit_count = u32::from_le_bytes(
        bytes[commit_payload..commit_payload + 4]
            .try_into()
            .expect("commit change count"),
    );
    bytes[commit_payload..commit_payload + 4].copy_from_slice(&(commit_count - 1).to_le_bytes());
    let aggregate = xxhash_rust::xxh3::xxh3_64(&bytes[envelope..commit]);
    bytes[commit_payload + 8..commit_payload + 16].copy_from_slice(&aggregate.to_le_bytes());
    repair_wal_record(&mut bytes, commit);
    replace_first_graph_envelope(path, &bytes);
}

fn run_ze40_complete_mixed_commit_close_reopen_is_coherent()
-> (MixedObservation, RecoveryPathReceipt) {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let document = EmbeddingTower {
        model_id: "ze40-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x40, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let infrastructure: Arc<dyn Vfs> = Arc::new(StdVfs);
    let options = OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024);
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        options.clone(),
        Some(document.clone()),
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let coordinates = [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)];
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let embedding = CanonicalEmbedding::new(&document, &coordinates).expect("embedding");
        let mut properties = [GraphProperty::new(
            GraphName::new("rank").expect("property name"),
            PropertyValue::new(PropertyData::I64(7)).expect("property value"),
        )];
        let mut labels = [GraphName::new("Document").expect("label")];
        let first = CanonicalContents::node(&mut labels, &mut properties, Some("first text"), None)
            .expect("first node");
        let second =
            CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).expect("second node");
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "a").expect("node key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "b").expect("node key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "ab")
                    .expect("relationship key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).expect("first local node")),
                    target: NodeRef::Local(refs.node(1).expect("second local node")),
                    relationship_type: GraphName::new("LINKS").expect("relationship type"),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("mixed commit")
    });
    let first = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("first receipt domain"),
    };
    let second = match receipts[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("second receipt domain"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("relationship receipt domain"),
    };
    let expected = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveRecoveredMixed {
                first,
                second,
                relationship,
            },
        )
        .expect("pre-close mixed observation");
    assert_eq!(expected.rank_property, [3_u8, 7, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(expected.text, b"first text");
    assert_eq!(expected.vector_bits, coordinates.map(f32::to_bits));
    assert_eq!(expected.sparse_vector_bits, coordinates.map(f32::to_bits));
    assert_eq!((expected.text_count, expected.vector_count), (1, 1));
    assert_eq!(expected.text_membership, [true, false]);
    assert_eq!(expected.vector_membership, [false, true]);
    assert_eq!(expected.relationship.source, first);
    assert_eq!(expected.relationship.target, second);
    assert_eq!(expected.outgoing, [expected.relationship]);
    assert_eq!(expected.incoming, [expected.relationship]);
    assert_eq!((expected.generation.get(), expected.sequence), (2, 1));
    store.close().expect("close native store");
    drop(store);

    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        options,
        Some(document),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen native store");
    isolate_recovery_from_foreground_reclaim(&reopened);
    let actual = reopened
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveRecoveredMixed {
                first,
                second,
                relationship,
            },
        )
        .expect("post-reopen mixed observation");
    let mut missing_reverse = expected.clone();
    missing_reverse.incoming.clear();
    assert!(compare_mixed(&missing_reverse, &actual).is_err());
    let mut wrong_generation = expected.clone();
    wrong_generation.generation = GraphGeneration::new(3);
    assert!(compare_mixed(&wrong_generation, &actual).is_err());
    compare_mixed(&expected, &actual).expect("coherent recovered mixed graph");

    let third = CanonicalContents::node(&mut [], &mut [], None, None).expect("third node");
    let next = reopened
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "c").expect("third key"),
                revision: GraphRevision::new(1).expect("third revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&third)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("post-recovery write");
    let third_id = match next[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("third receipt domain"),
    };
    assert!(third_id.get() > second.get());
    assert_eq!(next[0].generation.get(), 3);
    reopened.close().expect("close recovered store");
    (
        actual,
        finish_recovery_path("property-graph.recovery.complete-reopen"),
    )
}

#[cfg_attr(test, test)]
fn ze40_complete_mixed_commit_close_reopen_is_coherent() {
    let _ = run_ze40_complete_mixed_commit_close_reopen_is_coherent();
}

fn run_ze40_committed_artifact_and_framing_damage_fail_without_partial_admission()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let document = EmbeddingTower {
        model_id: "ze40-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x40, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let options = OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024);
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        options.clone(),
        Some(document.clone()),
        Arc::new(StdVfs),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let first =
        CanonicalContents::node(&mut [], &mut [], Some("first text"), None).expect("text node");
    let coordinates = [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)];
    let embedding = CanonicalEmbedding::new(&document, &coordinates).expect("embedding");
    let second =
        CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).expect("vector node");
    crate::property_graph::with_local_refs(|refs| {
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "a").expect("node key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&first)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "b").expect("node key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&second)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "app", "ab")
                            .expect("relationship key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).expect("first local node")),
                            target: NodeRef::Local(refs.node(1).expect("second local node")),
                            relationship_type: GraphName::new("LINKS").expect("relationship type"),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("mixed commit")
    });
    store.close().expect("close native store");
    drop(store);
    corrupt_first_membership_with_valid_checksums(&wal_path(&path));
    let vfs = Arc::new(RecordingVfs::default());
    assert_refused_without_vfs_mutation(&path, options.clone(), Some(document.clone()), &vfs);

    let omitted_path = parent.path().join("omitted-mutation");
    let store = Store::create_native_graph(&omitted_path, options.clone(), None)
        .expect("fresh omitted-mutation store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let receipts = store
        .apply_native_graph(
            &[
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "omitted-a")
                        .expect("first node key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "omitted-b")
                        .expect("second node key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                },
            ],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("two-node graph-only commit");
    let omitted_nodes = receipts
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Node(node) => node,
            EntityId::Relationship(_) => panic!("omitted control node domain"),
        })
        .collect::<Vec<_>>();
    store.close().expect("close omitted-mutation store");
    drop(store);
    let omitted_wal = wal_path(&omitted_path);
    let clean_wal = std::fs::read(&omitted_wal).expect("clean omitted-mutation WAL");
    omit_second_mutation_with_valid_framing(&omitted_wal);
    assert_refused_without_vfs_mutation(&omitted_path, options.clone(), None, &vfs);
    std::fs::write(&omitted_wal, clean_wal).expect("restore omitted-mutation WAL");
    let clean = Store::open_native_graph(&omitted_path, options.clone(), None)
        .expect("clean omitted-mutation control");
    for node in omitted_nodes {
        assert_eq!(observe_node(&clean, node), Some((2, 1, 1)));
    }
    clean.close().expect("close clean omitted-mutation control");

    let checkpoint_path = parent.path().join("checkpoint-only");
    let store = Store::create_native_graph_with_infrastructure(
        &checkpoint_path,
        options.clone(),
        Some(document.clone()),
        Arc::new(StdVfs),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh checkpoint damage store");
    let checkpoint_image = CanonicalContents::node(&mut [], &mut [], Some("checkpoint text"), None)
        .expect("checkpoint text node");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "checkpoint-damage")
                    .expect("checkpoint key"),
                revision: GraphRevision::new(1).expect("checkpoint revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&checkpoint_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("checkpoint damage commit");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint damage cutoff");
    store.close().expect("close checkpoint damage store");
    drop(store);
    let checkpoint = checkpoint_from_selected(&checkpoint_path);
    let mut cancelled = || false;
    let mut resources = crate::property_graph::wal::WalResources::new(
        16 * 1024 * 1024,
        crate::property_graph::wal::STACK_RESERVATION_BYTES,
        &mut cancelled,
    )
    .expect("checkpoint inventory resources");
    let prepared = checkpoint
        .state
        .prepared_inventories
        .get(0, &mut resources)
        .expect("checkpoint prepared inventory");
    let graph = checkpoint
        .state
        .graph
        .slots
        .into_iter()
        .flatten()
        .next()
        .expect("checkpoint native tree");
    let text = checkpoint.state.text.expect("checkpoint sparse text");
    for required in [checkpoint.state.catalog, graph, text, prepared] {
        remove_required_and_assert_refused(
            &checkpoint_path,
            required,
            options.clone(),
            Some(document.clone()),
            &vfs,
        );
    }

    let later_path = parent.path().join("later-envelope");
    let store = Store::create_native_graph(&later_path, options.clone(), None)
        .expect("fresh later-envelope store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    for key in ["first", "second"] {
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", key).expect("node key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("complete envelope");
    }
    store.close().expect("close later-envelope store");
    drop(store);
    let later_wal = wal_path(&later_path);
    let mut bytes = std::fs::read(&later_wal).expect("later WAL bytes");
    let last = bytes.len().checked_sub(1).expect("complete WAL trailer");
    bytes[last] ^= 1;
    std::fs::write(later_wal, bytes).expect("corrupt later complete envelope");
    assert_refused_without_vfs_mutation(&later_path, options, None, &vfs);
    finish_recovery_path("property-graph.recovery.corrupt-missing-artifact")
}

#[cfg_attr(test, test)]
fn ze40_committed_artifact_and_framing_damage_fail_without_partial_admission() {
    let _ = run_ze40_committed_artifact_and_framing_damage_fail_without_partial_admission();
}

fn run_ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let options = native_options();
    let parent = tempfile::tempdir().expect("temporary parent");
    let vfs = Arc::new(RecordingVfs::default());

    let invalid = parent.path().join("invalid");
    std::fs::create_dir(&invalid).expect("invalid native directory");
    std::fs::write(invalid.join(crate::lifecycle::lock::STORE_LOCK_FILE), b"")
        .expect("existing writer lock");
    std::fs::write(invalid.join("graph-root.ze"), b"invalid selector").expect("invalid selector");
    assert_refused_without_vfs_mutation(&invalid, options.clone(), None, &vfs);

    let complete = parent.path().join("complete");
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &complete,
        options.clone(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    store.close().expect("close native store");
    drop(store);
    let checkpoint = checkpoint_from_selected(&complete);

    let manifest_path = complete.join("manifest.ze");
    let manifest_bytes = std::fs::read(&manifest_path).unwrap();
    let mut corrupt = manifest_bytes.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    std::fs::write(&manifest_path, corrupt).unwrap();
    assert_refused_without_vfs_mutation(&complete, options.clone(), None, &vfs);
    std::fs::write(&manifest_path, manifest_bytes).unwrap();
    remove_required_and_assert_refused(
        &complete,
        checkpoint.state.catalog,
        options.clone(),
        None,
        &vfs,
    );
    let documented = parent.path().join("documented");
    let document = EmbeddingTower {
        model_id: "ze40-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x40, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &documented,
        options.clone(),
        Some(document.clone()),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("document native store");
    store.close().expect("close document native store");
    drop(store);
    let mut wrong_document = document;
    wrong_document.weights_digest = vec![0xff];
    assert_refused_without_vfs_mutation(&documented, options.clone(), Some(wrong_document), &vfs);

    let legacy = parent.path().join("legacy");
    let store = Store::open(&legacy, OpenOptions::new()).expect("create legacy store");
    store.close().expect("close legacy store");
    drop(store);
    assert_refused_without_vfs_mutation(&legacy, options, None, &vfs);
    finish_recovery_path("property-graph.recovery.refusal")
}

#[cfg_attr(test, test)]
fn ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation() {
    let _ = run_ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation();
}

fn run_ze40_lost_ack_and_stopped_writer_resolve_on_reopen() -> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("lost-ack");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let image =
        CanonicalContents::node(&mut [], &mut [], Some("durable"), None).expect("node image");
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "lost").expect("node key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    store
        .native_graph
        .fail_next_publication
        .store(true, Ordering::Release);
    assert!(matches!(
        store.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::CommitIndeterminate { .. })
    ));
    assert!(
        !store
            .native_graph
            .fail_next_publication
            .load(Ordering::Acquire)
    );
    assert!(matches!(
        store.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::WritesStopped)
    ));
    store.close().expect("close stopped store");
    drop(store);

    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("recover durable lost acknowledgement");
    isolate_recovery_from_foreground_reclaim(&reopened);
    let replay = reopened
        .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
        .expect("exact retry");
    assert!(replay[0].replayed);
    assert_eq!(replay[0].generation.get(), 2);
    let node = match replay[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node receipt domain"),
    };
    assert_eq!(observe_node(&reopened, node), Some((2, 1, 1)));
    let altered =
        CanonicalContents::node(&mut [], &mut [], Some("altered"), None).expect("altered node");
    assert!(
        reopened
            .apply_native_graph(
                &[StructuredWrite {
                    image: Some(WriteImage::Node(&altered)),
                    ..request[0]
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .is_err()
    );
    reopened.close().expect("close recovered store");
    drop(reopened);

    let old_path = parent.path().join("append-failed");
    let old = Store::create_native_graph_with_infrastructure(
        &old_path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh append-failure store");
    vfs.arm_fault(FaultPoint::Append);
    assert!(matches!(
        old.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::CommitIndeterminate { .. })
    ));
    vfs.assert_fired_once();
    old.close().expect("close append-failed store");
    drop(old);
    let old = Store::open_native_graph_with_infrastructure(
        &old_path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen old state");
    let committed = old
        .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
        .expect("clean control commit");
    assert!(!committed[0].replayed);
    assert_eq!(committed[0].generation.get(), 2);
    old.close().expect("close clean control");
    finish_recovery_path("property-graph.recovery.lost-ack")
}

#[cfg_attr(test, test)]
fn ze40_lost_ack_and_stopped_writer_resolve_on_reopen() {
    let _ = run_ze40_lost_ack_and_stopped_writer_resolve_on_reopen();
}

fn run_ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("torn");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let first_image =
        CanonicalContents::node(&mut [], &mut [], Some("first"), None).expect("first image");
    let first_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "first").expect("first key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first_image)),
    }];
    let first = store
        .apply_native_graph(&first_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("first durable commit");
    let first_node = match first[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node receipt domain"),
    };
    let old_wal = wal_path(&path);
    let complete_prefix = std::fs::read(&old_wal).expect("complete WAL prefix");
    let second_image =
        CanonicalContents::node(&mut [], &mut [], Some("second"), None).expect("second image");
    let second_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "second").expect("second key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&second_image)),
    }];
    vfs.arm_fault(FaultPoint::PartialAppend);
    assert!(matches!(
        store.apply_native_graph(&second_request, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::CommitIndeterminate { .. })
    ));
    vfs.assert_fired_once();
    let torn_bytes = std::fs::read(&old_wal).expect("torn WAL");
    assert!(torn_bytes.starts_with(&complete_prefix));
    assert!(torn_bytes.len() > complete_prefix.len());
    store.close().expect("close torn store");
    drop(store);

    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("recover torn tail");
    isolate_recovery_from_foreground_reclaim(&reopened);
    assert_eq!(observe_node(&reopened, first_node), Some((2, 1, 1)));
    assert_eq!(
        std::fs::read(&old_wal).expect("cut outer WAL"),
        complete_prefix
    );
    let rotated = {
        let guard = reopened
            .native_graph
            .writer
            .lock()
            .expect("recovered writer");
        {
            assert!(guard.as_ref().is_some());
            reopened.directory.join("wal.ze")
        }
    };
    assert_eq!(rotated, old_wal);
    let second = reopened
        .apply_native_graph(&second_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("post-rotation append");
    assert!(!second[0].replayed);
    assert_eq!(second[0].generation.get(), 3);
    reopened.close().expect("close recovered store");
    finish_recovery_path("property-graph.recovery.torn-tail")
}

#[cfg_attr(test, test)]
fn ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append() {
    let _ =
        run_ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append();
}

fn run_ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials() -> RecoveryPathReceipt
{
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("empty-history");
    let store =
        Store::create_native_graph(&path, native_options(), None).expect("fresh native store");
    let node_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let created = crate::property_graph::with_local_refs(|refs| {
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "left")
                            .expect("left key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "right")
                            .expect("right key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "app", "edge")
                            .expect("edge key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).expect("left local")),
                            target: NodeRef::Local(refs.node(1).expect("right local")),
                            relationship_type: GraphName::new("LINKS").expect("type"),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("create graph")
    });
    let left = match created[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("left domain"),
    };
    let right = match created[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("right domain"),
    };
    let edge = match created[2].entity {
        EntityId::Relationship(edge) => edge,
        EntityId::Node(_) => panic!("edge domain"),
    };
    let deleted = [
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", "edge").expect("edge key"),
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Delete(
                EntityId::Relationship(edge),
                GraphDeleteMode::Restrict,
            ),
            image: None,
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "left").expect("left key"),
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Delete(EntityId::Node(left), GraphDeleteMode::Restrict),
            image: None,
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "right").expect("right key"),
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Delete(
                EntityId::Node(right),
                GraphDeleteMode::Restrict,
            ),
            image: None,
        },
    ];
    store
        .apply_native_graph(&deleted, &QueryControl::Cancel(CancelToken::new()))
        .expect("delete complete graph");
    store.close().expect("close empty graph");
    drop(store);

    let reopened = Store::open_native_graph(&path, native_options(), None)
        .expect("reopen empty graph history");
    isolate_recovery_from_foreground_reclaim(&reopened);
    assert_eq!(observe_node(&reopened, left), None);
    assert_eq!(observe_node(&reopened, right), None);
    let before = file_snapshot(&path);
    let replay = reopened
        .apply_native_graph(&deleted, &QueryControl::Cancel(CancelToken::new()))
        .expect("exact delete replay");
    assert!(replay.iter().all(|receipt| receipt.replayed));
    assert_eq!(file_snapshot(&path), before);
    let recreated = reopened
        .apply_native_graph(
            &[
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "left").expect("left key"),
                    revision: GraphRevision::new(3).expect("revision"),
                    operation: StructuredOperation::Recreate(
                        GraphRevision::new(2).expect("deletion revision"),
                    ),
                    image: Some(WriteImage::Node(&node_image)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "right").expect("right key"),
                    revision: GraphRevision::new(3).expect("revision"),
                    operation: StructuredOperation::Recreate(
                        GraphRevision::new(2).expect("deletion revision"),
                    ),
                    image: Some(WriteImage::Node(&node_image)),
                },
            ],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("explicit recreation");
    let recreated_left = match recreated[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("recreated node domain"),
    };
    let recreated_right = match recreated[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("recreated node domain"),
    };
    assert!(recreated_left > left);
    assert!(recreated_right > right);
    reopened.close().expect("close recreated graph");

    let detach_path = parent.path().join("detach-checkpoint");
    let detached = Store::create_native_graph(&detach_path, native_options(), None)
        .expect("fresh detach store");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        detached
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "detach-left")
                            .expect("left key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "detach-right")
                            .expect("right key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "app", "detach-edge")
                            .expect("edge key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).expect("left local")),
                            target: NodeRef::Local(refs.node(1).expect("right local")),
                            relationship_type: GraphName::new("LINKS").expect("type"),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("create detach topology")
    });
    let detach_left = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("detach left domain"),
    };
    let detach_right = match receipts[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("detach right domain"),
    };
    let detach_edge = match receipts[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("detach edge domain"),
    };
    detached
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "detach-left").expect("left key"),
                revision: GraphRevision::new(2).expect("revision"),
                operation: StructuredOperation::Delete(
                    EntityId::Node(detach_left),
                    GraphDeleteMode::Detach,
                ),
                image: None,
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("detach node");
    assert!(!relationship_is_visible(&detached, detach_edge));
    detached
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint detached topology");
    detached.close().expect("close detached topology");
    drop(detached);
    let detached = Store::open_native_graph(&detach_path, native_options(), None)
        .expect("reopen detached checkpoint");
    assert_eq!(observe_node(&detached, detach_left), None);
    assert_eq!(observe_node(&detached, detach_right), Some((3, 2, 1)));
    assert!(!relationship_is_visible(&detached, detach_edge));
    detached.close().expect("close recovered detached topology");
    finish_recovery_path("property-graph.recovery.empty-history")
}

#[cfg_attr(test, test)]
fn ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials() {
    let _ = run_ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials();
}

fn run_ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("checkpoint");
    let store =
        Store::create_native_graph(&path, native_options(), None).expect("fresh native store");
    let first_image =
        CanonicalContents::node(&mut [], &mut [], Some("one"), None).expect("first image");
    let first_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "checkpoint").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first_image)),
    }];
    let first = store
        .apply_native_graph(&first_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("first commit");
    let node = match first[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node domain"),
    };
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("explicit checkpoint");
    let selected = checkpoint_from_selected(&path);
    assert_eq!(selected.state.sequence, 1);
    assert_eq!(selected.state.generation.get(), 2);
    assert_eq!(selected.first_sequence, 2);
    let checkpoint_inventories = selected
        .state
        .prepared_inventories
        .len()
        .expect("checkpoint inventory count");
    assert_eq!(checkpoint_inventories, 1);

    let second_image =
        CanonicalContents::node(&mut [], &mut [], Some("two"), None).expect("second image");
    let second_request = [StructuredWrite {
        key: first_request[0].key,
        revision: GraphRevision::new(2).expect("revision"),
        operation: StructuredOperation::Put(EntityId::Node(node)),
        image: Some(WriteImage::Node(&second_image)),
    }];
    store
        .apply_native_graph(&second_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("post-checkpoint commit");
    let expected_protected = store
        .native_graph
        .writer
        .lock()
        .expect("writer state")
        .as_ref()
        .expect("writer")
        .protected
        .len();
    store.close().expect("close checkpoint store");
    drop(store);

    let reopened = Store::open_native_graph(&path, native_options(), None)
        .expect("reopen checkpoint and tail");
    assert_eq!(observe_node(&reopened, node), Some((4, 2, 2)));
    let admission = reopened.admit_native_read().expect("recovered admission");
    assert_eq!(admission.bundle().base().fold.envelope_sequence, 1);
    assert_eq!(admission.bundle().base().generation.get(), 4);
    assert_eq!(admission.bundle().prepared_inventories().len(), 2);
    drop(admission);
    {
        let writer_guard = reopened
            .native_graph
            .writer
            .lock()
            .expect("recovered writer");
        let writer = writer_guard.as_ref().expect("recovered writer state");
        assert_eq!(writer.complete_envelopes, 1);
        assert!(writer.envelope_bytes > crate::property_graph::wal::HEADER_BYTES);
        assert_eq!(writer.protected.len(), expected_protected);
    }
    reopened.close().expect("close recovered checkpoint store");

    let threshold_path = parent.path().join("count-threshold");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let threshold = Store::create_native_graph_with_infrastructure(
        &threshold_path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh count-threshold store");
    isolate_recovery_from_foreground_reclaim(&threshold);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("threshold image");
    let key = ApplicationKey::new(EntityKind::Node, "app", "threshold").expect("threshold key");
    let first = threshold
        .apply_native_graph(
            &[StructuredWrite {
                key,
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("first threshold commit");
    let threshold_node = match first[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("threshold node domain"),
    };
    for revision in 2..=64 {
        threshold
            .apply_native_graph(
                &[StructuredWrite {
                    key,
                    revision: GraphRevision::new(revision).expect("revision"),
                    operation: StructuredOperation::Put(EntityId::Node(threshold_node)),
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("count-threshold envelope");
    }
    let retained = threshold
        .admit_native_read()
        .expect("retained generation 64");
    assert_eq!(retained.bundle().prepared_inventories().len(), 64);
    vfs.take();
    assert!(
        threshold
            .apply_native_graph(&[], &QueryControl::Cancel(CancelToken::new()))
            .expect("threshold no-op")
            .is_empty()
    );
    assert!(
        threshold
            .apply_native_graph(
                &[StructuredWrite {
                    key,
                    revision: GraphRevision::new(64).expect("revision"),
                    operation: StructuredOperation::Put(EntityId::Node(threshold_node)),
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("threshold replay")[0]
            .replayed
    );
    assert!(vfs.take().is_empty());
    threshold
        .apply_native_graph(
            &[StructuredWrite {
                key,
                revision: GraphRevision::new(65).expect("revision"),
                operation: StructuredOperation::Put(EntityId::Node(threshold_node)),
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("checkpoint and post-cutoff tail");
    assert_eq!(
        observe_node_with_lease(&threshold, &retained, threshold_node),
        Some((65, 64, 64))
    );
    drop(retained);
    let current = threshold
        .admit_native_read()
        .expect("generation 65 admission");
    assert_eq!(current.bundle().base().fold.envelope_sequence, 64);
    assert_eq!(current.bundle().base().generation.get(), 67);
    assert_eq!(current.bundle().prepared_inventories().len(), 65);
    drop(current);
    let (threshold_count, threshold_bytes, threshold_protected) = {
        let guard = threshold
            .native_graph
            .writer
            .lock()
            .expect("threshold writer");
        let writer = guard.as_ref().expect("threshold writer state");
        (
            writer.complete_envelopes,
            writer.envelope_bytes,
            writer.protected.len(),
        )
    };
    assert_eq!(threshold_count, 1);
    threshold.close().expect("close count-threshold store");
    drop(threshold);
    let threshold = Store::open_native_graph_with_infrastructure(
        &threshold_path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen count-threshold store");
    isolate_recovery_from_foreground_reclaim(&threshold);
    assert_eq!(observe_node(&threshold, threshold_node), Some((67, 65, 65)));
    {
        let guard = threshold
            .native_graph
            .writer
            .lock()
            .expect("recovered threshold writer");
        let writer = guard.as_ref().expect("recovered threshold writer state");
        assert_eq!(writer.complete_envelopes, threshold_count);
        assert_eq!(writer.envelope_bytes, threshold_bytes);
        assert_eq!(writer.protected.len(), threshold_protected);
    }
    threshold
        .close()
        .expect("close recovered count-threshold store");

    let byte_path = parent.path().join("byte-threshold");
    let byte_store = Store::create_native_graph(&byte_path, native_options(), None)
        .expect("fresh byte-threshold store");
    isolate_recovery_from_foreground_reclaim(&byte_store);
    let long_key = "k".repeat(512 * 1024);
    let byte_key =
        ApplicationKey::new(EntityKind::Node, "app", &long_key).expect("large threshold key");
    let mut byte_node = None;
    let mut expected_writer = None;
    let mut byte_revision = None;
    for revision in 1..=40 {
        let old_count = {
            let guard = byte_store.native_graph.writer.lock().expect("byte writer");
            guard
                .as_ref()
                .expect("byte writer state")
                .complete_envelopes
        };
        let receipt = byte_store
            .apply_native_graph(
                &[StructuredWrite {
                    key: byte_key,
                    revision: GraphRevision::new(revision).expect("revision"),
                    operation: byte_node.map_or(StructuredOperation::Create, |node| {
                        StructuredOperation::Put(EntityId::Node(node))
                    }),
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("byte-threshold commit");
        if byte_node.is_none() {
            byte_node = match receipt[0].entity {
                EntityId::Node(node) => Some(node),
                EntityId::Relationship(_) => panic!("byte threshold node domain"),
            };
        }
        let guard = byte_store.native_graph.writer.lock().expect("byte writer");
        let writer = guard.as_ref().expect("byte writer state");
        if old_count != 0 && writer.complete_envelopes == 1 {
            byte_revision = Some(revision);
            assert_eq!(writer.complete_envelopes, 1);
            expected_writer = Some((
                writer.complete_envelopes,
                writer.envelope_bytes,
                writer.protected.len(),
            ));
            break;
        }
    }
    let expected_writer = expected_writer.expect("encoded-byte checkpoint threshold");
    let byte_node = byte_node.expect("byte threshold node");
    let byte_revision = byte_revision.expect("actual byte-triggered fold");
    let byte_generation = byte_store
        .admit_native_read()
        .expect("byte threshold admission")
        .bundle()
        .base()
        .generation;
    assert_eq!(byte_generation.get(), byte_revision + 2);
    byte_store.close().expect("close byte-threshold store");
    drop(byte_store);
    let byte_store = Store::open_native_graph(&byte_path, native_options(), None)
        .expect("reopen byte-threshold store");
    assert_eq!(
        observe_node(&byte_store, byte_node),
        Some((byte_revision + 2, byte_revision, byte_revision))
    );
    {
        let guard = byte_store
            .native_graph
            .writer
            .lock()
            .expect("recovered byte writer");
        let writer = guard.as_ref().expect("recovered byte writer state");
        assert_eq!(
            (
                writer.complete_envelopes,
                writer.envelope_bytes,
                writer.protected.len()
            ),
            expected_writer
        );
    }
    byte_store
        .close()
        .expect("close recovered byte-threshold store");

    let historical_path = parent.path().join("rotated-outer-wal");
    let historical = Store::create_native_graph_with_infrastructure(
        &historical_path,
        native_options(),
        None,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], Some("history"), None).unwrap();
    let key = ApplicationKey::new(EntityKind::Node, "app", "historical").unwrap();
    let receipt = historical
        .apply_native_graph(
            &[StructuredWrite {
                key,
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let EntityId::Node(node) = receipt[0].entity else {
        panic!("node domain")
    };
    let manifest_before = std::fs::read(historical_path.join("manifest.ze")).unwrap();
    vfs.arm_fault(FaultPoint::ManifestSync);
    let error = historical
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect_err("manifest sync refusal must preserve the acknowledged graph");
    assert!(
        matches!(
            error,
            super::super::NativeGraphError::Store(crate::lifecycle::StoreError::Manifest(_))
        ),
        "{error:?}"
    );
    vfs.assert_fired_once();
    assert_eq!(
        std::fs::read(historical_path.join("manifest.ze")).unwrap(),
        manifest_before
    );
    assert_eq!(observe_node(&historical, node), Some((2, 1, 1)));
    assert_shared_writer_stopped(&historical);
    drop(historical);
    let historical = Store::open_native_graph(&historical_path, native_options(), None).unwrap();
    assert_eq!(observe_node(&historical, node), Some((2, 1, 1)));
    historical
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let selected = checkpoint_from_selected(&historical_path);
    assert_eq!(selected.state.sequence, 1);
    historical.close().unwrap();
    let historical = Store::open_native_graph(&historical_path, native_options(), None).unwrap();
    isolate_recovery_from_foreground_reclaim(&historical);
    assert_eq!(observe_node(&historical, node), Some((2, 1, 1)));
    let receipt = historical
        .apply_native_graph(
            &[StructuredWrite {
                key,
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Put(EntityId::Node(node)),
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert_eq!(receipt[0].generation.get(), 4);
    let wal = crate::wal::WalReader::open(&StdVfs, &historical_path.join("wal.ze")).unwrap();
    assert_eq!(wal.records().first().unwrap().seq.get(), 2);
    historical.close().unwrap();
    finish_recovery_path("property-graph.recovery.checkpoint")
}

#[cfg_attr(test, test)]
fn ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories() {
    let _ = run_ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories();
}

fn run_ze40_read_only_replay_preserves_tail_and_all_files() -> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("read-only");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let first_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("first image");
    let first_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "visible").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first_image)),
    }];
    let receipt = store
        .apply_native_graph(&first_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("durable first commit");
    let node = match receipt[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node domain"),
    };
    let torn_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("torn image");
    let torn_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "torn").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&torn_image)),
    }];
    vfs.arm_fault(FaultPoint::PartialAppend);
    assert!(
        store
            .apply_native_graph(&torn_request, &QueryControl::Cancel(CancelToken::new()))
            .is_err()
    );
    vfs.assert_fired_once();
    store.close().expect("close torn store");
    drop(store);
    let before = file_snapshot(&path);
    vfs.take();
    let read_options = OpenOptions::read_only()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024);
    let first = Store::open_native_graph_with_infrastructure(
        &path,
        read_options.clone(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("first shared read-only recovery");
    let second = Store::open_native_graph_with_infrastructure(
        &path,
        read_options,
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("second shared read-only recovery");
    assert_eq!(observe_node(&first, node), Some((2, 1, 1)));
    assert_eq!(observe_node(&second, node), Some((2, 1, 1)));
    assert!(matches!(
        first.apply_native_graph(&[], &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::Store(
            crate::lifecycle::StoreError::ReadOnly
        ))
    ));
    assert!(matches!(
        first.checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::Store(
            crate::lifecycle::StoreError::ReadOnly
        ))
    ));
    assert_eq!(file_snapshot(&path), before);
    first.close().expect("close first read-only store");
    second.close().expect("close second read-only store");
    drop(first);
    drop(second);
    assert_eq!(file_snapshot(&path), before);
    assert!(
        vfs.take().is_empty(),
        "read-only recovery must issue no VFS mutation call"
    );

    let writable = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("writable rotation after read-only close");
    isolate_recovery_from_foreground_reclaim(&writable);
    assert_eq!(observe_node(&writable, node), Some((2, 1, 1)));
    assert_eq!(
        writable
            .apply_native_graph(&torn_request, &QueryControl::Cancel(CancelToken::new()))
            .expect("post-rotation write")[0]
            .generation
            .get(),
        3
    );
    writable.close().expect("close writable store");
    finish_recovery_path("property-graph.recovery.read-only")
}

#[cfg_attr(test, test)]
fn ze40_read_only_replay_preserves_tail_and_all_files() {
    let _ = run_ze40_read_only_replay_preserves_tail_and_all_files();
}

fn rewrite_artifact_identity(bytes: &mut [u8], artifact: u128, serial: u64) {
    bytes[48..64].copy_from_slice(&artifact.to_le_bytes());
    bytes[88..96].copy_from_slice(&serial.to_le_bytes());
    let trailer = bytes.len() - 8;
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[..trailer]);
    bytes[trailer..].copy_from_slice(&checksum.to_le_bytes());
}

fn run_ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("serials");
    let store =
        Store::create_native_graph(&path, native_options(), None).expect("fresh native store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "serial").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    store
        .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
        .expect("durable commit");
    store.close().expect("close native store");
    drop(store);
    let source = std::fs::read_dir(&path)
        .expect("native directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("graph-") && name.ends_with(".zgraph"))
                && std::fs::read(candidate).is_ok_and(|bytes| {
                    bytes
                        .get(8..10)
                        .is_some_and(|family| family == 17_u16.to_le_bytes())
                })
        })
        .expect("native object artifact");
    let original = std::fs::read(&source).expect("source artifact");
    let high_artifact = (1_u128 << 124) + 0x40;
    let mut high = original.clone();
    rewrite_artifact_identity(&mut high, high_artifact, 10_000);
    let high_path = path.join(format!("graph-{high_artifact:032x}.zgraph"));
    std::fs::write(&high_path, high).expect("complete higher-serial orphan");
    let unknown = path.join("unknown.keep");
    std::fs::write(&unknown, b"keep").expect("unknown file");

    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("recover with complete orphan");
    assert_eq!(
        reopened.native_graph.serial_fence().expect("serial fence"),
        10_000
    );
    assert_eq!(vfs.enumeration_calls().0, 1);
    assert!(vfs.enumeration_calls().1 >= 1);
    // Inject the interrupted object creation, after the new mandatory fold.
    reopened
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .unwrap();
    reopened
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let before_partial = file_snapshot(&path);
    vfs.arm_fault(FaultPoint::PartialCreate);
    assert!(
        reopened
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "partial-create")
                        .expect("partial key"),
                    ..request[0]
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .is_err()
    );
    vfs.assert_fired_once();
    reopened.close().expect("close orphan store");
    drop(reopened);
    assert_eq!(std::fs::read(&unknown).expect("unknown retained"), b"keep");
    assert!(high_path.exists());
    let after_partial = file_snapshot(&path);
    let (partial_path, partial_bytes) = after_partial
        .iter()
        .find(|(candidate, _)| !before_partial.contains_key(*candidate))
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .expect("real partial create artifact");
    assert!(partial_bytes.len() >= crate::property_graph::storage::artifact::HEADER_BYTES);
    let partial_serial = u64::from_le_bytes(
        partial_bytes
            .get(88..96)
            .and_then(|bytes| bytes.first_chunk::<8>())
            .copied()
            .expect("partial creation serial"),
    );
    assert!(partial_serial > 10_000);

    let ambiguous_artifact = (1_u128 << 124) + 0x41;
    let ambiguous_path = path.join(format!("graph-{ambiguous_artifact:032x}.zgraph"));
    std::fs::write(&ambiguous_path, &original).expect("ambiguous complete object");
    assert!(
        Store::open_native_graph_with_infrastructure(
            &path,
            native_options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        )
        .is_err()
    );
    std::fs::remove_file(&ambiguous_path).expect("remove test corruption");

    let duplicate_artifact = (1_u128 << 124) + 0x42;
    let original_serial = u64::from_le_bytes(
        original
            .get(88..96)
            .and_then(|bytes| bytes.first_chunk::<8>())
            .copied()
            .expect("original creation serial"),
    );
    let mut duplicate = original.clone();
    rewrite_artifact_identity(&mut duplicate, duplicate_artifact, original_serial);
    let duplicate_path = path.join(format!("graph-{duplicate_artifact:032x}.zgraph"));
    std::fs::write(&duplicate_path, duplicate).expect("duplicate serial object");
    assert!(
        Store::open_native_graph_with_infrastructure(
            &path,
            native_options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        )
        .is_err()
    );
    std::fs::remove_file(&duplicate_path).expect("remove duplicate serial object");

    let overflow_artifact = (1_u128 << 124) + 0x43;
    let mut overflow = original;
    rewrite_artifact_identity(&mut overflow, overflow_artifact, u64::MAX);
    overflow.truncate(overflow.len() / 2);
    let overflow_path = path.join(format!("graph-{overflow_artifact:032x}.zgraph"));
    std::fs::write(&overflow_path, overflow).expect("overflow partial object");
    assert!(matches!(
        Store::open_native_graph_with_infrastructure(
            &path,
            native_options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
        Err(super::super::NativeGraphError::Store(crate::lifecycle::StoreError::Manifest(
            crate::manifest::ManifestError::Decode(message)
        ))) if message == "native graph read identity exhausted"
    ));
    std::fs::remove_file(&overflow_path).expect("remove overflow object");

    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("classify partial pre-WAL object");
    isolate_recovery_from_foreground_reclaim(&reopened);
    assert_eq!(
        reopened.native_graph.serial_fence().expect("serial fence"),
        partial_serial
    );
    reopened
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "after-orphan")
                    .expect("next key"),
                ..request[0]
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("write above partial serial");
    assert!(reopened.native_graph.serial_fence().expect("serial fence") > partial_serial);
    reopened.close().expect("close serial store");
    assert!(partial_path.exists());
    assert!(high_path.exists());
    assert!(unknown.exists());
    finish_recovery_path("property-graph.recovery.serial-orphan")
}

#[cfg_attr(test, test)]
fn ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption() {
    let _ = run_ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption();
}

#[cfg(feature = "test-seams")]
pub(super) fn run_actual_probe(
    _seed: u64,
) -> crate::graph_recovery_test_support::RecoveryProbeReport {
    use crate::graph_read_view_test_support::ObservedRelationship;
    use crate::graph_recovery_test_support::{RecoveryProbeReport, RecoveryState};

    let (mixed, complete) = run_ze40_complete_mixed_commit_close_reopen_is_coherent();
    let observed = [
        complete,
        run_ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation(),
        run_ze40_lost_ack_and_stopped_writer_resolve_on_reopen(),
        run_ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append(),
        run_ze40_committed_artifact_and_framing_damage_fail_without_partial_admission(),
        run_ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials(),
        run_ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption(),
        run_ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories(),
        run_ze40_read_only_replay_preserves_tail_and_all_files(),
    ];
    let receipts = observed
        .into_iter()
        .map(|receipt| crate::graph_read_view_test_support::PathReceipt {
            key: receipt.key,
            fires: receipt.fires,
            clean_controls: receipt.clean_controls,
        })
        .collect();
    let relationship = ObservedRelationship {
        rel: mixed.relationship.rel.get(),
        source: mixed.relationship.source.get(),
        target: mixed.relationship.target.get(),
        relationship_type: mixed.relationship.relationship_type.get(),
    };
    RecoveryProbeReport {
        receipts,
        state: RecoveryState {
            store: mixed.store.get(),
            generation: mixed.generation.get(),
            sequence: mixed.sequence,
            first_node: relationship.source,
            second_node: relationship.target,
            relationship,
        },
    }
}

// Recovery fixtures pin replay byte identity and exact WAL/checkpoint cuts.
// Foreground automatic reclamation is independently covered by ZE-316.
fn isolate_recovery_from_foreground_reclaim(store: &Store) {
    store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .expect("isolate recovery fixture from foreground reclaim");
}

#[test]
fn replays_document_and_graph_records_from_their_own_watermarks() {
    use crate::ingest::wal_payload::{GRAPH_COMMIT_V1, encode_graph_commit};
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    use crate::property_graph::wal::{
        BatchId, CommitState, Envelope, EnvelopeKind, STACK_RESERVATION_BYTES, WalResources,
        encode_envelope,
    };
    use crate::wal::{LogSeq, WalWriter};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let ack = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(91), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .unwrap();
    assert_eq!(ack.generation(), 1);
    assert_eq!(store.enable_graph().unwrap(), 2);
    store.close().unwrap();
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 1)
            .unwrap();
    assert_eq!(manifest.log_seq, 0);
    assert_eq!(manifest.graph.as_ref().unwrap().graph_absorbed_through, 1);
    let base = manifest.graph.as_ref().unwrap().state().unwrap();
    let target = CommitState {
        generation: GraphGeneration::new(3),
        sequence: 1,
        ..base
    };
    let mut cancelled = || false;
    let mut resources =
        WalResources::new(u64::MAX, STACK_RESERVATION_BYTES, &mut cancelled).unwrap();
    let mut bytes = vec![0; 16 * 1024];
    let length = encode_envelope(
        base,
        Envelope {
            batch: BatchId::new(91).unwrap(),
            kind: EnvelopeKind::Mutation,
            state: target,
            changes: &[],
        },
        &mut bytes,
        &mut resources,
    )
    .unwrap();
    let payload = encode_graph_commit(&bytes[..length]).unwrap();
    let writer = WalWriter::resume(
        &StdVfs,
        &directory.path().join("wal.ze"),
        crate::wal::WalReader::open(&StdVfs, &directory.path().join("wal.ze"))
            .unwrap()
            .into_clean()
            .unwrap(),
        crate::lifecycle::durability::DurabilityPolicy::new(
            DurabilityMode::Durable,
            CommitTier::Durable,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        writer.commit(GRAPH_COMMIT_V1, &payload).unwrap(),
        LogSeq::new(2)
    );
    drop(writer);
    let recovered = Store::open(
        directory.path(),
        OpenOptions::read_only().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    assert_eq!(recovered.count_documents(None, None).unwrap().count, 1);
    assert_eq!(recovered.snapshot().unwrap().generation(), 3);
    let read = recovered.admit_native_read().unwrap();
    assert_eq!(read.bundle().sequence(), 1);
    assert_eq!(read.bundle().base().generation, GraphGeneration::new(3));
    drop(read);
    recovered.close().unwrap();
}

fn append_unified_empty_graph(directory: &Path, generation: u64) {
    use crate::ingest::wal_payload::{GRAPH_COMMIT_V1, encode_graph_commit};
    use crate::property_graph::wal::{
        BatchId, CommitState, Envelope, EnvelopeKind, STACK_RESERVATION_BYTES, WalResources,
        encode_envelope,
    };
    let clean = crate::wal::WalReader::open(&StdVfs, &directory.join("wal.ze"))
        .unwrap()
        .into_clean()
        .unwrap();
    let durable_end = clean.records().last().map_or(0, |record| record.seq.get());
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.join("manifest.ze"), durable_end)
            .unwrap();
    let base = manifest.graph.as_ref().unwrap().state().unwrap();
    let target = CommitState {
        generation: GraphGeneration::new(generation),
        sequence: base.sequence + 1,
        ..base
    };
    let mut cancelled = || false;
    let mut resources =
        WalResources::new(128 * 1024, STACK_RESERVATION_BYTES, &mut cancelled).unwrap();
    let mut envelope = vec![0; 16 * 1024];
    let length = encode_envelope(
        base,
        Envelope {
            batch: BatchId::new(81).unwrap(),
            kind: EnvelopeKind::Mutation,
            state: target,
            changes: &[],
        },
        &mut envelope,
        &mut resources,
    )
    .unwrap();
    let payload = encode_graph_commit(&envelope[..length]).unwrap();
    let writer = crate::wal::WalWriter::resume(
        &StdVfs,
        &directory.join("wal.ze"),
        clean,
        crate::lifecycle::durability::DurabilityPolicy::new(
            DurabilityMode::Durable,
            CommitTier::Durable,
        )
        .unwrap(),
    )
    .unwrap();
    writer.commit(GRAPH_COMMIT_V1, &payload).unwrap();
}

fn unified_replay_fixture(generation: u64) -> tempfile::TempDir {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(store.enable_graph().unwrap(), 1);
    assert_eq!(
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(91), Revision::new(1)),
                vec![1.0, 0.0],
            )]))
            .unwrap()
            .generation(),
        2
    );
    store.close().unwrap();
    append_unified_empty_graph(directory.path(), generation);
    directory
}

#[test]
fn a_graph_commit_whose_generation_disagrees_fails_loudly() {
    let directory = unified_replay_fixture(4);
    let before = file_snapshot(directory.path());
    let result = Store::open(
        directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
    );
    assert!(
        matches!(result, Err(crate::lifecycle::StoreError::WalMutation { seq, op: crate::ingest::wal_payload::GRAPH_COMMIT_V1, .. }) if seq == crate::wal::LogSeq::new(2)),
        "mismatched graph generation must refuse"
    );
    assert_eq!(file_snapshot(directory.path()), before);
}

#[test]
fn read_only_open_replays_without_writing() {
    let directory = unified_replay_fixture(3);
    let before = file_snapshot(directory.path());
    let vfs = Arc::new(RecordingVfs::default());
    let store = Store::open_with_test_dependencies(
        directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
        crate::lifecycle::StoreTestDependencies::new(
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
    )
    .unwrap();
    assert_eq!(store.snapshot().unwrap().generation(), 3);
    assert_eq!(store.count_documents(None, None).unwrap().count, 1);
    assert_eq!(store.admit_native_read().unwrap().bundle().sequence(), 1);
    store.close().unwrap();
    assert!(!vfs.take().iter().any(|event| matches!(
        event,
        DurabilityEvent::Create(_)
            | DurabilityEvent::Write(_)
            | DurabilityEvent::Append(_)
            | DurabilityEvent::OpenAppend(_)
            | DurabilityEvent::Sync(_, _)
            | DurabilityEvent::Rename(_, _)
            | DurabilityEvent::Delete(_)
    )));
    assert_eq!(file_snapshot(directory.path()), before);
}

fn commit_tail_test_node(store: &Store, key: &str) -> NodeId {
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "tail", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("expected node"),
    }
}

fn graph_torn_tail_fixture(needed: usize) -> (tempfile::TempDir, NodeId, Vec<u8>) {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(91), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .unwrap();
    let node = commit_tail_test_node(&store, "a");
    drop(store);
    let wal = directory.path().join("wal.ze");
    let prefix = std::fs::read(&wal).unwrap();
    let mut tail = Vec::new();
    tail.extend_from_slice(
        &u32::try_from(needed - crate::wal::record::MIN_RECORD_LEN)
            .unwrap()
            .to_le_bytes(),
    );
    tail.extend_from_slice(&3_u64.to_le_bytes());
    tail.extend_from_slice(&crate::ingest::wal_payload::GRAPH_COMMIT_V1.to_le_bytes());
    StdVfs.open_append(&wal).unwrap().append(&tail).unwrap();
    (directory, node, prefix)
}

#[test]
fn an_oversized_torn_tail_with_graph_is_refused_not_dropped() {
    let (directory, _, _) = graph_torn_tail_fixture(32 * 1_048_576);
    let before = file_snapshot(directory.path());
    for access in [
        crate::lifecycle::AccessMode::ReadWrite,
        crate::lifecycle::AccessMode::ReadOnly,
    ] {
        let result = Store::open(directory.path(), native_options().with_access_mode(access));
        assert!(
            matches!(result, Err(crate::lifecycle::StoreError::WalRecovery(
                crate::wal::WalRecoveryError::CorruptAt {
                    reason: crate::wal::replay::CorruptionReason::Record {
                        error: crate::wal::record::RecordError::BodyTruncated { needed, .. },
                        ..
                    },
                    ..
                }
            )) if needed > crate::wal::DEFAULT_MAX_GROUP_BYTES_DURABLE),
            "oversized corruption must refuse before admitting another writer"
        );
        assert_eq!(file_snapshot(directory.path()), before);
    }
}

#[test]
fn a_bounded_torn_tail_with_graph_is_cut_before_acknowledging_another_write() {
    for needed in [64, crate::wal::DEFAULT_MAX_GROUP_BYTES_DURABLE] {
        let (directory, first, prefix) = graph_torn_tail_fixture(needed);
        let before = file_snapshot(directory.path());
        let reader = Store::open(
            directory.path(),
            native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
        )
        .unwrap();
        assert_eq!(reader.count_documents(None, None).unwrap().count, 1);
        assert_eq!(observe_node(&reader, first), Some((3, 1, 1)));
        reader.close().unwrap();
        assert_eq!(file_snapshot(directory.path()), before);
        let writer = Store::open(directory.path(), native_options()).unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("wal.ze")).unwrap(),
            prefix
        );
        // Isolate tail repair and the next acknowledged append from the
        // separate automatic-maintenance cycle triggered by retained history.
        writer
            .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
                automatic: false,
                ..Default::default()
            })
            .unwrap();
        let second = commit_tail_test_node(&writer, "b");
        drop(writer);
        let recovered = Store::open(directory.path(), native_options()).unwrap();
        assert_eq!(recovered.count_documents(None, None).unwrap().count, 1);
        assert_eq!(observe_node(&recovered, first), Some((4, 2, 1)));
        assert_eq!(observe_node(&recovered, second), Some((4, 2, 1)));
        recovered.close().unwrap();
    }
}

#[test]
fn a_reader_that_observes_graph_enable_after_its_version_probe_refuses() {
    let directory = tempfile::tempdir().unwrap();
    let writer = Arc::new(Store::open(directory.path(), native_options()).unwrap());
    writer
        .ingest(crate::ingest::IngestBatch::new(vec![
            crate::ingest::IngestDocument::new(
                crate::ingest::DocumentVersion::new(
                    crate::ingest::DocId::new(91),
                    crate::ingest::Revision::new(1),
                ),
                vec![1.0, 0.0],
            ),
        ]))
        .unwrap();
    writer.seal().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    vfs.after_manifest_version(move || {
        writer.enable_graph().unwrap();
        writer.close().unwrap();
    });
    let result = Store::open_with_test_dependencies(
        directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
        crate::lifecycle::StoreTestDependencies::new(
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
    );
    assert!(
        matches!(&result, Err(crate::lifecycle::StoreError::StoreBusy { .. })),
        "raced open returned {:?}",
        result.as_ref().err()
    );
    assert!(!vfs.take().iter().any(|event| matches!(
        event,
        DurabilityEvent::Create(_)
            | DurabilityEvent::Write(_)
            | DurabilityEvent::Append(_)
            | DurabilityEvent::OpenAppend(_)
            | DurabilityEvent::Sync(_, _)
            | DurabilityEvent::Rename(_, _)
            | DurabilityEvent::Delete(_)
    )));
    let reader = Store::open(
        directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
    )
    .unwrap();
    assert!(matches!(
        Store::open(directory.path(), native_options()),
        Err(crate::lifecycle::StoreError::StoreBusy { .. })
    ));
    reader.close().unwrap();
}

fn generation_fixture(store: &Store, id: u128) {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    let mut batch = IngestBatch::new(vec![
        IngestDocument::new(
            DocumentVersion::new(DocId::new(id), Revision::new(1)),
            vec![1.0, 0.0],
        )
        .with_timestamp(id as i64)
        .with_text("generation fixture"),
    ]);
    if let Some(epoch) = store.epoch_identity() {
        batch = batch.with_epoch(epoch);
    }
    store.ingest(batch).unwrap();
    store.seal().unwrap();
}

fn generation_epoch() -> crate::epoch::StoreEpoch {
    let document = EmbeddingTower {
        model_id: "generation-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![1],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    crate::epoch::StoreEpoch {
        embedding: crate::epoch::EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: crate::fts::tokenizer::TokenizerConfig::text_default().epoch(),
    }
}

fn disable_generation_fixture_maintenance(store: &Store) {
    store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .unwrap();
}

fn assert_generation_reopens(
    path: &Path,
    node: NodeId,
    generation: u64,
    graph_generation: u64,
    count: u64,
) {
    assert_generation_reopens_with_options(
        path,
        native_options(),
        node,
        generation,
        graph_generation,
        count,
    );
}

fn assert_generation_reopens_with_options(
    path: &Path,
    options: OpenOptions,
    node: NodeId,
    generation: u64,
    graph_generation: u64,
    count: u64,
) {
    for access in [
        crate::lifecycle::AccessMode::ReadOnly,
        crate::lifecycle::AccessMode::ReadWrite,
        crate::lifecycle::AccessMode::ReadWrite,
    ] {
        let recovered = Store::open(path, options.clone().with_access_mode(access)).unwrap();
        assert_eq!(recovered.snapshot().unwrap().generation(), generation);
        assert_eq!(recovered.count_documents(None, None).unwrap().count, count);
        assert_eq!(
            observe_node(&recovered, node),
            Some((graph_generation, 1, 1))
        );
        drop(recovered);
    }
}

#[test]
fn a_delete_after_the_last_graph_write_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-delete");
    assert_eq!(
        store
            .delete(crate::ingest::DeleteBatch::new(vec![
                crate::ingest::DocId::new(91)
            ]))
            .unwrap()
            .generation(),
        5
    );
    drop(store);
    assert_generation_reopens(directory.path(), node, 5, 4, 0);
}

#[test]
fn additive_schema_evolution_on_a_graph_store_reopens() {
    use crate::meta::{ColumnDefinition, ColumnId, ColumnType, Schema};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-schema");
    drop(store);
    let schema = Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(71),
        "extra",
        ColumnType::U64,
        true,
    )])
    .unwrap();
    let evolved = Store::open(
        directory.path(),
        native_options().with_schema(schema.clone()),
    )
    .unwrap();
    assert_eq!(evolved.schema(), &schema);
    assert_eq!(evolved.snapshot().unwrap().generation(), 3);
    assert_eq!(observe_node(&evolved, node), Some((2, 1, 1)));
    drop(evolved);
    assert_generation_reopens(directory.path(), node, 3, 2, 0);
}

#[test]
fn a_manifest_only_publication_after_an_unfinished_batch_reopens() {
    publication_after_unfinished_batch("schema");
}

#[test]
fn reindex_after_an_unfinished_batch_reopens() {
    publication_after_unfinished_batch("reindex");
}

#[test]
fn retention_after_an_unfinished_batch_reopens() {
    publication_after_unfinished_batch("retention");
}

fn publication_after_unfinished_batch(publisher: &str) {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    use crate::meta::{ColumnDefinition, ColumnId, ColumnType, Schema};
    {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), native_options()).unwrap();
        generation_fixture(&store, 91);
        store.enable_graph().unwrap();
        disable_generation_fixture_maintenance(&store);
        let node = commit_tail_test_node(&store, "before-unfinished-batch");
        let wal = directory.path().join("wal.ze");
        let prefix = std::fs::read(&wal).unwrap();
        store
            .ingest(IngestBatch::new(
                [92, 93]
                    .into_iter()
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(1)),
                            vec![1.0, 0.0],
                        )
                    })
                    .collect(),
            ))
            .unwrap();
        let full = std::fs::read(&wal).unwrap();
        let replay = crate::wal::replay::replay(&full);
        let member = replay
            .records
            .iter()
            .find(|record| record.op == crate::ingest::wal_payload::UPSERT_V2_BATCH_MEMBER)
            .unwrap();
        let mut unfinished = prefix;
        crate::wal::record::append_record_into(
            crate::wal::record::WalRecord {
                seq: member.seq,
                op: member.op,
                payload: member.payload,
            },
            &mut unfinished,
        )
        .unwrap();
        drop(store);
        std::fs::write(&wal, unfinished).unwrap();
        let store = if publisher == "schema" {
            let schema = Schema::new(vec![ColumnDefinition::new(
                ColumnId::new(71),
                "extra",
                ColumnType::U64,
                true,
            )])
            .unwrap();
            Store::open(directory.path(), native_options().with_schema(schema)).unwrap()
        } else {
            let store = Store::open(directory.path(), native_options()).unwrap();
            if publisher == "reindex" {
                assert_eq!(store.reindex_text().unwrap(), 5);
            } else {
                assert_eq!(store.drop_partition(91..92).unwrap().generation(), 5);
            }
            store
        };
        assert_eq!(store.snapshot().unwrap().generation(), 5);
        assert_eq!(observe_node(&store, node), Some((4, 1, 1)));
        drop(store);
        let recovered = Store::open(directory.path(), native_options())
            .unwrap_or_else(|error| panic!("{publisher}: {error}"));
        assert_eq!(recovered.snapshot().unwrap().generation(), 5);
        assert_eq!(
            recovered.count_documents(None, None).unwrap().count,
            u64::from(publisher != "retention")
        );
        assert_eq!(observe_node(&recovered, node), Some((4, 1, 1)));
        disable_generation_fixture_maintenance(&recovered);
        let next = commit_tail_test_node(&recovered, "after-unfinished-batch");
        drop(recovered);
        for _ in 0..2 {
            let reopened = Store::open(directory.path(), native_options()).unwrap();
            assert_eq!(reopened.snapshot().unwrap().generation(), 6);
            assert_eq!(
                reopened.count_documents(None, None).unwrap().count,
                u64::from(publisher != "retention")
            );
            assert_eq!(observe_node(&reopened, node), Some((6, 2, 1)));
            assert_eq!(observe_node(&reopened, next), Some((6, 2, 1)));
            drop(reopened);
        }
    }
}

#[test]
fn reindex_after_the_last_graph_write_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-reindex");
    assert_eq!(store.reindex_text().unwrap(), 5);
    drop(store);
    assert_generation_reopens(directory.path(), node, 5, 4, 1);
}

#[test]
fn merge_after_the_last_graph_write_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    generation_fixture(&store, 91);
    generation_fixture(&store, 92);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-merge");
    assert_eq!(store.merge_sealed().unwrap(), 7);
    drop(store);
    assert_generation_reopens(directory.path(), node, 7, 6, 2);
}

#[test]
fn partition_drop_after_the_last_graph_write_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-partition-drop");
    assert_eq!(store.drop_partition(91..92).unwrap().generation(), 5);
    drop(store);
    assert_generation_reopens(directory.path(), node, 5, 4, 0);
}

#[test]
fn a_namespace_batch_preserves_prior_generation_boundaries() {
    use crate::lifecycle::{LiveNamespaceMutation, NamespaceMutation, namespace_batch_live};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("alpha");
    let other = Store::open(directory.path().join("beta"), native_options()).unwrap();
    let store = Store::open(&path, native_options()).unwrap();
    generation_fixture(&store, 91);
    generation_fixture(&store, 92);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-namespace");
    store
        .delete(crate::ingest::DeleteBatch::new(vec![
            crate::ingest::DocId::new(91),
        ]))
        .unwrap();
    assert_eq!(
        namespace_batch_live(
            directory.path(),
            vec![
                LiveNamespaceMutation {
                    store: &store,
                    mutation: NamespaceMutation {
                        name: "alpha".to_owned(),
                        options: native_options(),
                        upserts: vec![crate::ingest::IngestDocument::new(
                            crate::ingest::DocumentVersion::new(
                                crate::ingest::DocId::new(92),
                                crate::ingest::Revision::new(2)
                            ),
                            vec![1.0, 0.0]
                        )],
                        deletes: Vec::new(),
                        delete_where: None,
                    }
                },
                LiveNamespaceMutation {
                    store: &other,
                    mutation: NamespaceMutation {
                        name: "beta".to_owned(),
                        options: native_options(),
                        upserts: Vec::new(),
                        deletes: Vec::new(),
                        delete_where: None,
                    }
                }
            ]
        )
        .unwrap(),
        vec![8, 0]
    );
    drop(store);
    assert_generation_reopens(&path, node, 8, 6, 1);
}

#[test]
fn snapshot_replacement_after_the_last_graph_write_reopens() {
    use crate::lifecycle::{InMemorySegment, InMemorySegmentFactors};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-snapshot");
    let columns = crate::meta::ColumnStoreBuilder::new(store.schema().clone())
        .finish()
        .unwrap();
    let alive = crate::meta::AliveSet::new(0);
    let prepared = store
        .prepare_segment(InMemorySegment {
            id: crate::segment::SegmentId::new(6, [6; 10]),
            scheme: 4,
            dims: 2,
            codes: Vec::new(),
            factors: InMemorySegmentFactors::Bit4(Vec::new()),
            rescore: Vec::new(),
            columns: &columns,
            alive: &alive,
        })
        .unwrap();
    assert_eq!(store.seal_snapshot(prepared).unwrap(), 3);
    drop(store);
    assert_generation_reopens(directory.path(), node, 3, 2, 0);
}

#[test]
fn repeated_graph_checkpoints_without_watermark_movement_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-checkpoints");
    for _ in 0..2 {
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
    }
    let generation = store.snapshot().unwrap().generation();
    drop(store);
    assert_generation_reopens(directory.path(), node, generation, 2, 0);
}

#[test]
fn manifest_only_bumps_between_graph_writes_keep_their_original_positions() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let first = commit_tail_test_node(&store, "before-first-bump");
    assert_eq!(store.reindex_text().unwrap(), 5);
    let second = commit_tail_test_node(&store, "between-bumps");
    assert_eq!(store.reindex_text().unwrap(), 7);
    assert_eq!(store.reindex_text().unwrap(), 8);
    drop(store);
    for access in [
        crate::lifecycle::AccessMode::ReadOnly,
        crate::lifecycle::AccessMode::ReadWrite,
    ] {
        let recovered =
            Store::open(directory.path(), native_options().with_access_mode(access)).unwrap();
        assert_eq!(recovered.snapshot().unwrap().generation(), 8);
        assert_eq!(observe_node(&recovered, first), Some((6, 2, 1)));
        assert_eq!(observe_node(&recovered, second), Some((6, 2, 1)));
        drop(recovered);
    }
    let recovered = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&recovered);
    let third = commit_tail_test_node(&recovered, "after-reopen");
    assert_eq!(observe_node(&recovered, third), Some((9, 3, 1)));
    drop(recovered);
    let recovered = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(observe_node(&recovered, third), Some((9, 3, 1)));
}

#[test]
fn failed_graph_admission_does_not_publish_additive_schema_evolution() {
    use crate::meta::{ColumnDefinition, ColumnId, ColumnType, Schema};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-failed-schema");
    drop(store);
    let manifest_path = directory.path().join("manifest.ze");
    let before = std::fs::read(&manifest_path).unwrap();
    let manifest = crate::manifest::decode_manifest("before", &before).unwrap();
    let artifact = crate::property_graph::storage::allocation::artifact_path(
        directory.path(),
        manifest.graph.unwrap().objects[0].artifact,
    );
    let bytes = std::fs::read(&artifact).unwrap();
    std::fs::remove_file(&artifact).unwrap();
    let schema = Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(71),
        "extra",
        ColumnType::U64,
        true,
    )])
    .unwrap();
    assert!(Store::open(directory.path(), native_options().with_schema(schema)).is_err());
    assert_eq!(std::fs::read(&manifest_path).unwrap(), before);
    std::fs::write(artifact, bytes).unwrap();
    assert_generation_reopens(directory.path(), node, 2, 2, 0);
}

#[test]
fn tier_promotion_after_the_last_graph_write_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        native_options().with_epoch(generation_epoch()),
    )
    .unwrap();
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-promotion");
    let report = store.maintain_with_test_thresholds(
        crate::tier::maintain::MaintenanceBudget {
            wall_time: std::time::Duration::from_secs(30),
            bytes: u64::MAX,
        },
        crate::tier::TierThresholds { graph_min_rows: 1 },
    );
    assert!(
        matches!(
            report.status,
            crate::tier::maintain::MaintenanceStatus::Complete
        ),
        "{report:?}"
    );
    assert_eq!(report.graphs_built, 1);
    assert_eq!(report.consolidations, 0);
    assert_eq!(report.passes_applied, 4);
    assert_eq!(report.refinement_generation, Some(9));
    drop(store);
    assert_generation_reopens_with_options(
        directory.path(),
        native_options().with_epoch(generation_epoch()),
        node,
        9,
        4,
        1,
    );
}

#[test]
fn tier_consolidation_after_the_last_graph_write_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        native_options().with_epoch(generation_epoch()),
    )
    .unwrap();
    for id in [91, 92, 93] {
        generation_fixture(&store, id);
    }
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-consolidation");
    let report = store.maintain_with_test_thresholds(
        crate::tier::maintain::MaintenanceBudget {
            wall_time: std::time::Duration::from_secs(30),
            bytes: u64::MAX,
        },
        crate::tier::TierThresholds { graph_min_rows: 1 },
    );
    assert!(
        matches!(
            report.status,
            crate::tier::maintain::MaintenanceStatus::Complete
        ),
        "{report:?}"
    );
    assert_eq!(report.graphs_built, 3);
    assert_eq!(report.consolidations, 1);
    assert_eq!(report.consolidation_generation, Some(16));
    drop(store);
    assert_generation_reopens_with_options(
        directory.path(),
        native_options().with_epoch(generation_epoch()),
        node,
        16,
        8,
        3,
    );
}

#[test]
fn sealed_physical_purge_records_its_manifest_only_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-physical-purge");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let token = store.purge(&[crate::ingest::DocId::new(91)]).unwrap();
    assert_eq!(store.await_physical_purge(token).unwrap().generation(), 6);
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 2)
            .unwrap();
    assert_eq!(manifest.graph.unwrap().generation_bumps, vec![(2, 2)]);
    drop(store);
    assert_generation_reopens(directory.path(), node, 6, 4, 0);
}

#[test]
fn graph_enable_records_its_manifest_only_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(store.enable_graph().unwrap(), 1);
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 0)
            .unwrap();
    assert_eq!(manifest.graph.unwrap().generation_bumps, vec![(0, 1)]);
    drop(store);
    let reopened = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(reopened.snapshot().unwrap().generation(), 1);
}

#[test]
fn active_physical_purge_preserves_generation_when_rewriting_the_wal() {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    for retain_document in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), native_options()).unwrap();
        store.enable_graph().unwrap();
        disable_generation_fixture_maintenance(&store);
        let first = commit_tail_test_node(&store, "before-active-purge");
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        let mut documents = vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(91), Revision::new(1)),
            vec![1.0, 0.0],
        )];
        if retain_document {
            documents.push(IngestDocument::new(
                DocumentVersion::new(DocId::new(92), Revision::new(1)),
                vec![0.0, 1.0],
            ));
        }
        assert_eq!(
            store
                .ingest(IngestBatch::new(documents))
                .unwrap()
                .generation(),
            4
        );
        let token = store.purge(&[DocId::new(91)]).unwrap();
        assert_eq!(store.await_physical_purge(token).unwrap().generation(), 5);
        drop(store);
        let store = Store::open(directory.path(), native_options()).unwrap();
        assert_eq!(store.snapshot().unwrap().generation(), 5);
        assert_eq!(
            store.count_documents(None, None).unwrap().count,
            u64::from(retain_document)
        );
        assert_eq!(observe_node(&store, first), Some((2, 1, 1)));
        disable_generation_fixture_maintenance(&store);
        let second = commit_tail_test_node(&store, "after-active-purge");
        assert_eq!(observe_node(&store, second), Some((6, 2, 1)));
        drop(store);
        for _ in 0..2 {
            let recovered = Store::open(directory.path(), native_options()).unwrap();
            assert_eq!(recovered.snapshot().unwrap().generation(), 6);
            assert_eq!(
                recovered.count_documents(None, None).unwrap().count,
                u64::from(retain_document)
            );
            assert_eq!(observe_node(&recovered, first), Some((6, 2, 1)));
            assert_eq!(observe_node(&recovered, second), Some((6, 2, 1)));
            drop(recovered);
        }
    }
}

#[test]
fn a_physical_purge_of_a_multi_member_batch_reopens() {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    for members in [7, 2, 3] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), native_options()).unwrap();
        assert_eq!(store.enable_graph().unwrap(), 1);
        disable_generation_fixture_maintenance(&store);
        let first = commit_tail_test_node(&store, "before-multi-purge");
        assert_eq!(observe_node(&store, first), Some((2, 1, 1)));
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        assert_eq!(store.snapshot().unwrap().generation(), 3);
        assert_eq!(
            store
                .ingest(IngestBatch::new(
                    (91..91 + members)
                        .map(|id| {
                            IngestDocument::new(
                                DocumentVersion::new(DocId::new(id), Revision::new(1)),
                                vec![1.0, 0.0],
                            )
                        })
                        .collect()
                ))
                .unwrap()
                .generation(),
            4
        );
        let token = store.purge(&[DocId::new(91)]).unwrap();
        assert_eq!(store.await_physical_purge(token).unwrap().generation(), 5);
        drop(store);
        for access in [
            crate::lifecycle::AccessMode::ReadOnly,
            crate::lifecycle::AccessMode::ReadWrite,
            crate::lifecycle::AccessMode::ReadOnly,
            crate::lifecycle::AccessMode::ReadWrite,
        ] {
            let reopened = Store::open(directory.path(), native_options().with_access_mode(access))
                .unwrap_or_else(|error| panic!("members={members}, access={access:?}: {error}"));
            assert_eq!(reopened.snapshot().unwrap().generation(), 5);
            assert_eq!(
                reopened.count_documents(None, None).unwrap().count,
                (members - 1) as u64
            );
            assert!(
                reopened
                    .get_documents(&[DocId::new(91)], crate::lifecycle::DocumentFields::NONE)
                    .unwrap()[0]
                    .is_none()
            );
            for id in 92..91 + members {
                assert!(
                    reopened
                        .get_documents(&[DocId::new(id)], crate::lifecycle::DocumentFields::NONE)
                        .unwrap()[0]
                        .is_some()
                );
            }
            assert_eq!(observe_node(&reopened, first), Some((2, 1, 1)));
            drop(reopened);
        }
        let reopened = Store::open(directory.path(), native_options()).unwrap();
        disable_generation_fixture_maintenance(&reopened);
        let second = commit_tail_test_node(&reopened, "after-multi-purge");
        assert_eq!(observe_node(&reopened, second), Some((6, 2, 1)));
        drop(reopened);
        let recovered = Store::open(directory.path(), native_options()).unwrap();
        assert_eq!(recovered.snapshot().unwrap().generation(), 6);
        assert_eq!(observe_node(&recovered, second), Some((6, 2, 1)));
        assert_eq!(
            recovered.count_documents(None, None).unwrap().count,
            (members - 1) as u64
        );
    }
}

#[test]
fn a_namespace_rollback_keeps_an_earlier_publication_fence() {
    use crate::ingest::{DeleteBatch, DocId, DocumentVersion, IngestDocument, Revision};
    use crate::lifecycle::{LiveNamespaceMutation, NamespaceMutation, namespace_batch_live};
    let root = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let first = Store::open_with_test_dependencies(
        root.path().join("a"),
        native_options(),
        crate::lifecycle::StoreTestDependencies::new(
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
    )
    .unwrap();
    let second = Store::open(root.path().join("b"), native_options()).unwrap();
    generation_fixture(&first, 91);
    generation_fixture(&second, 92);
    assert_eq!(first.enable_graph().unwrap(), 3);
    disable_generation_fixture_maintenance(&first);
    let node = commit_tail_test_node(&first, "before-namespace-rollback");
    assert_eq!(observe_node(&first, node), Some((4, 1, 1)));
    vfs.arm_fault_after(FaultPoint::Rename, 1);
    assert!(
        first
            .delete(DeleteBatch::new(vec![DocId::new(91)]))
            .is_err()
    );
    vfs.assert_fired_once();
    assert_eq!(first.snapshot().unwrap().generation(), 4);
    // No write probes between the earlier failure and the namespace append.
    let participants = [(&first, "a", 101), (&second, "b", 102)]
        .into_iter()
        .map(|(store, name, id)| LiveNamespaceMutation {
            store,
            mutation: NamespaceMutation {
                name: name.to_owned(),
                options: native_options(),
                upserts: vec![IngestDocument::new(
                    DocumentVersion::new(DocId::new(id), Revision::new(1)),
                    vec![1.0, 0.0],
                )],
                deletes: Vec::new(),
                delete_where: None,
            },
        })
        .collect();
    let result = namespace_batch_live(root.path(), participants);
    assert!(
        matches!(result, Err(crate::lifecycle::StoreError::WalWrite(_))),
        "{result:?}"
    );
    assert_shared_writer_stopped(&first);
    assert_eq!(first.snapshot().unwrap().generation(), 4);
    drop(first);
    drop(second);
    assert_clean_reopen_after_publication_failure(&root.path().join("a"), node, 0);
    let second = Store::open(root.path().join("b"), native_options()).unwrap();
    assert_eq!(second.count_documents(None, None).unwrap().count, 1);
    assert_eq!(second.snapshot().unwrap().generation(), 2);
}

#[test]
fn a_failed_delete_publication_poisons_later_graph_writes() {
    failed_document_publication_poisons_writes(false);
}

#[test]
fn a_failed_replacement_publication_poisons_later_graph_writes() {
    failed_document_publication_poisons_writes(true);
}

pub(super) fn assert_shared_writer_stopped(store: &Store) {
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    assert!(
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "tail", "must-not-ack").unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new())
            )
            .is_err(),
        "a stale generation must never acknowledge another graph write"
    );
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    assert!(
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(99), Revision::new(1)),
                vec![1.0, 0.0]
            )]))
            .is_err(),
        "the document writer must also stop"
    );
    assert!(
        store.reindex_text().is_err(),
        "manifest-only writers must also stop"
    );
}

fn assert_clean_reopen_after_publication_failure(path: &Path, first: NodeId, count: u64) {
    let recovered = Store::open(path, native_options()).unwrap();
    assert_eq!(recovered.snapshot().unwrap().generation(), 5);
    assert_eq!(recovered.count_documents(None, None).unwrap().count, count);
    assert_eq!(observe_node(&recovered, first), Some((4, 1, 1)));
    disable_generation_fixture_maintenance(&recovered);
    let second = commit_tail_test_node(&recovered, "after-publication-failure");
    assert_eq!(observe_node(&recovered, second), Some((6, 2, 1)));
    drop(recovered);
    for _ in 0..2 {
        let reopened = Store::open(path, native_options()).unwrap();
        assert_eq!(reopened.snapshot().unwrap().generation(), 6);
        assert_eq!(reopened.count_documents(None, None).unwrap().count, count);
        assert_eq!(observe_node(&reopened, first), Some((6, 2, 1)));
        assert_eq!(observe_node(&reopened, second), Some((6, 2, 1)));
        drop(reopened);
    }
}

fn failed_document_publication_poisons_writes(replacement: bool) {
    use crate::ingest::{
        DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
    };
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
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-publication-failure");
    // Let the replacement segment rename finish; fail the manifest rename.
    vfs.arm_fault_after(FaultPoint::Rename, 1);
    if replacement {
        assert!(
            store
                .ingest(IngestBatch::new(vec![IngestDocument::new(
                    DocumentVersion::new(DocId::new(91), Revision::new(2)),
                    vec![0.0, 1.0]
                )]))
                .is_err()
        );
    } else {
        assert!(
            store
                .delete(DeleteBatch::new(vec![DocId::new(91)]))
                .is_err()
        );
    }
    vfs.assert_fired_once();
    assert_eq!(store.snapshot().unwrap().generation(), 4);
    let reader = crate::wal::WalReader::open(&StdVfs, &directory.path().join("wal.ze")).unwrap();
    assert_eq!(
        reader.records().last().unwrap().seq.get(),
        3,
        "the rejected publication follows a complete durable document mutation"
    );
    assert_shared_writer_stopped(&store);
    drop(store);
    assert_clean_reopen_after_publication_failure(directory.path(), node, u64::from(replacement));
}

#[test]
fn a_failed_reindex_directory_sync_poisons_later_graph_writes() {
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
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-reindex-failure");
    // SelectorSync fires only on the directory sync immediately after manifest rename.
    vfs.arm_fault(FaultPoint::SelectorSync);
    assert!(store.reindex_text().is_err());
    vfs.assert_fired_once();
    assert_eq!(store.snapshot().unwrap().generation(), 4);
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 2)
            .unwrap();
    assert_eq!(
        manifest.generation, 5,
        "rename published the generation before directory sync failed"
    );
    assert_shared_writer_stopped(&store);
    drop(store);
    assert_clean_reopen_after_publication_failure(directory.path(), node, 1);
}

#[test]
fn failed_purge_cutoff_publication_fences_graph_writes_and_recovers() {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
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
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let first = commit_tail_test_node(&store, "before-purge-fault");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    store
        .ingest(IngestBatch::new(
            [91, 92]
                .into_iter()
                .map(|id| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(id), Revision::new(1)),
                        vec![1.0, 0.0],
                    )
                })
                .collect(),
        ))
        .unwrap();
    let token = store.purge(&[DocId::new(91)]).unwrap();
    // First rename commits the active purge generation; second replaces WAL;
    // the third publishes the rewritten batches' generation cutoff.
    vfs.arm_fault_after(FaultPoint::Rename, 2);
    assert!(store.await_physical_purge(token).is_err());
    vfs.assert_fired_once();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    assert!(
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "tail", "must-not-ack").unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new())
            )
            .is_err()
    );
    drop(store);
    let recovered = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(recovered.count_documents(None, None).unwrap().count, 1);
    assert_eq!(observe_node(&recovered, first), Some((2, 1, 1)));
    disable_generation_fixture_maintenance(&recovered);
    let second = commit_tail_test_node(&recovered, "after-purge-fault");
    let observation = observe_node(&recovered, second);
    drop(recovered);
    for _ in 0..2 {
        let reopened = Store::open(directory.path(), native_options()).unwrap();
        assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
        assert_eq!(observe_node(&reopened, second), observation);
        drop(reopened);
    }
}

#[test]
fn a_sealed_tombstone_recovery_bump_does_not_break_the_next_reopen() {
    use crate::ingest::{
        DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    let document = DocId::new(91);
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(document, Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .unwrap();
    store.seal().unwrap();
    store.enable_graph().unwrap();
    store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .unwrap();
    let node = commit_tail_test_node(&store, "before-recovered-delete");
    let manifest_path = directory.path().join("manifest.ze");
    let before_delete = std::fs::read(&manifest_path).unwrap();
    let sealed = file_snapshot(directory.path())
        .into_iter()
        .filter(|(path, _)| {
            path.extension()
                .is_some_and(|extension| extension == "zseg")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store
            .delete(DeleteBatch::new(vec![document]))
            .unwrap()
            .generation(),
        5
    );
    // Keep the acknowledged graph and delete WAL records, but lose the
    // delete's manifest publication. Dropping avoids a close checkpoint.
    drop(store);
    std::fs::write(&manifest_path, before_delete).unwrap();
    for (path, bytes) in sealed {
        std::fs::write(directory.path().join(path), bytes).unwrap();
    }
    let first = Store::open(directory.path(), native_options()).unwrap();
    let generation = first.snapshot().unwrap().generation();
    assert_eq!(first.count_documents(None, None).unwrap().count, 0);
    assert_eq!(observe_node(&first, node), Some((4, 1, 1)));
    drop(first);
    for reopen in [2, 3] {
        let recovered = Store::open(directory.path(), native_options())
            .unwrap_or_else(|error| panic!("reopen {reopen}: {error}"));
        assert_eq!(recovered.snapshot().unwrap().generation(), generation);
        assert_eq!(recovered.count_documents(None, None).unwrap().count, 0);
        assert_eq!(observe_node(&recovered, node), Some((4, 1, 1)));
        drop(recovered);
    }
    assert_eq!(
        generation, 5,
        "recovery must retain the acknowledged generation"
    );
}

#[test]
fn a_manifest_generation_bump_without_watermark_move_does_not_break_graph_replay() {
    use crate::ingest::{
        DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    let document = DocId::new(91);
    assert_eq!(
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(document, Revision::new(1)),
                vec![1.0, 0.0],
            )]))
            .unwrap()
            .generation(),
        1
    );
    assert_eq!(store.seal().unwrap(), 2);
    assert_eq!(store.enable_graph().unwrap(), 3);
    assert_eq!(
        store
            .delete(DeleteBatch::new(vec![document]))
            .unwrap()
            .generation(),
        4
    );
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 2)
            .unwrap();
    assert_eq!(manifest.generation, 4);
    assert_eq!(manifest.log_seq, 1);
    assert_eq!(manifest.graph.as_ref().unwrap().graph_absorbed_through, 1);
    let node = commit_tail_test_node(&store, "after-delete");
    assert_eq!(observe_node(&store, node), Some((5, 1, 1)));
    // Drop the handle without the explicit close/checkpoint path: the synced
    // delete and graph records must recover against the generation-4 manifest.
    drop(store);
    let before = file_snapshot(directory.path());
    for access in [
        crate::lifecycle::AccessMode::ReadOnly,
        crate::lifecycle::AccessMode::ReadWrite,
    ] {
        let reopened = Store::open(directory.path(), native_options().with_access_mode(access))
            .expect("manifest-covered delete must not burn its generation twice");
        assert_eq!(reopened.snapshot().unwrap().generation(), 5);
        assert_eq!(reopened.count_documents(None, None).unwrap().count, 0);
        assert_eq!(observe_node(&reopened, node), Some((5, 1, 1)));
        drop(reopened);
        if access == crate::lifecycle::AccessMode::ReadOnly {
            assert_eq!(file_snapshot(directory.path()), before);
        }
    }
}

#[test]
fn a_manifest_generation_boundary_counts_batches_on_both_sides_exactly_once() {
    use crate::ingest::{
        DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
    };
    for replace_sealed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), native_options()).unwrap();
        let document = |id, revision| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(id), Revision::new(revision)),
                vec![1.0, 0.0],
            )
        };
        store
            .ingest(IngestBatch::new(vec![document(91, 1), document(92, 1)]))
            .unwrap();
        store.seal().unwrap();
        store.enable_graph().unwrap();
        let (first, second, expected_generation, expected_count) = if replace_sealed {
            // One multi-record batch replaces sealed rows and publishes its
            // generation, while the active documents still need WAL replay.
            assert_eq!(
                store
                    .ingest(IngestBatch::new(vec![document(91, 2), document(92, 2)]))
                    .unwrap()
                    .generation(),
                4
            );
            (commit_tail_test_node(&store, "after-upserts"), None, 5, 2)
        } else {
            // The manifest counts the earlier active document and graph batch
            // too. A later unknown-id delete still consumes its own generation.
            assert_eq!(
                store
                    .ingest(IngestBatch::new(vec![document(93, 1)]))
                    .unwrap()
                    .generation(),
                4
            );
            let first = commit_tail_test_node(&store, "before-delete");
            assert_eq!(
                store
                    .delete(DeleteBatch::new(vec![DocId::new(91)]))
                    .unwrap()
                    .generation(),
                6
            );
            assert_eq!(
                store
                    .delete(DeleteBatch::new(vec![DocId::new(999)]))
                    .unwrap()
                    .generation(),
                7
            );
            (
                first,
                Some(commit_tail_test_node(&store, "after-delete")),
                8,
                2,
            )
        };
        drop(store);
        let before = file_snapshot(directory.path());
        for access in [
            crate::lifecycle::AccessMode::ReadOnly,
            crate::lifecycle::AccessMode::ReadWrite,
        ] {
            let recovered =
                Store::open(directory.path(), native_options().with_access_mode(access)).unwrap();
            assert_eq!(
                recovered.snapshot().unwrap().generation(),
                expected_generation
            );
            assert_eq!(
                recovered.count_documents(None, None).unwrap().count,
                expected_count
            );
            assert_eq!(
                observe_node(&recovered, first),
                Some((expected_generation, if second.is_some() { 2 } else { 1 }, 1))
            );
            if let Some(second) = second {
                assert_eq!(
                    observe_node(&recovered, second),
                    Some((expected_generation, 2, 1))
                );
            }
            drop(recovered);
            if access == crate::lifecycle::AccessMode::ReadOnly {
                assert_eq!(file_snapshot(directory.path()), before);
            }
        }
    }
}

fn mixed_replay_fixture(graph_first: bool) -> (tempfile::TempDir, Vec<Vec<u8>>) {
    use crate::ingest::wal_payload::{
        GRAPH_COMMIT_V1, MIXED_BATCH_MEMBER_V1, UPSERT_V2, encode_mixed_batch_member,
        encode_upsert_v2,
    };
    use crate::ingest::{DocId, DocumentVersion, IngestDocument, Revision};
    use crate::property_graph::wal::{
        BatchId, CommitState, Envelope, EnvelopeKind, STACK_RESERVATION_BYTES, WalResources,
        encode_envelope,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    store.close().unwrap();
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 0)
            .unwrap();
    let base = manifest.graph.as_ref().unwrap().state().unwrap();
    let mut cancelled = || false;
    let mut resources =
        WalResources::new(128 * 1024, STACK_RESERVATION_BYTES, &mut cancelled).unwrap();
    let mut envelope = vec![0; 16 * 1024];
    let length = encode_envelope(
        base,
        Envelope {
            batch: BatchId::new(82).unwrap(),
            kind: EnvelopeKind::Mutation,
            state: CommitState {
                generation: GraphGeneration::new(2),
                sequence: 1,
                ..base
            },
            changes: &[],
        },
        &mut envelope,
        &mut resources,
    )
    .unwrap();
    let documents: Vec<_> = [101, 102]
        .into_iter()
        .map(|id| {
            let document = IngestDocument::new(
                DocumentVersion::new(DocId::new(id), Revision::new(1)),
                vec![1.0, 0.0],
            );
            (UPSERT_V2, encode_upsert_v2(&document).unwrap())
        })
        .collect();
    let mut members = documents;
    members.insert(
        if graph_first { 0 } else { 2 },
        (GRAPH_COMMIT_V1, envelope[..length].to_vec()),
    );
    let payloads: Vec<_> = members
        .iter()
        .enumerate()
        .map(|(index, (op, payload))| {
            encode_mixed_batch_member(index as u32, 3, *op, payload).unwrap()
        })
        .collect();
    let writer = crate::wal::WalWriter::create(
        &StdVfs,
        &directory.path().join("wal.ze"),
        crate::wal::LogSeq::new(1),
        crate::lifecycle::durability::DurabilityPolicy::new(
            DurabilityMode::Durable,
            CommitTier::Durable,
        )
        .unwrap(),
    )
    .unwrap();
    let records: Vec<_> = payloads
        .iter()
        .map(|payload| (MIXED_BATCH_MEMBER_V1, payload.as_slice()))
        .collect();
    writer.commit_many(&records).unwrap();
    drop(writer);
    (directory, payloads)
}

#[test]
fn a_watermark_inside_a_mixed_run_is_refused() {
    let (directory, _) = mixed_replay_fixture(false);
    let path = directory.path().join("manifest.ze");
    let mut manifest = crate::manifest::io::load_manifest(&StdVfs, &path, 3).unwrap();
    manifest.log_seq = 1;
    crate::manifest::io::commit_manifest(
        &StdVfs,
        directory.path(),
        &manifest,
        crate::lifecycle::durability::DurabilityPolicy::new(
            DurabilityMode::Durable,
            CommitTier::Durable,
        )
        .unwrap(),
    )
    .unwrap();
    let before = file_snapshot(directory.path());
    assert!(
        matches!(Store::open(directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly)),
        Err(crate::lifecycle::StoreError::Manifest(
            crate::manifest::ManifestError::Decode(message))) if message == "watermark splits a committed batch"),
        "a watermark inside a committed run cannot hide a document member"
    );
    assert_eq!(file_snapshot(directory.path()), before);
}

#[test]
fn a_graph_member_before_the_end_of_a_mixed_batch_is_refused() {
    let (directory, _) = mixed_replay_fixture(true);
    let before = file_snapshot(directory.path());
    let result = Store::open(
        directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
    );
    assert!(
        matches!(result, Err(crate::lifecycle::StoreError::WalMutation { seq, .. }) if seq == crate::wal::LogSeq::new(1))
    );
    assert_eq!(file_snapshot(directory.path()), before);
}

#[test]
fn a_mixed_batch_is_one_generation_and_a_torn_run_is_not_admitted() {
    let (directory, payloads) = mixed_replay_fixture(false);
    let complete = Store::open(
        directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
    )
    .unwrap();
    assert_eq!(complete.snapshot().unwrap().generation(), 2);
    assert_eq!(complete.count_documents(None, None).unwrap().count, 2);
    assert_eq!(
        complete
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation
            .get(),
        2
    );
    complete.close().unwrap();
    let bytes = std::fs::read(directory.path().join("wal.ze")).unwrap();
    let last = crate::wal::record::MIN_RECORD_LEN + payloads.last().unwrap().len();
    std::fs::write(
        directory.path().join("wal.ze"),
        &bytes[..bytes.len() - last],
    )
    .unwrap();
    let torn = Store::open(
        directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
    )
    .unwrap();
    assert_eq!(torn.snapshot().unwrap().generation(), 1);
    assert_eq!(torn.count_documents(None, None).unwrap().count, 0);
    assert_eq!(torn.admit_native_read().unwrap().bundle().sequence(), 0);
    torn.close().unwrap();
}

#[cfg(test)]
mod legacy {
    use super::*;

    #[test]
    fn family_19_fixture_is_an_explicit_rejected_input() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/graph-reclaim/pre-ze380-pending");
        let scratch = tempfile::tempdir().unwrap();
        for entry in std::fs::read_dir(&fixture).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), scratch.path().join(entry.file_name())).unwrap();
        }
        assert!(std::fs::read_dir(scratch.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("graph-wal-")
        }));
        let before = file_snapshot(scratch.path());
        for access in [
            crate::lifecycle::AccessMode::ReadOnly,
            crate::lifecycle::AccessMode::ReadWrite,
        ] {
            let vfs = Arc::new(RecordingVfs::default());
            let result = Store::open_with_test_dependencies(
                scratch.path(),
                native_options().with_access_mode(access),
                crate::lifecycle::StoreTestDependencies::new(
                    vfs.clone(),
                    Arc::new(crate::lifecycle::SystemMonotonicClock),
                ),
            );
            assert!(matches!(
                result,
                Err(crate::lifecycle::StoreError::NativeGraphDirectory { .. })
            ));
            assert_eq!(file_snapshot(scratch.path()), before);
            assert!(vfs.take().is_empty());
        }
    }
}

#[test]
fn a_v2_manifest_refuses_graph_records_even_below_its_watermark() {
    let directory = unified_replay_fixture(3);
    let mut manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 2)
            .unwrap();
    manifest.graph = None;
    manifest.log_seq = 2;
    manifest.generation = 3;
    crate::manifest::io::commit_manifest(
        &StdVfs,
        directory.path(),
        &manifest,
        crate::lifecycle::durability::DurabilityPolicy::new(
            DurabilityMode::Durable,
            CommitTier::Durable,
        )
        .unwrap(),
    )
    .unwrap();
    let before = file_snapshot(directory.path());
    let result = Store::open(
        directory.path(),
        native_options().with_access_mode(crate::lifecycle::AccessMode::ReadOnly),
    );
    assert!(
        matches!(result, Err(crate::lifecycle::StoreError::UnsupportedWalMutation { seq, op: crate::ingest::wal_payload::GRAPH_COMMIT_V1 }) if seq == crate::wal::LogSeq::new(2))
    );
    assert_eq!(file_snapshot(directory.path()), before);
}

#[test]
fn graph_text_and_vector_roots_survive_a_manifest_fold_and_wal_replay() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let document = EmbeddingTower {
        model_id: "ze346-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x34, 0x60],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let options = native_options();
    let store = Store::create_native_graph(&path, options.clone(), Some(document.clone())).unwrap();
    let coordinates = [0.25, 0.75];
    let embedding = CanonicalEmbedding::new(&document, &coordinates).unwrap();
    let contents = CanonicalContents::node(
        &mut [],
        &mut [],
        Some("unified persisted text"),
        Some(embedding),
    )
    .unwrap();
    for key in ["folded", "tail"] {
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ze346", key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&contents)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap();
        if key == "folded" {
            store
                .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                .unwrap();
            let manifest =
                crate::manifest::io::load_manifest(&StdVfs, &path.join("manifest.ze"), 1).unwrap();
            let state = manifest.graph.as_ref().unwrap().state().unwrap();
            assert!(state.text.is_some() && state.vector.is_some());
        }
    }
    let before = store.admit_native_read().unwrap();
    let state = super::super::write::commit_state(before.bundle());
    let expected = (state.generation, state.sequence, state.text, state.vector);
    drop(before);
    store.close().unwrap();
    for read_only in [true, false] {
        let access = if read_only {
            crate::lifecycle::AccessMode::ReadOnly
        } else {
            crate::lifecycle::AccessMode::ReadWrite
        };
        let reopened = Store::open_native_graph(
            &path,
            options.clone().with_access_mode(access),
            Some(document.clone()),
        )
        .unwrap();
        let reader = reopened.admit_native_read().unwrap();
        let state = super::super::write::commit_state(reader.bundle());
        assert_eq!(
            (state.generation, state.sequence, state.text, state.vector),
            expected
        );
        drop(reader);
        reopened.close().unwrap();
    }
}

#[test]
fn a_cutoff_inside_a_committed_batch_is_refused() {
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    store
        .ingest(IngestBatch::new(
            (91..94)
                .map(|id| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(id), Revision::new(1)),
                        vec![1.0, 0.0],
                    )
                })
                .collect(),
        ))
        .unwrap();
    drop(store);
    let path = directory.path().join("manifest.ze");
    let mut manifest = crate::manifest::io::load_manifest(&StdVfs, &path, 3).unwrap();
    manifest.generation = 3;
    manifest.record_generation_bump(1).unwrap();
    crate::manifest::io::commit_manifest(
        &StdVfs,
        directory.path(),
        &manifest,
        crate::lifecycle::durability::DurabilityPolicy::new(
            DurabilityMode::Durable,
            CommitTier::Durable,
        )
        .unwrap(),
    )
    .unwrap();
    let before = file_snapshot(directory.path());
    for access in [
        crate::lifecycle::AccessMode::ReadOnly,
        crate::lifecycle::AccessMode::ReadWrite,
    ] {
        assert!(
            matches!(Store::open(directory.path(), native_options().with_access_mode(access)),
            Err(crate::lifecycle::StoreError::Manifest(crate::manifest::ManifestError::Decode(message)))
                if message == "generation boundary is not a committed batch boundary")
        );
        assert_eq!(file_snapshot(directory.path()), before);
    }
}

#[test]
fn a_purge_checkpoint_does_not_absorb_an_unrepaired_sealed_delete() {
    use crate::ingest::DocId;
    use crate::ingest::wal_payload::{
        DELETE_V1, GRAPH_COMMIT_V1, MIXED_BATCH_MEMBER_V1, encode_delete, encode_mixed_batch_member,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.ingest(purge_documents(&[91, 92], 1)).unwrap();
    store.seal().unwrap();
    store.enable_graph().unwrap();
    store.purge(&[DocId::new(92)]).unwrap();
    let node = commit_tail_test_node(&store, "mixed-delete-purge");
    drop(store);
    let wal_path = directory.path().join("wal.ze");
    let clean = crate::wal::WalReader::open(&StdVfs, &wal_path)
        .unwrap()
        .into_clean()
        .unwrap();
    let graph = clean
        .records()
        .iter()
        .find(|record| record.op == GRAPH_COMMIT_V1)
        .unwrap();
    let delete = encode_delete(&[DocId::new(91)]).unwrap();
    let members = [
        encode_mixed_batch_member(0, 2, DELETE_V1, &delete).unwrap(),
        encode_mixed_batch_member(1, 2, GRAPH_COMMIT_V1, graph.payload().unwrap()).unwrap(),
    ];
    std::fs::remove_file(&wal_path).unwrap();
    let writer = crate::wal::WalWriter::create(
        &StdVfs,
        &wal_path,
        crate::wal::LogSeq::new(3),
        crate::lifecycle::durability::DurabilityPolicy::new(
            DurabilityMode::Durable,
            CommitTier::Durable,
        )
        .unwrap(),
    )
    .unwrap();
    writer
        .commit_many(
            &members
                .iter()
                .map(|payload| (MIXED_BATCH_MEMBER_V1, payload.as_slice()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
    drop(writer); // Cut 1: complete mixed batch synced, no document publication.

    let vfs = Arc::new(RecordingVfs::default());
    let manifest_path = directory.path().join("manifest.ze");
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = Arc::clone(&reached);
    vfs.after_selector_sync(move || {
        let manifest = crate::manifest::io::load_manifest(&StdVfs, &manifest_path, 4).unwrap();
        if manifest.graph.as_ref().unwrap().graph_absorbed_through == 4 {
            observed.store(true, Ordering::Release);
            // Cut 2: checkpoint selector is durable, purge has not continued.
            return Err(std::io::Error::other("durable purge checkpoint cut"));
        }
        Ok(())
    });
    let error = Store::open_with_test_dependencies(
        directory.path(),
        native_options(),
        crate::lifecycle::StoreTestDependencies::new(
            vfs,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
    )
    .err()
    .expect("stop at the durable purge checkpoint");
    assert!(
        error.to_string().contains("durable purge checkpoint cut"),
        "{error}"
    );
    assert!(reached.load(Ordering::Acquire));
    for reopen in 1..=2 {
        let recovered = Store::open(directory.path(), native_options())
            .unwrap_or_else(|error| panic!("reopen {reopen}: {error}"));
        assert_eq!(
            recovered.count_documents(None, None).unwrap().count,
            0,
            "DELETE A must survive the purge checkpoint (reopen {reopen})"
        );
        assert!(
            observe_node(&recovered, node).is_some(),
            "N must survive with DELETE A"
        );
        drop(recovered);
    }
}

#[test]
fn a_checkpoint_that_loses_a_race_with_purge_retries_without_fencing() {
    use crate::ingest::DocId;
    use std::sync::Barrier;
    for caller in ["write", "explicit", "close", "maintenance"] {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(directory.path(), native_options()).unwrap());
        store.ingest(purge_documents(&[91], 1)).unwrap();
        store.enable_graph().unwrap();
        disable_generation_fixture_maintenance(&store);
        let mut nodes = Vec::new();
        for index in 0..64 {
            nodes.push(commit_tail_test_node(&store, &format!("race-{index}")));
        }
        let token = store.purge(&[DocId::new(91)]).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        store
            .native_graph
            .state
            .lock()
            .unwrap()
            .checkpoint_inventory_hook = Some((Arc::clone(&entered), Arc::clone(&release)));
        let racing = Arc::clone(&store);
        let worker = std::thread::spawn(move || {
            let control = QueryControl::Cancel(CancelToken::new());
            match caller {
                "write" => {
                    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
                    racing
                        .apply_native_graph(
                            &[StructuredWrite {
                                key: ApplicationKey::new(EntityKind::Node, "tail", "racing-write")
                                    .unwrap(),
                                revision: GraphRevision::new(1).unwrap(),
                                operation: StructuredOperation::Create,
                                image: Some(WriteImage::Node(&image)),
                            }],
                            &control,
                        )
                        .map(|_| ())
                }
                "explicit" => racing.checkpoint_native_graph(&control),
                "close" => racing.checkpoint_native_graph_for_close(),
                "maintenance" => racing.maintain_native_graph_step(&control).map(|_| ()),
                _ => unreachable!(),
            }
        });
        entered.wait(); // Inventory is captured; WAL lock is not yet held.
        let purge = store.await_physical_purge(token);
        release.wait(); // Release even if purge failed, so no blocked worker remains.
        purge.unwrap();
        let result = worker.join().unwrap();
        assert!(
            result.is_ok(),
            "{caller} checkpoint must retry without fencing: {result:?}"
        );
        {
            let writer = store.native_graph.writer.lock().unwrap();
            assert!(!writer.as_ref().unwrap().checkpoint_failed);
        }
        nodes.push(commit_tail_test_node(&store, "after-checkpoint-race"));
        let generation = store.snapshot().unwrap().generation();
        drop(store);
        assert_purge_reopens(directory.path(), generation, &[], &[91], &nodes);
    }
}

#[test]
fn repeated_active_purges_keep_the_generation_history_bounded() {
    use crate::ingest::DocId;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "before-repeated-active-purges");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    assert_eq!(store.snapshot().unwrap().generation(), 3);
    let mut maximum_rows = 0;
    for cycle in 0..200 {
        let id = 1000 + cycle;
        store.ingest(purge_documents(&[id], 1)).unwrap();
        let token = store.purge(&[DocId::new(id)]).unwrap();
        let report = store.await_physical_purge(token).unwrap();
        assert_eq!(report.generation(), 3 + 2 * (cycle as u64 + 1));
        let clean = crate::wal::WalReader::open(&StdVfs, &directory.path().join("wal.ze"))
            .unwrap()
            .into_clean()
            .unwrap();
        assert!(
            clean.records().is_empty(),
            "each purge retires the entire WAL"
        );
        let manifest = crate::manifest::io::load_manifest(
            &StdVfs,
            &directory.path().join("manifest.ze"),
            clean.retained_first_seq() - 1,
        )
        .unwrap();
        maximum_rows = maximum_rows.max(manifest.graph.as_ref().unwrap().generation_bumps.len());
    }
    // One row suffices: no retained record needs chronology before the retired boundary.
    assert!(
        maximum_rows <= 1,
        "retired generation history grew to {maximum_rows} rows"
    );
    drop(store);
    assert_purge_reopens(directory.path(), 403, &[], &[1199], &[node]);
    let recovered = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&recovered);
    let next = commit_tail_test_node(&recovered, "after-repeated-active-purges");
    assert_eq!(recovered.snapshot().unwrap().generation(), 404);
    drop(recovered);
    assert_purge_reopens(directory.path(), 404, &[], &[1199], &[node, next]);
}

#[test]
fn a_manifest_rename_that_reports_failure_after_succeeding_fences_the_writers() {
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
    assert_eq!(store.enable_graph().unwrap(), 1);
    disable_generation_fixture_maintenance(&store);
    assert_eq!(
        store
            .ingest(purge_documents(&[91], 1))
            .unwrap()
            .generation(),
        2
    );
    vfs.arm_fault(FaultPoint::PostManifestRename);
    assert!(store.seal().is_err());
    vfs.assert_fired_once();
    assert_eq!(store.snapshot().unwrap().generation(), 2);
    let manifest =
        crate::manifest::io::load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 1)
            .unwrap();
    assert_eq!(manifest.generation, 3);
    assert_shared_writer_stopped(&store);
    drop(store);
    let recovered = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(recovered.snapshot().unwrap().generation(), 3);
    assert_eq!(recovered.count_documents(None, None).unwrap().count, 1);
    disable_generation_fixture_maintenance(&recovered);
    let node = commit_tail_test_node(&recovered, "after-manifest-rename");
    assert_eq!(observe_node(&recovered, node), Some((4, 1, 1)));
    drop(recovered);
    let recovered = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(recovered.snapshot().unwrap().generation(), 4);
    assert_eq!(observe_node(&recovered, node), Some((4, 1, 1)));
}

#[test]
fn a_purge_manifest_rename_post_commit_error_fences_the_writers() {
    manifest_rename_publisher_failure("purge");
}

#[test]
fn a_checkpoint_manifest_rename_post_commit_error_fences_the_writers() {
    manifest_rename_publisher_failure("checkpoint");
}

#[test]
fn a_reindex_manifest_rename_post_commit_error_fences_the_writers() {
    manifest_rename_publisher_failure("reindex");
}

pub(super) fn manifest_rename_publisher_failure(family: &str) {
    manifest_publisher_failure(family, FaultPoint::PostManifestRename);
}

#[test]
fn a_manifest_temp_sync_error_fences_the_writers() {
    manifest_publisher_failure("reindex", FaultPoint::ManifestSync);
}

fn manifest_publisher_failure(family: &str, point: FaultPoint) {
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
    generation_fixture(&store, 91);
    if family == "merge" {
        generation_fixture(&store, 92);
    }
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    if family == "seal" {
        store.ingest(purge_documents(&[92], 1)).unwrap();
    }
    let node = if family == "checkpoint" {
        Some(commit_tail_test_node(&store, "before-rename-fault"))
    } else {
        None
    };
    let before = store.snapshot().unwrap().generation();
    let token = if family == "purge" {
        Some(store.purge(&[crate::ingest::DocId::new(91)]).unwrap())
    } else {
        None
    };
    vfs.arm_fault(point);
    match family {
        "purge" => assert!(store.await_physical_purge(token.unwrap()).is_err()),
        "checkpoint" => assert!(
            store
                .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                .is_err()
        ),
        "reindex" => assert!(store.reindex_text().is_err()),
        "seal" => assert!(store.seal().is_err()),
        "retention" => assert!(store.drop_partition(91..92).is_err()),
        "merge" => assert!(store.merge_sealed().is_err()),
        "snapshot" => {
            use crate::lifecycle::{InMemorySegment, InMemorySegmentFactors};
            let columns = crate::meta::ColumnStoreBuilder::new(store.schema().clone())
                .finish()
                .unwrap();
            let alive = crate::meta::AliveSet::new(0);
            let prepared = store
                .prepare_segment(InMemorySegment {
                    id: crate::segment::SegmentId::new(6, [6; 10]),
                    scheme: 4,
                    dims: 2,
                    codes: Vec::new(),
                    factors: InMemorySegmentFactors::Bit4(Vec::new()),
                    rescore: Vec::new(),
                    columns: &columns,
                    alive: &alive,
                })
                .unwrap();
            assert!(store.seal_snapshot(prepared).is_err());
        }
        _ => panic!("unknown publisher"),
    }
    vfs.assert_fired_once();
    assert_eq!(store.snapshot().unwrap().generation(), before);
    let manifest = crate::manifest::io::load_manifest(
        &StdVfs,
        &directory.path().join("manifest.ze"),
        u64::MAX,
    )
    .unwrap();
    let durable_generation = before + u64::from(point == FaultPoint::PostManifestRename);
    assert_eq!(manifest.generation, durable_generation);
    assert_shared_writer_stopped(&store);
    drop(store);
    let recovered = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(
        recovered.snapshot().unwrap().generation(),
        durable_generation
    );
    assert_eq!(
        recovered.count_documents(None, None).unwrap().count,
        match family {
            "purge" | "retention" | "snapshot" => 0,
            "seal" | "merge" => 2,
            _ => 1,
        }
    );
    if let Some(node) = node {
        assert!(observe_node(&recovered, node).is_some());
    }
    disable_generation_fixture_maintenance(&recovered);
    let next = commit_tail_test_node(&recovered, "after-rename-fault");
    let observation = observe_node(&recovered, next);
    drop(recovered);
    let recovered = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(
        recovered.snapshot().unwrap().generation(),
        durable_generation + 1
    );
    assert_eq!(observe_node(&recovered, next), observation);
}

#[test]
fn a_failed_manifest_publication_fences_even_no_op_publishers() {
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
    generation_fixture(&store, 91);
    store.enable_graph().unwrap();
    vfs.arm_fault(FaultPoint::PostManifestRename);
    assert!(store.reindex_text().is_err());
    vfs.assert_fired_once();
    let replay = crate::ingest::IngestDocument::new(
        crate::ingest::DocumentVersion::new(
            crate::ingest::DocId::new(91),
            crate::ingest::Revision::new(1),
        ),
        vec![1.0, 0.0],
    )
    .with_timestamp(91)
    .with_text("generation fixture");
    assert!(
        store
            .ingest(crate::ingest::IngestBatch::new(vec![replay]))
            .is_err(),
        "an ingest replay must respect the shared fence"
    );
    assert!(
        store.purge(&[crate::ingest::DocId::new(999)]).is_err(),
        "an unknown-id purge must respect the shared fence"
    );
    assert!(
        store.seal().is_err(),
        "an empty seal must respect the shared fence"
    );
    assert!(
        store.merge_sealed().is_err(),
        "a one-segment merge must respect the shared fence"
    );
    assert!(
        store.drop_partition(10..10).is_err(),
        "an empty partition drop must respect the shared fence"
    );
}

#[test]
fn a_namespace_post_adoption_error_fences_every_participant_writer() {
    use crate::lifecycle::{
        LiveNamespaceMutation, NamespaceMutation, namespace_batch_live_with_steps,
    };
    let root = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let open = |name| {
        Store::open_with_test_dependencies(
            root.path().join(name),
            native_options(),
            crate::lifecycle::StoreTestDependencies::new(
                vfs.clone(),
                Arc::new(crate::lifecycle::SystemMonotonicClock),
            ),
        )
        .unwrap()
    };
    let first = open("a");
    let second = open("b");
    for store in [&first, &second] {
        generation_fixture(store, 91);
        store.enable_graph().unwrap();
        disable_generation_fixture_maintenance(store);
    }
    let mut fired = false;
    let mut installed = false;
    let result = namespace_batch_live_with_steps(
        root.path(),
        vec![
            LiveNamespaceMutation {
                store: &first,
                mutation: NamespaceMutation {
                    name: "a".into(),
                    options: native_options(),
                    upserts: Vec::new(),
                    deletes: vec![crate::ingest::DocId::new(91)],
                    delete_where: None,
                },
            },
            LiveNamespaceMutation {
                store: &second,
                mutation: NamespaceMutation {
                    name: "b".into(),
                    options: native_options(),
                    upserts: vec![crate::ingest::IngestDocument::new(
                        crate::ingest::DocumentVersion::new(
                            crate::ingest::DocId::new(92),
                            crate::ingest::Revision::new(1),
                        ),
                        vec![1.0, 0.0],
                    )],
                    deletes: Vec::new(),
                    delete_where: None,
                },
            },
        ],
        &mut |step| {
            if step == "live states installed" {
                installed = true;
            }
            if installed && step == "commit rename" && !fired {
                fired = true;
                vfs.arm_fault(FaultPoint::PurgeIntentOpen);
            }
            Ok(())
        },
    );
    assert!(fired, "the cleanup publication seam must fire: {result:?}");
    assert!(result.is_err());
    vfs.assert_fired_once();
    assert_shared_writer_stopped(&first);
    assert_shared_writer_stopped(&second);
    drop(first);
    drop(second);
    for (name, count) in [("a", 0), ("b", 2)] {
        let recovered = Store::open(root.path().join(name), native_options()).unwrap();
        assert_eq!(recovered.count_documents(None, None).unwrap().count, count);
    }
}

#[test]
fn a_retention_manifest_rename_post_commit_error_fences_the_writers() {
    manifest_rename_publisher_failure("retention");
}

#[test]
fn a_merge_manifest_rename_post_commit_error_fences_the_writers() {
    manifest_rename_publisher_failure("merge");
}

#[test]
fn a_snapshot_manifest_rename_post_commit_error_fences_the_writers() {
    manifest_rename_publisher_failure("snapshot");
}

#[test]
fn a_schema_manifest_rename_post_commit_error_returns_no_stale_writer() {
    schema_manifest_rename_failure();
}

pub(super) fn schema_manifest_rename_failure() {
    use crate::meta::{ColumnDefinition, ColumnId, ColumnType, Schema};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    assert_eq!(store.enable_graph().unwrap(), 1);
    drop(store);
    let schema = Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(71),
        "extra",
        ColumnType::U64,
        true,
    )])
    .unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    vfs.arm_fault(FaultPoint::PostManifestRename);
    let result = Store::open_with_test_dependencies(
        directory.path(),
        native_options().with_schema(schema.clone()),
        crate::lifecycle::StoreTestDependencies::new(
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
    );
    assert!(result.is_err(), "failed construction must return no writer");
    vfs.assert_fired_once();
    let recovered = Store::open(
        directory.path(),
        native_options().with_schema(schema.clone()),
    )
    .unwrap();
    assert_eq!(recovered.snapshot().unwrap().generation(), 2);
    assert_eq!(recovered.schema(), &schema);
    disable_generation_fixture_maintenance(&recovered);
    let node = commit_tail_test_node(&recovered, "after-schema-rename");
    assert_eq!(observe_node(&recovered, node), Some((3, 1, 1)));
    drop(recovered);
    let recovered = Store::open(directory.path(), native_options().with_schema(schema)).unwrap();
    assert_eq!(recovered.snapshot().unwrap().generation(), 3);
    assert_eq!(observe_node(&recovered, node), Some((3, 1, 1)));
}

#[test]
fn a_tier_promotion_manifest_rename_post_commit_error_fences_the_writers() {
    tier_manifest_rename_failure(false);
}

#[test]
fn a_tier_consolidation_manifest_rename_post_commit_error_fences_the_writers() {
    tier_manifest_rename_failure(true);
}

pub(super) fn tier_manifest_rename_failure(consolidate: bool) {
    let directory = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let options = native_options().with_epoch(generation_epoch());
    let store = Store::open_with_test_dependencies(
        directory.path(),
        options.clone(),
        crate::lifecycle::StoreTestDependencies::new(
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
    )
    .unwrap();
    let rows = if consolidate { 3 } else { 1 };
    for id in 91..91 + rows {
        generation_fixture(&store, id);
    }
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let before = store.snapshot().unwrap().generation();
    let preceding_promotions = if consolidate { 3 } else { 0 };
    vfs.arm_fault_after(FaultPoint::PostManifestRename, preceding_promotions);
    let report = store.maintain_with_test_thresholds(
        crate::tier::maintain::MaintenanceBudget {
            wall_time: std::time::Duration::from_secs(30),
            bytes: u64::MAX,
        },
        crate::tier::TierThresholds { graph_min_rows: 1 },
    );
    assert!(
        matches!(
            report.status,
            crate::tier::maintain::MaintenanceStatus::Failed(_)
        ),
        "{report:?}"
    );
    vfs.assert_fired_once();
    assert_eq!(
        store.snapshot().unwrap().generation(),
        before + preceding_promotions
    );
    assert_shared_writer_stopped(&store);
    drop(store);
    let recovered = Store::open(directory.path(), options.clone()).unwrap();
    assert_eq!(
        recovered.snapshot().unwrap().generation(),
        before + preceding_promotions + 1
    );
    assert_eq!(
        recovered.count_documents(None, None).unwrap().count,
        rows as u64
    );
    disable_generation_fixture_maintenance(&recovered);
    let node = commit_tail_test_node(&recovered, "after-tier-rename");
    let observation = observe_node(&recovered, node);
    drop(recovered);
    let recovered = Store::open(directory.path(), options).unwrap();
    assert_eq!(observe_node(&recovered, node), observation);
}

#[test]
fn an_epoch_alias_manifest_rename_post_commit_error_fences_the_writers() {
    epoch_manifest_rename_failure(false);
}

#[test]
fn an_epoch_drop_manifest_rename_post_commit_error_fences_the_writers() {
    epoch_manifest_rename_failure(true);
}

pub(super) fn epoch_manifest_rename_failure(drop_epoch: bool) {
    use crate::manifest::{EpochMeta, Manifest};
    use crate::segment::writer::{
        SegmentBuild, SegmentDocumentVersions, SegmentFactors, write_segment_with_documents,
    };
    let directory = tempfile::tempdir().unwrap();
    let a = generation_epoch();
    let mut b = a.clone();
    b.embedding.alignment_digest = vec![2];
    let columns = crate::meta::ColumnStoreBuilder::new(crate::meta::Schema::timestamp_only())
        .finish()
        .unwrap();
    let alive = crate::meta::AliveSet::new(0);
    let policy = crate::lifecycle::durability::DurabilityPolicy::new(
        DurabilityMode::Durable,
        CommitTier::Durable,
    )
    .unwrap();
    let mut segments = Vec::new();
    for (ordinal, epoch) in [(1, &a), (2, &b)] {
        let mut segment = write_segment_with_documents(
            &StdVfs,
            directory.path(),
            SegmentBuild {
                id: crate::segment::SegmentId::new(ordinal, [ordinal as u8; 10]),
                scheme: 4,
                dims: 2,
                codes: &[],
                factors: SegmentFactors::Bit4(&[]),
                rescore: &[],
                columns: &columns,
                alive: &alive,
            },
            SegmentDocumentVersions {
                doc_ids: &[],
                revisions: &[],
            },
            policy,
        )
        .unwrap();
        segment.epoch_id = Some(epoch.identity().embedding);
        segments.push(segment);
    }
    crate::manifest::io::commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            graph: None,
            generation: 1,
            log_seq: 0,
            segments,
            epochs: vec![EpochMeta::from(&a), EpochMeta::from(&b)],
            epoch_alias: Some(a.identity()),
            schema: crate::meta::Schema::timestamp_only(),
        },
        policy,
    )
    .unwrap();
    let options = native_options().with_epoch(a.clone());
    let vfs = Arc::new(RecordingVfs::default());
    let store = Store::open_with_test_dependencies(
        directory.path(),
        options.clone(),
        crate::lifecycle::StoreTestDependencies::new(
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
    )
    .unwrap();
    assert_eq!(store.enable_graph().unwrap(), 2);
    disable_generation_fixture_maintenance(&store);
    vfs.arm_fault(FaultPoint::PostManifestRename);
    if drop_epoch {
        assert!(store.drop_epoch(b.identity().embedding).is_err());
    } else {
        assert!(store.switch_epoch_alias(b.identity()).is_err());
    }
    vfs.assert_fired_once();
    assert_eq!(store.snapshot().unwrap().generation(), 2);
    assert_shared_writer_stopped(&store);
    drop(store);
    let options = native_options().with_epoch(if drop_epoch { a } else { b });
    let recovered = Store::open(directory.path(), options.clone()).unwrap();
    assert_eq!(recovered.snapshot().unwrap().generation(), 3);
    disable_generation_fixture_maintenance(&recovered);
    let node = commit_tail_test_node(&recovered, "after-epoch-rename");
    let observation = observe_node(&recovered, node);
    drop(recovered);
    let recovered = Store::open(directory.path(), options).unwrap();
    assert_eq!(observe_node(&recovered, node), observation);
}
