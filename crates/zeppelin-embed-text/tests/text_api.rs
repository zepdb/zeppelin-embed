#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed_text::{
    ChunkPolicy, IngestOptions, Legs, QueryOptions, TextDocument, TextStore,
};

mod common;

#[test]
fn ingest_text_then_query_text_returns_the_ingested_text_with_the_declared_epoch() {
    let directory = tempdir().expect("store directory");
    let bundle_path = directory.path().join("fixture.zem");
    let declared_epoch = common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        &bundle_path,
        Default::default(),
    )
    .expect("open text store");

    store
        .ingest_text(
            &[TextDocument::new(7, 1, "the bronze zeppelin")],
            IngestOptions::default(),
        )
        .expect("ingest text");
    let hits = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(1).with_legs(Legs::Dense),
        )
        .expect("query text");

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc_id, 7);
    assert_eq!(hits[0].revision, 1);
    assert_eq!(hits[0].chunk, 0);
    assert_eq!(hits[0].text, "the bronze zeppelin");
    assert_eq!(hits[0].epoch, declared_epoch);
    for legs in [Legs::Lexical, Legs::Hybrid] {
        let hits = store
            .query_text("bronze zeppelin", QueryOptions::new(1).with_legs(legs))
            .expect("query text leg");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, 7);
        assert_eq!(hits[0].text, "the bronze zeppelin");
        assert_eq!(hits[0].epoch, declared_epoch);
    }
}

#[test]
fn deleting_a_caller_id_deletes_every_chunk_row() {
    let directory = tempdir().expect("store directory");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        bundle_path,
        Default::default(),
    )
    .expect("open text store");
    store
        .ingest_text(
            &[TextDocument::new(41, 1, "bronze zeppelin bronze zeppelin")],
            IngestOptions {
                embed_batch_size: 1,
                seal_every: 8,
                chunk_policy: ChunkPolicy::Tokens { max: 2, overlap: 0 },
                ..Default::default()
            },
        )
        .expect("ingest chunks");
    let before = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(8).with_legs(Legs::Dense),
        )
        .expect("query before delete");
    assert_eq!(before.len(), 2);

    store.delete_text(&[41]).expect("delete every chunk");

    let after = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(8).with_legs(Legs::Dense),
        )
        .expect("query after delete");
    assert!(after.is_empty());
}
