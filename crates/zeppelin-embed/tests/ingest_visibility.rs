#![allow(clippy::expect_used)]

use std::error::Error as _;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;
use tempfile::tempdir;
use zeppelin_embed::ingest::wal_payload::PayloadError;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError,
    IngestRetentionCheckpoint, IngestRetentionFaultController, IngestRetentionFaultEffect,
    IngestRetentionFaultKind, IngestRetentionIoKind, IngestRetentionOperation,
    IngestRetentionTestFault, PartialBatchAppendVfs, Revision, RowSource, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, Store, StoreError, StoreTestDependencies,
    SystemMonotonicClock,
};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{Bit4Factors, QuantError};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed::vfs::fault::BlockingVfs;
use zeppelin_embed::wal::header::{WAL_HEADER_LEN, WalHeaderError};
use zeppelin_embed::wal::record::{RECORD_HEADER_LEN, RecordError};
use zeppelin_embed::wal::replay::{CorruptionLocation, CorruptionReason};
use zeppelin_embed::wal::{
    LogSeq, VisibleRecordError, WalReadError, WalRecoveryError, WalWriteError,
};

#[test]
fn committed_write_is_visible_to_next_query() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let version = DocumentVersion::new(DocId::new(1), Revision::new(1));

    let ack = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            version,
            vec![1.0, 0.0],
        )]))
        .expect("commit ingest");
    let query = [1.0_f32, 0.0];
    let outcome = store
        .search(
            SearchRequest::new(&query),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search store data");

    assert_eq!(ack.seq().get(), 1);
    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(outcome.candidates[0].document(), Some(version));
}

#[test]
fn queries_admit_while_a_durable_commit_is_in_flight() {
    let directory = tempdir().expect("store directory");
    let blocking = BlockingVfs::new(StdVfs);
    let dependencies =
        StoreTestDependencies::new(Arc::new(blocking.clone()), Arc::new(SystemMonotonicClock));
    let options = OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable);
    let store = Arc::new(
        Store::open_with_test_dependencies(directory.path(), options, dependencies)
            .expect("open durable store"),
    );
    blocking.block_next_syncs(1).expect("arm WAL sync");

    let ingest_store = Arc::clone(&store);
    let ingest = std::thread::spawn(move || {
        ingest_store.ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(2), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
    });
    blocking
        .wait_until_blocked(1)
        .expect("ingest reached WAL sync");

    let (query_returned_tx, query_returned_rx) = mpsc::channel();
    let query_store = Arc::clone(&store);
    let query = std::thread::spawn(move || {
        let outcome = query_store.search(
            SearchRequest::new(&[1.0, 0.0]),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        );
        query_returned_tx.send(()).expect("report query return");
        outcome
    });
    let query_returned_before_release = query_returned_rx
        .recv_timeout(Duration::from_secs(1))
        .is_ok();

    blocking.release_syncs(1).expect("release WAL sync");
    let ack = ingest
        .join()
        .expect("ingest thread")
        .expect("durable ingest");
    let outcome = query
        .join()
        .expect("query thread")
        .expect("query during commit");

    assert!(
        query_returned_before_release,
        "query admission waited for the in-flight WAL sync"
    );
    assert!(
        outcome.candidates.is_empty(),
        "query observed the unpublished ingest working copy"
    );
    assert_eq!(ack.seq().get(), 1);
    assert_eq!(ack.generation(), 1);
}

#[test]
fn reopen_after_ingest_recovers_acknowledged_writes() {
    let directory = tempdir().expect("store directory");
    let version = DocumentVersion::new(DocId::new(100), Revision::new(1));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let first_ack = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            version,
            vec![1.0, 0.0],
        )]))
        .expect("commit ingest");
    store.close().expect("close store");

    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");
    let query = [1.0_f32, 0.0];
    let outcome = reopened
        .search(
            SearchRequest::new(&query),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search recovered store");

    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(outcome.candidates[0].document(), Some(version));
    let recovered_generation = reopened
        .snapshot()
        .expect("snapshot recovered generation")
        .generation();
    assert!(
        recovered_generation >= first_ack.generation(),
        "recovered generation {recovered_generation} preceded acknowledged generation {}",
        first_ack.generation()
    );

    let second = DocumentVersion::new(DocId::new(104), Revision::new(1));
    let ack = reopened
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            second,
            vec![0.0, 1.0],
        )]))
        .expect("append after recovery");
    assert_eq!(ack.seq().get(), 2);
    assert!(ack.generation() > recovered_generation);
    reopened.close().expect("close recovered writer");

    let read_only = Store::open(directory.path(), OpenOptions::read_only())
        .expect("reopen recovered read-only");
    let outcome = read_only
        .search(
            SearchRequest::new(&[1.0_f32, 1.0]),
            2,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search twice-recovered store");
    let versions = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        versions,
        std::collections::BTreeSet::from([version, second])
    );
    read_only.close().expect("close read-only store");
}

