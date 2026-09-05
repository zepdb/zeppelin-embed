#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed::epoch::{EmbeddingEpoch, StoreEpoch};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::lifecycle::{OpenOptions, Store, StoreError};
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::{TextError, TextStore};

mod common;

#[test]
fn pre_layout_fix_store_is_refused_without_modifying_persisted_bytes() {
    let directory = tempdir().expect("tempdir");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let bundle = Bundle::open(&bundle_path).expect("bundle");
    let document = bundle.document_tower().embedding.clone();
    let tokenizer = TokenizerConfig::text_default();
    // This is the exact pre-fix text-store declaration: raw bundle metadata
    // without a revision for the MLX output-layout interpretation.
    let old_epoch = StoreEpoch {
        embedding: EmbeddingEpoch {
            document: document.clone(),
            query: document,
            alignment_digest: Vec::new(),
        },
        tokenizer: tokenizer.epoch(),
    };
    let store_path = directory.path().join("store");
    let old = Store::open(
        &store_path,
        OpenOptions::default()
            .with_epoch(old_epoch)
            .with_tokenizer(tokenizer),
    )
    .expect("old store");
    old.close().expect("close old store");
    let manifest = std::fs::read(store_path.join("manifest.ze")).expect("old manifest");
    let wal = std::fs::read(store_path.join("wal.ze")).expect("old WAL");
    let error = match TextStore::open(&store_path, &bundle_path, Default::default()) {
        Ok(store) => {
            store.close().expect("close unexpectedly accepted store");
            panic!("pre-layout-fix store must refuse open; its vectors need re-embedding");
        }
        Err(error) => error,
    };
    assert!(matches!(
        error,
        TextError::Store(StoreError::EpochMismatch(_))
    ));
    assert_eq!(
        std::fs::read(store_path.join("manifest.ze")).expect("manifest"),
        manifest
    );
    assert_eq!(std::fs::read(store_path.join("wal.ze")).expect("WAL"), wal);
}

#[test]
fn swapping_the_query_tower_keeps_the_store_open_and_swapping_the_document_tower_refuses_open() {
    let directory = tempdir().expect("tempdir");
    let store_path = directory.path().join("store");
    let first_path = directory.path().join("first.zem");
    common::write_fixture_bundle(
        &first_path,
        &[
            common::FixtureTower::document("document-a"),
            common::FixtureTower::query("query-a"),
        ],
        b"alignment-a",
    );
    let first = TextStore::open(&store_path, &first_path, Default::default()).expect("first open");
    let first_epoch = first.epoch();
    first.close().expect("close first");

    let query_swap_path = directory.path().join("query-swap.zem");
    common::write_fixture_bundle(
        &query_swap_path,
        &[
            common::FixtureTower::document("document-a"),
            common::FixtureTower::query("query-b"),
        ],
        b"alignment-b",
    );
    let query_swap = TextStore::open(&store_path, &query_swap_path, Default::default())
        .expect("matching document tower keeps the store open");
    assert_ne!(query_swap.epoch(), first_epoch);
    query_swap.close().expect("close query swap");

    let document_swap_path = directory.path().join("document-swap.zem");
    common::write_fixture_bundle(
        &document_swap_path,
        &[
            common::FixtureTower::document("document-b"),
            common::FixtureTower::query("query-b"),
        ],
        b"alignment-c",
    );
    let error = match TextStore::open(&store_path, &document_swap_path, Default::default()) {
        Ok(_) => panic!("changed document tower must refuse open"),
        Err(error) => error,
    };
    assert!(matches!(error, TextError::Store(_)));
}
