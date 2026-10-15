//! Exact per-row sizes from the official SF0.1 loader's first Comment nodes.
use super::*;
use crate::ingest::{DocId, DocumentVersion, IngestDocument, Revision};
use crate::lifecycle::native_graph::NativeGraphError;
use crate::property_graph::storage::preparation_work_capture as capture;
use crate::property_graph::{GraphBatch, GraphNodeDocument};
use crate::property_graph::{PropertyData, PropertyValue};

#[test]
fn ze444_ldbc_first_8192_comments_commit() {
    ldbc_comments(8_192, false, None);
}

#[test]
fn ze444_ldbc_8193_comments_are_refused_at_admission() {
    ldbc_comments(8_193, false, Some(true));
}

#[test]
fn ze444_text_comments_limit() {
    ldbc_comments(4_096, true, Some(true));
}

#[test]
fn ze444_text_comments_boundary() {
    ldbc_comments(2_719, true, None);
    ldbc_comments(2_720, true, Some(false));
}

#[test]
fn ze444_wal_limit_kind() {
    let error = GraphStoreError::from(NativeGraphError::Store(
        crate::lifecycle::StoreError::WalWrite(crate::wal::WalWriteError::GroupTooLarge {
            encoded_bytes: 1_490_877,
            max_group_bytes: 1_048_576,
        }),
    ));
    assert_eq!(error.kind(), GraphStoreErrorKind::Limit);
    assert!(error.nothing_committed());
}

// Refusal's bool says whether admission must avoid all preparation work.
fn ldbc_comments(count: usize, text: bool, refusal: Option<bool>) {
    let widths: Vec<Vec<usize>> = include_str!("ldbc-comment-sf01-widths.txt")
        .lines()
        .filter(|line| !line.starts_with('#'))
        .take(count)
        .map(|line| {
            line.split_whitespace()
                .map(|n| n.parse().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(widths.len(), count);
    let keys: Vec<_> = widths
        .iter()
        .enumerate()
        .map(|(i, w)| format!("{i:0width$}", width = w[0]))
        .collect();
    let strings: Vec<_> = widths
        .iter()
        .map(|w| ["x".repeat(w[1]), "x".repeat(w[2]), "x".repeat(w[3])])
        .collect();
    let mut labels = vec![
        [
            GraphName::new("Comment").unwrap(),
            GraphName::new("Message").unwrap()
        ];
        count
    ];
    let mut properties: Vec<_> = strings
        .iter()
        .enumerate()
        .map(|(i, strings)| {
            [
                ("creationDate", PropertyData::I64(1_311_843_544_909)),
                ("id", PropertyData::I64(i as i64)),
                ("locationIP", PropertyData::String(&strings[0])),
                ("browserUsed", PropertyData::String(&strings[1])),
                ("content", PropertyData::String(&strings[2])),
                ("length", PropertyData::I64(strings[2].len() as i64)),
            ]
            .map(|(name, value)| {
                GraphProperty::new(
                    GraphName::new(name).unwrap(),
                    PropertyValue::new(value).unwrap(),
                )
            })
        })
        .collect();
    let images: Vec<_> = labels
        .iter_mut()
        .zip(&mut properties)
        .map(|(labels, properties)| {
            CanonicalContents::node(labels, properties, None, None).unwrap()
        })
        .collect();
    let writes: Vec<_> = keys
        .iter()
        .zip(&images)
        .map(|(key, image)| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "Comment", key).unwrap(),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(image)),
        })
        .collect();
    let documents: Vec<_> = (0..count)
        .map(|i| GraphNodeDocument {
            item_index: i,
            document: {
                let document = IngestDocument::new(
                    DocumentVersion::new(DocId::new((2u128 << 64) | i as u128), Revision::new(1)),
                    vec![1.0],
                );
                if text {
                    document.with_text(&strings[i][2])
                } else {
                    document
                }
            },
        })
        .collect();
    let bound: Vec<_> = writes
        .iter()
        .copied()
        .enumerate()
        .map(|(i, mut write)| {
            write.operation = StructuredOperation::CreateWithId(
                NodeId::new(documents[i].document.version().doc_id().get()).unwrap(),
            );
            write
        })
        .collect();
    let estimate = super::super::preparation::estimate_preparation_bytes(&bound).unwrap();
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = if text {
        let store = Store::open(&path, options()).unwrap();
        store.enable_graph().unwrap();
        store
    } else {
        Store::create_graph(&path, options(), None).unwrap()
    };
    let before = snapshot(&path);
    capture::start();
    let result = store.graph_apply(
        GraphBatch::with_node_documents(&writes, &documents),
        &control(),
    );
    let report = capture::take();
    let wal_path = path.join("wal.ze");
    let wal_added =
        std::fs::metadata(&wal_path).unwrap().len() - before.get(&wal_path).unwrap().len() as u64;
    println!(
        "ZE-444 comments count={count} estimate={estimate} actual={} wal_added={wal_added} buffer_refusals={:?} result={:?}",
        report.storage_peak_bytes,
        report.buffer_refusals,
        result.as_ref().map(|r| r.outcome())
    );
    assert!(estimate >= report.storage_peak_bytes);
    if let Some(admission) = refusal {
        let error = result.expect_err("batch must refuse");
        assert_eq!(error.kind(), GraphStoreErrorKind::Limit);
        assert!(error.nothing_committed());
        assert!(
            error.to_string().contains(if text {
                "WAL group"
            } else {
                "estimated graph preparation exceeds 32 MiB"
            }),
            "{error}"
        );
        if admission {
            assert!(report.phases.is_empty());
            assert_eq!(report.storage_peak_bytes, 0);
        }
        assert_eq!(snapshot(&path), before);
        return;
    }
    let result = result.expect("LDBC comments commit within 32 MiB");
    assert!(matches!(
        result.outcome(),
        GraphWriteOutcome::Committed { .. }
    ));
    assert_eq!(result.receipts().len(), count);
    assert_eq!(
        node_id(&result, count - 1).get(),
        documents[count - 1].document.version().doc_id().get()
    );
}