#[test]
fn reopen_replays_delete_into_active_segment() {
    let directory = tempdir().expect("store directory");
    let doc_id = DocId::new(105);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(doc_id, Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("commit ingest");
    store
        .delete(DeleteBatch::new(vec![doc_id]))
        .expect("commit delete");
    store.close().expect("close store");

    let reopened =
        Store::open(directory.path(), OpenOptions::read_only()).expect("reopen read-only");
    let outcome = reopened
        .search(
            SearchRequest::new(&[1.0_f32, 0.0]),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search recovered delete");

    assert!(outcome.candidates.is_empty());
    assert_eq!(
        reopened.stats().expect("recovered stats").tombstone_count,
        1
    );
    reopened.close().expect("close reopened store");
}

#[test]
fn reopen_rejects_torn_tail_and_middle_checksum_corruption_typed() {
    let torn_directory = tempdir().expect("torn store directory");
    write_wal_records(torn_directory.path(), 1);
    let torn_path = torn_directory.path().join("wal.ze");
    let mut torn = std::fs::read(&torn_path).expect("read torn WAL source");
    torn.pop().expect("remove checksum tail byte");
    std::fs::write(&torn_path, torn).expect("write torn WAL");

    let torn_error = Store::open(torn_directory.path(), OpenOptions::default())
        .err()
        .expect("torn WAL must fail open");
    assert!(matches!(
        torn_error,
        StoreError::WalRecovery(WalRecoveryError::CorruptAt {
            offset: WAL_HEADER_LEN,
            reason: CorruptionReason::Record {
                location: CorruptionLocation::Tail,
                error: RecordError::BodyTruncated { .. },
            },
        })
    ));

    let middle_directory = tempdir().expect("middle store directory");
    write_wal_records(middle_directory.path(), 2);
    let middle_path = middle_directory.path().join("wal.ze");
    let mut middle = std::fs::read(&middle_path).expect("read middle WAL source");
    let payload_byte = WAL_HEADER_LEN + RECORD_HEADER_LEN;
    middle[payload_byte] ^= 0x80;
    std::fs::write(&middle_path, middle).expect("write middle-corrupt WAL");

    let middle_error = Store::open(middle_directory.path(), OpenOptions::default())
        .err()
        .expect("middle-corrupt WAL must fail open");
    assert!(matches!(
        middle_error,
        StoreError::WalRecovery(WalRecoveryError::CorruptAt {
            offset: WAL_HEADER_LEN,
            reason: CorruptionReason::Record {
                location: CorruptionLocation::Middle,
                error: RecordError::ChecksumMismatch { .. },
            },
        })
    ));
}

fn write_wal_records(directory: &Path, records: u128) {
    let store = Store::open(directory, OpenOptions::default()).expect("open WAL fixture store");
    for value in 0..records {
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(500 + value), Revision::new(1)),
                vec![1.0, 0.0],
            )]))
            .expect("commit WAL fixture record");
    }
    store.close().expect("close WAL fixture store");
}

#[test]
fn ingest_batch_commits_every_document_in_one_generation() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let first = DocumentVersion::new(DocId::new(101), Revision::new(1));
    let second = DocumentVersion::new(DocId::new(102), Revision::new(1));

    let ack = store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(first, vec![1.0, 0.0]),
            IngestDocument::new(second, vec![0.0, 1.0]),
        ]))
        .expect("commit two-document batch");
    let query = [1.0_f32, 1.0];
    let outcome = store
        .search(
            SearchRequest::new(&query),
            2,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search committed batch");
    let returned = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(ack.seq().get(), 2);
    assert_eq!(ack.generation(), 1);
    assert_eq!(returned, std::collections::BTreeSet::from([first, second]));
}

