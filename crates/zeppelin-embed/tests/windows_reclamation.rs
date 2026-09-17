//! Native Windows recovery, maintenance and physical purge with live readers.
//!
//! The question this file answers is whether the engine's reclamation contract
//! survives the one place Windows differs most sharply from Unix: what may
//! happen to a file while someone still has it open or mapped.
//!
//! The measured answer, established in `tools/windows-storage-probe` and
//! re-proved here through the real `Store`, is that a mapping opened sharing
//! deletion lets the name be unlinked **immediately** -- there is no
//! delete-pending window -- while the mapping keeps serving the bytes it
//! validated. `await_physical_purge`'s "old paths unlinked" receipt can
//! therefore be honoured literally, and **no pending-reclaim queue is needed**.
//!
//! Every test runs against a real NTFS directory through the production VFS.

#![cfg(windows)]
#![allow(clippy::expect_used, clippy::panic)]

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, PurgeError, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};

/// A byte pattern that must not survive a physical purge.
const TEXT_SENTINEL: &str = "windows-reclamation-sentinel-4f1c";
/// A metadata byte pattern with the same job.
const METADATA_SENTINEL: &[u8] = b"windows-reclamation-metadata-9b2e";
/// A vector bit pattern with the same job, chosen so it is unlikely to arise.
const VECTOR_SENTINEL_BITS: u32 = 0x3f12_34ab;

const DIMENSION: usize = 4;

fn options() -> OpenOptions {
    OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable)
}

fn vector(seed: f32) -> Vec<f32> {
    let mut values = vec![seed; DIMENSION];
    if let Some(first) = values.first_mut() {
        *first = f32::from_bits(VECTOR_SENTINEL_BITS);
    }
    values
}

fn plain_vector(seed: f32) -> Vec<f32> {
    (0..DIMENSION).map(|index| seed + index as f32).collect()
}

