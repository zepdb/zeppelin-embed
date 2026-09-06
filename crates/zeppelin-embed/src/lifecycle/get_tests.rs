#![allow(clippy::expect_used, clippy::indexing_slicing)]

use tempfile::tempdir;

use crate::ingest::{DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::meta::{ColumnDefinition, ColumnId, ColumnType, PredicateValue, Schema};

use super::{DocumentFields, OpenOptions, Store};

#[test]
fn get_documents_returns_active_document() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let id = DocId::new(7);
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(DocumentVersion::new(id, Revision::new(3)), vec![1.0, 2.0])
                .with_timestamp(41)
                .with_text("active text")
                .with_metadata(vec![4, 5, 6]),
        ]))
        .expect("ingest");

    let documents = store
        .get_documents(&[id], DocumentFields::ALL)
        .expect("get");

    assert_eq!(documents.len(), 1);
    let document = documents[0].as_ref().expect("document");
    assert_eq!(document.doc_id, id);
    assert_eq!(document.revision, Revision::new(3));
    assert_eq!(document.timestamp, 41);
    assert_eq!(document.vector.as_deref(), Some([1.0, 2.0].as_slice()));
    assert_eq!(document.text.as_deref(), Some("active text"));
    assert_eq!(document.metadata.as_deref(), Some([4, 5, 6].as_slice()));
    assert_eq!(document.attributes.as_deref(), Some([].as_slice()));
}

#[test]
fn get_documents_returns_sealed_document() {
    let directory = tempdir().expect("directory");
    let attribute = ColumnId::new(9);
    let schema = Schema::new(vec![ColumnDefinition::new(
        attribute,
        "category",
        ColumnType::RawString,
        false,
    )])
    .expect("schema");
    let store =
        Store::open(directory.path(), OpenOptions::default().with_schema(schema)).expect("open");
    let id = DocId::new(11);
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(DocumentVersion::new(id, Revision::new(4)), vec![3.0, 5.0])
                .with_timestamp(73)
                .with_text("sealed text")
                .with_metadata(vec![8, 9])
                .with_columns(vec![(attribute, PredicateValue::String("blue".to_owned()))]),
        ]))
        .expect("ingest");
    store.seal().expect("seal");

    let documents = store
        .get_documents(&[id], DocumentFields::ALL)
        .expect("get");

    let document = documents[0].as_ref().expect("document");
    assert_eq!(document.doc_id, id);
    assert_eq!(document.revision, Revision::new(4));
    assert_eq!(document.timestamp, 73);
    assert_eq!(document.vector.as_deref(), Some([3.0, 5.0].as_slice()));
    assert_eq!(document.text.as_deref(), Some("sealed text"));
    assert_eq!(document.metadata.as_deref(), Some([8, 9].as_slice()));
    assert_eq!(
        document.attributes.as_deref(),
        Some([(attribute, PredicateValue::String("blue".to_owned()))].as_slice())
    );
}

#[test]
fn get_documents_reads_mixed_active_and_sealed_documents() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let sealed_id = DocId::new(1);
    let active_id = DocId::new(2);
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(sealed_id, Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("sealed ingest");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(active_id, Revision::new(1)),
            vec![0.0, 1.0],
        )]))
        .expect("active ingest");

    let documents = store
        .get_documents(&[active_id, sealed_id], DocumentFields::VECTOR)
        .expect("get");

    assert_eq!(
        documents[0].as_ref().map(|document| document.doc_id),
        Some(active_id)
    );
    assert_eq!(
        documents[1].as_ref().map(|document| document.doc_id),
        Some(sealed_id)
    );
}

#[test]
fn get_documents_returns_none_for_active_and_sealed_tombstones() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let sealed_id = DocId::new(1);
    let active_id = DocId::new(2);
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(sealed_id, Revision::new(1)),
            vec![1.0],
        )]))
        .expect("sealed ingest");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(active_id, Revision::new(1)),
            vec![2.0],
        )]))
        .expect("active ingest");
    store
        .delete(DeleteBatch::new(vec![sealed_id, active_id]))
        .expect("delete");

    let documents = store
        .get_documents(&[sealed_id, active_id], DocumentFields::ALL)
        .expect("get");

    assert_eq!(documents, vec![None, None]);
}