#[test]
fn post_ack_batch_retry_emits_one_replay_receipt_without_mutation() {
    let directory = tempdir().expect("post-ack retry Store directory");
    let controller = IngestRetentionFaultController::new(20);
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .expect("open post-ack retry Store");
    let batch = IngestBatch::new(vec![
        IngestDocument::new(
            DocumentVersion::new(DocId::new(120), Revision::new(1)),
            vec![1.0, 0.0],
        )
        .with_timestamp(12),
        IngestDocument::new(
            DocumentVersion::new(DocId::new(121), Revision::new(1)),
            vec![0.0, 1.0],
        )
        .with_timestamp(13),
    ]);
    let first = store
        .ingest(batch.clone())
        .expect("first acknowledged batch");
    let wal_before = std::fs::read(directory.path().join("wal.ze")).expect("read WAL before retry");
    let generation_before = store
        .snapshot()
        .expect("snapshot before post-ack retry")
        .generation();
    controller
        .arm(IngestRetentionTestFault::PostAckRetry {
            first_ack_seq: first.seq().get(),
            first_ack_generation: first.generation(),
        })
        .expect("arm post-ack retry receipt");

    let retry = store.ingest(batch).expect("retry acknowledged batch");
    let wal_after = std::fs::read(directory.path().join("wal.ze")).expect("read WAL after retry");
    let generation_after = store
        .snapshot()
        .expect("snapshot after post-ack retry")
        .generation();
    let receipts = controller
        .take_receipts()
        .expect("take post-ack retry receipts");

    assert_eq!(retry, first);
    assert_eq!(wal_after, wal_before);
    assert_eq!(generation_after, generation_before);
    assert_eq!(
        receipts.len(),
        1,
        "selected feature fault post-ack-retry produced {} Store receipts, expected 1",
        receipts.len()
    );
    let receipt = &receipts[0];
    assert_eq!(receipt.campaign(), "ingest-retention");
    assert_eq!(receipt.operation(), IngestRetentionOperation::BatchCommit);
    assert_eq!(receipt.fault(), IngestRetentionFaultKind::PostAckRetry);
    assert_eq!(
        receipt.checkpoint(),
        IngestRetentionCheckpoint::IngestReplayNoWalAppend
    );
    assert_eq!(receipt.cardinality(), 1);
    assert_eq!(receipt.invocation_id(), 20);
    assert_eq!(
        receipt.effect(),
        &IngestRetentionFaultEffect::PostAckRetry {
            batch_count: 2,
            replay_count: 2,
            returned_seq: first.seq().get(),
            returned_generation: first.generation(),
            wal_records_appended: 0,
            generation_delta: 0,
            active_published: false,
        }
    );
}

