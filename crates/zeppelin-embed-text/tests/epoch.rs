#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed_text::{TextError, TextStore};

mod common;

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
