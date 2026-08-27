#![allow(clippy::expect_used)]

use std::time::Duration;
use std::{io::Seek, io::SeekFrom, io::Write};
use tempfile::tempdir;
use zeppelin_embed::diag::{ArtifactRef, HealthFaultKind, HealthStatus, MaintenanceOutcome};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, RetentionPolicy, Revision,
};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::tier::SegmentTier;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus};

fn flip_byte(path: &std::path::Path, offset: u64) {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open segment mutation target");
    file.seek(SeekFrom::Start(offset))
        .expect("seek mutation byte");
    let mut byte = [0_u8; 1];
    std::io::Read::read_exact(&mut file, &mut byte).expect("read mutation byte");
    byte[0] ^= 0x80;
    file.seek(SeekFrom::Start(offset))
        .expect("rewind mutation byte");
    file.write_all(&byte).expect("write mutation byte");
    file.flush().expect("flush mutation byte");
}

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
fn health_starts_unchecked_and_full_self_check_marks_examined_artifacts_healthy() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let initial = store.health().expect("initial health");
    assert_eq!(initial.status, HealthStatus::Unchecked);
    assert!(initial.unresolved_faults.is_empty());

    let report = store.self_check(0, 0x63);
    assert_eq!(report.health_status, HealthStatus::Healthy, "{report:?}");
    assert!(
        report
            .examined_artifacts
            .iter()
            .any(|artifact| { matches!(artifact, zeppelin_embed::diag::ArtifactRef::Manifest) })
    );
    assert!(
        report
            .examined_artifacts
            .iter()
            .any(|artifact| { matches!(artifact, zeppelin_embed::diag::ArtifactRef::Wal) })
    );
    let checked = store.health().expect("checked health");
    assert_eq!(checked.status, HealthStatus::Healthy);
    assert!(checked.unresolved_faults.is_empty());
    store.close().expect("close store");
}

#[test]
fn self_check_attributes_and_clears_only_the_exact_revalidated_region() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    for id in [201, 202] {
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(id), Revision::new(1)),
                vec![id as f32, 1.0],
            )]))
            .expect("ingest health segment");
        store.seal().expect("seal health segment");
    }
    let snapshot = store.snapshot().expect("pin health snapshot");
    let scopes = snapshot
        .segments()
        .iter()
        .map(|segment| {
            let entry = segment
                .directory()
                .iter()
                .find(|entry| {
                    entry.kind == zeppelin_embed::segment::layout::RegionKind::VectorCodes.id()
                })
                .expect("vector-code region");
            (segment.meta().id, entry.offset)
        })
        .collect::<Vec<_>>();
    drop(snapshot);
    assert_eq!(scopes.len(), 2);

    let first_path = directory.path().join(scopes[0].0.file_name());
    let second_path = directory.path().join(scopes[1].0.file_name());
    flip_byte(&first_path, scopes[0].1);
    let first = store.self_check(0, 0x64);
    assert_eq!(first.health_status, HealthStatus::Unhealthy);
    let first_health = store.health().expect("first corrupt health");
    assert!(
        first_health.unresolved_faults.keys().any(|key| {
            key.kind == HealthFaultKind::Format
                && key.artifact
                    == (ArtifactRef::Region {
                        segment_id: scopes[0].0,
                        kind: zeppelin_embed::segment::layout::RegionKind::VectorCodes.id(),
                    })
        }),
        "{first_health:?}"
    );
    assert!(!first_health.unresolved_faults.keys().any(|key| {
        matches!(
            key.artifact,
            ArtifactRef::Region { segment_id, .. } if segment_id == scopes[1].0
        )
    }));

    flip_byte(&first_path, scopes[0].1);
    flip_byte(&second_path, scopes[1].1);
    let second = store.self_check(0, 0x65);
    assert_eq!(second.health_status, HealthStatus::Unhealthy);
    let second_health = store.health().expect("second corrupt health");
    assert!(!second_health.unresolved_faults.keys().any(|key| {
        matches!(
            key.artifact,
            ArtifactRef::Region { segment_id, .. } if segment_id == scopes[0].0
        )
    }));
    assert!(second_health.unresolved_faults.keys().any(|key| {
        key.artifact
            == (ArtifactRef::Region {
                segment_id: scopes[1].0,
                kind: zeppelin_embed::segment::layout::RegionKind::VectorCodes.id(),
            })
    }));

    flip_byte(&second_path, scopes[1].1);
    assert_eq!(
        store.self_check(0, 0x66).health_status,
        HealthStatus::Healthy
    );
    store.close().expect("close store");
}

