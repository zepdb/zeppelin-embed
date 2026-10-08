use super::recovery::{
    commit_tail_test_node, disable_generation_fixture_maintenance, native_options, observe_node,
};
use super::tempfile;
use crate::lifecycle::{OpenOptions, Store};
use std::collections::BTreeMap;
use std::path::Path;

fn image(path: &Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect()
}

#[test]
fn a_zgraph_beside_a_v2_manifest_is_unlinked_only_by_a_writable_open() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.close().unwrap();
    let before = image(directory.path());
    let orphan = directory
        .path()
        .join("graph-00000000000000000000000000000001.zgraph");
    std::fs::write(&orphan, b"interrupted enable").unwrap();
    let planted = image(directory.path());
    Store::open(directory.path(), OpenOptions::read_only())
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(image(directory.path()), planted);
    Store::open(directory.path(), native_options())
        .unwrap()
        .close()
        .unwrap();
    assert!(
        !orphan.exists(),
        "v2 manifest has no reachable graph objects"
    );
    assert_eq!(
        image(directory.path()),
        before,
        "graph-free manifest and WAL bytes must stay unchanged"
    );
}

#[test]
fn the_legacy_sweep_never_touches_zgraph_when_graph_is_enabled() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    store.close().unwrap();
    let orphan = directory
        .path()
        .join("graph-00000000000000000000000000000001.zgraph");
    std::fs::write(&orphan, b"unknown graph object").unwrap();
    let before = image(directory.path());
    Store::open(directory.path(), native_options())
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(image(directory.path()), before);
}

#[test]
fn an_object_named_by_an_unabsorbed_record_is_protected() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let node = commit_tail_test_node(&store, "unabsorbed");
    store.close().unwrap();
    let before = image(directory.path());
    let manifest =
        crate::manifest::decode_manifest("test", &before[std::ffi::OsStr::new("manifest.ze")])
            .unwrap();
    assert_eq!(manifest.graph.unwrap().graph_absorbed_through, 0);
    let objects: BTreeMap<_, _> = before
        .iter()
        .filter(|(name, _)| name.to_str().unwrap().ends_with(".zgraph"))
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
        .collect();
    assert!(!objects.is_empty());
    for options in [OpenOptions::read_only(), native_options()] {
        let reopened = Store::open(directory.path(), options).unwrap();
        assert!(observe_node(&reopened, node).is_some());
        for (name, bytes) in &objects {
            assert_eq!(&std::fs::read(directory.path().join(name)).unwrap(), bytes);
        }
        reopened.close().unwrap();
    }
}

#[test]
fn sweep_faults_refuse_and_resume_without_poisoning_a_list_refusal() {
    sweep_faults(1);
}

pub(super) fn sweep_faults(payload: u8) {
    use super::publication::{FaultPoint, RecordingVfs};
    use std::sync::Arc;
    for (point, skip) in [
        (FaultPoint::List, 0),
        (FaultPoint::Delete, 0),
        (FaultPoint::Delete, 1),
        (FaultPoint::DirectorySync, 0),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let vfs = Arc::new(RecordingVfs::default());
        let store = Store::open_with_infrastructure(
            directory.path(),
            native_options(),
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        for id in [1, 2] {
            std::fs::write(
                directory.path().join(format!("graph-{id:032x}.zgraph")),
                [payload],
            )
            .unwrap();
        }
        let before = image(directory.path());
        vfs.arm_fault_after(point, skip);
        assert!(
            crate::lifecycle::cleanup_open_orphans(&store).is_err(),
            "{point:?} must fail the sweep"
        );
        vfs.assert_fired_once();
        if point == FaultPoint::List {
            assert_eq!(image(directory.path()), before);
            store
                .enable_graph()
                .expect("a list refusal writes nothing and must leave the writer usable");
        } else {
            assert!(
                matches!(
                    store.enable_graph(),
                    Err(crate::lifecycle::StoreError::WalWrite(
                        crate::wal::WalWriteError::Failed { .. }
                    ))
                ),
                "unlink/sync failure must fence the shared writer"
            );
        }
        drop(store);
        let reopened = Store::open(directory.path(), native_options()).unwrap();
        if point != FaultPoint::List {
            assert!(
                !directory
                    .path()
                    .join(format!("graph-{:032x}.zgraph", 1))
                    .exists()
            );
            assert!(
                !directory
                    .path()
                    .join(format!("graph-{:032x}.zgraph", 2))
                    .exists()
            );
        }
        reopened.close().unwrap();
    }
}
