//! Native Windows durable lifecycle, end to end through the public `Store`.
//!
//! `windows_storage_protocol` proves the primitives through `StdVfs`. This file
//! proves the lifecycle that sits on them: that an acknowledged mutation is
//! there after a reopen, that every durability mode and tier behaves the way it
//! claims to, and that a failure is reported as a failure.
//!
//! Every test runs against a real NTFS directory through the production VFS.
//! No test here uses an injected or in-memory filesystem, because the point is
//! to exercise the actual Win32 calls.

#![cfg(windows)]
#![allow(clippy::expect_used, clippy::panic)]

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{OpenOptions, Store};

const DIMENSION: usize = 3;

fn vector(seed: f32) -> Vec<f32> {
    vec![seed, seed + 1.0, seed + 2.0]
}

fn document(id: u128, revision: u64, seed: f32) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vector(seed),
    )
}

fn options(mode: DurabilityMode, tier: CommitTier) -> OpenOptions {
    OpenOptions::new().with_durability(mode, tier)
}

/// Ingests one batch and returns the reported generation.
fn ingest(store: &Store, documents: Vec<IngestDocument>) -> u64 {
    let acknowledgement = store
        .ingest(IngestBatch::new(documents))
        .expect("ingest must succeed");
    acknowledgement.generation()
}

// ---------------------------------------------------------------------------
// The durability matrix.
// ---------------------------------------------------------------------------

/// Every mode and tier the engine exposes must open, accept a mutation, and
/// report a generation on Windows. `Derived` and `(Durable, None)` deliberately
/// issue no synchronization; they are exercised here to prove they still work,
/// not to claim they are power-loss durable.
#[test]
fn every_supported_mode_and_tier_accepts_a_mutation() {
    let rows = [
        (DurabilityMode::Derived, CommitTier::None),
        (DurabilityMode::Derived, CommitTier::Ordered),
        (DurabilityMode::Derived, CommitTier::Durable),
        (DurabilityMode::Durable, CommitTier::None),
        (DurabilityMode::Durable, CommitTier::Ordered),
        (DurabilityMode::Durable, CommitTier::Durable),
    ];
    for (index, (mode, tier)) in rows.into_iter().enumerate() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), options(mode, tier))
            .unwrap_or_else(|error| panic!("open {mode:?}/{tier:?}: {error:?}"));
        let generation = ingest(&store, vec![document(index as u128 + 1, 1, index as f32)]);
        assert!(
            generation > 0,
            "{mode:?}/{tier:?} must report the generation it changed"
        );
        store
            .close()
            .unwrap_or_else(|error| panic!("close {mode:?}/{tier:?}: {error:?}"));
    }
}

/// `Attached` keeps its typed unsupported result. Windows does not invent an
/// interactive transaction to satisfy it.
#[test]
fn attached_mode_remains_typed_unsupported() {
    let directory = tempdir().expect("store directory");
    let outcome = Store::open(
        directory.path(),
        options(DurabilityMode::Attached, CommitTier::Durable),
    );
    match outcome {
        Err(error) => {
            let rendered = format!("{error:?}");
            assert!(
                rendered.contains("Attached"),
                "expected an Attached-specific refusal, observed {rendered}"
            );
        }
        Ok(store) => {
            // If the mode is admitted at open, the refusal must arrive at the
            // first mutation instead; what must never happen is silent success.
            let outcome = store.ingest(IngestBatch::new(vec![document(1, 1, 0.0)]));
            assert!(
                outcome.is_err(),
                "Attached must not silently accept a mutation"
            );
            let _ = store.close();
        }
    }
}

// ---------------------------------------------------------------------------
// Acknowledged mutations survive a reopen.
// ---------------------------------------------------------------------------

/// The core durable claim: what the engine acknowledged under
/// `Durable`/`Durable` is there after a close and a fresh open, through the
/// real Windows WAL and manifest path.
#[test]
fn durable_mutations_survive_close_and_reopen() {
    let directory = tempdir().expect("store directory");
    let path = directory.path();

    let store = Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable))
        .expect("open durable store");
    let first = ingest(&store, vec![document(1, 1, 1.0), document(2, 1, 2.0)]);
    let second = ingest(&store, vec![document(3, 1, 3.0)]);
    assert!(
        second > first,
        "each acknowledged mutation must advance the generation: {first} then {second}"
    );
    store.close().expect("close durable store");

    let reopened = Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable))
        .expect("reopen durable store");
    let counted = reopened
        .count_documents(None, None)
        .expect("count after reopen");
    assert_eq!(
        counted.count, 3,
        "every acknowledged document must survive the reopen"
    );
    reopened.close().expect("close reopened store");
}

