#![allow(clippy::expect_used, clippy::indexing_slicing)]
//! Expected-revision conditions on upsert and delete batches (ZE-224).

use std::path::Path;
use std::sync::Arc;

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, ExpectedRevision, IngestBatch, IngestDocument,
    IngestError, Revision,
};
use zeppelin_embed::lifecycle::{DocumentFields, OpenOptions, Store};

fn document(id: u128, revision: u64) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vec![1.0, 0.0],
    )
}

fn exactly(revision: u64) -> Option<ExpectedRevision> {
    Some(ExpectedRevision::Exactly(Revision::new(revision)))
}

fn upsert(store: &Store, documents: Vec<IngestDocument>) -> Result<u64, IngestError> {
    store
        .ingest(IngestBatch::new(documents))
        .map(|ack| ack.generation())
}

fn live_revision(store: &Store, id: u128) -> Option<u64> {
    store
        .get_documents(&[DocId::new(id)], DocumentFields::NONE)
        .expect("get document")[0]
        .as_ref()
        .map(|document| document.revision.get())
}

fn generation(store: &Store) -> u64 {
    store.snapshot().expect("snapshot").generation()
}

fn wal_bytes(directory: &Path) -> u64 {
    std::fs::metadata(directory.join("wal.ze"))
        .expect("wal metadata")
        .len()
}

fn assert_conflict(
    result: Result<u64, IngestError>,
    index: usize,
    id: u128,
    expected: ExpectedRevision,
    current: Option<u64>,
) {
    match result {
        Err(IngestError::RevisionConflict {
            index: actual_index,
            doc_id,
            expected: actual_expected,
            current: actual_current,
        }) => {
            assert_eq!(actual_index, index, "conflict index");
            assert_eq!(doc_id, DocId::new(id), "conflict document");
            assert_eq!(actual_expected, expected, "conflict expectation");
            assert_eq!(
                actual_current.map(Revision::get),
                current,
                "conflict current revision"
            );
        }
        other => panic!("expected a revision conflict, got {other:?}"),
    }
}

#[test]
fn a_matching_expected_revision_commits_the_upsert() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    upsert(&store, vec![document(1, 1)]).expect("seed");

    let committed = upsert(
        &store,
        vec![document(1, 2).with_expected_revision(ExpectedRevision::Exactly(Revision::new(1)))],
    )
    .expect("conditional upsert");

    assert_eq!(committed, generation(&store));
    assert_eq!(live_revision(&store, 1), Some(2));
}

#[test]
fn a_mismatched_expected_revision_writes_nothing() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    upsert(&store, vec![document(1, 3)]).expect("seed");
    let before = (generation(&store), wal_bytes(directory.path()));

    assert_conflict(
        upsert(
            &store,
            vec![
                document(1, 4).with_expected_revision(ExpectedRevision::Exactly(Revision::new(2))),
            ],
        ),
        0,
        1,
        ExpectedRevision::Exactly(Revision::new(2)),
        Some(3),
    );

    assert_eq!((generation(&store), wal_bytes(directory.path())), before);
    assert_eq!(live_revision(&store, 1), Some(3));
}

#[test]
fn an_absent_condition_rejects_a_live_id_and_accepts_unknown_and_deleted_ids() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    upsert(&store, vec![document(1, 1), document(2, 5)]).expect("seed");
    store
        .delete(DeleteBatch::new(vec![DocId::new(2)]))
        .expect("delete");
    let before = (generation(&store), wal_bytes(directory.path()));

    assert_conflict(
        upsert(
            &store,
            vec![document(1, 2).with_expected_revision(ExpectedRevision::Absent)],
        ),
        0,
        1,
        ExpectedRevision::Absent,
        Some(1),
    );
    assert_eq!((generation(&store), wal_bytes(directory.path())), before);

    upsert(
        &store,
        vec![
            document(3, 1).with_expected_revision(ExpectedRevision::Absent),
            document(2, 6).with_expected_revision(ExpectedRevision::Absent),
        ],
    )
    .expect("absent condition holds for unknown and deleted ids");
    assert_eq!(live_revision(&store, 3), Some(1));
    assert_eq!(live_revision(&store, 2), Some(6));
}

