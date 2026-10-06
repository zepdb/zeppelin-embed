//! ZE-216: one upsert or delete batch is all or nothing after a crash.
//!
//! The WAL is cut at every byte offset across three batches, the way a kill
//! or power cut leaves it, and every reopen must show whole batches only.
//! Hand-framed logs pin the replay rules for batch members.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::Path;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::wal_payload::{
    PayloadError, UPSERT_V2_BATCH_MEMBER, encode_upsert_v2, encode_upsert_v2_batch_member,
};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::lock::STORE_LOCK_FILE;
use zeppelin_embed::lifecycle::{DocumentFields, OpenOptions, Store, StoreError};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed::wal::record::RecordError;
use zeppelin_embed::wal::replay::{CorruptionLocation, CorruptionReason, ReplayTerminator, replay};
use zeppelin_embed::wal::{LogSeq, WalRecoveryError, WalWriter};

const IDS: [u128; 6] = [1, 2, 3, 4, 5, 9];

mod recovery {
    use super::*;

    #[test]
    fn an_op_8_batch_recovers_to_the_generation_the_writer_returned() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
        let ack = store
            .ingest(IngestBatch::new(vec![document(1, 1), document(2, 1)]))
            .expect("commit op-8 batch");
        assert_eq!(ack.generation(), 1);
        store.close().expect("close writer");

        for options in [OpenOptions::read_only(), OpenOptions::default()] {
            let reopened = Store::open(directory.path(), options).expect("reopen store");
            assert_eq!(
                reopened
                    .snapshot()
                    .expect("recovered snapshot")
                    .generation(),
                ack.generation()
            );
            assert_eq!(
                state(&reopened),
                vec![Some(1), Some(1), None, None, None, None]
            );
            reopened.close().expect("close recovered store");
        }
    }

    #[cfg(feature = "graph-cypher")]
    #[test]
    fn document_replay_counts_only_batches_beyond_both_watermarks() {
        use zeppelin_embed::format::golden::decode_hex;
        use zeppelin_embed::manifest::{decode_manifest, encode_manifest};

        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
        upsert(&store, &[(1, 1), (2, 1)]);
        upsert(&store, &[(3, 1), (4, 1)]);
        store.close().expect("close writer");

        // The manifest generation already includes the first batch, folded by
        // the graph side. Documents still need both batches replayed.
        let bytes =
            decode_hex(include_str!("fixtures/format/manifest_v3.hex")).expect("v3 fixture");
        let mut manifest = decode_manifest("v3 fixture", &bytes).expect("v3 manifest");
        manifest.log_seq = 0;
        manifest.schema = zeppelin_embed::meta::Schema::timestamp_only();
        manifest
            .graph
            .as_mut()
            .expect("graph section")
            .graph_absorbed_through = 2;
        std::fs::write(
            directory.path().join("manifest.ze"),
            encode_manifest(&manifest).expect("encode manifest"),
        )
        .expect("write fold manifest");

        for options in [OpenOptions::read_only(), OpenOptions::default()] {
            let reopened = Store::open(directory.path(), options).expect("reopen store");
            assert_eq!(reopened.snapshot().expect("snapshot").generation(), 10);
            assert_eq!(
                state(&reopened),
                vec![Some(1), Some(1), Some(1), Some(1), None, None]
            );
            reopened.close().expect("close recovered store");
        }
    }
}

type State = Vec<Option<u64>>;

fn document(id: u128, revision: u64) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vec![id as f32, 1.0],
    )
    .with_text(format!("note {id} revision {revision}"))
}

fn upsert(store: &Store, documents: &[(u128, u64)]) {
    store
        .ingest(IngestBatch::new(
            documents
                .iter()
                .map(|&(id, revision)| document(id, revision))
                .collect(),
        ))
        .expect("commit upsert batch");
}

fn state(store: &Store) -> State {
    let ids = IDS.map(DocId::new);
    store
        .get_documents(&ids, DocumentFields::NONE)
        .expect("read documents")
        .into_iter()
        .map(|document| document.map(|document| document.revision.get()))
        .collect()
}

fn wal_len(directory: &Path) -> usize {
    std::fs::metadata(directory.join("wal.ze"))
        .expect("WAL metadata")
        .len() as usize
}

