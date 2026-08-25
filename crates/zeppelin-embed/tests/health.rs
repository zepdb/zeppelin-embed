#![allow(clippy::expect_used)]

use std::time::Duration;
use tempfile::tempdir;
use zeppelin_embed::diag::MaintenanceOutcome;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, RetentionPolicy, Revision,
};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::tier::SegmentTier;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus};

#[test]
fn pending_docs_drops_to_zero_after_seal_and_rises_by_the_batch_size_on_ingest() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    assert_eq!(store.health().expect("initial health").pending_docs, 0);

    let documents = (0..3)
        .map(|offset| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(offset + 1), Revision::new(1)),
                vec![offset as f32, 1.0],
            )
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest batch");
    let ingested = store.health().expect("ingested health");
    assert_eq!(ingested.pending_docs, 3);
    assert_eq!(ingested.segments.len(), 1);
    let active = ingested.segments.first().expect("active segment health");
    assert_eq!(active.tier, SegmentTier::ActiveScan);
    assert_eq!(active.rows, 3);

    store.seal().expect("seal active rows");
    let sealed = store.health().expect("sealed health");
    assert_eq!(sealed.pending_docs, 0);
    assert_eq!(sealed.segments.len(), 1);
    let immutable = sealed.segments.first().expect("sealed segment health");
    assert_eq!(immutable.tier, SegmentTier::SealedScan);
    assert_eq!(immutable.rows, 3);
    store.close().expect("close store");
}

#[test]
fn retained_through_reflects_an_executed_retention_run_and_last_maintenance_matches() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_timestamp(10),
        ]))
        .expect("ingest retained row");
    store.seal().expect("seal retained row");
    let retention = store
        .apply_retention(RetentionPolicy::new(10).expect("retention window"), 100)
        .expect("execute retention");
    assert!(!retention.is_no_op());

    let maintenance = store.maintain(MaintenanceBudget {
        wall_time: Duration::ZERO,
        bytes: 0,
    });
    assert!(matches!(
        maintenance.status,
        MaintenanceStatus::BudgetExhausted
    ));
    let health = store.health().expect("health after reports");
    assert_eq!(health.retained_through, Some(90));
    let last = health.last_maintenance.expect("last maintenance");
    assert_eq!(last.graphs_built, maintenance.graphs_built);
    assert_eq!(last.bytes_consumed, maintenance.bytes_consumed);
    assert_eq!(last.checkpoints_resumed, maintenance.checkpoints_resumed);
    assert_eq!(last.outcome, MaintenanceOutcome::BudgetExhausted);
    store.close().expect("close store");
}
