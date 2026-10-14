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
