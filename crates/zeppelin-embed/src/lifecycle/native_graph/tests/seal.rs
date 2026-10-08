//! Seal folds the document segment and current graph into one manifest.
use super::publication::{FaultPoint, RecordingVfs};
use super::recovery::{
    assert_shared_writer_stopped, commit_tail_test_node, disable_generation_fixture_maintenance,
    native_options, observe_node,
};
use super::tempfile;
use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::lifecycle::{AccessMode, CancelToken, QueryControl, Store};
use crate::manifest::io::load_manifest;
use crate::vfs::StdVfs;
use std::sync::Arc;

fn document(store: &Store, id: u128) {
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(id), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .unwrap();
}

#[test]
fn folds_graph_state_into_the_same_manifest() {
    assert_fold();
}

fn assert_fold() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    document(&store, 91);
    let node = commit_tail_test_node(&store, "seal-fold");
    let before = store.snapshot().unwrap().generation();
    let observation = observe_node(&store, node);
    let generation = store.seal().expect("seal must fold the graph tail");
    assert_eq!(generation, before + 1, "one fold, one generation");
    let manifest = load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 2).unwrap();
    assert_eq!(manifest.generation, generation);
    assert_eq!(manifest.log_seq, 2);
    assert_eq!(manifest.segments.len(), 1);
    let graph = manifest.graph.as_ref().unwrap();
    assert_eq!(graph.graph_absorbed_through, 2);
    assert_eq!(graph.state().unwrap().sequence, 1);
    assert_eq!(
        std::fs::metadata(directory.path().join("wal.ze"))
            .unwrap()
            .len(),
        40
    );
    assert_eq!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .fold
            .manifest_generation,
        generation
    );
    drop(store);
    for access in [
        AccessMode::ReadWrite,
        AccessMode::ReadOnly,
        AccessMode::ReadWrite,
    ] {
        let reopened =
            Store::open(directory.path(), native_options().with_access_mode(access)).unwrap();
        assert_eq!(reopened.snapshot().unwrap().generation(), generation);
        assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
        assert_eq!(observe_node(&reopened, node), observation);
    }
}

fn rotation_fixture(store: &Store) -> [crate::property_graph::NodeId; 2] {
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(store);
    document(store, 91);
    let first = commit_tail_test_node(store, "before-fold");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let second = commit_tail_test_node(store, "after-fold");
    [first, second]
}

#[test]
fn rotation_waits_for_graph_absorbed_through() {
    assert_rotation_control();
}

fn assert_rotation_control() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    let nodes = rotation_fixture(&store);
    let before = load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 3).unwrap();
    assert_eq!(before.log_seq, 0);
    assert_eq!(before.graph.as_ref().unwrap().graph_absorbed_through, 2);
    let wal = crate::wal::WalReader::open(&StdVfs, &directory.path().join("wal.ze"))
        .unwrap()
        .into_clean()
        .unwrap();
    assert_eq!(wal.records().last().unwrap().seq.get(), 3);
    assert_eq!(
        wal.records().last().unwrap().op,
        crate::ingest::wal_payload::GRAPH_COMMIT_V1
    );
    let generation = store.seal().unwrap();
    assert_eq!(generation, 6);
    let after = load_manifest(&StdVfs, &directory.path().join("manifest.ze"), 3).unwrap();
    assert_eq!(after.log_seq, 3);
    assert_eq!(after.graph.as_ref().unwrap().graph_absorbed_through, 3);
    assert_eq!(
        std::fs::metadata(directory.path().join("wal.ze"))
            .unwrap()
            .len(),
        40
    );
    drop(store);
    assert_reopens(directory.path(), generation, &nodes, (5, 2));
}

fn assert_reopens(
    path: &std::path::Path,
    generation: u64,
    nodes: &[crate::property_graph::NodeId],
    graph_state: (u64, u64),
) {
    for access in [
        AccessMode::ReadWrite,
        AccessMode::ReadOnly,
        AccessMode::ReadWrite,
    ] {
        let store = Store::open(path, native_options().with_access_mode(access)).unwrap();
        assert_eq!(store.snapshot().unwrap().generation(), generation);
        assert_eq!(store.count_documents(None, None).unwrap().count, 1);
        for &node in nodes {
            assert_eq!(
                observe_node(&store, node),
                Some((graph_state.0, graph_state.1, 1))
            );
        }
    }
}

pub(super) fn rotation_fault(point: FaultPoint, skip: u64) {
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
    let nodes = rotation_fixture(&store);
    let expected_generation = store.snapshot().unwrap().generation() + 1;
    assert_eq!(expected_generation, 6);
    vfs.arm_fault_after(point, skip);
    store.seal().expect_err("rotation failure must be reported");
    vfs.assert_fired_once();
    assert_shared_writer_stopped(&store);
    assert_eq!(store.snapshot().unwrap().generation(), expected_generation);
    drop(store);
    assert_reopens(directory.path(), expected_generation, &nodes, (5, 2));
    let store = Store::open(directory.path(), native_options()).unwrap();
    disable_generation_fixture_maintenance(&store);
    let next = commit_tail_test_node(&store, "after-rotation-crash");
    let wal = crate::wal::WalReader::open(&StdVfs, &directory.path().join("wal.ze"))
        .unwrap()
        .into_clean()
        .unwrap();
    assert_eq!(wal.records().last().unwrap().seq.get(), 4);
    drop(store);
    assert_reopens(
        directory.path(),
        expected_generation + 1,
        &[nodes[0], nodes[1], next],
        (7, 3),
    );
}

#[test]
fn every_crash_state_of_the_wal_truncation_resumes_after_the_boundary() {
    rotation_fault(FaultPoint::Rename, 2);
    rotation_fault(FaultPoint::PostWalRename, 0);
    rotation_fault(FaultPoint::DirectorySync, 2);
}

pub(crate) fn run_rotation_probe() -> Vec<crate::graph_read_view_test_support::PathReceipt> {
    assert_rotation_control();
    [
        (
            "storage-durability.seal.graph-rotation.rename",
            FaultPoint::Rename,
            2,
        ),
        (
            "storage-durability.seal.graph-rotation.post-rename",
            FaultPoint::PostWalRename,
            0,
        ),
        (
            "storage-durability.seal.graph-rotation.sync",
            FaultPoint::DirectorySync,
            2,
        ),
    ]
    .into_iter()
    .map(|(key, point, skip)| {
        rotation_fault(point, skip);
        crate::graph_read_view_test_support::PathReceipt {
            key,
            fires: 1,
            clean_controls: 1,
        }
    })
    .collect()
}
