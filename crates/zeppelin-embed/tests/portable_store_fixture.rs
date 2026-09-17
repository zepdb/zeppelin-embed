//! A deterministic store fixture whose logical results are specified
//! independently of the engine, so the same bytes can be checked on any
//! platform.
//!
//! This file is deliberately **not** `cfg(windows)`. Its purpose is the
//! cross-platform cell of the Windows port: a store written on one platform
//! must open on another and return the same logical answers. The generator and
//! the verifier are the same code, so whichever platform wrote the bytes, the
//! reader checks them against expectations that were written down by hand
//! rather than read back out of the engine.
//!
//! Three entry points:
//!
//! * `a_portable_fixture_round_trips_in_this_process` -- always runs. Builds,
//!   closes, reopens and verifies on whatever platform is executing.
//! * `emit_portable_fixture` -- ignored by default. Run deliberately with
//!   `ZE_PORTABLE_FIXTURE_OUT` set to write the fixture somewhere it can be
//!   handed to another platform.
//! * `a_fixture_written_elsewhere_opens_here` -- ignored by default. Run
//!   deliberately with `ZE_PORTABLE_FIXTURE_DIR` pointing at a fixture another
//!   platform produced.
//!
//! The two ignored tests are the exchange. Until both halves have actually run
//! on two platforms, that cell is unexecuted, and reporting it as ignored is
//! how this file says so rather than passing vacuously.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::Path;

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};

/// Fixture vector width.
const DIMENSION: usize = 8;

/// Documents the fixture ingests, as `(id, revision, seed, text)`.
///
/// The first two ids deliberately share their low 64 bits and differ only
/// above them, so a reader that truncates a document id to 64 bits collapses
/// them and fails. `SUPERSEDED` is re-ingested at a higher revision, and
/// `DELETED` is removed, so the expected live set is neither "everything
/// ingested" nor "the last write wins" by accident.
const HIGH_A: u128 = (1_u128 << 64) | 0x0000_0000_0000_0007;
const HIGH_B: u128 = (2_u128 << 64) | 0x0000_0000_0000_0007;
const PLAIN: u128 = 11;
const SUPERSEDED: u128 = 12;
const DELETED: u128 = 13;

/// Multibyte UTF-8 that must survive a round trip byte for byte.
const UNICODE_TEXT: &str = "zeppelin \u{4e2d}\u{6587} caf\u{e9} \u{1f6f0}";

fn options() -> OpenOptions {
    OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable)
}

/// Deterministic vector for a seed. Integer-valued so every coordinate is
/// exactly representable and the oracle's arithmetic is exact.
fn fixture_vector(seed: u32) -> Vec<f32> {
    (0..DIMENSION)
        .map(|index| ((seed as usize * 7 + index * 3) % 17) as f32)
        .collect()
}

fn document(id: u128, revision: u64, seed: u32) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        fixture_vector(seed),
    )
}

/// Writes the fixture into `path`, leaving a closed, sealed store behind.
fn build_fixture(path: &Path) {
    let store = Store::open(path, options()).expect("open fixture store");

    store
        .ingest(IngestBatch::new(vec![
            document(HIGH_A, 1, 1).with_text(UNICODE_TEXT),
            document(HIGH_B, 1, 2),
            document(PLAIN, 1, 3).with_text("plain ascii text"),
            document(SUPERSEDED, 1, 4),
            document(DELETED, 1, 5),
        ]))
        .expect("ingest fixture batch");

    // A later revision must win; the earlier one must not resurface.
    store
        .ingest(IngestBatch::new(vec![document(SUPERSEDED, 2, 9)]))
        .expect("supersede");

    store
        .delete(DeleteBatch::new(vec![DocId::new(DELETED)]))
        .expect("delete");

    store.seal().expect("seal fixture");
    store.close().expect("close fixture");
}

/// The expected live document set, written down by hand rather than read back
/// out of the engine.
fn expected_live() -> Vec<(u128, u64, u32)> {
    vec![
        (HIGH_A, 1, 1),
        (HIGH_B, 1, 2),
        (PLAIN, 1, 3),
        (SUPERSEDED, 2, 9),
    ]
}

