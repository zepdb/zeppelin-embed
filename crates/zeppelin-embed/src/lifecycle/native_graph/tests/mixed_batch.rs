use super::publication::{DurabilityEvent, RecordingVfs};
use super::recovery::{disable_generation_fixture_maintenance, native_options, observe_node};
use super::tempfile;
use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::lifecycle::{CancelToken, DocumentFields, QueryControl, Store};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphRevision,
};
use crate::vfs::SyncKind;
use std::sync::Arc;

fn document(id: u128) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(1)),
        vec![1.0, 0.0],
    )
    .with_text("mixed orchard")
}

fn open(path: &std::path::Path, vfs: Arc<RecordingVfs>) -> Store {
    let store = Store::open_with_infrastructure(
        path,
        native_options(),
        vfs,
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
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    store
}

#[test]
fn returns_one_generation_for_documents_and_graph() {
    let directory = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let store = open(directory.path(), vfs.clone());
    let before = store.snapshot().unwrap().generation();
    let manifest = std::fs::read(directory.path().join("manifest.ze")).unwrap();
    vfs.clear_events();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let requests = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "mixed", "first").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    let result = store
        .apply_native_mixed(
            &IngestBatch::new(vec![document(91), document(92)]),
            &requests,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert_eq!(result.changed_generation().unwrap().get(), before + 1);
    assert_eq!(store.snapshot().unwrap().generation(), before + 1);
    let EntityId::Node(node) = result[0].entity else {
        panic!("expected node")
    };
    assert_eq!(observe_node(&store, node).unwrap().0, before + 1);
    let events = vfs.take();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, DurabilityEvent::Append(p) if p.ends_with("wal.ze")))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(
                |e| matches!(e, DurabilityEvent::Sync(p, SyncKind::Full) if p.ends_with("wal.ze"))
            )
            .count(),
        1
    );
    assert_eq!(
        std::fs::read(directory.path().join("manifest.ze")).unwrap(),
        manifest
    );
    let wal =
        crate::wal::WalReader::open(&crate::vfs::StdVfs, &directory.path().join("wal.ze")).unwrap();
    let records = wal.records();
    assert_eq!(records.len(), 3);
    assert!(
        records
            .iter()
            .all(|r| r.op == crate::ingest::wal_payload::MIXED_BATCH_MEMBER_V1)
    );
    for id in [91, 92] {
        assert!(
            store
                .get_documents(&[DocId::new(id)], DocumentFields::NONE)
                .unwrap()[0]
                .is_some()
        );
    }
    drop(store);
    for access in [
        crate::lifecycle::AccessMode::ReadWrite,
        crate::lifecycle::AccessMode::ReadOnly,
    ] {
        let reopened =
            Store::open(directory.path(), native_options().with_access_mode(access)).unwrap();
        assert_eq!(reopened.snapshot().unwrap().generation(), before + 1);
        assert_eq!(reopened.count_documents(None, None).unwrap().count, 2);
        assert!(observe_node(&reopened, node).is_some());
    }
}

#[test]
fn a_rejected_item_commits_nothing() {
    for reject_graph in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let vfs = Arc::new(RecordingVfs::default());
        let store = open(directory.path(), vfs.clone());
        let before = store.snapshot().unwrap().generation();
        let manifest = std::fs::read(directory.path().join("manifest.ze")).unwrap();
        let wal = std::fs::read(directory.path().join("wal.ze")).unwrap();
        vfs.clear_events();
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let request = StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "mixed", "rejected").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: if reject_graph {
                StructuredOperation::Put(EntityId::Node(
                    crate::property_graph::NodeId::new(1).unwrap(),
                ))
            } else {
                StructuredOperation::Create
            },
            image: Some(WriteImage::Node(&image)),
        };
        let documents = IngestBatch::new(vec![
            document(91),
            if reject_graph {
                document(92)
            } else {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(92), Revision::new(1)),
                    vec![1.0],
                )
            },
        ]);
        assert!(
            store
                .apply_native_mixed(
                    &documents,
                    &[request],
                    &QueryControl::Cancel(CancelToken::new())
                )
                .is_err()
        );
        assert_eq!(store.snapshot().unwrap().generation(), before);
        assert_eq!(store.count_documents(None, None).unwrap().count, 0);
        assert_eq!(
            std::fs::read(directory.path().join("manifest.ze")).unwrap(),
            manifest
        );
        assert_eq!(std::fs::read(directory.path().join("wal.ze")).unwrap(), wal);
        assert!(vfs.take().is_empty(), "a rejected item must write nothing");
        store
            .ingest(IngestBatch::new(vec![document(93)]))
            .expect("definite rejection leaves writer usable");
    }
}