#[test]
fn partial_batch_append_error_preserves_clean_prefix_and_emits_receipt() {
    let directory = tempdir().expect("partial-batch Store directory");
    let controller = IngestRetentionFaultController::new(21);
    let vfs = Arc::new(PartialBatchAppendVfs::new(
        Arc::new(StdVfs),
        controller.clone(),
    ));
    let dependencies = StoreTestDependencies::new(vfs, Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .expect("open partial-batch Store");
    let baseline = DocumentVersion::new(DocId::new(130), Revision::new(1));
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(baseline, vec![1.0, 0.0]).with_timestamp(13),
        ]))
        .expect("commit partial-batch baseline");
    let submitted = [
        DocumentVersion::new(DocId::new(131), Revision::new(1)),
        DocumentVersion::new(DocId::new(132), Revision::new(1)),
    ];
    let batch = IngestBatch::new(vec![
        IngestDocument::new(submitted[0], vec![0.0, 1.0]).with_timestamp(14),
        IngestDocument::new(submitted[1], vec![0.5, 0.5]).with_timestamp(15),
    ]);
    let generation_before = store
        .snapshot()
        .expect("snapshot before partial batch")
        .generation();
    let wal_before =
        std::fs::read(directory.path().join("wal.ze")).expect("read WAL before partial batch");
    controller
        .arm(IngestRetentionTestFault::PartialBatchAppend { prefix_bytes: 7 })
        .expect("arm partial batch append");

    let error = store
        .ingest(batch.clone())
        .expect_err("partial WAL append must reject the complete batch");
    let expected_detail = "injected partial-batch-append after 7/140 bytes";
    match &error {
        IngestError::Store(StoreError::WalWrite(WalWriteError::Failed { kind, detail })) => {
            assert_eq!(*kind, std::io::ErrorKind::Other);
            assert_eq!(detail.as_ref(), expected_detail);
        }
        other => panic!("partial batch returned the wrong typed error: {other:?}"),
    }
    let receipts = controller
        .take_receipts()
        .expect("take partial-batch receipts");
    assert_eq!(
        receipts.len(),
        1,
        "selected feature fault partial-batch-append produced {} Store receipts, expected 1",
        receipts.len()
    );

    let generation_after = store
        .snapshot()
        .expect("snapshot after partial batch")
        .generation();
    assert_eq!(generation_after, generation_before);
    let immediate = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            8,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search after partial batch");
    assert_eq!(
        immediate
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([baseline])
    );
    assert_eq!(
        std::fs::read(directory.path().join("wal.ze")).expect("read repaired WAL"),
        wal_before
    );

    store.close().expect("close repaired Store");
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .expect("reopen repaired Store after partial append");
    let reopened_rows = reopened
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            8,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search reopened partial-batch Store");
    assert_eq!(
        reopened_rows
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([baseline])
    );

    let retry = reopened
        .ingest(batch)
        .expect("clean retry after repaired WAL");
    assert_eq!(retry.generation(), generation_before + 1);
    let final_rows = reopened
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            8,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search after clean retry");
    assert_eq!(
        final_rows
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([baseline, submitted[0], submitted[1]])
    );

    let receipt = &receipts[0];
    assert_eq!(receipt.campaign(), "ingest-retention");
    assert_eq!(receipt.operation(), IngestRetentionOperation::BatchCommit);
    assert_eq!(
        receipt.fault(),
        IngestRetentionFaultKind::PartialBatchAppend
    );
    assert_eq!(
        receipt.checkpoint(),
        IngestRetentionCheckpoint::IngestCommitManyAppendError
    );
    assert_eq!(receipt.cardinality(), 1);
    assert_eq!(receipt.invocation_id(), 21);
    assert_eq!(
        receipt.effect(),
        &IngestRetentionFaultEffect::PartialBatchAppend {
            submitted_count: 2,
            changed_records: 2,
            encoded_bytes: 140,
            prefix_bytes: 7,
            io_kind: IngestRetentionIoKind::Other,
            detail: expected_detail.to_owned(),
            active_published: false,
            generation_delta: 0,
        }
    );
}

#[test]
fn ingest_public_batches_and_errors_preserve_typed_context() {
    let version = DocumentVersion::new(DocId::new(103), Revision::new(4));
    let batch = IngestBatch::new(vec![IngestDocument::new(version, vec![1.0])]);
    let deletes = DeleteBatch::new(vec![version.doc_id()]);
    assert_eq!(batch.documents()[0].version(), version);
    assert_eq!(deletes.doc_ids(), &[version.doc_id()]);

    let errors = [
        IngestError::Store(StoreError::Closed),
        IngestError::EmptyBatch,
        IngestError::StaleRevision {
            doc_id: version.doc_id(),
            current: Revision::new(4),
            attempted: Revision::new(3),
        },
        IngestError::Vector(QuantError::EmptyVector),
        IngestError::Payload(PayloadError::Truncated),
    ];
    let expected_sources = [true, false, false, true, true];
    for (error, expected_source) in errors.into_iter().zip(expected_sources) {
        assert!(!error.to_string().is_empty());
        assert_eq!(error.source().is_some(), expected_source);
    }
}