/// Copies the store files (not the writer lock) with `wal.ze` cut to `cut`.
fn torn_copy(source: &Path, wal: &[u8], cut: usize) -> TempDir {
    let copy = tempdir().expect("torn copy directory");
    for entry in std::fs::read_dir(source).expect("list store") {
        let entry = entry.expect("store entry");
        let name = entry.file_name();
        if name == STORE_LOCK_FILE || name == "wal.ze" {
            continue;
        }
        std::fs::copy(entry.path(), copy.path().join(&name)).expect("copy store file");
    }
    std::fs::write(copy.path().join("wal.ze"), &wal[..cut]).expect("write torn WAL");
    copy
}

#[test]
fn every_wal_tear_across_multi_document_batches_recovers_whole_batches() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    // Batch 0 is the committed baseline the sweep starts after.
    upsert(&store, &[(1, 1), (2, 1)]);
    let mut ends = vec![wal_len(directory.path())];
    let mut states = vec![state(&store)];
    // Batch 1 replaces a head and adds a body: four records in one batch.
    upsert(&store, &[(1, 2), (3, 1), (4, 1), (5, 1)]);
    ends.push(wal_len(directory.path()));
    states.push(state(&store));
    // Batch 2 deletes across two batches' documents in one record.
    store
        .delete(DeleteBatch::new(vec![DocId::new(2), DocId::new(3)]))
        .expect("commit delete batch");
    ends.push(wal_len(directory.path()));
    states.push(state(&store));
    // Batch 3 moves two documents together.
    upsert(&store, &[(4, 2), (5, 2)]);
    ends.push(wal_len(directory.path()));
    states.push(state(&store));
    store.close().expect("close source store");
    let wal = std::fs::read(directory.path().join("wal.ze")).expect("read WAL");

    let mut outcomes = [0_usize; 3];
    for cut in ends[0]..=ends[ends.len() - 1] {
        let whole = ends
            .iter()
            .rposition(|end| *end <= cut)
            .expect("baseline end");
        let expected = &states[whole];
        let torn = torn_copy(directory.path(), &wal, cut);
        let clean_end = matches!(replay(&wal[..cut]).terminator, ReplayTerminator::CleanEnd);

        // A reader never repairs: it sees whole batches at a record boundary
        // and refuses a torn final record, typed, instead of guessing.
        match Store::open(torn.path(), OpenOptions::read_only()) {
            Ok(reader) => {
                assert!(
                    clean_end,
                    "cut {cut}: read-only open accepted a torn record"
                );
                assert_eq!(&state(&reader), expected, "cut {cut}: read-only state");
                reader.close().expect("close reader");
            }
            Err(error) => {
                assert!(
                    !clean_end,
                    "cut {cut}: read-only open refused a clean WAL: {error}"
                );
                assert!(
                    matches!(
                        error,
                        StoreError::WalRecovery(WalRecoveryError::CorruptAt {
                            reason: CorruptionReason::Record {
                                location: CorruptionLocation::Tail,
                                error: RecordError::HeaderTruncated { .. }
                                    | RecordError::BodyTruncated { .. },
                            },
                            ..
                        })
                    ),
                    "cut {cut}: read-only refusal was {error:?}"
                );
            }
        }

        // The writer cuts an interrupted final append, so a crash at any
        // byte reopens with whole batches and keeps accepting writes.
        let writer = Store::open(torn.path(), OpenOptions::default())
            .unwrap_or_else(|error| panic!("cut {cut}: writable open failed: {error}"));
        assert_eq!(&state(&writer), expected, "cut {cut}: recovered state");
        upsert(&writer, &[(9, 1), (1, 9)]);
        writer.close().expect("close writer");
        let reopened = Store::open(torn.path(), OpenOptions::default())
            .unwrap_or_else(|error| panic!("cut {cut}: reopen after new writes failed: {error}"));
        let mut after = expected.clone();
        after[0] = Some(9);
        after[5] = Some(1);
        assert_eq!(state(&reopened), after, "cut {cut}: state after new writes");
        reopened.close().expect("close reopened");
        outcomes[usize::from(clean_end) + usize::from(cut == ends[whole])] += 1;
    }
    eprintln!(
        "batch_atomicity cuts={} torn_record={} inner_record_boundary={} batch_boundary={}",
        ends[ends.len() - 1] - ends[0] + 1,
        outcomes[0],
        outcomes[1],
        outcomes[2]
    );
    assert!(
        outcomes.iter().all(|count| *count > 0),
        "every cut class ran"
    );
}

