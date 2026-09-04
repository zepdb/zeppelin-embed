#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::{IngestOptions, Legs, QueryOptions, TextDocument, TextStore};

mod common;

#[test]
fn the_query_prefix_is_applied_from_the_bundle_and_never_by_the_caller() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("pair.zem");
    common::write_fixture_bundle(
        &path,
        &[
            common::FixtureTower::document("document-fixture"),
            common::FixtureTower::query("query-fixture"),
        ],
        b"aligned",
    );
    let bundle = Bundle::open(&path).expect("open fixture");

    assert_eq!(
        bundle.query_input("bronze zeppelin"),
        "query: bronze zeppelin"
    );
}

#[test]
fn a_lexical_query_does_not_evaluate_the_query_tower() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("pair.zem");
    let mut unusable_query = common::FixtureTower::query("unused-query");
    unusable_query.word_vectors = [[0.0, 0.0]; 7];
    common::write_fixture_bundle(
        &path,
        &[common::FixtureTower::document("document"), unusable_query],
        b"aligned",
    );
    let store = TextStore::open(directory.path().join("store"), path, Default::default())
        .expect("open store");
    store
        .ingest_text(
            &[TextDocument::new(1, 1, "bronze zeppelin")],
            IngestOptions::default(),
        )
        .expect("ingest text");

    let hits = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(1).with_legs(Legs::Lexical),
        )
        .expect("lexical query must not evaluate the unusable query tower");

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc_id, 1);
}
