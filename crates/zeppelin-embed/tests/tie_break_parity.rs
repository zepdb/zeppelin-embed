#![allow(clippy::expect_used)]

use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::meta::{Predicate, PredicateValue, TIMESTAMP_COLUMN};

const QUERY: [f32; 2] = [1.0, 0.0];

#[test]
fn exact_top_k_set_is_independent_of_ingestion_order_for_tied_scores() {
    let (_first_directory, first) = tied_store([10, 7, 5]);
    let (_second_directory, second) = tied_store([10, 5, 7]);

    assert_eq!(exact_documents(&first), [DocId::new(10), DocId::new(5)]);
    assert_eq!(exact_documents(&second), [DocId::new(10), DocId::new(5)]);
}

#[test]
fn filtered_exact_top_k_set_is_independent_of_ingestion_order_for_tied_scores() {
    let (_first_directory, first) = tied_store([10, 7, 5]);
    let (_second_directory, second) = tied_store([10, 5, 7]);

    assert_eq!(filtered_documents(&first), [DocId::new(10), DocId::new(5)]);
    assert_eq!(filtered_documents(&second), [DocId::new(10), DocId::new(5)]);
}

fn tied_store(order: [u128; 3]) -> (TempDir, Store) {
    let directory = tempdir().expect("tie-order store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open tie store");
    let documents = order
        .into_iter()
        .map(|document| {
            let vector = if document == 10 {
                QUERY.to_vec()
            } else {
                vec![0.0, 1.0]
            };
            IngestDocument::new(
                DocumentVersion::new(DocId::new(document), Revision::new(1)),
                vector,
            )
            .with_timestamp(1)
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest tied rows");
    store.seal().expect("seal tied rows");
    (directory, store)
}

fn filtered_documents(store: &Store) -> Vec<DocId> {
    store
        .search_filtered(
            SearchRequest::new(&QUERY),
            &Predicate::Eq {
                column: TIMESTAMP_COLUMN,
                value: PredicateValue::I64(1),
            },
            2,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filtered exact tied search")
        .candidates
        .into_iter()
        .filter_map(|candidate| candidate.document().map(DocumentVersion::doc_id))
        .collect()
}

fn exact_documents(store: &Store) -> Vec<DocId> {
    store
        .search(
            SearchRequest::new(&QUERY),
            2,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("exact tied search")
        .candidates
        .into_iter()
        .filter_map(|candidate| candidate.document().map(DocumentVersion::doc_id))
        .collect()
}
