#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed_text::{
    IngestControl, IngestOptions, Legs, QueryOptions, TextDocument, TextError, TextFaultSite,
    TextStore,
};

mod common;

#[test]
fn ingest_text_over_a_corpus_produces_the_same_rows_and_vectors_as_the_serialized_path() {
    let directory = tempdir().expect("tempdir");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let pipeline = TextStore::open(
        directory.path().join("pipeline"),
        &bundle_path,
        Default::default(),
    )
    .expect("open pipeline store");
    let serial = TextStore::open(
        directory.path().join("serial"),
        &bundle_path,
        Default::default(),
    )
    .expect("open serial store");
    let documents = (1..=9)
        .map(|id| TextDocument::new(id, 1, format!("the bronze zeppelin {id}")))
        .collect::<Vec<_>>();

    let pipeline_report = pipeline
        .ingest_text(
            &documents,
            IngestOptions {
                embed_batch_size: 3,
                seal_every: 4,
                ..Default::default()
            },
        )
        .expect("pipeline ingest");
    let serial_report = serial
        .ingest_text_serialized(&documents, IngestOptions::default())
        .expect("serialized ingest");
    assert_eq!(pipeline_report.chunks, serial_report.chunks);

    let query = QueryOptions::new(9).with_legs(Legs::Dense);
    let pipeline_hits = pipeline
        .query_text("bronze zeppelin", query)
        .expect("pipeline query");
    let serial_hits = serial
        .query_text("bronze zeppelin", query)
        .expect("serial query");
    let pipeline_rows = pipeline_hits
        .iter()
        .map(|hit| (hit.doc_id, hit.revision, hit.chunk, hit.vector_squared_l2))
        .collect::<Vec<_>>();
    let serial_rows = serial_hits
        .iter()
        .map(|hit| (hit.doc_id, hit.revision, hit.chunk, hit.vector_squared_l2))
        .collect::<Vec<_>>();
    assert_eq!(pipeline_rows, serial_rows);
}

#[test]
fn a_failing_embed_worker_surfaces_as_a_typed_error_and_every_thread_is_joined() {
    let directory = tempdir().expect("tempdir");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        &bundle_path,
        Default::default(),
    )
    .expect("open store");
    let control = IngestControl::new().with_fault(TextFaultSite::EmbedWorkerPanic);

    let error = store
        .ingest_text_controlled(
            &[TextDocument::new(1, 1, "bronze zeppelin")],
            IngestOptions::default(),
            control.clone(),
        )
        .expect_err("injected embed failure must surface");

    assert!(matches!(
        error,
        TextError::Pipeline {
            stage: "embed worker",
            ..
        }
    ));
    assert!(control.fault_fired(TextFaultSite::EmbedWorkerPanic));
    store
        .close()
        .expect("all workers join after the caught panic");
}

#[test]
fn back_pressure_bounds_in_flight_batches_to_the_channel_capacity() {
    let directory = tempdir().expect("tempdir");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        &bundle_path,
        Default::default(),
    )
    .expect("open store");
    let documents = (1..=64)
        .map(|id| TextDocument::new(id, 1, "the bronze zeppelin"))
        .collect::<Vec<_>>();
    let capacity = 1;

    let report = store
        .ingest_text(
            &documents,
            IngestOptions {
                embed_batch_size: 1,
                seal_every: 64,
                channel_capacity: capacity,
                ..Default::default()
            },
        )
        .expect("bounded ingest");

    assert!(report.max_in_flight_batches <= capacity);
}