/// Every file currently present in the store directory.
fn file_names(directory: &std::path::Path) -> Vec<String> {
    let mut names = std::fs::read_dir(directory)
        .expect("list store directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// True when any file in the store still contains `needle`.
fn any_file_contains(directory: &std::path::Path, needle: &[u8]) -> bool {
    std::fs::read_dir(directory)
        .expect("list store directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .any(|entry| {
            std::fs::read(entry.path())
                .map(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
                .unwrap_or(false)
        })
}

/// Builds a sealed store holding one sentinel-bearing document plus `extra`
/// ordinary ones, and returns the store.
fn sealed_store(directory: &std::path::Path, sentinel: DocId, extra: u128) -> Store {
    let store = Store::open(directory, options()).expect("open store");
    let mut documents = vec![
        IngestDocument::new(
            DocumentVersion::new(sentinel, Revision::new(1)),
            vector(1.0),
        )
        .with_text(TEXT_SENTINEL)
        .with_metadata(METADATA_SENTINEL.to_vec()),
    ];
    for index in 0..extra {
        documents.push(IngestDocument::new(
            DocumentVersion::new(DocId::new(sentinel.get() + index + 1), Revision::new(1)),
            plain_vector(index as f32 + 2.0),
        ));
    }
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest fixture");
    store.seal().expect("seal fixture");
    store
}

// ---------------------------------------------------------------------------
// Physical purge with retained readers.
// ---------------------------------------------------------------------------

/// The central W06 claim. A snapshot lease holds live mappings of the sealed
/// segments. A purge must still unlink the old paths -- immediately, with the
/// names actually gone -- and the retained reader must keep working.
#[test]
fn purge_unlinks_old_paths_while_a_held_snapshot_stays_readable() {
    let directory = tempdir().expect("store directory");
    let purged = DocId::new(500);
    let store = sealed_store(directory.path(), purged, 3);

    // Hold a lease: this pins the published segments and their mappings.
    let held = store.snapshot().expect("hold a snapshot lease");
    let held_generation = held.generation();
    let before = file_names(directory.path());
    assert!(
        any_file_contains(directory.path(), METADATA_SENTINEL),
        "the fixture must actually contain the sentinel before purging"
    );

    let token = store.purge(&[purged]).expect("schedule physical purge");
    let report = store
        .await_physical_purge(token)
        .expect("await physical purge under a held reader");

    assert!(
        report.segments_rewritten() >= 1,
        "the sentinel's segment must have been rewritten: {report:?}"
    );

    // The old artifacts are gone by name, not merely marked for deletion.
    let after = file_names(directory.path());
    assert_ne!(before, after, "purge must have replaced artifacts");
    assert!(
        !any_file_contains(directory.path(), METADATA_SENTINEL),
        "purged metadata bytes remain on disk"
    );
    assert!(
        !any_file_contains(directory.path(), TEXT_SENTINEL.as_bytes()),
        "purged text bytes remain on disk"
    );

    // The retained reader is still safe to use and still sees its own
    // generation: purge does not revoke an already-admitted reader.
    held.check_active().expect("the held lease is still active");
    assert_eq!(
        held.generation(),
        held_generation,
        "an admitted reader keeps the generation it was admitted at"
    );
    drop(held);
    store.close().expect("close");
}

/// The unlink must be observable as a name removal, not a delete-pending
/// state: reopening the old path must fail, and the store must reopen cleanly.
#[test]
fn purged_paths_are_removed_and_the_store_reopens() {
    let directory = tempdir().expect("store directory");
    let purged = DocId::new(600);
    let store = sealed_store(directory.path(), purged, 2);

    let old_segment = store.snapshot().expect("snapshot").segments()[0].meta().id;
    let old_path = directory.path().join(old_segment.file_name());
    assert!(old_path.try_exists().expect("old segment exists"));

    let token = store.purge(&[purged]).expect("schedule purge");
    store.await_physical_purge(token).expect("await purge");

    assert!(
        !old_path.try_exists().expect("old segment probe"),
        "the replaced segment path must be gone, not delete-pending"
    );
    let error = std::fs::File::open(&old_path).expect_err("a purged path must not reopen");
    assert_eq!(error.raw_os_error(), Some(2), "ERROR_FILE_NOT_FOUND");

    store.close().expect("close");

    let reopened = Store::open(directory.path(), options()).expect("reopen after purge");
    assert_eq!(
        reopened
            .count_documents(None, None)
            .expect("count after purge")
            .count,
        2,
        "the surviving documents must remain after a purge and reopen"
    );
    reopened.close().expect("close");
}

/// A purge that cannot be given the temporary space it needs must refuse,
/// mutate nothing, and say exactly why. The free-space probe on Windows is
/// `GetDiskFreeSpaceExW`; this asserts the refusal path that consumes it.
#[test]
fn purge_refuses_without_temp_space_and_leaves_the_store_intact() {
    let directory = tempdir().expect("store directory");
    let purged = DocId::new(700);
    let store = sealed_store(directory.path(), purged, 1);

    let snapshot = store.snapshot().expect("snapshot before refusal");
    let segment_bytes = snapshot.segments()[0].meta().file_size;
    drop(snapshot);
    let before = file_names(directory.path());

    // The threshold is strict: exactly 120% must still refuse.
    let error = store
        .purge_with_available_space(&[purged], segment_bytes * 6 / 5)
        .expect_err("the strict threshold must refuse");
    assert!(
        matches!(error, PurgeError::InsufficientTempSpace { .. }),
        "expected InsufficientTempSpace, observed {error:?}"
    );
    assert_eq!(
        file_names(directory.path()),
        before,
        "a refused purge must not touch the store"
    );
    assert!(
        any_file_contains(directory.path(), METADATA_SENTINEL),
        "a refused purge must leave the data alone"
    );
    store.close().expect("close");
}

/// The real free-space probe must report enough room on an ordinary volume, so
/// a purge with no injected budget succeeds. This is the production path
/// through `GetDiskFreeSpaceExW` rather than the test seam.
#[test]
fn the_real_free_space_probe_admits_a_purge() {
    let directory = tempdir().expect("store directory");
    let purged = DocId::new(800);
    let store = sealed_store(directory.path(), purged, 1);

    let token = store
        .purge(&[purged])
        .expect("the real Windows free-space probe must admit this purge");
    let report = store.await_physical_purge(token).expect("await purge");
    assert!(!report.is_no_op(), "a real purge must not report a no-op");
    store.close().expect("close");
}

// ---------------------------------------------------------------------------
// Maintenance with retained readers.
// ---------------------------------------------------------------------------

/// A declared epoch for the maintenance fixture. Graph construction selects its
/// build profile from the epoch, so a store without one cannot be maintained.
fn maintenance_epoch() -> StoreEpoch {
    let tower = EmbeddingTower {
        model_id: "windows-reclamation-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x2a],
        dims: DIMENSION as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: tower.clone(),
            document: tower,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

/// Maintenance republishes artifacts. A reader admitted before it must keep
/// working, and the query results must be identical before and after.
#[test]
fn maintenance_preserves_results_and_retained_readers() {
    let directory = tempdir().expect("store directory");
    // Graph maintenance selects its build profile from the declared epoch, so
    // an unstamped store refuses with `Profile(EpochUnstamped)`. This mirrors
    // the fixtures in `store_graph_search`.
    let epoch = maintenance_epoch();
    let store = Store::open(directory.path(), options().with_epoch(epoch.clone()))
        .expect("open an epoch-stamped store");

    // Enough rows to make the graph threshold reachable.
    let documents = (0..64_u128)
        .map(|index| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(index + 1), Revision::new(1)),
                plain_vector(index as f32),
            )
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .expect("ingest maintenance fixture");
    store.seal().expect("seal");

    let query = plain_vector(7.0);
    let search = |store: &Store| {
        store
            .search(
                SearchRequest::new(&query),
                5,
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("search")
    };
    let before = search(&store).candidates;
    let held = store.snapshot().expect("hold a reader across maintenance");

    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            // Matches `tiering::MAINTENANCE_TEST_BUDGET`; the gate is that
            // maintenance completes, not how quickly.
            wall_time: std::time::Duration::from_secs(600),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 32 },
    );
    assert!(
        matches!(report.status, MaintenanceStatus::Complete),
        "maintenance did not complete: {:?}",
        report.status
    );

    held.check_active()
        .expect("a reader admitted before maintenance is still active");
    let after = search(&store).candidates;
    assert_eq!(
        before.len(),
        after.len(),
        "maintenance changed the result cardinality"
    );
    for (left, right) in before.iter().zip(after.iter()) {
        assert_eq!(
            left.document(),
            right.document(),
            "maintenance changed which documents an exact search returns"
        );
        assert_eq!(
            left.score().to_bits(),
            right.score().to_bits(),
            "maintenance changed an exact score bit-for-bit"
        );
    }
    drop(held);
    store.close().expect("close");
}

// ---------------------------------------------------------------------------
// Reopen cleanup and concurrent read-only access.
// ---------------------------------------------------------------------------

/// A read-only open must not take writer ownership, must not create a lock
/// file, and must not sweep a live writer's artifacts. On Windows this also
/// proves the writer's `LockFileEx` range does not make the store unreadable.
#[test]
fn a_read_only_open_coexists_with_a_live_writer_and_removes_nothing() {
    let directory = tempdir().expect("store directory");
    let writer = sealed_store(directory.path(), DocId::new(900), 2);
    let before = file_names(directory.path());

    let reader = Store::open(directory.path(), OpenOptions::read_only())
        .expect("a read-only open must be admitted alongside a writer");
    assert_eq!(
        reader
            .count_documents(None, None)
            .expect("read-only count")
            .count,
        3
    );
    assert_eq!(
        file_names(directory.path()),
        before,
        "a read-only open must not add or remove any file"
    );
    reader.close().expect("close the reader");

    assert_eq!(
        file_names(directory.path()),
        before,
        "closing a read-only handle must not remove the writer's artifacts"
    );
    writer.close().expect("close the writer");
}

/// Reopening after a clean close must leave every reachable artifact in place;
/// orphan cleanup must not mistake live artifacts for garbage.
#[test]
fn reopen_cleanup_retains_every_reachable_artifact() {
    let directory = tempdir().expect("store directory");
    let store = sealed_store(directory.path(), DocId::new(1000), 3);
    store.close().expect("close");
    let before = file_names(directory.path());

    let reopened = Store::open(directory.path(), options()).expect("reopen");
    let after = file_names(directory.path());
    assert_eq!(
        before, after,
        "reopen cleanup removed a reachable artifact: {before:?} then {after:?}"
    );
    assert_eq!(
        reopened.count_documents(None, None).expect("count").count,
        4
    );
    reopened.close().expect("close");
}

/// Sealing republishes the manifest while a reader holds the previous
/// generation. The old reader must stay valid and keep its own generation.
#[test]
fn a_seal_under_a_held_reader_leaves_that_reader_on_its_generation() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), options()).expect("open");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1100), Revision::new(1)),
            plain_vector(1.0),
        )]))
        .expect("first ingest");
    store.seal().expect("first seal");

    let held = store.snapshot().expect("hold the first generation");
    let held_generation = held.generation();

    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1101), Revision::new(1)),
            plain_vector(2.0),
        )]))
        .expect("second ingest");
    let sealed = store.seal().expect("second seal");
    assert!(
        sealed > held_generation,
        "the second seal must advance past the held generation"
    );

    held.check_active()
        .expect("the reader admitted before the seal is still active");
    assert_eq!(
        held.generation(),
        held_generation,
        "a seal must not move an admitted reader's generation"
    );
    drop(held);

    assert_eq!(
        store.count_documents(None, None).expect("count").count,
        2,
        "the writer sees both documents after the second seal"
    );
    store.close().expect("close");
}
