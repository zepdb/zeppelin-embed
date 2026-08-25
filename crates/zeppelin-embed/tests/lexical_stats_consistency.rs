#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed::fts::bm25::CorpusStats;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};

const REMOVED: DocId = DocId::new(1);

fn text_corpus() -> Vec<IngestDocument> {
    [
        (
            1_u128,
            "removed padding padding padding padding padding padding padding",
        ),
        (2, "common short"),
        (3, "common medium filler"),
        (4, "other filler"),
    ]
    .into_iter()
    .map(|(id, text)| {
        IngestDocument::new(
            DocumentVersion::new(DocId::new(id), Revision::new(1)),
            vec![id as f32, 1.0],
        )
        .with_text(text)
    })
    .collect()
}

fn sealed_text_store(path: &std::path::Path) -> Store {
    let store = Store::open(path, OpenOptions::default()).expect("open text store");
    store
        .ingest(IngestBatch::new(text_corpus()))
        .expect("ingest text corpus");
    store.seal().expect("seal text corpus");
    store
}

fn query_scores(store: &Store) -> Vec<(DocId, f64)> {
    store
        .search_lexical(
            &TermQuery::flat(vec![b"common".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search fixed lexical query")
        .candidates
        .into_iter()
        .map(|candidate| (candidate.document.doc_id(), candidate.score))
        .collect()
}

fn stats_tuple(stats: CorpusStats) -> (u64, u64) {
    (stats.document_count(), stats.total_tokens())
}

#[test]
fn delete_and_purge_of_the_same_rows_yield_identical_bm25_stats() {
    let deleted_directory = tempdir().expect("delete store directory");
    let purged_directory = tempdir().expect("purge store directory");
    let deleted = sealed_text_store(deleted_directory.path());
    let purged = sealed_text_store(purged_directory.path());

    deleted
        .delete(DeleteBatch::new(vec![REMOVED]))
        .expect("delete row");
    let token = purged.purge(&[REMOVED]).expect("schedule purge");
    purged.await_physical_purge(token).expect("complete purge");

    let deleted_stats = stats_tuple(
        deleted
            .lexical_corpus_stats()
            .expect("deleted corpus statistics"),
    );
    let purged_stats = stats_tuple(
        purged
            .lexical_corpus_stats()
            .expect("purged corpus statistics"),
    );
    assert_eq!(
        deleted_stats, purged_stats,
        "delete stats {deleted_stats:?}; purge stats {purged_stats:?}"
    );

    let deleted_scores = query_scores(&deleted);
    let purged_scores = query_scores(&purged);
    assert_eq!(
        deleted_scores, purged_scores,
        "delete scores {deleted_scores:?}; purge scores {purged_scores:?}"
    );
}

#[test]
fn purging_already_deleted_rows_is_a_no_op_on_bm25_stats() {
    let directory = tempdir().expect("store directory");
    let store = sealed_text_store(directory.path());
    store
        .delete(DeleteBatch::new(vec![REMOVED]))
        .expect("delete row");
    let stats_after_delete = store
        .lexical_corpus_stats()
        .expect("statistics after delete");
    let scores_after_delete = query_scores(&store);

    let token = store
        .purge(&[REMOVED])
        .expect("schedule already-deleted purge");
    store
        .await_physical_purge(token)
        .expect("complete already-deleted purge");

    assert_eq!(
        stats_tuple(
            store
                .lexical_corpus_stats()
                .expect("statistics after purge"),
        ),
        stats_tuple(stats_after_delete),
        "purging a tombstoned row changed N or total tokens"
    );
    assert_eq!(
        query_scores(&store),
        scores_after_delete,
        "purging a tombstoned row changed BM25 scores"
    );
}