#[test]
fn recovery_errors_preserve_typed_context() {
    let errors = [
        StoreError::WalRecovery(WalRecoveryError::MissingFile),
        StoreError::WalRecovery(WalRecoveryError::InvalidHeader(WalHeaderError::Missing)),
        StoreError::WalRecovery(WalRecoveryError::CorruptAt {
            offset: WAL_HEADER_LEN,
            reason: CorruptionReason::Record {
                location: CorruptionLocation::Tail,
                error: RecordError::HeaderTruncated {
                    needed: RECORD_HEADER_LEN,
                    available: 3,
                },
            },
        }),
        StoreError::WalRecord {
            seq: LogSeq::new(7),
            source: VisibleRecordError::InvalidEncodedRange {
                start: 4,
                end: 9,
                available: 8,
            },
        },
        StoreError::WalMutation {
            seq: LogSeq::new(8),
            op: zeppelin_embed::ingest::wal_payload::UPSERT_V1,
            source: PayloadError::Truncated,
        },
        StoreError::WalRevisionOrder {
            seq: LogSeq::new(9),
            doc_id: DocId::new(10),
            current: Revision::new(4),
            attempted: Revision::new(3),
        },
        StoreError::UnsupportedWalMutation {
            seq: LogSeq::new(10),
            op: zeppelin_embed::ingest::wal_payload::METADATA_EDIT_V1,
        },
        StoreError::WalVector {
            seq: LogSeq::new(11),
            source: QuantError::EmptyVector,
        },
        StoreError::Wal(WalReadError::Io(std::io::Error::other("read failed"))),
        StoreError::Wal(WalReadError::Header(WalHeaderError::Missing)),
        StoreError::Wal(WalReadError::InvalidRecoveredRange {
            start: 5,
            end: 12,
            available: 10,
        }),
        StoreError::WalWrite(WalWriteError::RecoveredBytesOverflow),
    ];
    let expected_sources = [
        true, true, true, true, true, false, false, true, true, true, true, true,
    ];

    for (error, expected_source) in errors.into_iter().zip(expected_sources) {
        assert!(!error.to_string().is_empty());
        assert_eq!(error.source().is_some(), expected_source);
    }

    let recovery_errors = [
        WalRecoveryError::MissingFile,
        WalRecoveryError::InvalidHeader(WalHeaderError::Missing),
        WalRecoveryError::CorruptAt {
            offset: WAL_HEADER_LEN,
            reason: CorruptionReason::Record {
                location: CorruptionLocation::Tail,
                error: RecordError::HeaderTruncated {
                    needed: RECORD_HEADER_LEN,
                    available: 3,
                },
            },
        },
    ];
    for (error, expected_source) in recovery_errors.into_iter().zip([false, true, false]) {
        assert!(!error.to_string().is_empty());
        assert_eq!(error.source().is_some(), expected_source);
    }

    let read_errors = [
        WalReadError::Io(std::io::Error::other("read failed")),
        WalReadError::Header(WalHeaderError::Missing),
        WalReadError::InvalidRecoveredRange {
            start: 5,
            end: 12,
            available: 10,
        },
    ];
    for (error, expected_source) in read_errors.into_iter().zip([true, true, false]) {
        assert!(!error.to_string().is_empty());
        assert_eq!(error.source().is_some(), expected_source);
    }
}

#[test]
fn ingest_ack_returns_the_generation_it_acted_on() {
    let directory = tempdir().expect("store directory");
    let store =
        Arc::new(Store::open(directory.path(), OpenOptions::default()).expect("open store"));
    let writer = Arc::clone(&store);
    let ingest = std::thread::spawn(move || {
        writer.ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(2), Revision::new(1)),
            vec![0.0, 1.0],
        )]))
    });

    let ack = ingest
        .join()
        .expect("ingest thread")
        .expect("commit ingest");
    let observed = store.snapshot().expect("snapshot after concurrent ingest");

    assert_eq!(ack.generation(), 1);
    assert_eq!(ack.generation(), observed.generation());
}

#[test]
fn replaying_same_revision_is_noop() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let version = DocumentVersion::new(DocId::new(3), Revision::new(7));
    let batch = || IngestBatch::new(vec![IngestDocument::new(version, vec![1.0, -1.0])]);

    let first = store.ingest(batch()).expect("first ingest");
    let manifest_before = manifest_bytes(directory.path());
    let active_bytes_before = store
        .stats()
        .expect("stats before replay")
        .active_segment_bytes;
    let replay = store.ingest(batch()).expect("idempotent replay");
    let manifest_after = manifest_bytes(directory.path());
    let active_bytes_after = store
        .stats()
        .expect("stats after replay")
        .active_segment_bytes;

    assert_eq!(replay, first);
    assert_eq!(manifest_after, manifest_before);
    assert_eq!(active_bytes_after, active_bytes_before);
}

