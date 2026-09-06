#![allow(clippy::expect_used, clippy::indexing_slicing)]

use tempfile::tempdir;

use crate::ingest::{DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::meta::{ColumnDefinition, ColumnId, ColumnType, Predicate, PredicateValue, Schema};
use crate::segment::reader::SegmentCostAudit;

use super::{
    CancelToken, DocumentFields, DocumentScanRequest, OpenOptions, QueryControl, QueryError,
    ScanOrder, Store, StoreError,
};

fn document(id: u128, timestamp: i64) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(1)),
        vec![id as f32],
    )
    .with_timestamp(timestamp)
}

fn ids(page: &super::DocumentScanPage) -> Vec<DocId> {
    page.documents
        .iter()
        .map(|document| document.doc_id)
        .collect()
}

fn request(limit: usize) -> DocumentScanRequest<'static> {
    DocumentScanRequest::new(
        limit,
        DocumentFields::NONE,
        QueryControl::Cancel(CancelToken::new()),
    )
}

#[test]
fn scan_documents_timestamp_orders_use_doc_id_as_the_tie_breaker() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(vec![document(3, 20), document(2, 10)]))
        .expect("sealed ingest");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![document(1, 20), document(4, 30)]))
        .expect("active ingest");

    let ascending = store
        .scan_documents(request(10).with_order(ScanOrder::TimestampAscending))
        .expect("ascending");
    let descending = store
        .scan_documents(request(10).with_order(ScanOrder::TimestampDescending))
        .expect("descending");

    assert_eq!(
        ids(&ascending),
        vec![DocId::new(2), DocId::new(1), DocId::new(3), DocId::new(4)]
    );
    assert_eq!(
        ids(&descending),
        vec![DocId::new(4), DocId::new(1), DocId::new(3), DocId::new(2)]
    );
}

#[test]
fn scan_documents_timestamp_range_prunes_a_segment_and_filters_live_rows() {
    let directory = tempdir().expect("directory");
    let category = ColumnId::new(1);
    let schema = Schema::new(vec![ColumnDefinition::new(
        category,
        "category",
        ColumnType::U64,
        true,
    )])
    .expect("schema");
    let store =
        Store::open(directory.path(), OpenOptions::default().with_schema(schema)).expect("open");
    let with_category = |id: u128, timestamp: i64, value: u64| {
        document(id, timestamp).with_columns(vec![(category, PredicateValue::U64(value))])
    };
    store
        .ingest(IngestBatch::new(vec![with_category(9, 1, 7)]))
        .expect("pruned ingest");
    store.seal().expect("first seal");
    store
        .ingest(IngestBatch::new(vec![
            with_category(1, 10, 7),
            document(2, 11),
        ]))
        .expect("matching ingest");
    store.seal().expect("second seal");
    store
        .ingest(IngestBatch::new(vec![
            with_category(3, 12, 7),
            with_category(4, 13, 8),
        ]))
        .expect("active ingest");
    store
        .delete(DeleteBatch::new(vec![DocId::new(3)]))
        .expect("tombstone");
    let predicate = Predicate::Eq {
        column: category,
        value: PredicateValue::U64(7),
    };

    let snapshot = store.snapshot().expect("snapshot");
    let matching_segment = snapshot
        .segments()
        .iter()
        .find(|segment| {
            segment.meta().clustering_key_range
                == crate::segment::ClusteringKeyRange::Bounded {
                    min_ts: 10,
                    max_ts: 11,
                }
        })
        .expect("matching segment");
    let expected = SegmentCostAudit::new();
    expected.measure(|| {
        matching_segment.alive().expect("alive");
        matching_segment.columns().expect("columns");
    });
    drop(snapshot);
    let actual = SegmentCostAudit::new();
    let page = actual.measure(|| {
        store
            .scan_documents(
                request(10)
                    .with_timestamp_range(10, 13)
                    .with_predicate(&predicate),
            )
            .expect("filtered range")
    });

    assert_eq!(ids(&page), vec![DocId::new(1)]);
    assert_eq!(
        actual.snapshot().alive_decode_bytes,
        expected.snapshot().alive_decode_bytes
    );
    assert_eq!(
        actual.snapshot().columns_decode_bytes,
        expected.snapshot().columns_decode_bytes
    );
}

