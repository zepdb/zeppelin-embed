#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::time::Duration;
use tempfile::tempdir;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus};
use zeppelin_embed_text::{IngestOptions, Legs, QueryOptions, TextDocument, TextError, TextStore};

mod common;

#[test]
fn paired_towers_keep_query_routing_and_chunk_deletion_across_reopen() {
    let root = tempdir().expect("root");
    let model = root.path().join("pair.zem");
    let mut document = common::FixtureTower::document("document");
    document.word_vectors[2] = [0., 0.];
    document.word_vectors[6] = [0., 1.];
    let mut query = common::FixtureTower::query("query");
    query.prefix = "";
    query.word_vectors[2] = [0., 0.];
    query.word_vectors[5] = [0., 1.];
    common::write_fixture_bundle(&model, &[document, query], b"explicit-pair");
    let path = root.path().join("store");
    let store = TextStore::open(&path, &model, Default::default()).expect("pair");
    let documents = [
        TextDocument::new(1, 1, "bronze"),
        TextDocument::new(2, 1, "zeppelin"),
    ];
    let options = IngestOptions {
        embed_batch_size: 1,
        seal_every: 1,
        ..Default::default()
    };
    let report = store
        .ingest_text_serialized(&documents, options)
        .expect("two sealed chunks");
    assert_eq!((report.documents, report.chunks, report.seals), (2, 2, 2));
    assert!(report.all_threads_joined);
    let query_options = QueryOptions::new(1).with_legs(Legs::Dense);
    let hits = store
        .query_text("bronze", query_options)
        .expect("query tower");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].doc_id, 2,
        "query tower deliberately points at the other document"
    );
    assert_eq!(hits[0].text, "zeppelin");
    let budget = MaintenanceBudget {
        wall_time: Duration::from_secs(1),
        bytes: 1024 * 1024,
    };
    let maintained = store
        .maintain_to_completion(budget)
        .expect("complete bounded maintenance");
    assert!(matches!(maintained.status, MaintenanceStatus::Complete));
    assert!(matches!(
        store.maintain_to_completion(MaintenanceBudget {
            wall_time: Duration::ZERO,
            ..budget
        }),
        Err(TextError::InvalidInput(
            "maintenance-to-completion wall time must be nonzero"
        ))
    ));
    assert!(matches!(
        store.maintain_to_completion(MaintenanceBudget { bytes: 0, ..budget }),
        Err(TextError::InvalidInput(
            "maintenance-to-completion byte budget must be nonzero"
        ))
    ));
    store.close().expect("close both runtime roles");
    drop(store);
    let reopened = TextStore::open(&path, &model, Default::default()).expect("reopen pair");
    assert_eq!(
        reopened
            .query_text("bronze", query_options)
            .expect("reopened query")[0]
            .doc_id,
        2
    );
    assert!(matches!(
        reopened.delete_text(&[]),
        Err(TextError::InvalidInput("delete caller id batch is empty"))
    ));
    assert!(matches!(
        reopened.delete_text(&[1_u128 << 96]),
        Err(TextError::InvalidInput(
            "caller document id exceeds 96 bits"
        ))
    ));
    reopened
        .delete_text(&[2, 2, 999])
        .expect("deduplicated deletion including missing caller");
    assert_eq!(
        reopened
            .query_text("bronze", query_options)
            .expect("remaining row")[0]
            .doc_id,
        1
    );
    reopened.close().expect("close reader");
}

#[cfg(target_os = "macos")]
#[test]
fn discovered_broken_coreml_artifact_fails_open_without_silent_backend_fallback() {
    let root = tempdir().expect("root");
    let model = root.path().join("broken.zem");
    common::write_symmetric_fixture_bundle(&model);
    std::fs::create_dir(model.with_extension("mlmodelc")).expect("invalid empty CoreML artifact");
    let result = TextStore::open(root.path().join("store"), &model, Default::default());
    assert!(
        matches!(result, Err(TextError::Runtime(zeppelin_embed_text::runtime::RuntimeError::Mlx(detail))) if !detail.is_empty()),
        "present invalid artifact must fail open"
    );
    // The failed initialization releases the writer. Removing the invalid artifact
    // permits a new open with the explicitly absent-CoreML policy.
    std::fs::remove_dir(model.with_extension("mlmodelc")).expect("remove invalid fixture");
    let clean = TextStore::open(root.path().join("store"), &model, Default::default())
        .expect("clean reopen after initialization error");
    assert_eq!(clean.query_backend().runtime.name, "mlx-c");
    clean.close().expect("join clean worker");
}
