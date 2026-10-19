#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed::epoch::{EmbeddingEpoch, StoreEpoch};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::{
    ChunkPolicy, IngestOptions, Legs, QueryOptions, TextDocument, TextOpenOptions, TextStore,
};

mod common;

#[test]
fn same_revision_retry_keeps_live_chunks() {
    for serialized in [false, true] {
        let directory = tempdir().expect("store directory");
        let bundle = directory.path().join("fixture.zem");
        common::write_symmetric_fixture_bundle(&bundle);
        let path = directory.path().join("store");
        let store = TextStore::open(&path, &bundle, Default::default()).expect("open store");
        let document = TextDocument::new(41, 1, "the bronze zeppelin zeppelin zeppelin zeppelin");
        let options = IngestOptions {
            embed_batch_size: 1,
            seal_every: 1,
            chunk_policy: ChunkPolicy::Tokens { max: 2, overlap: 0 },
            ..Default::default()
        };
        let ingest = |documents: &[TextDocument], options| {
            if serialized {
                store.ingest_text_serialized(documents, options)
            } else {
                store.ingest_text(documents, options)
            }
            .expect("ingest same revision")
        };
        let assert_live_chunks = |store: &TextStore| {
            let mut rows = store
                .query_text("zeppelin", QueryOptions::new(8).with_legs(Legs::Lexical))
                .expect("query live revision's later chunks")
                .into_iter()
                .map(|hit| (hit.doc_id, hit.revision, hit.chunk))
                .collect::<Vec<_>>();
            rows.sort_unstable();
            assert_eq!(rows, [(41, 1, 1), (41, 1, 2)]);
        };
        ingest(
            &[
                document.clone(),
                TextDocument::new(42, 1, "unrelated filler"),
            ],
            options,
        );
        assert_live_chunks(&store);
        ingest(
            &[document],
            IngestOptions {
                chunk_policy: ChunkPolicy::Tokens {
                    max: 64,
                    overlap: 0,
                },
                ..options
            },
        );
        assert_live_chunks(&store);
        store.close().expect("close store");
        drop(store);
        let reopened = TextStore::open(&path, &bundle, Default::default()).expect("reopen store");
        assert_live_chunks(&reopened);
    }
}

#[test]
fn open_removes_stale_chunks_after_interrupted_revision_cleanup() {
    for seal_new_revision in [false, true] {
        let directory = tempdir().expect("store directory");
        let bundle_path = directory.path().join("fixture.zem");
        common::write_symmetric_fixture_bundle(&bundle_path);
        let bundle = Bundle::open(&bundle_path).expect("bundle");
        let mut tower = bundle.document_tower().embedding.clone();
        tower
            .model_version
            .push_str(";ze-text-output-layout=2;ze-text-wordpiece=2");
        let tokenizer = TokenizerConfig::text_default();
        let epoch = StoreEpoch {
            embedding: EmbeddingEpoch {
                document: tower.clone(),
                query: tower,
                alignment_digest: Vec::new(),
            },
            tokenizer: tokenizer.epoch(),
        };
        let identity = epoch.identity();
        let path = directory.path().join("store");
        let core = Store::open(
            &path,
            OpenOptions::default()
                .with_epoch(epoch)
                .with_tokenizer(tokenizer),
        )
        .expect("open core store");
        let chunk = |number, revision, text: &str| {
            IngestDocument::new(
                DocumentVersion::new(
                    DocId::new((41_u128 << 32) | number),
                    Revision::new(revision),
                ),
                vec![1.0, 0.0],
            )
            .with_text(text)
        };
        core.ingest(
            IngestBatch::new(vec![
                chunk(0, 1, "the bronze"),
                chunk(1, 1, "zeppelin zeppelin"),
                chunk(2, 1, "zeppelin zeppelin"),
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(42_u128 << 32), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_text("unrelated filler"),
            ])
            .with_epoch(identity),
        )
        .expect("old revision");
        core.seal().expect("seal old revision");
        core.ingest(IngestBatch::new(vec![chunk(0, 2, "the bronze")]).with_epoch(identity))
            .expect("commit replacement without stale cleanup");
        if seal_new_revision {
            core.seal().expect("seal replacement");
        }
        core.close().expect("close without stale delete");
        drop(core);

        let read_only = TextStore::open(
            &path,
            &bundle_path,
            TextOpenOptions::new(OpenOptions::read_only(), TokenizerConfig::text_default()),
        )
        .expect("read-only text open must succeed before reconciliation");
        read_only.close().expect("close read-only text store");
        drop(read_only);

        let store =
            TextStore::open(&path, &bundle_path, Default::default()).expect("recover text store");
        for legs in [Legs::Lexical, Legs::Dense] {
            assert_current_revision_only(&store, legs);
        }
        store
            .delete_text(&[41])
            .expect("delete all rebuilt registry chunks");
        store
            .close()
            .expect("close after delete without another seal");
        drop(store);
        let reopened =
            TextStore::open(&path, &bundle_path, Default::default()).expect("reopen deleted store");
        for legs in [Legs::Lexical, Legs::Dense] {
            assert!(
                reopened
                    .query_text("bronze zeppelin", QueryOptions::new(8).with_legs(legs))
                    .expect("query after delete and reopen")
                    .iter()
                    .all(|hit| hit.doc_id != 41)
            );
        }
    }
}