#[test]
fn every_crash_state_of_the_first_mixed_batch_leaves_both_or_neither() {
    use crate::vfs::crash::{
        CrashOperation, MemoryVfs, RecordingVfs as ByteRecorder, replay_prefix,
    };
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let recorder = Arc::new(ByteRecorder::new(RecordingVfs::default()));
    let store = Store::open_with_infrastructure(
        &root,
        native_options(),
        recorder.clone(),
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
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let start = recorder.operations().unwrap().len();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let requests = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "mixed", "crash").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    let result = store
        .apply_native_mixed(
            &IngestBatch::new(vec![document(91), document(92)]),
            &requests,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let EntityId::Node(node) = result[0].entity else {
        panic!("expected node")
    };
    let operations = recorder.operations().unwrap();
    drop(store);
    let mut images = Vec::new();
    for cut in start..=operations.len() {
        images.push((
            format!("prefix {cut}"),
            replay_prefix(&MemoryVfs::new(), &operations, cut).unwrap(),
        ));
    }
    // Every byte cut of the tiny WAL append includes each complete member
    // boundary and every torn member, without probabilistic sector sampling.
    for (index, operation) in operations.iter().enumerate().skip(start) {
        if let CrashOperation::Append { path, bytes } = operation
            && path.ends_with("wal.ze")
        {
            for cut in 0..bytes.len() {
                let image = replay_prefix(&MemoryVfs::new(), &operations, index).unwrap();
                use crate::vfs::Vfs;
                image
                    .open_append(path)
                    .unwrap()
                    .append(&bytes[..cut])
                    .unwrap();
                images.push((format!("append {index} byte {cut}"), image));
            }
        }
    }
    let mut both = 0;
    let mut neither = 0;
    for (cut, image) in images {
        clear_files(&root);
        for (path, bytes) in image.files().unwrap() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let wal_path = root.join("wal.ze");
        let wal = std::fs::read(&wal_path).unwrap();
        if !wal.is_empty() && wal.len() < crate::wal::header::WAL_HEADER_LEN {
            // The established storage contract requires a torn fresh header
            // to refuse loudly, without guessing a sequence or repairing bytes.
            assert!(
                matches!(
                    Store::open(&root, native_options()),
                    Err(crate::lifecycle::StoreError::WalRecovery(
                        crate::wal::WalRecoveryError::InvalidHeader(_)
                    ))
                ),
                "{cut}: torn fresh header must refuse"
            );
            assert_eq!(std::fs::read(&wal_path).unwrap(), wal, "{cut}");
            continue;
        }
        let recovered =
            Store::open(&root, native_options()).unwrap_or_else(|e| panic!("{cut}: {e}"));
        let count = recovered.count_documents(None, None).unwrap().count;
        let graph = observe_node(&recovered, node).is_some();
        assert!(
            matches!((count, graph), (0, false) | (2, true)),
            "{cut}: docs={count}, graph={graph}"
        );
        assert_eq!(
            recovered.snapshot().unwrap().generation(),
            if graph { 2 } else { 1 },
            "{cut}"
        );
        both += usize::from(graph);
        neither += usize::from(!graph);
    }
    assert!(both > 0 && neither > 0);
}

fn clear_files(directory: &std::path::Path) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            clear_files(&entry.path());
        } else if entry.path().extension().is_none_or(|e| e != "lock") {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
}