#[test]
fn one_failed_condition_aborts_the_whole_batch() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    upsert(&store, vec![document(1, 1), document(2, 1)]).expect("seed");
    let before = (generation(&store), wal_bytes(directory.path()));

    assert_conflict(
        upsert(
            &store,
            vec![
                document(1, 2).with_expected_revision(ExpectedRevision::Exactly(Revision::new(1))),
                document(3, 1),
                document(2, 2).with_expected_revision(ExpectedRevision::Exactly(Revision::new(7))),
            ],
        ),
        2,
        2,
        ExpectedRevision::Exactly(Revision::new(7)),
        Some(1),
    );

    assert_eq!((generation(&store), wal_bytes(directory.path())), before);
    assert_eq!(live_revision(&store, 1), Some(1));
    assert_eq!(live_revision(&store, 2), Some(1));
    assert_eq!(live_revision(&store, 3), None);
}

#[test]
fn conditional_delete_checks_every_condition_before_tombstoning() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    upsert(&store, vec![document(1, 2), document(2, 4)]).expect("seed");
    let before = (generation(&store), wal_bytes(directory.path()));

    let mismatch = store
        .delete(DeleteBatch::conditional(vec![
            (DocId::new(1), exactly(2)),
            (DocId::new(2), exactly(3)),
        ]))
        .map(|ack| ack.generation());
    assert_conflict(
        mismatch,
        1,
        2,
        ExpectedRevision::Exactly(Revision::new(3)),
        Some(4),
    );
    assert_eq!((generation(&store), wal_bytes(directory.path())), before);
    assert_eq!(live_revision(&store, 1), Some(2));

    store
        .delete(DeleteBatch::conditional(vec![
            (DocId::new(1), exactly(2)),
            (DocId::new(2), None),
            (DocId::new(9), Some(ExpectedRevision::Absent)),
        ]))
        .expect("conditional delete");
    assert_eq!(live_revision(&store, 1), None);
    assert_eq!(live_revision(&store, 2), None);
}

#[test]
fn conditions_see_sealed_rows_and_sealed_tombstones() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    upsert(&store, vec![document(1, 3), document(2, 1)]).expect("seed");
    store.seal().expect("seal live rows");
    store
        .delete(DeleteBatch::new(vec![DocId::new(2)]))
        .expect("delete a sealed row");
    store.seal().expect("seal the tombstone");

    assert_conflict(
        upsert(
            &store,
            vec![
                document(1, 4).with_expected_revision(ExpectedRevision::Exactly(Revision::new(2))),
            ],
        ),
        0,
        1,
        ExpectedRevision::Exactly(Revision::new(2)),
        Some(3),
    );
    assert_conflict(
        upsert(
            &store,
            vec![document(1, 4).with_expected_revision(ExpectedRevision::Absent)],
        ),
        0,
        1,
        ExpectedRevision::Absent,
        Some(3),
    );
    upsert(
        &store,
        vec![
            document(1, 4).with_expected_revision(ExpectedRevision::Exactly(Revision::new(3))),
            document(2, 2).with_expected_revision(ExpectedRevision::Absent),
        ],
    )
    .expect("sealed conditions hold");
    assert_eq!(live_revision(&store, 1), Some(4));
    assert_eq!(live_revision(&store, 2), Some(2));
}

#[test]
fn racing_compare_and_set_writers_never_lose_an_update() {
    const WRITERS: u64 = 8;
    const INCREMENTS: u64 = 25;
    let directory = tempdir().expect("store directory");
    let store = Arc::new(Store::open(directory.path(), OpenOptions::default()).expect("open"));
    upsert(&store, vec![document(1, 0)]).expect("seed head");

    let threads = (0..WRITERS)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let mut conflicts = 0_u64;
                let mut applied = 0_u64;
                while applied < INCREMENTS {
                    assert!(conflicts < 100_000, "compare-and-set made no progress");
                    let current = live_revision(&store, 1).expect("live head");
                    let next = document(1, current + 1)
                        .with_expected_revision(ExpectedRevision::Exactly(Revision::new(current)));
                    match upsert(&store, vec![next]) {
                        Ok(_) => applied += 1,
                        Err(IngestError::RevisionConflict { .. }) => conflicts += 1,
                        Err(error) => panic!("unexpected write error: {error}"),
                    }
                }
                conflicts
            })
        })
        .collect::<Vec<_>>();
    let conflicts = threads
        .into_iter()
        .map(|thread| thread.join().expect("writer thread"))
        .sum::<u64>();

    assert_eq!(live_revision(&store, 1), Some(WRITERS * INCREMENTS));
    eprintln!("conflicts retried: {conflicts}");
}