#[test]
fn higher_revision_supersedes_and_old_never_returned() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let doc_id = DocId::new(4);
    let old = DocumentVersion::new(doc_id, Revision::new(1));
    let new = DocumentVersion::new(doc_id, Revision::new(2));
    let other = DocumentVersion::new(DocId::new(5), Revision::new(1));
    for (version, vector) in [(old, vec![1.0, 0.0]), (other, vec![-1.0, 0.0])] {
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(version, vector)]))
            .expect("seed active row");
    }

    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            new,
            vec![0.0, 1.0],
        )]))
        .expect("supersede revision");
    let query = [1.0_f32, 0.0];
    for k in 1..=2 {
        let outcome = store
            .search(
                SearchRequest::new(&query),
                k,
                ScanOptions { thread_budget: 1 },
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("search after supersede");
        let versions = outcome
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .collect::<Vec<_>>();
        assert!(versions.contains(&new), "k={k} omitted the new revision");
        assert!(!versions.contains(&old), "k={k} returned the old revision");
    }
}

#[test]
fn lower_revision_after_higher_is_rejected_typed() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let doc_id = DocId::new(6);
    let current = DocumentVersion::new(doc_id, Revision::new(9));
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            current,
            vec![1.0, 0.0],
        )]))
        .expect("seed higher revision");
    let before_manifest = manifest_bytes(directory.path());
    let before_stats = store.stats().expect("stats before rejection");
    let before_generation = store
        .snapshot()
        .expect("snapshot before rejection")
        .generation();

    let error = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(doc_id, Revision::new(8)),
            vec![0.0, 1.0],
        )]))
        .expect_err("lower revision must fail");

    assert!(matches!(
        error,
        IngestError::StaleRevision {
            doc_id: rejected,
            current: current_revision,
            attempted,
        } if rejected == doc_id
            && current_revision == Revision::new(9)
            && attempted == Revision::new(8)
    ));
    let after_stats = store.stats().expect("stats after rejection");
    assert_eq!(manifest_bytes(directory.path()), before_manifest);
    assert_eq!(
        after_stats.active_segment_bytes,
        before_stats.active_segment_bytes
    );
    assert_eq!(after_stats.wal_bytes, before_stats.wal_bytes);
    assert_eq!(after_stats.tombstone_count, before_stats.tombstone_count);
    assert_eq!(
        store
            .snapshot()
            .expect("snapshot after rejection")
            .generation(),
        before_generation
    );
    let query = [1.0_f32, 0.0];
    let outcome = store
        .search(
            SearchRequest::new(&query),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search unchanged store");
    assert_eq!(outcome.candidates[0].document(), Some(current));
}

#[test]
fn deleted_doc_is_not_returned_and_is_durable() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let doc_id = DocId::new(7);
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(doc_id, Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("seed document");
    let generation_before = store
        .snapshot()
        .expect("snapshot before delete")
        .generation();
    let wal_before = store.stats().expect("stats before delete").wal_bytes;

    let ack = store
        .delete(DeleteBatch::new(vec![doc_id]))
        .expect("commit delete");
    let query = [1.0_f32, 0.0];
    let outcome = store
        .search(
            SearchRequest::new(&query),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search after delete");
    let stats = store.stats().expect("stats after delete");

    assert_eq!(ack.seq().get(), 2);
    assert!(ack.generation() > generation_before);
    assert_eq!(
        store
            .snapshot()
            .expect("snapshot after delete")
            .generation(),
        ack.generation()
    );
    assert!(outcome.candidates.is_empty());
    assert_eq!(stats.tombstone_count, 1);
    assert_eq!(stats.tombstone_bytes, std::mem::size_of::<u32>() as u64);
    assert!(stats.active_segment_bytes >= stats.tombstone_bytes);
    assert_eq!(
        stats.resident_owned_bytes,
        stats.active_segment_bytes
            + stats.wal_bytes
            + stats.cache_bytes
            + stats.temporary_bytes
            + stats.query_pool_bytes
    );
    assert!(stats.wal_bytes > wal_before);
    store.close().expect("close store");

    let wal = std::fs::read(directory.path().join("wal.ze")).expect("read durable WAL");
    let replayed = zeppelin_embed::wal::replay::replay(&wal);
    let record = replayed.records.last().expect("delete WAL record");
    assert_eq!(record.op, zeppelin_embed::ingest::wal_payload::DELETE_V1);
    assert_eq!(
        zeppelin_embed::ingest::wal_payload::decode_delete(record.payload)
            .expect("decode delete payload"),
        vec![doc_id]
    );
}

#[test]
fn ingest_on_a_read_only_store_is_typed_error() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::read_only()).expect("open read-only");

    let error = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(8), Revision::new(1)),
            vec![1.0_f32],
        )]))
        .expect_err("read-only ingest must fail");

    assert!(matches!(
        error,
        IngestError::Store(zeppelin_embed::lifecycle::StoreError::ReadOnly)
    ));
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("read store directory")
            .count(),
        0,
        "read-only rejection created store files"
    );
}