#[test]
fn ze453_retained_wal_refuses_before_preparation() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(
        &path,
        options().with_max_resident_bytes(6 * 1024 * 1024),
        None,
    )
    .unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let mut committed = 0;
    for batch in 0..64 {
        let keys: Vec<_> = (0..128)
            .map(|item| format!("{batch:04}-{item:04}-{}", "k".repeat(500)))
            .collect();
        let writes: Vec<_> = keys
            .iter()
            .map(|key| StructuredWrite {
                key: node_key(key),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        let before = store.stats().unwrap();
        let estimate = super::super::preparation::estimate_preparation_bytes(&writes).unwrap();
        capture::start();
        let result = store.graph_apply(&writes, &control());
        let report = capture::take();
        match result {
            Ok(_) => committed += 1,
            Err(error) => {
                println!(
                    "ZE-453 committed={committed} resident={} wal={} temporary={} estimate={estimate} storage_peak={} error={error}",
                    before.resident_owned_bytes,
                    before.wal_bytes,
                    before.temporary_bytes,
                    report.storage_peak_bytes,
                );
                assert!(committed > 0, "the same batch size must fit initially");
                assert!(before.wal_bytes > 128 * 1024, "retain real WAL history");
                assert_eq!(error.kind(), GraphStoreErrorKind::Limit);
                assert!(error.nothing_committed());
                assert!(
                    error.to_string().contains("estimated graph preparation"),
                    "{error}"
                );
                assert_eq!(report.storage_peak_bytes, 0, "refuse before preparation");
                let split = store.graph_apply(&writes[..32], &control()).unwrap();
                assert_eq!(
                    split.receipts().len(),
                    32,
                    "splitting must restore progress"
                );
                store.close().unwrap();
                return;
            }
        }
    }
    panic!("fixture did not exhaust resident headroom");
}

#[test]
fn ze454_write_only_mixed_graph_bounds_retained_wal() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let document = [GraphNodeDocument {
        item_index: 0,
        document: IngestDocument::new(
            DocumentVersion::new(DocId::new(91), Revision::new(1)),
            vec![1.0],
        ),
    }];
    let writes = [StructuredWrite {
        key: node_key("bound"),
        revision: revision(1),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    store
        .graph_apply(
            GraphBatch::with_node_documents(&writes, &document),
            &control(),
        )
        .unwrap();
    let first = store.stats().unwrap().wal_bytes;
    let mut peak = first;
    for batch in 1..=192 {
        let key = format!("{batch}-{}", "k".repeat(128));
        let writes = [StructuredWrite {
            key: node_key(&key),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }];
        store.graph_apply(&writes, &control()).unwrap();
        let retained = store.stats().unwrap().wal_bytes;
        peak = peak.max(retained);
        if batch % 64 == 0 {
            println!("ZE454 batch={batch} retained={retained} peak={peak} first={first}");
            assert!(
                retained <= 32 * 1024,
                "checkpoint must release the absorbed WAL"
            );
        }
        assert!(
            retained <= 2 * 1024 * 1024,
            "write-only retained WAL must stay bounded"
        );
    }
    drop(store);
    let reopened = Store::open(&path, options()).unwrap();
    assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
}

#[test]
fn ze454_graph_only_folds_cannot_suppress_document_wal_checkpoint() {
    use crate::lifecycle::native_graph::WAL_CHECKPOINT_LIMIT;
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    WAL_CHECKPOINT_LIMIT.with(|limit| limit.set(32 * 1024));
    let mut peak = 0;
    for batch in 1..=96 {
        let key = format!("{batch}-{}", "k".repeat(128));
        let documents = [GraphNodeDocument {
            item_index: 0,
            document: IngestDocument::new(
                DocumentVersion::new(DocId::new(20_000 + batch), Revision::new(1)),
                vec![1.0],
            ),
        }];
        let writes = [StructuredWrite {
            key: node_key(&key),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }];
        store
            .graph_apply(
                GraphBatch::with_node_documents(&writes, &documents),
                &control(),
            )
            .unwrap();
        // Inventory folds and explicit graph checkpoints reset graph counters
        // without consuming active document rows. They cannot suppress the
        // physical WAL threshold on a subsequent ordinary write.
        store.checkpoint_native_graph(&control()).unwrap();
        let retained = store.stats().unwrap().wal_bytes;
        peak = peak.max(retained);
        assert!(retained <= 64 * 1024, "batch={batch} retained={retained}");
    }
    WAL_CHECKPOINT_LIMIT.with(|limit| limit.set(16 * 1024 * 1024));
    println!("ZE454 repeated graph folds batches=96 retained_peak={peak}");
    drop(store);
    let reopened = Store::open(&path, options()).unwrap();
    assert_eq!(reopened.count_documents(None, None).unwrap().count, 96);
}

#[test]
fn ze454_automatic_checkpoint_without_wal_progress_refuses() {
    use crate::ingest::DeleteBatch;
    use crate::lifecycle::native_graph::WAL_CHECKPOINT_LIMIT;
    let parent = tempfile::tempdir().unwrap();
    let store = Store::create_graph(parent.path().join("graph"), options(), None).unwrap();
    create_node(&store, "before-delete", 1);
    // A document-only record advances durable_end beyond the last graph
    // record, while leaving the active segment empty. Neither checkpoint
    // watermark pair can release this WAL.
    store
        .delete(DeleteBatch::new(vec![DocId::new(999)]))
        .unwrap();
    assert!(
        store
            .active
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .segment
            .is_empty()
    );
    let before = store.stats().unwrap().wal_bytes;
    assert!(before > 0);
    let result = WAL_CHECKPOINT_LIMIT.with(|limit| {
        let previous = limit.replace(before);
        let result = store.checkpoint_native_graph_on_open();
        limit.set(previous);
        result
    });
    // Exercise one automatic checkpoint first: RED fails here without ever
    // entering the unbounded mutation retry loop.
    assert!(
        matches!(result, Err(NativeGraphError::WalTailBoundExceeded)),
        "{result:?}"
    );
    assert_eq!(store.stats().unwrap().wal_bytes, before);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let writes = [StructuredWrite {
        key: node_key("after-delete"),
        revision: revision(1),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    let error = WAL_CHECKPOINT_LIMIT
        .with(|limit| {
            let previous = limit.replace(before);
            let result = store.graph_apply(&writes, &control());
            limit.set(previous);
            result
        })
        .unwrap_err();
    assert_eq!(error.kind(), GraphStoreErrorKind::Limit);
    assert!(error.nothing_committed());
    assert_eq!(store.stats().unwrap().wal_bytes, before);
}

#[test]
fn ze455_recovery_limit_is_not_corruption() {
    use crate::lifecycle::native_graph::recovery::RECOVERY_WORK_LIMIT;
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    create_node(&store, "recover", 1);
    drop(store);
    RECOVERY_WORK_LIMIT.with(|limit| limit.set(Some(1)));
    let error = Store::open(&path, options())
        .err()
        .expect("tight recovery work must refuse");
    RECOVERY_WORK_LIMIT.with(|limit| limit.set(None));
    println!("ZE455 kind={:?} error={error}", error.kind());
    assert_eq!(error.kind(), StoreErrorKind::BudgetExceeded);
    let reopened =
        Store::open(&path, options()).expect("valid store reopens with its required work");
    assert!(reopened.admit_native_read().is_ok());
}

#[test]
fn ze454_reopen_releases_legacy_pinned_wal() {
    use crate::lifecycle::native_graph::WAL_CHECKPOINT_LIMIT;
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let documents = [GraphNodeDocument {
        item_index: 0,
        document: IngestDocument::new(
            DocumentVersion::new(DocId::new(91), Revision::new(1)),
            vec![1.0],
        ),
    }];
    let writes = [StructuredWrite {
        key: node_key("bound"),
        revision: revision(1),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    store
        .graph_apply(
            GraphBatch::with_node_documents(&writes, &documents),
            &control(),
        )
        .unwrap();
    // Explicit folds reproduce old writers: graph watermarks advance while
    // active document rows keep all WAL bytes pinned.
    for batch in 0..32 {
        create_node(&store, &format!("legacy-{batch}"), 1);
        store.checkpoint_native_graph(&control()).unwrap();
    }
    let before = store.stats().unwrap().wal_bytes;
    assert!(before > 32 * 1024);
    drop(store);
    WAL_CHECKPOINT_LIMIT.with(|limit| limit.set(32 * 1024));
    let reopened = Store::open(&path, options());
    WAL_CHECKPOINT_LIMIT
        .with(|limit| limit.set(crate::property_graph::wal::MAX_ENVELOPE_BYTES as u64));
    let reopened = reopened.unwrap();
    let after = reopened.stats().unwrap().wal_bytes;
    println!("ZE454 legacy retained before={before} after={after}");
    assert_eq!(
        after, 0,
        "writable reopen must discharge an oversized absorbed log"
    );
    assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
}

#[test]
fn ze455_large_directory_reopens_with_bounded_validation_work() {
    use crate::lifecycle::native_graph::recovery::RECOVERY_WORK_LIMIT;
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let mut nodes = Vec::new();
    for first in (0..512).step_by(128) {
        let keys: Vec<_> = (first..first + 128).map(|n| n.to_string()).collect();
        let writes: Vec<_> = keys
            .iter()
            .map(|key| StructuredWrite {
                key: node_key(key),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        let result = store.graph_apply(&writes, &control()).unwrap();
        nodes.extend((0..128).map(|i| node_id(&result, i)));
    }
    let keys: Vec<_> = (0..512).map(|n| format!("rel-{n}")).collect();
    let writes: Vec<_> = keys
        .iter()
        .enumerate()
        .map(|(i, key)| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", key).unwrap(),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Existing(nodes[i]),
                target: NodeRef::Existing(nodes[(i + 1) % 512]),
                relationship_type: GraphName::new("EDGE").unwrap(),
                properties: &[],
            }),
        })
        .collect();
    for chunk in writes.chunks(128) {
        store.graph_apply(chunk, &control()).unwrap();
    }
    store.checkpoint_native_graph(&control()).unwrap();
    drop(store);
    RECOVERY_WORK_LIMIT.with(|limit| limit.set(Some(25_165_824)));
    capture::start();
    let reopened = Store::open(&path, options());
    let report = capture::take();
    println!(
        "ZE455 recovery directory_work={} rejected={:?}",
        report.directory_work, report.rejected
    );
    RECOVERY_WORK_LIMIT.with(|limit| limit.set(None));
    let reopened = reopened
        .expect("engine-written relationship directory must reopen within its validation work");
    let (work, _, candidates, _) = crate::lifecycle::native_graph::open_metrics_for_test();
    println!(
        "ZE455 directory nodes=512 relationships=512 work={work} fence_candidates={candidates}"
    );
    assert!(work <= 25_165_824);
    assert_eq!(candidates, 1024);
    for i in [0, 511] {
        let result = reopened.graph_apply(&writes[i..i + 1], &control()).unwrap();
        assert_eq!(result.outcome(), GraphWriteOutcome::Replayed);
    }
}

#[test]
fn ze455_tail_mutations_reopen_with_bounded_key_matching() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let keys: Vec<_> = (0..2048).map(|n| format!("{n:04}")).collect();
    let mut nodes = Vec::new();
    for chunk in keys.chunks(128) {
        let writes: Vec<_> = chunk
            .iter()
            .map(|key| StructuredWrite {
                key: node_key(key),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        let result = store.graph_apply(&writes, &control()).unwrap();
        nodes.extend((0..chunk.len()).map(|i| node_id(&result, i)));
    }
    store.checkpoint_native_graph(&control()).unwrap();
    let writes: Vec<_> = keys
        .iter()
        .zip(&nodes)
        .map(|(key, node)| StructuredWrite {
            key: node_key(key),
            revision: revision(2),
            operation: StructuredOperation::Put(EntityId::Node(*node)),
            image: Some(WriteImage::Node(&image)),
        })
        .collect();
    assert!(matches!(
        store.graph_apply(&writes, &control()).unwrap().outcome(),
        GraphWriteOutcome::Committed { .. }
    ));
    drop(store);
    capture::start();
    let reopened = Store::open(&path, options());
    let report = capture::take();
    println!(
        "ZE455 tail directory_work={} rejected={:?}",
        report.directory_work, report.rejected
    );
    let reopened = reopened.expect("valid tail must reopen with bounded mutation-key matching");
    let (work, _, _, _) = crate::lifecycle::native_graph::open_metrics_for_test();
    println!("ZE455 tail nodes=2048 mutations=2048 work={work}");
    assert!(
        report.directory_work <= 100_000_000,
        "tail matching must not scan every mutation for every stored key"
    );
    for i in [0, 2047] {
        assert_eq!(
            reopened
                .graph_apply(&writes[i..i + 1], &control())
                .unwrap()
                .outcome(),
            GraphWriteOutcome::Replayed
        );
    }
}

#[test]
fn ze283_checkpoint_open_work_scales_with_relationships() {
    let mut measurements = Vec::new();
    for count in [32, 64] {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("graph");
        let store = Store::create_graph(&path, options(), None).unwrap();
        let source = node_id(&create_node(&store, "source", 1), 0);
        let target = node_id(&create_node(&store, "target", 1), 0);
        // Fixed-size commits make artifact count grow with relationship count.
        // Capture only open, excluding per-commit marking and census work.
        for index in 0..count {
            let key = format!("rel-{index:04}");
            store
                .graph_apply(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "app", &key).unwrap(),
                        revision: revision(1),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Existing(source),
                            target: NodeRef::Existing(target),
                            relationship_type: GraphName::new("EDGE").unwrap(),
                            properties: &[],
                        }),
                    }],
                    &control(),
                )
                .unwrap();
        }
        store.checkpoint_native_graph(&control()).unwrap();
        drop(store);
        capture::start();
        let start = std::time::Instant::now();
        let reopened = Store::open(&path, options()).unwrap();
        let elapsed = start.elapsed();
        let report = capture::take();
        let (_, work) = report
            .phases
            .iter()
            .find(|(phase, _)| *phase == "checkpoint_descriptor_comparisons")
            .unwrap();
        let (open_work, _, _, _) = crate::lifecycle::native_graph::open_metrics_for_test();
        println!(
            "ZE283 relationships={count} checkpoint_descriptor_comparisons={work} open_work={open_work} open_ms={:.3}",
            elapsed.as_secs_f64() * 1000.0
        );
        assert!(*work > 0);
        measurements.push(*work);
        drop(reopened);
    }
    println!(
        "ZE283 checkpoint inventory doubling ratio={:.6}",
        measurements[1] as f64 / measurements[0] as f64
    );
    assert!(
        measurements[1] * 10 <= measurements[0] * 25,
        "checkpoint open inventory work must grow at most 2.5x for 2x relationships: {measurements:?}"
    );
}