#[test]
fn successful_checkpoint_scope_revalidation_clears_a_removed_fault() {
    let directory = tempdir().expect("store directory");
    let checkpoint = directory.path().join(".tier-health.graph.checkpoint");
    std::fs::write(&checkpoint, b"not-a-checkpoint").expect("write damaged checkpoint");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");

    assert_eq!(
        store.self_check(0, 0x67).health_status,
        HealthStatus::Unhealthy
    );
    let artifact = ArtifactRef::Checkpoint {
        name: ".tier-health.graph.checkpoint".to_owned(),
    };
    assert!(
        store
            .health()
            .expect("faulted health")
            .unresolved_faults
            .keys()
            .any(|key| key.artifact == artifact)
    );

    std::fs::remove_file(&checkpoint).expect("complete checkpoint cleanup");
    assert_eq!(
        store.self_check(0, 0x68).health_status,
        HealthStatus::Healthy
    );
    assert!(
        !store
            .health()
            .expect("revalidated health")
            .unresolved_faults
            .keys()
            .any(|key| key.artifact == artifact)
    );
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

#[test]
fn self_check_samples_only_live_active_and_sealed_documents_against_the_exact_oracle() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");

    let empty = store.self_check(8, 0x24_5e1f_c4ec);
    assert_eq!(empty.requested_samples, 8);
    assert_eq!(empty.sampled_documents, 0);
    assert_eq!(empty.expected_hits, 0);
    assert_eq!(empty.recalled_hits, 0);
    assert_eq!(empty.recall, 1.0);
    assert!(empty.failures.is_empty());

    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(11), Revision::new(1)),
                vec![1.0, 0.0, 0.0, 0.0],
            ),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(12), Revision::new(1)),
                vec![0.0, 1.0, 0.0, 0.0],
            ),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(13), Revision::new(1)),
                vec![0.0, 0.0, 1.0, 0.0],
            ),
        ]))
        .expect("ingest self-check rows");
    store
        .delete(DeleteBatch::new(vec![DocId::new(12)]))
        .expect("tombstone one active row");

    let active = store.self_check(8, 0x24_5e1f_c4ec);
    assert_eq!(active.sampled_documents, 2);
    assert_eq!(active.expected_hits, 4);
    assert_eq!(active.recalled_hits, 4);
    assert_eq!(active.recall, 1.0);
    assert!(
        (2.0..2.1).contains(&active.max_score_delta),
        "the self-check must expose the coarse-score delta: {active:?}"
    );
    assert!(active.failures.is_empty());

    store.seal().expect("seal self-check rows");
    let sealed = store.self_check(1, 0x24_5e1f_c4ec);
    assert_eq!(sealed.sampled_documents, 1);
    assert_eq!(sealed.expected_hits, 2);
    assert_eq!(sealed.recalled_hits, 2);
    assert_eq!(sealed.recall, 1.0);
    assert!(
        (2.0..2.1).contains(&sealed.max_score_delta),
        "the sealed self-check must retain score provenance: {sealed:?}"
    );
    assert!(sealed.failures.is_empty());
}

#[test]
fn self_check_after_close_reports_a_typed_state_failure_without_partial_samples() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store.close().expect("close store");

    let report = store.self_check(3, 0x24_c105_ed00);
    assert_eq!(report.requested_samples, 3);
    assert_eq!(report.sampled_documents, 0);
    assert_eq!(report.expected_hits, 0);
    assert_eq!(report.recalled_hits, 0);
    assert_eq!(report.recall, 0.0);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].stage, "state");
    assert_eq!(report.failures[0].detail, "store is closed");
}