#[test]
fn an_oversized_run_is_a_typed_refusal_before_any_write() {
    let directory = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let store = open(directory.path(), vfs.clone());
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let request = StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "mixed", "oversized").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    };
    let documents = IngestBatch::new(
        (91..108)
            .map(|id| document(id).with_metadata(vec![0; 1024 * 1024]))
            .collect(),
    );
    vfs.clear_events();
    let error = store
        .apply_native_mixed(
            &documents,
            &[request],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .err()
        .expect("group cap must reject whole run");
    assert!(
        matches!(
            error,
            crate::lifecycle::native_graph::NativeGraphError::Store(
                crate::lifecycle::StoreError::WalWrite(
                    crate::wal::WalWriteError::GroupTooLarge { .. }
                )
            )
        ),
        "{error}"
    );
    assert!(
        vfs.take().is_empty(),
        "group cap is checked before graph protection writes"
    );
    assert_eq!(store.count_documents(None, None).unwrap().count, 0);
    store
        .ingest(IngestBatch::new(vec![document(200)]))
        .expect("oversized refusal does not poison writer");
}

#[test]
fn an_exact_graph_retry_does_not_drop_new_documents() {
    let directory = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let store = open(directory.path(), vfs.clone());
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let requests = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "mixed", "replay").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    store
        .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let batch = IngestBatch::new(vec![document(91)]);
    let result = store
        .apply_native_mixed(&batch, &requests, &QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    assert_eq!(
        result.disposition(),
        crate::property_graph::BatchDisposition::Changed
    );
    assert_eq!(result.changed_generation().unwrap().get(), 3);
    assert!(result[0].replayed);
    assert_eq!(store.count_documents(None, None).unwrap().count, 1);
    let before = std::fs::read(directory.path().join("wal.ze")).unwrap();
    let replay = store
        .apply_native_mixed(&batch, &requests, &QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    assert!(replay.changed_generation().is_none());
    assert_eq!(store.snapshot().unwrap().generation(), 3);
    assert_eq!(
        std::fs::read(directory.path().join("wal.ze")).unwrap(),
        before
    );
}

#[test]
fn a_sealed_replacement_recovers_at_the_mixed_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), native_options()).unwrap();
    store.ingest(IngestBatch::new(vec![document(91)])).unwrap();
    store.seal().unwrap();
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let before = store.snapshot().unwrap().generation();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let request = StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "mixed", "replace").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    };
    let replacement = IngestDocument::new(
        DocumentVersion::new(DocId::new(91), Revision::new(2)),
        vec![1.0, 0.0],
    )
    .with_text("mixed replacement");
    let result = store
        .apply_native_mixed(
            &IngestBatch::new(vec![replacement]),
            &[request],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let EntityId::Node(node) = result[0].entity else {
        panic!("expected node")
    };
    assert_eq!(result.changed_generation().unwrap().get(), before + 1);
    assert_eq!(store.count_documents(None, None).unwrap().count, 1);
    drop(store);
    for access in [
        crate::lifecycle::AccessMode::ReadWrite,
        crate::lifecycle::AccessMode::ReadOnly,
    ] {
        let store =
            Store::open(directory.path(), native_options().with_access_mode(access)).unwrap();
        assert_eq!(store.snapshot().unwrap().generation(), before + 1);
        assert_eq!(store.count_documents(None, None).unwrap().count, 1);
        assert!(observe_node(&store, node).is_some());
        assert_eq!(
            store
                .get_documents(&[DocId::new(91)], DocumentFields::NONE)
                .unwrap()[0]
                .as_ref()
                .unwrap()
                .revision,
            Revision::new(2)
        );
    }
}

