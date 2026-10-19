#![allow(clippy::expect_used)]

use zeppelin_embed_text::{
    DEFAULT_MODEL_MANIFEST_SHA256, DEFAULT_MODEL_REPO, DEFAULT_MODEL_REVISION, IngestOptions, Legs,
    ModelOpenOptions, ModelRef, QueryOptions, TextDocument, TextStore, resolve_model,
};

#[test]
fn default_model_is_the_published_immutable_pair() {
    let model = ModelRef::default_model();
    assert_eq!(model.repo, DEFAULT_MODEL_REPO);
    assert_eq!(model.revision, DEFAULT_MODEL_REVISION);
    assert_eq!(model.manifest_sha256, DEFAULT_MODEL_MANIFEST_SHA256);
    assert_eq!(model.repo, "aghatage/zeppelin-embed-clean71-arctic-m-v2");
    assert_eq!(model.revision, "d64b817c4021a3f1b9bded3152d9afdf9ac813e7");
    assert_eq!(
        model.manifest_sha256,
        "123c6308e00f769c68498f9df60dfb9efc688f58c4ff2c4da266f978fcb215dc"
    );
}

#[test]
fn open_default_model_offline_miss_uses_the_resolver() {
    let root = tempfile::tempdir().expect("tempdir");
    let result = TextStore::open_default_model(
        root.path().join("store"),
        ModelOpenOptions {
            cache_dir: Some(root.path().join("cache")),
            offline: true,
            ..Default::default()
        },
    );
    assert!(matches!(
        result,
        Err(zeppelin_embed_text::TextError::ModelResolution(
            zeppelin_embed_text::ModelError::OfflineMiss
        ))
    ));
}

#[test]
#[ignore = "downloads the pinned HF model into ZE_MODEL_CACHE or the default cache; no MLX"]
fn real_default_model_downloads_verifies_and_reopens_offline() {
    let model = ModelRef::default_model();
    let bundle =
        resolve_model(&model, &ModelOpenOptions::default()).expect("resolve published model");
    assert!(
        !zeppelin_embed_text::bundle::Bundle::open(&bundle)
            .expect("verified model bundle")
            .is_symmetric()
    );
    assert_eq!(
        resolve_model(
            &model,
            &ModelOpenOptions {
                offline: true,
                ..Default::default()
            },
        )
        .expect("reuse verified model offline"),
        bundle
    );
}

#[test]
#[ignore = "needs network, real Hugging Face model download and MLX/Metal outside sandbox"]
fn real_default_model_downloads_and_answers_queries() {
    let root = tempfile::tempdir().expect("tempdir");
    let store =
        TextStore::open_default_model(root.path().join("store"), ModelOpenOptions::default())
            .expect("download and open published default");
    store
        .ingest_text(
            &[
                TextDocument::new(1, 1, "The Eiffel Tower is a landmark in Paris, France."),
                TextDocument::new(
                    2,
                    1,
                    "Sourdough bread is made with flour, water and a fermented starter.",
                ),
                TextDocument::new(3, 1, "Dolphins are marine mammals that live in the ocean."),
            ],
            IngestOptions::default(),
        )
        .expect("ingest three documents");
    for legs in [Legs::Dense, Legs::Lexical, Legs::Hybrid] {
        let hits = store
            .query_text(
                "Eiffel Tower in Paris",
                QueryOptions::new(1).with_legs(legs),
            )
            .expect("query published default");
        assert_eq!(hits.first().map(|hit| hit.doc_id), Some(1), "{legs:?}");
    }
    store.close().expect("close");
}
