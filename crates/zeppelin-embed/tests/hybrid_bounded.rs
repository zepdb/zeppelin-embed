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
use zeppelin_embed::fusion::{FusedHit, FusionTermination, HybridQuery};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};

const DIMENSION: usize = 6;
const TERMS: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

#[test]
fn astra_06_rescored_scan_reports_exact_scores_and_approximate_coverage() {
    use zeppelin_embed::fusion::CandidateCoverage;
    use zeppelin_embed::lifecycle::ScanRescoreOptions;
    let directory = tempdir().expect("rescore hybrid");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(
            (0..100)
                .map(|row| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                        vec![1.0 + row as f32 * 0.125, 0.0],
                    )
                    .with_text(if row == 0 { "alpha" } else { "omega" })
                })
                .collect(),
        ))
        .expect("ingest");
    let exact = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            1,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("independent ceiling");
    let ceiling = exact.vector_ceiling.expect("norm enclosure");
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0]),
            &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(10).with_alpha(0.7),
            SearchOptions::default()
                .with_scan_rescore(ScanRescoreOptions::new(1, 200).expect("controls")),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("explicit rescore hybrid");
    let counters = outcome
        .diagnostics
        .scan_rescore
        .expect("hybrid must retain rescore receipts");
    assert_eq!((counters.coarse_rows, counters.eligible_rows), (100, 100));
    assert_eq!(
        (
            counters.candidates_rescored,
            counters.coarse_bytes,
            counters.rescore_bytes
        ),
        (51, 100, 408)
    );
    assert!(outcome.diagnostics.approximate);
    assert!(outcome.diagnostics.exact_rescore);
    let report = outcome.diagnostics.hybrid.expect("hybrid report");
    assert_eq!(
        report.provenance.vector_coverage,
        CandidateCoverage::Approximate
    );
    assert!(report.provenance.cross_scores_complete);
    assert_eq!(report.normalization_policy_version, 1);
    assert_eq!(report.total_cross_filled_vector, 1);
    let fusion = outcome.diagnostics.fusion.expect("fusion report");
    assert_eq!(fusion.termination, FusionTermination::ApproximateCandidates);
    assert_eq!(fusion.rounds, 1);
    assert_eq!(
        outcome.hits[0].key,
        DocId::new(1),
        "lexical cross-fill recovers the unseen exact winner"
    );
    for hit in &outcome.hits {
        let row = hit.key.get() - 1;
        let distance = (row as f64 * 0.125).powi(2);
        assert_eq!(
            hit.vector_squared_l2.map(f64::to_bits),
            Some(distance.to_bits())
        );
        let expected = 0.7 * ((ceiling - distance) / ceiling) + (1.0 - 0.7) * f64::from(row == 0);
        assert_eq!(hit.fused_score.to_bits(), expected.to_bits());
    }
    assert_eq!(outcome.diagnostics.counters.scan.bytes_read, 100 + 408 + 8);
    store.close().expect("close");
}

fn astra_03_fixture() -> (tempfile::TempDir, Store) {
    let directory = tempdir().expect("fixed-policy fixture");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let document = |row: usize, revision| {
        let tf = if row == 0 {
            1
        } else if row <= 80 {
            81 - row
        } else {
            0
        };
        let mut words = vec!["alpha"; tf];
        words.extend(vec!["omega"; 90 - tf]);
        IngestDocument::new(
            DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(revision)),
            vec![astra_03_coordinate(row), 1.0],
        )
        .with_text(&words.join(" "))
    };
    store
        .ingest(IngestBatch::new(
            (0..70).map(|row| document(row, 1)).collect(),
        ))
        .expect("sealed rows");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(
            (70..130).map(|row| document(row, 1)).collect(),
        ))
        .expect("active rows");
    store
        .ingest(IngestBatch::new(vec![document(0, 2)]))
        .expect("replace a sealed identity in active");
    store
        .delete(DeleteBatch::new(vec![DocId::new(8), DocId::new(76)]))
        .expect("tombstones across both sources");
    (directory, store)
}

fn astra_03_coordinate(row: usize) -> f32 {
    if row == 0 {
        0.125
    } else if row <= 80 {
        3.0 + (row % 5) as f32 * 0.01
    } else {
        0.2 + (row - 81) as f32 * 0.001
    }
}