#[cfg(feature = "allocation-audit")]
#[test]
fn mixed_publication_allocates_nothing_after_the_wal_commit() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path(), Arc::new(RecordingVfs::default()));
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let requests = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "mixed", "allocation").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    store
        .apply_native_mixed(
            &IngestBatch::new(vec![document(91)]),
            &requests,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    assert_eq!(
        store
            .native_graph
            .commit_allocations
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        store
            .native_graph
            .commit_allocation_denials
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[test]
fn a_replayed_document_does_not_bypass_the_group_cap() {
    let directory = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let store = Store::open_with_infrastructure(
        directory.path(),
        crate::lifecycle::OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
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
    store.enable_graph().unwrap();
    disable_generation_fixture_maintenance(&store);
    let documents = IngestBatch::new(vec![document(91)]);
    store.ingest(documents.clone()).unwrap();
    vfs.clear_events();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let key = "x".repeat(crate::wal::DEFAULT_MAX_GROUP_BYTES);
    let requests = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "mixed", &key).unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    let error = store
        .apply_native_mixed(
            &documents,
            &requests,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .err()
        .unwrap();
    assert!(
        matches!(
            error,
            crate::lifecycle::native_graph::NativeGraphError::Store(
                crate::lifecycle::StoreError::WalWrite(
                    crate::wal::WalWriteError::GroupTooLarge { .. }
                )
            )
        ),
        "{error:?}"
    );
    assert!(vfs.take().is_empty());
    store
        .ingest(IngestBatch::new(vec![document(92)]))
        .expect("definite refusal does not fence");
}

#[test]
fn a_failed_artifact_sync_fences_graph_writes_before_they_write_again() {
    let directory = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let store = open(directory.path(), vfs.clone());
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let requests = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "mixed", "artifact-failure").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    vfs.arm_fault(super::publication::FaultPoint::ObjectSync);
    assert!(
        store
            .apply_native_mixed(
                &IngestBatch::new(vec![document(91)]),
                &requests,
                &QueryControl::Cancel(CancelToken::new())
            )
            .is_err()
    );
    vfs.assert_fired_once();
    assert!(store.ingest(IngestBatch::new(vec![document(92)])).is_err());
    vfs.clear_events();
    assert!(
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .is_err()
    );
    assert!(
        vfs.take().is_empty(),
        "a fenced native writer must refuse before creating further private artifacts"
    );
}

#[test]
fn ze399_associated_node_commit_has_one_wal_append_and_sync() {
    use crate::property_graph::{GraphBatch, GraphNodeDocument, NodeId};
    let directory = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let store = open(directory.path(), vfs.clone());
    let before = store.snapshot().unwrap().generation();
    let image = CanonicalContents::node(&mut [], &mut [], Some("mixed orchard"), None).unwrap();
    let documents = [GraphNodeDocument {
        item_index: 0,
        document: document(91),
    }];
    vfs.clear_events();
    let result = crate::property_graph::with_local_refs(|refs| {
        let node = crate::property_graph::NodeRef::Local(refs.node(0).unwrap());
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "mixed", "associated").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "mixed", "self").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    relationship_type: crate::property_graph::GraphName::new("LINK").unwrap(),
                    properties: &[],
                    source: node,
                    target: node,
                }),
            },
        ];
        store
            .graph_apply(
                GraphBatch::with_node_documents(&requests, &documents),
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap()
    });
    let EntityId::Relationship(id) = result.receipts()[1].entity else {
        panic!("relationship receipt")
    };
    let relationships = store
        .get_relationships(&[id], &QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let relationship = relationships.relationships()[0].as_ref().unwrap();
    assert_eq!(relationship.source, NodeId::new(91).unwrap());
    assert_eq!(relationship.target, NodeId::new(91).unwrap());
    assert_eq!(
        result.receipts()[0].entity,
        EntityId::Node(NodeId::new(91).unwrap())
    );
    assert_eq!(result.ack().generation(), before + 1);
    let events = vfs.take();
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, DurabilityEvent::Append(path) if path.ends_with("wal.ze"))
            )
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event| matches!(event, DurabilityEvent::Sync(path, SyncKind::Full) if path.ends_with("wal.ze"))).count(), 1);
}