/// An independent brute-force top-k over the expected live set.
///
/// The engine's exact score is `-squared_L2`, larger-is-better, with ties
/// broken by ascending document id. This recomputes that in `f64` from the
/// fixture definition; it never calls the engine's scorer, decoder or
/// comparator, so agreement is evidence rather than tautology.
fn oracle_top_k(query: &[f32], k: usize) -> Vec<(u128, f32)> {
    let mut scored = expected_live()
        .into_iter()
        .map(|(id, _revision, seed)| {
            let vector = fixture_vector(seed);
            let distance: f64 = query
                .iter()
                .zip(vector.iter())
                .map(|(left, right)| {
                    let delta = f64::from(*left) - f64::from(*right);
                    delta * delta
                })
                .sum();
            (id, -distance as f32)
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.truncate(k);
    scored
}

/// Checks every logical expectation against an already-open store.
fn verify_fixture(store: &Store) {
    let live = expected_live();

    // 1. Exactly the expected documents are live.
    assert_eq!(
        store
            .count_documents(None, None)
            .expect("count fixture")
            .count,
        live.len() as u64,
        "the live document count must match the specified fixture"
    );

    // 2. Exact search agrees with the independent oracle, document for
    //    document and bit for bit.
    let query = fixture_vector(3);
    let expected = oracle_top_k(&query, live.len());
    let outcome = store
        .search(
            SearchRequest::new(&query),
            live.len(),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("exact search");
    assert_eq!(
        outcome.candidates.len(),
        expected.len(),
        "exact search returned a different number of candidates than the oracle"
    );
    for (candidate, (expected_id, expected_score)) in outcome.candidates.iter().zip(expected.iter())
    {
        let document = candidate
            .document()
            .expect("an exact candidate must carry its document identity");
        assert_eq!(
            document.doc_id().get(),
            *expected_id,
            "exact ordering disagrees with the oracle: {:?} vs {expected:?}",
            outcome.candidates
        );
        assert_eq!(
            candidate.score().to_bits(),
            expected_score.to_bits(),
            "exact score for {expected_id} disagrees with the oracle bit-for-bit"
        );
    }

    // 3. Full-width document ids survive: two ids sharing their low 64 bits
    //    must both be present and distinct.
    let returned = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document().map(|document| document.doc_id().get()))
        .collect::<Vec<_>>();
    assert!(
        returned.contains(&HIGH_A) && returned.contains(&HIGH_B),
        "a reader that truncates document ids to 64 bits would collapse these: {returned:?}"
    );

    // 4. The superseded revision is the surviving one, and the deleted
    //    document is absent.
    let superseded = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .find(|document| document.doc_id().get() == SUPERSEDED)
        .expect("the superseded document must still be live");
    assert_eq!(
        superseded.revision().get(),
        2,
        "the later revision must win after a reopen"
    );
    assert!(
        !returned.contains(&DELETED),
        "a deleted document must not come back: {returned:?}"
    );

    // 5. Stored text round-trips byte for byte, multibyte UTF-8 included.
    let text = store
        .stored_text(DocumentVersion::new(DocId::new(HIGH_A), Revision::new(1)))
        .expect("stored text lookup");
    assert_eq!(
        text.as_deref(),
        Some(UNICODE_TEXT),
        "multibyte stored text did not survive the round trip"
    );

    // 6. A request for more results than exist returns what exists, not an
    //    error and not padding.
    let over = store
        .search(
            SearchRequest::new(&query),
            live.len() + 32,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("over-wide search");
    assert_eq!(over.candidates.len(), live.len());

    // 7. A mismatched query dimension is refused rather than silently scored.
    let wrong = vec![0.0_f32; DIMENSION + 3];
    assert!(
        store
            .search(
                SearchRequest::new(&wrong),
                1,
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .is_err(),
        "a mismatched query dimension must be refused"
    );
}

/// Builds and verifies the fixture in one process on the running platform.
#[test]
fn a_portable_fixture_round_trips_in_this_process() {
    let directory = tempdir().expect("fixture directory");
    build_fixture(directory.path());

    let store = Store::open(directory.path(), options()).expect("reopen fixture");
    verify_fixture(&store);
    store.close().expect("close");
}

/// Writes the fixture to `ZE_PORTABLE_FIXTURE_OUT` so another platform can
/// read it. Deliberately ignored: this is half of a two-platform exchange, run
/// on purpose rather than as part of an ordinary suite.
#[test]
#[ignore = "cross-platform exchange: set ZE_PORTABLE_FIXTURE_OUT and run deliberately"]
fn emit_portable_fixture() {
    let target = std::env::var_os("ZE_PORTABLE_FIXTURE_OUT")
        .map(std::path::PathBuf::from)
        .expect("ZE_PORTABLE_FIXTURE_OUT must name an empty directory");
    std::fs::create_dir_all(&target).expect("create fixture output directory");
    assert!(
        std::fs::read_dir(&target)
            .expect("list fixture output directory")
            .next()
            .is_none(),
        "ZE_PORTABLE_FIXTURE_OUT must be empty so the fixture is unambiguous"
    );
    build_fixture(&target);

    // Re-verify what was just written, so an emitted fixture is never handed
    // over unchecked.
    let store = Store::open(&target, options()).expect("reopen emitted fixture");
    verify_fixture(&store);
    store.close().expect("close");
    println!("emitted portable fixture to {}", target.display());
}

/// Opens a fixture another platform wrote and checks it against the same
/// hand-written expectations. Ignored until such a fixture actually exists.
#[test]
#[ignore = "cross-platform exchange: set ZE_PORTABLE_FIXTURE_DIR and run deliberately"]
fn a_fixture_written_elsewhere_opens_here() {
    let source = std::env::var_os("ZE_PORTABLE_FIXTURE_DIR")
        .map(std::path::PathBuf::from)
        .expect("ZE_PORTABLE_FIXTURE_DIR must name a fixture written on another platform");
    assert!(
        source.is_dir(),
        "ZE_PORTABLE_FIXTURE_DIR must be a directory: {}",
        source.display()
    );

    // Read-only, so verifying a supplied fixture cannot mutate it.
    let store = Store::open(&source, OpenOptions::read_only())
        .expect("open a fixture written on another platform");
    verify_fixture(&store);
    store.close().expect("close");
    println!("verified foreign fixture at {}", source.display());
}