fn astra_03_query(
    store: &Store,
    k: usize,
    alpha: f64,
    rounds: usize,
) -> zeppelin_embed::ingest::StoreHybridSearchOutcome {
    let mut query = HybridQuery::new(k).with_max_rounds(rounds);
    query.alpha = Some(alpha);
    store
        .search_hybrid(
            SearchRequest::new(&[0.0, 1.0]),
            &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
            &query,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("fixed-policy hybrid")
}

fn astra_03_bm25(tf: u32) -> f64 {
    // Literal corpus: 128 live documents, 79 contain alpha, every length is
    // 90. k1=1.2, b=.75, so length normalization is exactly one.
    let idf = (1.0_f64 + (128.0 - 79.0 + 0.5) / (79.0 + 0.5)).ln();
    let tf = f64::from(tf);
    idf * (tf * 2.2) / (tf + 1.2)
}

#[test]
fn astra_03_window_size_does_not_renormalize_seen_documents() {
    let (_directory, store) = astra_03_fixture();
    let run = |k| {
        let mut query = HybridQuery::new(k);
        query.alpha = Some(0.0);
        store
            .search_hybrid(
                SearchRequest::new(&[0.0, 1.0]),
                &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
                &query,
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("ordinary bounded window")
    };
    let narrow = run(2);
    let wide = run(20);
    let id = DocId::new(3); // second-best BM25, present in both windows
    let a = narrow
        .hits
        .iter()
        .find(|hit| hit.key == id)
        .expect("narrow hit");
    let b = wide
        .hits
        .iter()
        .find(|hit| hit.key == id)
        .expect("wide hit");
    assert_eq!(a.lexical_bm25, b.lexical_bm25);
    assert_eq!(
        a.fused_score.to_bits(),
        b.fused_score.to_bits(),
        "a wider request must not move a seen document's normalization floor"
    );
    store.close().expect("close");
}

#[test]
fn astra_03_vector_winner_receives_bm25_below_lexical_window() {
    let (_directory, store) = astra_03_fixture();
    // Ordinary bounded execution exposes the incomplete first window on the
    // parent; explicit Exact would widen and hide this missing-score defect.
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&[0.0, 1.0]),
            &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(1),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("bounded default");
    assert_eq!(outcome.hits[0].key, DocId::new(1));
    assert_eq!(
        outcome.hits[0].lexical_bm25.map(f64::to_bits),
        Some(astra_03_bm25(1).to_bits()),
        "rank-79 lexical contribution must be computed for the vector winner"
    );
    assert_eq!(outcome.diagnostics.hybrid.expect("hybrid").window, 50);
    store.close().expect("close");
}

#[test]
fn astra_03_unseen_lexical_contribution_is_not_erased_by_window_floor() {
    let (_directory, store) = astra_03_fixture();
    let outcome = astra_03_query(&store, 130, 0.7, 0);
    let ceiling = store
        .search(
            SearchRequest::new(&[0.0, 1.0]),
            1,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("ceiling")
        .vector_ceiling
        .expect("norm enclosure");
    let hit = outcome
        .hits
        .iter()
        .find(|hit| hit.key == DocId::new(1))
        .expect("lowest lexical match");
    let expected = 0.7 * ((ceiling - 0.015625) / ceiling)
        + (1.0 - 0.7) * (astra_03_bm25(1) / astra_03_bm25(80));
    assert_eq!(
        hit.fused_score.to_bits(),
        expected.to_bits(),
        "positive BM25 must retain its contribution at the lowest matching score"
    );
    store.close().expect("close");
}

#[test]
fn astra_03_bounded_exact_matches_independent_fixed_policy_oracle() {
    let (_directory, store) = astra_03_fixture();
    let ceiling = store
        .search(
            SearchRequest::new(&[0.0, 1.0]),
            1,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("ceiling")
        .vector_ceiling
        .expect("norm enclosure");
    let mut reference = (0..130)
        .filter(|row| ![7, 75].contains(row))
        .map(|row| {
            let x = f64::from(astra_03_coordinate(row));
            let distance = f64::from((x * x) as f32);
            let tf = if row == 0 {
                1
            } else if row <= 80 {
                81 - row
            } else {
                0
            };
            let bm25 = astra_03_bm25(tf as u32);
            let score =
                0.7 * ((ceiling - distance) / ceiling) + (1.0 - 0.7) * (bm25 / astra_03_bm25(80));
            (DocId::new(row as u128 + 1), distance, bm25, score)
        })
        .collect::<Vec<_>>();
    reference.sort_by(|a, b| b.3.total_cmp(&a.3).then(a.0.cmp(&b.0)));
    assert_eq!(reference[0].0, DocId::new(1), "literal balanced winner");
    for k in [1, 10, 20, 130] {
        for rounds in [0, 8] {
            let outcome = astra_03_query(&store, k, 0.7, rounds);
            assert_eq!(outcome.hits.len(), k.min(128));
            for (hit, expected) in outcome.hits.iter().zip(&reference) {
                assert_eq!(hit.key, expected.0);
                assert_eq!(
                    hit.vector_squared_l2.map(f64::to_bits),
                    Some(expected.1.to_bits())
                );
                assert_eq!(
                    hit.lexical_bm25.map(f64::to_bits),
                    Some(expected.2.to_bits())
                );
                assert_eq!(hit.fused_score.to_bits(), expected.3.to_bits());
            }
            assert!(
                outcome
                    .diagnostics
                    .hybrid
                    .expect("hybrid")
                    .provenance
                    .cross_scores_complete
            );
            assert_eq!(
                outcome
                    .diagnostics
                    .hybrid
                    .expect("hybrid")
                    .normalization_policy_version,
                1
            );
            assert!(matches!(
                outcome.diagnostics.fusion.expect("fusion").termination,
                FusionTermination::StableBound | FusionTermination::ListsExhausted
            ));
        }
    }
    store.close().expect("close");
}

#[test]
fn astra_03_empty_and_zero_range_legs_have_defined_scores() {
    use zeppelin_embed::fusion::FusionMethod;
    for texts in [vec![None, None], vec![Some("alpha"), Some("alpha")]] {
        let directory = tempdir().expect("degenerate fixture");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        let empty = store
            .search_hybrid(
                SearchRequest::new(&[0.0, 0.0]),
                &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
                &HybridQuery::new(10),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("empty store");
        assert!(empty.hits.is_empty());
        let documents = texts
            .iter()
            .enumerate()
            .map(|(row, text)| {
                let document = IngestDocument::new(
                    DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                    vec![0.0, 0.0],
                );
                text.map_or(document.clone(), |text| document.with_text(text))
            })
            .collect();
        store
            .ingest(IngestBatch::new(documents))
            .expect("equal vectors");
        let outcome = store
            .search_hybrid(
                SearchRequest::new(&[0.0, 0.0]),
                &TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]),
                &HybridQuery::new(10),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("degenerate legs");
        assert_eq!(
            outcome.diagnostics.fusion.expect("fusion").method,
            FusionMethod::ConvexCombination
        );
        assert_eq!(
            outcome.hits.iter().map(|hit| hit.key).collect::<Vec<_>>(),
            vec![DocId::new(1), DocId::new(2)]
        );
        for hit in outcome.hits {
            assert_eq!(hit.fused_score, 1.0);
            assert!(hit.lexical_bm25.is_some());
        }
        store.close().expect("close");
    }
}

#[test]
fn astra_03_structured_anchor_uses_the_exact_combined_maximum() {
    use zeppelin_embed::fts::query::LexicalQuery;
    let directory = tempdir().expect("combined fixture");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let documents = (0..110)
        .map(|row| {
            let (alpha, alpine) = match row {
                0..51 => (8, 0),
                51..102 => (0, 8),
                102 => (5, 5),
                _ => (0, 0),
            };
            let mut words = vec!["alpha"; alpha];
            words.extend(vec!["alpine"; alpine]);
            words.extend(vec!["omega"; 16 - words.len()]);
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text(&words.join(" "))
        })
        .collect();
    store.ingest(IngestBatch::new(documents)).expect("ingest");
    let outcome = store
        .search_hybrid_structured(
            SearchRequest::new(&[1.0, 0.0]),
            &LexicalQuery::prefix(b"al".to_vec(), DEFAULT_FIELD),
            &HybridQuery::new(1).with_alpha(0.0),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("combined hybrid");
    // Row 103 is rank 52 in both individual expansions, but wins their sum.
    let idf = (1.0_f64 + (110.0 - 52.0 + 0.5) / (52.0 + 0.5)).ln();
    let term = idf * (5.0 * 2.2) / (5.0 + 1.2);
    assert_eq!(outcome.hits[0].key, DocId::new(103));
    assert_eq!(
        outcome.hits[0].lexical_bm25.map(f64::to_bits),
        Some((term + term).to_bits())
    );
    assert_eq!(outcome.hits[0].fused_score, 1.0);
    assert_eq!(
        outcome
            .diagnostics
            .hybrid
            .expect("hybrid")
            .lexical_full_materializations,
        1
    );
    store.close().expect("close");
}

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
    let lexical = lexical_leg
        .candidates
        .iter()
        .map(|hit| (hit.document.doc_id(), hit.score))
        .collect::<std::collections::BTreeMap<_, _>>();
    let maximum = lexical
        .values()
        .copied()
        .max_by(f64::total_cmp)
        .unwrap_or(0.0);
    let ceiling = vector_leg.vector_ceiling.expect("validated norm enclosure");
    let alpha = if maximum == 0.0 { 1.0 } else { 0.7 };
    let mut hits = vector_leg
        .candidates
        .iter()
        .map(|candidate| {
            let key = candidate.document().expect("document identity").doc_id();
            let distance = -f64::from(candidate.score());
            let bm25 = lexical.get(&key).copied().unwrap_or(0.0);
            let vector_score = if ceiling == 0.0 {
                1.0
            } else {
                (ceiling - distance) / ceiling
            };
            let lexical_score = if maximum == 0.0 { 0.0 } else { bm25 / maximum };
            FusedHit {
                key,
                vector_squared_l2: Some(distance),
                lexical_bm25: Some(bm25),
                fused_score: alpha * vector_score + (1.0 - alpha) * lexical_score,
            }
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        right
            .fused_score
            .total_cmp(&left.fused_score)
            .then(left.key.cmp(&right.key))
    });
    hits.truncate(k);
    hits
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

fn astra_disjoint_windows_store() -> (tempfile::TempDir, Store) {
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
    (directory, store)
}

#[test]
fn astra_09_widening_cross_scores_each_candidate_once() {
    use std::collections::BTreeSet;
    use zeppelin_embed::lifecycle::{
        begin_hybrid_score_test_observations, take_hybrid_score_test_observations,
    };
    let (_directory, store) = astra_disjoint_windows_store();
    begin_hybrid_score_test_observations();
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(1),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("three disjoint producer windows");
    assert_eq!(
        outcome.diagnostics.fusion.as_ref().expect("fusion").rounds,
        3
    );
    let observed = take_hybrid_score_test_observations();
    let unique_vector = observed
        .vector_rows
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let unique_lexical = observed
        .lexical_rows
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    eprintln!(
        "vector calls={} unique={}; lexical calls={} unique={}",
        observed.vector_rows.len(),
        unique_vector.len(),
        observed.lexical_rows.len(),
        unique_lexical.len()
    );
    // Three windows contain 50, 100 and 200 rows from each disjoint half.
    // Each required physical row/leg pair needs exactly one cross-score.
    assert_eq!(unique_vector.len(), 200);
    assert_eq!(unique_lexical.len(), 200);
    assert_eq!(
        observed.vector_rows.len(),
        200,
        "repeated vector cross-scores"
    );
    assert_eq!(
        observed.lexical_rows.len(),
        200,
        "repeated lexical cross-scores"
    );
    assert_eq!(outcome.diagnostics.counters.scan.dims_touched, 8_400);
    assert_eq!(outcome.diagnostics.counters.scan.bytes_read, 33_600);
    store.close().expect("close store");
}

#[test]
fn astra_09_supported_scan_round_prepares_once_across_active_and_sealed_sources() {
    use zeppelin_embed::quant::{
        begin_query_preparation_test_observations, take_query_preparation_test_observations,
    };
    for layout in ["active", "sealed", "mixed"] {
        let (_directory, store) = astra_disjoint_windows_store();
        if layout != "active" {
            store.seal().expect("same vectors in a sealed scan");
        }
        if layout == "mixed" {
            store
                .ingest(IngestBatch::new(vec![
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(999), Revision::new(1)),
                        vec![2.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                    )
                    .with_text("zeppelin"),
                ]))
                .expect("active rows alongside sealed rows");
        }
        begin_query_preparation_test_observations();
        let result = store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
                &HybridQuery::new(1).with_max_rounds(3),
                SearchOptions::default().with_scan_rescore(
                    zeppelin_embed::lifecycle::ScanRescoreOptions::new(1, 400)
                        .expect("exact candidate scores"),
                ),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("one supported approximate scan round");
        let calls = take_query_preparation_test_observations();
        let rounds = result
            .diagnostics
            .fusion
            .as_ref()
            .expect("fusion rounds")
            .rounds;
        assert_eq!(rounds, 1);
        eprintln!("layout={layout} rounds={rounds} preparation_calls={calls:?}");
        assert_eq!(
            calls.bit4,
            vec![(DIMENSION, 0)],
            "same prepared vector across physical scan sources"
        );
        assert!(calls.int8.is_empty());
        assert_eq!(
            store.stats().expect("released preparation").temporary_bytes,
            0
        );
        store.close().expect("close");
    }
}

#[test]
fn astra_09_widening_prepares_lexical_statistics_once_for_both_legs() {
    use zeppelin_embed::fts::preparation_observer;
    let (_directory, store) = astra_disjoint_windows_store();
    preparation_observer::begin();
    let result = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(1),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("three exact widening rounds");
    let (scorers, frequencies) = preparation_observer::take();
    assert_eq!(
        result.diagnostics.fusion.as_ref().expect("fusion").rounds,
        3
    );
    eprintln!(
        "rounds=3 actual_scorer_preparations={scorers} actual_frequency_lookups={frequencies}"
    );
    assert_eq!(
        (scorers, frequencies),
        (1, 1),
        "one preparation shared by producer and cross-scorer"
    );
    assert_eq!(
        store
            .stats()
            .expect("released lexical query")
            .temporary_bytes,
        0
    );
    store.close().expect("close");
}

#[test]
fn astra_09_structured_widening_reuses_expansions_and_constants() {
    use zeppelin_embed::fts::{preparation_observer, query::LexicalQuery};
    let (_directory, store) = astra_disjoint_windows_store();
    preparation_observer::begin();
    let result = store
        .search_hybrid_structured(
            SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            &LexicalQuery::prefix(b"zep".to_vec(), DEFAULT_FIELD),
            &HybridQuery::new(1),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("three structured widening rounds");
    let (scorers, frequencies, expansions) = preparation_observer::take_with_expansions();
    assert_eq!(
        result.diagnostics.fusion.as_ref().expect("fusion").rounds,
        3
    );
    assert_eq!(result.lexical_expansions.len(), 1);
    assert_eq!(result.lexical_expansions[0].term, b"zeppelin");
    eprintln!("rounds=3 scorers={scorers} frequencies={frequencies} expansions={expansions}");
    assert_eq!((scorers, frequencies, expansions), (1, 1, 1));
    assert_eq!(
        store
            .stats()
            .expect("released structured preparation")
            .temporary_bytes,
        0
    );
    store.close().expect("close");
}

#[test]
fn astra_09_widening_retains_candidate_buffers_between_rounds() {
    use zeppelin_embed::lifecycle::{
        begin_hybrid_score_test_observations, take_hybrid_score_test_observations,
    };
    let (_directory, store) = astra_disjoint_windows_store();
    for _ in 0..2 {
        begin_hybrid_score_test_observations();
        let result = store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
                &HybridQuery::new(1),
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("three widening rounds");
        assert_eq!(
            result.diagnostics.fusion.as_ref().expect("fusion").rounds,
            3
        );
        let observed = take_hybrid_score_test_observations();
        eprintln!(
            "candidate_start_capacities={:?}",
            observed.candidate_start_capacities
        );
        eprintln!(
            "union_start_capacities={:?} candidate_peak={} union_peak={}",
            observed.union_start_capacities,
            observed.candidate_peak_bytes,
            observed.union_peak_bytes
        );
        assert_eq!(observed.union_start_capacities, vec![0, 200, 400]);
        assert!(observed.candidate_peak_bytes > 0);
        assert!(observed.union_peak_bytes > 0);
        assert_eq!(observed.candidate_start_capacities.len(), 3);
        assert_eq!(
            observed.candidate_start_capacities[0],
            [0, 0],
            "new admission"
        );
        assert!(
            observed.candidate_start_capacities[1]
                .iter()
                .all(|&capacity| capacity >= 100),
            "second round must retain its predecessor's complete disjoint union"
        );
        assert!(
            observed.candidate_start_capacities[2]
                .iter()
                .all(|&capacity| capacity >= 200),
            "third round must retain its predecessor's complete disjoint union"
        );
        assert_eq!(store.stats().expect("released scratch").temporary_bytes, 0);
    }
    store.close().expect("close");
}

#[test]
fn astra_09_small_then_large_query_releases_accounted_capacity_on_drop() {
    use zeppelin_embed::lifecycle::{
        begin_hybrid_score_test_observations, take_hybrid_score_test_observations,
    };
    let (_directory, store) = astra_disjoint_windows_store();
    let before = store.stats().expect("initial accounting").temporary_bytes;
    let mut small_peak = None;
    for k in [1, 400, 1] {
        begin_hybrid_score_test_observations();
        let result = store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
                &HybridQuery::new(k),
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("accounted hybrid query");
        let calls = take_hybrid_score_test_observations();
        let after = store
            .stats()
            .expect("post-query accounting")
            .temporary_bytes;
        assert_eq!(after, before, "query-owned capacity survives the admission");
        let rounds = result.diagnostics.fusion.as_ref().expect("fusion").rounds;
        eprintln!(
            "k={k} rounds={rounds} cache_peak_bytes={} temporary_after={after}",
            calls.cache_peak_bytes
        );
        if k == 400 {
            assert_eq!(rounds, 1);
            assert_eq!(
                calls.cache_peak_bytes, 0,
                "exhaustive one-round query cannot reuse new scores"
            );
        } else {
            assert_eq!(rounds, 3);
            assert!(
                calls.cache_peak_bytes > 0,
                "widening cache observation must fire"
            );
            if let Some(expected) = small_peak {
                assert_eq!(
                    calls.cache_peak_bytes, expected,
                    "later query must start with fresh capacity"
                );
            } else {
                small_peak = Some(calls.cache_peak_bytes);
            }
        }
    }
    store.close().expect("close accounted fixture");
}

#[test]
fn astra_09_query_reuse_never_crosses_epoch_source_or_revision() {
    use std::collections::BTreeSet;
    use zeppelin_embed::epoch::{
        ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
    };
    use zeppelin_embed::fts::tokenizer::TokenizerConfig;
    use zeppelin_embed::lifecycle::{
        begin_hybrid_fresh_round_test_observations, begin_hybrid_score_test_observations,
        take_hybrid_score_test_observations,
    };
    let mut prior_epoch = None;
    for epoch_number in [1_u8, 2] {
        let scale = f32::from(epoch_number);
        let tower = EmbeddingTower {
            model_id: "astra-09-identity-fixture".to_owned(),
            model_version: epoch_number.to_string(),
            weights_digest: vec![epoch_number],
            dims: DIMENSION as u32,
            normalization: Normalization::None,
            prompt_prefix: String::new(),
            max_tokens: 32,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        };
        let epoch = StoreEpoch {
            embedding: EmbeddingEpoch {
                query: tower.clone(),
                document: tower,
                alignment_digest: Vec::new(),
            },
            tokenizer: TokenizerConfig::text_default().epoch(),
        };
        assert_ne!(prior_epoch, Some(epoch.identity()));
        prior_epoch = Some(epoch.identity());
        let directory = tempdir().expect("identity fixture");
        let store = Store::open(
            directory.path(),
            OpenOptions::default().with_epoch(epoch.clone()),
        )
        .expect("stamped store");
        for part in 0..2 {
            let documents = (0..400)
                .filter(|row| (row % 200) / 100 == part)
                .map(|row| {
                    let mut vector = vec![0.0_f32; DIMENSION];
                    vector[0] = scale;
                    vector[1] = scale * row as f32 * 0.0025;
                    let document = IngestDocument::new(
                        DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                        vector,
                    );
                    if row < 200 {
                        document
                    } else {
                        document.with_text(&vec!["zeppelin"; 1 + (400 - row) % 5].join(" "))
                    }
                })
                .collect();
            store
                .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
                .expect("two physical namespaces with overlapping local rows");
            if part == 0 {
                store.seal().expect("first namespace is sealed");
            }
        }
        let vector = [scale, 0.0, 0.0, 0.0, 0.0, 0.0];
        let lexical = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
        let run = || {
            store
                .search_hybrid(
                    SearchRequest::new(&vector),
                    &lexical,
                    &HybridQuery::new(1),
                    SearchOptions::default().with_tier(SearchTier::Exact),
                    QueryControl::Cancel(CancelToken::new()),
                )
                .expect("pinned identity query")
        };
        for phase in 0..3 {
            if phase == 1 {
                store
                    .ingest(
                        IngestBatch::new(vec![
                            IngestDocument::new(
                                DocumentVersion::new(DocId::new(202), Revision::new(2)),
                                vec![scale, 2.0 * scale, 0.0, 0.0, 0.0, 0.0],
                            )
                            .with_text("zeppelin zeppelin zeppelin zeppelin zeppelin"),
                        ])
                        .with_epoch(epoch.identity()),
                    )
                    .expect("replace a previously cross-scored sealed revision");
            } else if phase == 2 {
                store
                    .seal()
                    .expect("move active rows to a new physical source");
            }
            begin_hybrid_fresh_round_test_observations();
            let fresh = run();
            let fresh_calls = take_hybrid_score_test_observations();
            begin_hybrid_score_test_observations();
            let reused = run();
            let calls = take_hybrid_score_test_observations();
            assert_eq!(store.epoch_identity(), Some(epoch.identity()));
            assert_same_hits(
                &reused.hits,
                &fresh.hits,
                "fresh scores for current identities",
            );
            let expected = fresh.diagnostics.fusion.as_ref().expect("fresh fusion");
            let actual = reused.diagnostics.fusion.as_ref().expect("reused fusion");
            assert_eq!(actual.rounds, expected.rounds);
            assert_eq!(actual.termination, expected.termination);
            for (scored, fresh_scored) in [
                (&calls.vector_rows, &fresh_calls.vector_rows),
                (&calls.lexical_rows, &fresh_calls.lexical_rows),
            ] {
                let distinct = scored.iter().copied().collect::<BTreeSet<_>>();
                assert_eq!(
                    scored.len(),
                    distinct.len(),
                    "each current identity scored once"
                );
                assert!(fresh_scored.len() >= scored.len());
            }
            if phase == 0 {
                assert_eq!(
                    actual.rounds, 3,
                    "the mixed-source widening witness must fire"
                );
                assert_eq!(calls.vector_rows.len(), 200);
                assert_eq!(calls.lexical_rows.len(), 200);
                let sources = calls
                    .vector_rows
                    .iter()
                    .map(|(row, _)| row.source())
                    .collect::<BTreeSet<_>>();
                let locals = calls
                    .vector_rows
                    .iter()
                    .map(|(row, _)| row.local_row())
                    .collect::<BTreeSet<_>>();
                assert_eq!(sources.len(), 2);
                assert!(
                    locals.len() < calls.vector_rows.len(),
                    "physical local-row collision must fire"
                );
            }
            let target_versions = calls
                .vector_rows
                .iter()
                .filter_map(|(_, version)| *version)
                .filter(|version| version.doc_id() == DocId::new(202))
                .collect::<Vec<_>>();
            assert_eq!(
                target_versions,
                vec![DocumentVersion::new(
                    DocId::new(202),
                    Revision::new(if phase == 0 { 1 } else { 2 }),
                )],
                "the current revision must be scored after source replacement"
            );
            assert_eq!(
                store
                    .stats()
                    .expect("released query memory")
                    .temporary_bytes,
                0
            );
        }
        store.close().expect("close identity fixture");
    }
}

#[test]
fn astra_09_reused_rounds_match_fresh_rounds_in_scores_and_termination() {
    use zeppelin_embed::lifecycle::{
        begin_hybrid_fresh_round_test_observations, begin_hybrid_score_test_observations,
        take_hybrid_score_test_observations,
    };
    let (_directory, store) = astra_disjoint_windows_store();
    let run = || {
        store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
                &HybridQuery::new(1),
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("three-round differential query")
    };
    begin_hybrid_fresh_round_test_observations();
    let fresh = run();
    let fresh_calls = take_hybrid_score_test_observations();
    begin_hybrid_score_test_observations();
    let reused = run();
    let reused_calls = take_hybrid_score_test_observations();
    assert_eq!(
        fresh_calls.vector_rows.len(),
        350,
        "fresh control must fire"
    );
    assert_eq!(
        fresh_calls.lexical_rows.len(),
        350,
        "fresh control must fire"
    );
    assert_eq!(reused_calls.vector_rows.len(), 200);
    assert_eq!(reused_calls.lexical_rows.len(), 200);
    assert_same_hits(
        &reused.hits,
        &fresh.hits,
        "same producer frontiers with reuse",
    );
    let fresh_fusion = fresh.diagnostics.fusion.as_ref().expect("fresh fusion");
    let reused_fusion = reused.diagnostics.fusion.as_ref().expect("reused fusion");
    assert_eq!(fresh_fusion.rounds, 3);
    assert_eq!(reused_fusion.rounds, fresh_fusion.rounds);
    assert_eq!(reused_fusion.termination, fresh_fusion.termination);
    assert_eq!(reused.diagnostics.plan, fresh.diagnostics.plan);
    let fresh_report = fresh.diagnostics.hybrid.as_ref().expect("fresh report");
    let reused_report = reused.diagnostics.hybrid.as_ref().expect("reused report");
    assert_eq!(reused_report.provenance, fresh_report.provenance);
    assert_eq!(reused_report.window, fresh_report.window);
    assert_eq!(reused_report.vector_returned, fresh_report.vector_returned);
    assert_eq!(
        reused_report.lexical_returned,
        fresh_report.lexical_returned
    );
    assert_eq!(fresh.diagnostics.counters.scan.dims_touched, 9_300);
    assert_eq!(reused.diagnostics.counters.scan.dims_touched, 8_400);
    store.close().expect("close differential store");
}

#[test]
fn astra_00_lexical_window_survives_worker_handoff() {
    let (_directory, store) = astra_disjoint_windows_store();
    let mut query = HybridQuery::new(1).with_alpha(1.0);
    query.max_rounds = 1;
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &query,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("bounded query");
    let report = outcome.diagnostics.hybrid.expect("hybrid");
    assert_eq!(
        report.lexical_candidates_produced, 51,
        "the worker must return its requested W+1 producer boundary"
    );
    store.close().expect("close store");
}

#[test]
// Alpha one certifies one round while both disjoint producers and their
// cross-fills still execute. This isolates the literal work receipt.
fn astra_00_cross_fill_counts_exact_rows_and_bytes() {
    let (_directory, store) = astra_disjoint_windows_store();
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(1).with_alpha(1.0),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("cross-fill query");
    let report = outcome.diagnostics.hybrid.expect("hybrid");
    assert_eq!(report.window, 50);
    assert_eq!(report.cross_filled_vector, 50);
    assert_eq!(
        outcome.diagnostics.fusion.as_ref().expect("fusion").rounds,
        1
    );
    // One exhaustive 400 x 6 scan plus 50 disjoint 6-dimensional f32 reads.
    assert_eq!(outcome.diagnostics.counters.scan.dims_touched, 2_700);
    assert_eq!(outcome.diagnostics.counters.scan.bytes_read, 10_800);
    // Secondary score control, in addition to the literal work oracle above.
    let offline = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(1).with_alpha(1.0).with_max_rounds(0),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("same-policy exhaustive work control");
    assert_same_hits(
        &outcome.hits,
        &offline.hits,
        "one-round cross-fill accounting",
    );
    store.close().expect("close");
}

#[test]
fn astra_00_cache_receipts_belong_to_the_query() {
    let (_directory, store) = astra_disjoint_windows_store();
    for (hits, builds) in [(0, 1), (1, 0)] {
        let outcome = store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
                &HybridQuery::new(1).with_alpha(1.0),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("cache query");
        assert_eq!(outcome.diagnostics.counters.lexical_cache_hits, hits);
        assert_eq!(outcome.diagnostics.counters.lexical_cache_builds, builds);
        assert_eq!(
            outcome.diagnostics.timings.is_some(),
            zeppelin_embed::diag::QUERY_TIMING_ENABLED
        );
    }
    store.close().expect("close");
}

#[test]
fn astra_01_exact_hybrid_can_still_certify() {
    use zeppelin_embed::fusion::CandidateCoverage;
    let (_directory, store) = astra_disjoint_windows_store();
    let vector = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let lexical = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&vector),
            &lexical,
            &HybridQuery::new(1).with_max_rounds(0),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("explicit exhaustive verification");
    assert_eq!(
        outcome
            .diagnostics
            .fusion
            .as_ref()
            .expect("fusion")
            .termination,
        FusionTermination::ListsExhausted
    );
    let provenance = outcome
        .diagnostics
        .hybrid
        .as_ref()
        .expect("hybrid")
        .provenance;
    assert_eq!(provenance.vector_coverage, CandidateCoverage::Exhaustive);
    assert_eq!(provenance.lexical_coverage, CandidateCoverage::Exhaustive);
    assert!(provenance.cross_scores_complete);
    let reference = offline_full_list_fusion(&store, &vector, &lexical, 1, 400);
    assert_same_hits(&outcome.hits, &reference, "explicit exact certificate");
    let bounded = store
        .search_hybrid(
            SearchRequest::new(&vector),
            &lexical,
            &HybridQuery::new(1),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("explicit exact bounded producers");
    let report = bounded.diagnostics.fusion.as_ref().expect("bounded fusion");
    assert!(matches!(
        report.termination,
        FusionTermination::StableBound | FusionTermination::ListsExhausted
    ));
    assert!(
        bounded
            .diagnostics
            .hybrid
            .as_ref()
            .expect("bounded provenance")
            .provenance
            .cross_scores_complete
    );
    assert_same_hits(&bounded.hits, &reference, "exact widened certificate");
    assert_eq!(report.rounds, 3);
    assert_eq!(bounded.diagnostics.plan.len(), 3);
    // Three full 400x6 scans plus 200 distinct cross-filled rows. Earlier
    // rounds' 50 and 100 lexical winners reuse their already computed scores.
    assert_eq!(bounded.diagnostics.counters.scan.dims_touched, 8_400);
    assert_eq!(bounded.diagnostics.counters.scan.bytes_read, 33_600);
    // Independent exhaustive fusion arithmetic over the raw fixture vectors
    // and complete BM25 receipts. No product fusion/range/sort helper is used.
    // The producer's published ceiling is the declared normalization anchor;
    // BM25 scoring itself belongs to the separate candidate-score contract.
    let raw_lexical = store
        .search_lexical(&lexical, 400, QueryControl::Cancel(CancelToken::new()))
        .expect("complete BM25 receipts")
        .candidates
        .into_iter()
        .map(|hit| (hit.document.doc_id().get(), hit.score))
        .collect::<Vec<_>>();
    let ceiling = store
        .search(
            SearchRequest::new(&vector),
            400,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("published normalization anchor")
        .vector_ceiling
        .expect("ceiling");
    let raw_vectors = (0..400)
        .map(|row| {
            let coordinate = f64::from(row as f32 * 0.0025);
            (row as u128 + 1, f64::from((coordinate * coordinate) as f32))
        })
        .collect::<Vec<_>>();
    let std_reference = astra_std_cc_reference(&raw_vectors, &raw_lexical, ceiling);
    assert_eq!(std_reference[0].0, 202, "literal best balanced fixture row");
    for certified in [&outcome, &bounded] {
        assert_eq!(certified.hits[0].key.get(), std_reference[0].0);
        assert_eq!(
            certified.hits[0].fused_score.to_bits(),
            std_reference[0].1.to_bits()
        );
    }
    store.close().expect("close exact certificate store");
}

fn astra_std_cc_reference(
    vector: &[(u128, f64)],
    lexical: &[(u128, f64)],
    ceiling: f64,
) -> Vec<(u128, f64)> {
    let lexical: std::collections::BTreeMap<_, _> = lexical.iter().copied().collect();
    let maximum_lexical = lexical
        .values()
        .copied()
        .max_by(f64::total_cmp)
        .expect("lexical scores");
    let mut ranked = vector
        .iter()
        .map(|(id, distance)| {
            let alpha = 0.7_f64; // Declared core default; text queries explicitly choose 0.5.
            let dense = alpha * ((ceiling - distance) / ceiling);
            let sparse = lexical
                .get(id)
                .map_or(0.0, |score| (1.0 - alpha) * (score / maximum_lexical));
            (*id, dense + sparse)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked
}