fn member_wal(directory: &Path, members: &[(u32, u32, u128)]) {
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("policy");
    let writer = WalWriter::create(&StdVfs, &directory.join("wal.ze"), LogSeq::new(1), policy)
        .expect("create WAL");
    for &(index, count, id) in members {
        let body = encode_upsert_v2(&document(id, 1)).expect("encode upsert");
        let payload = encode_upsert_v2_batch_member(index, count, &body).expect("frame member");
        writer
            .commit(UPSERT_V2_BATCH_MEMBER, &payload)
            .expect("commit member");
    }
}

#[test]
fn a_batch_member_that_continues_nothing_fails_open_loudly() {
    for (members, orphan) in [
        // Member 1 with no member 0 before it.
        (vec![(1, 2, 7)], (1, 2)),
        // Member 1 of a three-record batch after member 0 of a two-record one.
        (vec![(0, 2, 7), (1, 3, 8)], (1, 3)),
        // Member 2 after a complete two-record batch.
        (vec![(0, 2, 7), (1, 2, 8), (2, 3, 9)], (2, 3)),
    ] {
        let directory = tempdir().expect("store directory");
        member_wal(directory.path(), &members);
        let error = Store::open(directory.path(), OpenOptions::default())
            .err()
            .expect("an orphan member must refuse open");
        assert!(
            matches!(
                error,
                StoreError::WalMutation {
                    op: UPSERT_V2_BATCH_MEMBER,
                    source: PayloadError::OrphanBatchMember { index, count },
                    ..
                } if (index, count) == orphan
            ),
            "{members:?}: {error:?}"
        );
    }
}

#[test]
fn a_complete_hand_framed_batch_opens_and_a_cut_one_is_absent() {
    let complete = tempdir().expect("complete directory");
    member_wal(complete.path(), &[(0, 2, 7), (1, 2, 8)]);
    let store = Store::open(complete.path(), OpenOptions::read_only()).expect("open complete");
    let found = store
        .get_documents(&[DocId::new(7), DocId::new(8)], DocumentFields::NONE)
        .expect("read complete batch");
    assert!(found.iter().all(Option::is_some), "complete batch visible");
    store.close().expect("close complete");

    // A batch cut short by a new member 0 never returned: it is absent.
    let cut = tempdir().expect("cut directory");
    member_wal(cut.path(), &[(0, 3, 7), (1, 3, 8), (0, 2, 5), (1, 2, 6)]);
    let store = Store::open(cut.path(), OpenOptions::read_only()).expect("open cut");
    let found = store
        .get_documents(&[5, 6, 7, 8].map(DocId::new), DocumentFields::NONE)
        .expect("read after cut batch")
        .iter()
        .map(Option::is_some)
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [true, true, false, false],
        "cut batch absent, next whole"
    );
    store.close().expect("close cut");
}

#[test]
fn a_torn_final_record_longer_than_any_group_is_not_cut() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    upsert(&store, &[(1, 1)]);
    store.close().expect("close store");
    let path = directory.path().join("wal.ze");
    let mut wal = std::fs::read(&path).expect("read WAL");
    // A header that declares a payload no store writer can append, then a
    // few bytes: a damaged length field, not an interrupted append.
    wal.extend_from_slice(&(32_u32 * 1024 * 1024).to_le_bytes());
    wal.extend_from_slice(&2_u64.to_le_bytes());
    wal.extend_from_slice(&7_u16.to_le_bytes());
    wal.extend_from_slice(&[0x5a; 64]);
    std::fs::write(&path, &wal).expect("write damaged WAL");

    let error = Store::open(directory.path(), OpenOptions::default())
        .err()
        .expect("an impossible record length must refuse open");
    assert!(
        matches!(
            error,
            StoreError::WalRecovery(WalRecoveryError::CorruptAt {
                reason: CorruptionReason::Record {
                    error: RecordError::BodyTruncated { .. },
                    ..
                },
                ..
            })
        ),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read(&path).expect("reread WAL"),
        wal,
        "WAL untouched"
    );
}