#[test]
fn shorter_revision_removes_stale_chunks_from_lexical_search_and_after_reopen() {
    shorter_revision_removes_stale_chunks(Legs::Lexical);
}

#[test]
fn shorter_revision_removes_stale_chunks_from_dense_search_and_after_reopen() {
    shorter_revision_removes_stale_chunks(Legs::Dense);
}

fn shorter_revision_removes_stale_chunks(legs: Legs) {
    for serialized in [false, true] {
        let directory = tempdir().expect("store directory");
        let bundle = directory.path().join("fixture.zem");
        common::write_symmetric_fixture_bundle(&bundle);
        let path = directory.path().join("store");
        let store = TextStore::open(&path, &bundle, Default::default()).expect("open store");
        let options = IngestOptions {
            embed_batch_size: 1,
            seal_every: 1,
            chunk_policy: ChunkPolicy::Tokens { max: 2, overlap: 0 },
            ..Default::default()
        };
        let ingest = |documents: &[TextDocument]| {
            if serialized {
                store.ingest_text_serialized(documents, options)
            } else {
                store.ingest_text(documents, options)
            }
            .expect("ingest revision")
        };
        assert_eq!(
            ingest(&[
                TextDocument::new(41, 1, "the bronze zeppelin zeppelin zeppelin zeppelin"),
                TextDocument::new(42, 1, "unrelated filler"),
            ])
            .chunks,
            4
        );
        let query = QueryOptions::new(8).with_legs(legs);
        let before = store
            .query_text("zeppelin", query)
            .expect("query old revision");
        assert!(before.iter().any(|hit| hit.chunk == 2 && hit.revision == 1));

        assert_eq!(ingest(&[TextDocument::new(41, 2, "the bronze")]).chunks, 1);
        assert_current_revision_only(&store, legs);
        store.close().expect("close store");
        drop(store);

        let reopened = TextStore::open(&path, &bundle, Default::default()).expect("reopen store");
        assert_current_revision_only(&reopened, legs);
        reopened
            .delete_text(&[41])
            .expect("delete rebuilt registry's chunks");
        assert!(
            reopened
                .query_text("bronze", query)
                .expect("query deleted document")
                .iter()
                .all(|hit| hit.doc_id != 41)
        );
    }
}

fn assert_current_revision_only(store: &TextStore, legs: Legs) {
    let hits = store
        .query_text("zeppelin", QueryOptions::new(8).with_legs(legs))
        .expect("query removed text")
        .into_iter()
        .filter(|hit| hit.doc_id == 41)
        .collect::<Vec<_>>();
    assert!(
        hits.iter().all(|hit| hit.revision == 2 && hit.chunk == 0),
        "stale hits: {hits:?}"
    );
    if legs == Legs::Lexical {
        assert!(hits.is_empty(), "removed text must no longer match");
    } else {
        assert_eq!(
            hits.len(),
            1,
            "dense search must retain the replacement chunk"
        );
    }
}