#[test]
fn get_documents_returns_none_for_unknown_id() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");

    assert_eq!(
        store
            .get_documents(&[DocId::new(404)], DocumentFields::ALL)
            .expect("get"),
        vec![None]
    );
}

#[test]
fn get_documents_returns_only_live_superseding_revision() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let id = DocId::new(5);
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(id, Revision::new(1)),
            vec![1.0],
        )]))
        .expect("first ingest");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(id, Revision::new(2)),
            vec![2.0],
        )]))
        .expect("second ingest");

    let documents = store
        .get_documents(&[id], DocumentFields::VECTOR)
        .expect("get");
    let document = documents[0].as_ref().expect("document");

    assert_eq!(document.revision, Revision::new(2));
    assert_eq!(document.vector.as_deref(), Some([2.0].as_slice()));
}

#[test]
fn get_documents_preserves_request_order_with_duplicates_and_misses() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let first = DocId::new(10);
    let second = DocId::new(20);
    let missing = DocId::new(30);
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(DocumentVersion::new(first, Revision::new(1)), vec![1.0]),
            IngestDocument::new(DocumentVersion::new(second, Revision::new(1)), vec![2.0]),
        ]))
        .expect("ingest");

    let documents = store
        .get_documents(
            &[second, missing, first, second, missing],
            DocumentFields::NONE,
        )
        .expect("get");

    assert_eq!(
        documents
            .iter()
            .map(|document| document.as_ref().map(|document| document.doc_id))
            .collect::<Vec<_>>(),
        vec![Some(second), None, Some(first), Some(second), None]
    );
}

#[test]
fn get_documents_selects_exactly_requested_fields() {
    let directory = tempdir().expect("directory");
    let attribute = ColumnId::new(8);
    let schema = Schema::new(vec![ColumnDefinition::new(
        attribute,
        "rank",
        ColumnType::U64,
        false,
    )])
    .expect("schema");
    let store =
        Store::open(directory.path(), OpenOptions::default().with_schema(schema)).expect("open");
    let id = DocId::new(8);
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(DocumentVersion::new(id, Revision::new(2)), vec![1.0, 2.0])
                .with_timestamp(9)
                .with_text("fields")
                .with_metadata(vec![3, 4])
                .with_columns(vec![(attribute, PredicateValue::U64(12))]),
        ]))
        .expect("ingest");

    let cases = [
        (DocumentFields::NONE, [false, false, false, false]),
        (DocumentFields::VECTOR, [true, false, false, false]),
        (DocumentFields::TEXT, [false, true, false, false]),
        (DocumentFields::METADATA, [false, false, true, false]),
        (DocumentFields::ATTRIBUTES, [false, false, false, true]),
        (
            DocumentFields::VECTOR | DocumentFields::METADATA,
            [true, false, true, false],
        ),
        (DocumentFields::ALL, [true, true, true, true]),
    ];
    for (fields, expected) in cases {
        let documents = store.get_documents(&[id], fields).expect("get");
        let document = documents[0].as_ref().expect("document");
        assert_eq!(
            [
                document.vector.is_some(),
                document.text.is_some(),
                document.metadata.is_some(),
                document.attributes.is_some(),
            ],
            expected
        );
        assert_eq!(document.doc_id, id);
        assert_eq!(document.revision, Revision::new(2));
        assert_eq!(document.timestamp, 9);
    }
}

#[test]
fn get_documents_reports_absent_sealed_text_and_metadata_regions() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let id = DocId::new(6);
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(id, Revision::new(1)),
            vec![1.0],
        )]))
        .expect("ingest");
    store.seal().expect("seal");

    let documents = store
        .get_documents(&[id], DocumentFields::TEXT | DocumentFields::METADATA)
        .expect("get");
    let document = documents[0].as_ref().expect("document");

    assert_eq!(document.text, None);
    assert_eq!(document.metadata, None);
}