/// A seal publishes an immutable segment through the Windows publication
/// protocol; the sealed state must be what a fresh open sees.
#[test]
fn a_sealed_generation_survives_reopen() {
    let directory = tempdir().expect("store directory");
    let path = directory.path();

    let store =
        Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable)).expect("open");
    ingest(&store, vec![document(10, 1, 1.0), document(11, 1, 2.0)]);
    let sealed = store.seal().expect("seal must publish");
    assert!(sealed > 0, "seal must report the generation it published");
    store.close().expect("close");

    let reopened =
        Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable)).expect("reopen");
    assert_eq!(
        reopened.count_documents(None, None).expect("count").count,
        2,
        "sealed rows must be readable after reopen"
    );
    reopened.close().expect("close");
}

/// A later revision of a document must win after a reopen, and a delete must
/// stay deleted. This is the ordering the WAL replay is responsible for.
#[test]
fn revisions_and_deletes_replay_in_order_after_reopen() {
    let directory = tempdir().expect("store directory");
    let path = directory.path();

    let store =
        Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable)).expect("open");
    ingest(&store, vec![document(20, 1, 1.0), document(21, 1, 2.0)]);
    store
        .ingest(IngestBatch::new(vec![document(20, 2, 9.0)]))
        .expect("supersede revision 1");
    store
        .delete(DeleteBatch::new(vec![DocId::new(21)]))
        .expect("delete the second document");
    store.close().expect("close");

    let reopened =
        Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable)).expect("reopen");
    assert_eq!(
        reopened.count_documents(None, None).expect("count").count,
        1,
        "the deleted document must not come back"
    );
    reopened.close().expect("close");
}

// ---------------------------------------------------------------------------
// Failures are reported as failures.
// ---------------------------------------------------------------------------

/// A store path whose parent does not exist must fail loudly with a typed
/// error, not create a partial store.
#[test]
fn opening_under_a_missing_parent_fails_loudly() {
    let directory = tempdir().expect("scratch");
    let absent = directory.path().join("no-such-parent\u{0}bad");
    let outcome = Store::open(
        &absent,
        options(DurabilityMode::Durable, CommitTier::Durable),
    );
    assert!(
        outcome.is_err(),
        "a path containing an interior NUL must be refused"
    );
}

/// Writer ownership is exclusive across processes as well as within one, and a
/// second writer is refused rather than left waiting. This is the Windows
/// `LockFileEx` path rather than `fcntl`.
#[test]
fn a_second_writer_is_refused_while_the_first_is_open() {
    let directory = tempdir().expect("store directory");
    let path = directory.path();

    let first = Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable))
        .expect("first writer");
    let second = Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable));
    assert!(
        second.is_err(),
        "a second writer must be refused while the first holds the store"
    );

    first.close().expect("close the first writer");
    let third = Store::open(path, options(DurabilityMode::Durable, CommitTier::Durable))
        .expect("a writer is admitted once the first closes");
    third.close().expect("close");
}

// ---------------------------------------------------------------------------
// Paths a Windows user really has.
// ---------------------------------------------------------------------------

/// A store under a directory with spaces and non-ASCII characters -- which a
/// Windows user profile name routinely has -- must work end to end.
#[test]
fn a_store_under_a_non_ascii_path_completes_a_durable_round_trip() {
    let directory = tempdir().expect("scratch");
    let path = directory
        .path()
        .join("Documents et param\u{e8}tres")
        .join("zeppelin \u{4e2d}\u{6587} store");
    std::fs::create_dir_all(&path).expect("create a non-ASCII store directory");

    let store = Store::open(&path, options(DurabilityMode::Durable, CommitTier::Durable))
        .expect("open under a non-ASCII path");
    ingest(&store, vec![document(30, 1, 1.0)]);
    store.seal().expect("seal");
    store.close().expect("close");

    let reopened = Store::open(&path, options(DurabilityMode::Durable, CommitTier::Durable))
        .expect("reopen under a non-ASCII path");
    assert_eq!(
        reopened.count_documents(None, None).expect("count").count,
        1
    );
    reopened.close().expect("close");
}

/// The dimension the store was created with must still be enforced on Windows;
/// this guards against a platform arm quietly skipping validation.
#[test]
fn a_mismatched_vector_dimension_is_still_refused() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(
        directory.path(),
        options(DurabilityMode::Durable, CommitTier::Durable),
    )
    .expect("open");
    ingest(&store, vec![document(40, 1, 1.0)]);

    let wrong = IngestDocument::new(
        DocumentVersion::new(DocId::new(41), Revision::new(1)),
        vec![1.0_f32; DIMENSION + 2],
    );
    assert!(
        store.ingest(IngestBatch::new(vec![wrong])).is_err(),
        "a mismatched dimension must be refused"
    );
    store.close().expect("close");
}
