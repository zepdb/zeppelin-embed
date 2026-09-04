//! Bounded hybrid producers must not move the answer.
//!
//! The control arm for the whole bounded-producer change: every hybrid query
//! is fused a second time from the complete exact lists the store used to
//! hand fusion, and the two results must agree on keys and on `f64` score
//! bits. If a window, an anchor, a cross-fill, or the stability bound is
//! wrong, this is where it shows.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod test_support;

use rand::Rng;
use tempfile::tempdir;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fusion::{
    FusedHit, FusionTermination, HybridQuery, LegBounds, LexicalBounds, LexicalCandidate,
    VectorBounds, VectorCandidate, fuse_bounded,
};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};

const DIMENSION: usize = 6;
const TERMS: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

fn corpus_cases() -> usize {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(12)
}

/// Fuses complete exact lists with the same corpus-wide norm-ball bound used
/// by every vector tier: every alive row is exactly scored, and every matching
/// document carries its exact BM25.
fn offline_full_list_fusion(
    store: &Store,
    vector: &[f32],
    lexical: &TermQuery,
    k: usize,
    corpus_rows: usize,
) -> Vec<FusedHit<DocId>> {
    let vector_leg = store
        .search(
            SearchRequest::new(vector),
            corpus_rows,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("complete exact vector list");
    let lexical_leg = store
        .search_lexical(
            lexical,
            corpus_rows,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("complete exact lexical list");
    let vector_candidates = vector_leg
        .candidates
        .iter()
        .map(|candidate| {
            VectorCandidate::exact(
                candidate.document().map(|version| version.doc_id()),
                -f64::from(candidate.score()),
            )
        })
        .collect::<Vec<_>>();
    let lexical_candidates = lexical_leg
        .candidates
        .iter()
        .map(|candidate| LexicalCandidate::new(Some(candidate.document.doc_id()), candidate.score))
        .collect::<Vec<_>>();
    let bounds = LegBounds {
        vector: vector_candidates.first().map(|best| VectorBounds {
            min_squared_l2: best.squared_l2(),
            max_squared_l2: vector_leg
                .vector_ceiling
                .expect("complete exact vector leg reports its norm-ball ceiling"),
            next_unseen_squared_l2: None,
        }),
        lexical: lexical_candidates
            .first()
            .zip(lexical_candidates.last())
            .map(|(best, worst)| LexicalBounds {
                max_bm25: best.bm25(),
                min_bm25: worst.bm25(),
                next_unseen_bm25: None,
            }),
    };
    fuse_bounded(
        &HybridQuery::new(k),
        &vector_candidates,
        &lexical_candidates,
        bounds,
        |document: &Option<DocId>| *document,
        |document: &Option<DocId>| *document,
    )
    .expect("offline fusion of the complete lists")
    .hits
}

fn assert_same_hits(bounded: &[FusedHit<DocId>], offline: &[FusedHit<DocId>], context: &str) {
    assert_eq!(
        bounded.len(),
        offline.len(),
        "{context}: bounded fusion returned a different hit count"
    );
    for (rank, (left, right)) in bounded.iter().zip(offline).enumerate() {
        assert_eq!(left.key, right.key, "{context}: key differs at rank {rank}");
        assert_eq!(
            left.fused_score.to_bits(),
            right.fused_score.to_bits(),
            "{context}: fused score bits differ at rank {rank}"
        );
        assert_eq!(
            left.vector_squared_l2.map(f64::to_bits),
            right.vector_squared_l2.map(f64::to_bits),
            "{context}: vector score bits differ at rank {rank}"
        );
        assert_eq!(
            left.lexical_bm25.map(f64::to_bits),
            right.lexical_bm25.map(f64::to_bits),
            "{context}: lexical score bits differ at rank {rank}"
        );
    }
}

/// Builds one random text-and-vector store with tombstones and one to three
/// seals, and returns it with its total row count.
fn seeded_store(rng: &mut impl Rng) -> (tempfile::TempDir, Store, usize) {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let batches = rng.random_range(2..=4);
    let mut next_id = 1_u128;
    let mut rows = 0_usize;
    let mut live = Vec::new();
    for batch in 0..batches {
        let count = rng.random_range(120..=220);
        let mut documents = Vec::with_capacity(count);
        for _ in 0..count {
            let identity = DocumentVersion::new(DocId::new(next_id), Revision::new(1));
            next_id += 1;
            let vector = (0..DIMENSION)
                .map(|_| rng.random_range(-1.0_f32..1.0_f32))
                .collect::<Vec<_>>();
            let words = (0..rng.random_range(1..=5))
                .map(|_| TERMS[rng.random_range(0..TERMS.len())])
                .collect::<Vec<_>>()
                .join(" ");
            documents.push(IngestDocument::new(identity, vector).with_text(&words));
            live.push(identity.doc_id());
        }
        rows += count;
        store
            .ingest(IngestBatch::new(documents))
            .expect("ingest seeded batch");
        // Tombstone a few rows so the alive mask, not the row count, drives
        // both the window and the anchor.
        for _ in 0..rng.random_range(0..=3) {
            if live.is_empty() {
                break;
            }
            let victim = live.remove(rng.random_range(0..live.len()));
            store
                .delete(DeleteBatch::new(vec![victim]))
                .expect("tombstone a seeded row");
        }
        if batch + 1 < batches {
            store.seal().expect("seal a seeded batch");
        }
    }
    (directory, store, rows)
}

#[test]
fn hybrid_top_k_equals_offline_fusion_of_the_truncated_exact_lists() {
    let mut rng = test_support::seeded_rng(
        "hybrid_bounded::hybrid_top_k_equals_offline_fusion_of_the_truncated_exact_lists",
    );
    let mut bounded_rounds = 0_usize;
    let mut widened = 0_usize;
    for case in 0..corpus_cases() {
        let (_directory, store, rows) = seeded_store(&mut rng);
        for probe in 0..4 {
            let vector = (0..DIMENSION)
                .map(|_| rng.random_range(-1.0_f32..1.0_f32))
                .collect::<Vec<_>>();
            let term_count = rng.random_range(1..=3);
            let terms = (0..term_count)
                .map(|_| TERMS[rng.random_range(0..TERMS.len())].as_bytes().to_vec())
                .collect::<Vec<_>>();
            let lexical = TermQuery::flat(terms, &[DEFAULT_FIELD]);
            // A deep k pushes the k-th fused score down until the first
            // window can no longer prove it, which is what exercises the
            // widening loop rather than only the first round.
            let k = if probe == 3 {
                rng.random_range(30..=60)
            } else {
                rng.random_range(1..=5)
            };
            let context = format!("case {case} probe {probe} rows {rows} k {k}");
            let outcome = store
                .search_hybrid(
                    SearchRequest::new(&vector),
                    &lexical,
                    &HybridQuery::new(k),
                    SearchOptions::default(),
                    QueryControl::Cancel(CancelToken::new()),
                )
                .expect("bounded hybrid search");
            let report = outcome
                .diagnostics
                .hybrid
                .expect("hybrid diagnostics report");
            assert!(
                report.window <= rows,
                "{context}: window {} exceeded the corpus",
                report.window
            );
            if report.window < rows {
                bounded_rounds += 1;
            }
            let fusion = outcome.diagnostics.fusion.expect("fusion report");
            if fusion.rounds > 1 {
                widened += 1;
            }
            assert_ne!(
                fusion.termination,
                FusionTermination::WindowUnproven,
                "{context}: a returned hybrid result must never be unproven"
            );
            let offline = offline_full_list_fusion(&store, &vector, &lexical, k, rows);
            assert_same_hits(&outcome.hits, &offline, &context);
        }
        store.close().expect("close seeded store");
    }
    assert!(
        bounded_rounds > 0,
        "the seeded corpora must actually exercise a window narrower than the corpus"
    );
    let _ = widened;
}

/// A corpus whose best fused documents sit outside the first vector window:
/// the near rows carry no text and the matching rows are vector-far. The
/// first window cannot prove its own top-k, so the widening loop must run.
#[test]
fn hybrid_widens_and_reports_the_round_count_when_the_first_window_is_unproven() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    const ROWS: usize = 400;
    let mut documents = Vec::with_capacity(ROWS);
    for row in 0..ROWS {
        let identity = DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1));
        let mut vector = vec![0.0_f32; DIMENSION];
        vector[0] = 1.0;
        vector[1] = row as f32 * 0.0025;
        let document = IngestDocument::new(identity, vector);
        documents.push(if row < ROWS / 2 {
            document
        } else {
            let repeats = 1 + (ROWS - row) % 5;
            document.with_text(vec!["zeppelin"; repeats].join(" ").as_str())
        });
    }
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest the adversarial corpus");
    let vector = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0];
    let lexical = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&vector),
            &lexical,
            &HybridQuery::new(1),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("bounded hybrid search");
    let fusion = outcome.diagnostics.fusion.clone().expect("fusion report");
    assert!(
        fusion.rounds >= 2,
        "the first window must not have proved this corpus, got {} round(s)",
        fusion.rounds
    );
    assert_eq!(fusion.termination, FusionTermination::StableBound);
    assert!(!fusion.budget_exhausted);
    let report = outcome
        .diagnostics
        .hybrid
        .expect("hybrid diagnostics report");
    assert!(
        report.window > 50 && report.window < ROWS,
        "widening must have grown the window without materializing the corpus, got {}",
        report.window
    );
    let offline = offline_full_list_fusion(&store, &vector, &lexical, 1, ROWS);
    assert_same_hits(&outcome.hits, &offline, "widened window");
    store.close().expect("close store");
}