#[test]
fn scan_documents_pins_negative_null_and_empty_boolean_semantics() {
    let directory = tempdir().expect("directory");
    let category = ColumnId::new(1);
    let schema = Schema::new(vec![ColumnDefinition::new(
        category,
        "category",
        ColumnType::U64,
        true,
    )])
    .expect("schema");
    let store =
        Store::open(directory.path(), OpenOptions::default().with_schema(schema)).expect("open");
    store
        .ingest(IngestBatch::new(vec![
            document(1, 1).with_columns(vec![(category, PredicateValue::U64(7))]),
            document(2, 2),
        ]))
        .expect("ingest");
    let eq = Predicate::Eq {
        column: category,
        value: PredicateValue::U64(7),
    };
    let not_eq = Predicate::Not(Box::new(eq.clone()));

    assert_eq!(
        ids(&store
            .scan_documents(request(10).with_predicate(&eq))
            .expect("eq")),
        vec![DocId::new(1)]
    );
    assert_eq!(
        ids(&store
            .scan_documents(request(10).with_predicate(&not_eq))
            .expect("not eq")),
        vec![DocId::new(2)]
    );
    assert_eq!(
        ids(&store
            .scan_documents(request(10).with_predicate(&Predicate::And(Vec::new())))
            .expect("empty and")),
        vec![DocId::new(1), DocId::new(2)]
    );
    assert!(
        store
            .scan_documents(request(10).with_predicate(&Predicate::Or(Vec::new())))
            .expect("empty or")
            .documents
            .is_empty()
    );
}

#[test]
fn scan_documents_rejects_a_stale_continuation() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(vec![document(1, 1), document(2, 2)]))
        .expect("ingest");
    let first = store.scan_documents(request(1)).expect("first page");
    let cursor = first.continuation.expect("cursor");
    store
        .ingest(IngestBatch::new(vec![document(3, 3)]))
        .expect("write");
    store.seal().expect("seal");

    assert!(matches!(
        store.scan_documents(request(1).with_cursor(cursor)),
        Err(QueryError::Store(StoreError::ScanStale { .. }))
    ));
}

#[test]
fn scan_documents_storage_order_crosses_sealed_and_active_and_pages_every_live_row_once() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(vec![document(1, 30), document(2, 10)]))
        .expect("sealed ingest");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![document(3, 20), document(4, 40)]))
        .expect("active ingest");

    let first = store.scan_documents(request(2)).expect("first page");
    assert_eq!(
        first
            .documents
            .iter()
            .map(|document| document.doc_id)
            .collect::<Vec<_>>(),
        vec![DocId::new(1), DocId::new(2)]
    );
    let second = store
        .scan_documents(request(2).with_cursor(first.continuation.expect("cursor")))
        .expect("second page");
    assert_eq!(
        second
            .documents
            .iter()
            .map(|document| document.doc_id)
            .collect::<Vec<_>>(),
        vec![DocId::new(3), DocId::new(4)]
    );
    assert_eq!(second.continuation, None);

    for limit in [1, 2] {
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let mut scan = request(limit).with_order(ScanOrder::Storage);
            if let Some(next) = cursor {
                scan = scan.with_cursor(next);
            }
            let page = store.scan_documents(scan).expect("page");
            ids.extend(page.documents.into_iter().map(|document| document.doc_id));
            cursor = page.continuation;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            ids,
            vec![DocId::new(1), DocId::new(2), DocId::new(3), DocId::new(4)]
        );
    }
}