#[test]
fn every_active_segment_byte_is_accounted() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(9), Revision::new(1)),
                vec![1.0_f32, -1.0],
            )
            .with_metadata(vec![0xa1, 0xb2, 0xc3]),
        ]))
        .expect("ingest accounted row");

    let stats = store.stats().expect("exact active stats");
    let expected = std::mem::size_of::<DocId>()
        + std::mem::size_of::<Revision>()
        + std::mem::size_of::<zeppelin_embed::wal::LogSeq>()
        + std::mem::size_of::<i64>()
        + std::mem::size_of::<u64>()
        + 3
        + (2 * std::mem::size_of::<f32>())
        + 1
        + std::mem::size_of::<zeppelin_embed::quant::Bit4Factors>();
    let expected = u64::try_from(expected).expect("active byte count fits u64");

    assert_eq!(stats.active_segment_bytes, expected);
    assert_eq!(stats.tombstone_count, 0);
    assert_eq!(stats.tombstone_bytes, 0);
    assert_eq!(
        stats.resident_owned_bytes,
        stats.active_segment_bytes + stats.wal_bytes
    );
}

#[test]
fn global_row_ids_distinguish_active_and_every_sealed_row_zero() {
    let directory = tempdir().expect("store directory");
    let first = SegmentId::new(0x0102_0304_0506, [0x11; 10]);
    let second = SegmentId::new(0x0102_0304_0506, [0x22; 10]);
    publish_row_zero_segments(directory.path(), [first, second]);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(10), Revision::new(1)),
            vec![1.0_f32, 0.0],
        )]))
        .expect("ingest active row zero");

    let query = [1.0_f32, 0.0];
    let outcome = store
        .search(
            SearchRequest::new(&query),
            3,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search active and sealed rows");
    let addresses = outcome
        .candidates
        .iter()
        .map(|candidate| {
            let id = candidate.row_id();
            (id.source(), id.local_row())
        })
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(outcome.candidates.len(), 3);
    assert_eq!(addresses.len(), 3, "row-zero addresses collided");
    assert!(addresses.contains(&(RowSource::Active, 0)));
    assert!(addresses.contains(&(RowSource::Sealed(first), 0)));
    assert!(addresses.contains(&(RowSource::Sealed(second), 0)));
}

fn publish_row_zero_segments(directory: &Path, ids: [SegmentId; 2]) {
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
        .expect("derived policy");
    let mut segments = Vec::new();
    for (index, id) in ids.into_iter().enumerate() {
        let mut builder = ColumnStoreBuilder::new(schema.clone());
        builder
            .push_row(i64::try_from(index).expect("timestamp fits"), &[])
            .expect("fixture row");
        let columns = builder.finish().expect("fixture columns");
        let codes = [0x88_u8];
        let factors = [Bit4Factors::from_persisted(1.0, 1.0, 1.0)];
        let rescore = [0.0_f32, 0.0_f32];
        segments.push(
            write_segment(
                &StdVfs,
                directory,
                SegmentBuild {
                    id,
                    scheme: 4,
                    dims: 2,
                    codes: &codes,
                    factors: SegmentFactors::Bit4(&factors),
                    rescore: &rescore,
                    columns: &columns,
                    alive: &AliveSet::new(1),
                },
                policy,
            )
            .expect("write fixture segment"),
        );
    }
    commit_manifest(
        &StdVfs,
        directory,
        &Manifest {
            generation: 4,
            log_seq: 0,
            segments,
            epochs: Vec::new(),
            epoch_alias: None,
            schema,
        },
        policy,
    )
    .expect("publish fixture segments");
}

fn manifest_bytes(directory: &Path) -> Vec<u8> {
    match std::fs::read(directory.join("manifest.zem")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read manifest bytes: {error}"),
    }
}
