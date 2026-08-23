#![allow(clippy::expect_used)]

use std::error::Error as _;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;
use zeppelin_embed::ingest::wal_payload::PayloadError;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision,
    RowSource, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store, StoreError};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{Bit4Factors, QuantError};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::vfs::StdVfs;
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
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(9), Revision::new(1)),
            vec![1.0_f32, -1.0],
        )]))
        .expect("ingest accounted row");

    let stats = store.stats().expect("exact active stats");
    let expected = std::mem::size_of::<DocId>()
        + std::mem::size_of::<Revision>()
        + std::mem::size_of::<zeppelin_embed::wal::LogSeq>()
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
