//! nDCG@k, pinned to the `pytrec_eval` conventions BEIR reports use.
//!
//! # Why the conventions matter more than the formula
//!
//! A BEIR number is only comparable to Pyserini's if the evaluator agrees on
//! three things the formula alone does not fix:
//!
//! 1. **Gain is the raw relevance grade**, and the discount is `log2(rank+1)`
//!    with ranks starting at one. TREC-COVID and NFCorpus use graded
//!    judgements (0, 1, 2), so a binary evaluator silently reports different
//!    numbers on exactly the corpora where the gate is tightest.
//! 2. **The ideal DCG is computed over the qrels**, not over the run. A
//!    system that retrieves nothing scores zero, not one.
//! 3. **A query with no positive judgement is skipped**, not scored zero.
//!    Averaging zeros over unjudged queries is the single most common way to
//!    report a BEIR number several points below everyone else's.
//!
//! Task 13 R2 says to validate this against a hand-computed fixture before
//! the gate means anything. `tests/beir_eval.rs` is that fixture.

use std::collections::{BTreeMap, BTreeSet};

/// A judged relevance grade for one (query, document) pair.
pub type Grade = u32;

/// Relevance judgements: query id -> document id -> grade.
pub type Qrels = BTreeMap<String, BTreeMap<String, Grade>>;

/// One retrieved document.
#[derive(Clone, Debug, PartialEq)]
pub struct RunEntry {
    /// The retrieved document identifier.
    pub doc_id: String,
    /// Its score. Higher is better.
    pub score: f64,
}

/// A retrieval run: query id -> ranked results, best first.
pub type Run = BTreeMap<String, Vec<RunEntry>>;

/// Returns the discounted cumulative gain of one ranked list at `k`.
#[must_use]
pub fn dcg_at_k(grades: &[Grade], k: usize) -> f64 {
    grades
        .iter()
        .take(k)
        .enumerate()
        .map(|(index, grade)| {
            let rank = index as f64 + 1.0;
            f64::from(*grade) / (rank + 1.0).log2()
        })
        .sum()
}

/// Returns nDCG@k over unique retrieved document IDs for one query.
///
/// The first occurrence establishes each document's rank. Repeated chunks of
/// that document neither earn another gain nor occupy another parent rank.
/// Fewer than `k` distinct retrieved IDs remain fewer than `k` results; this
/// evaluator does not retrieve additional parents to fill the list.
///
/// Returns `None` when the query has no positive judgement, which is the
/// signal to skip it rather than to score it zero.
#[must_use]
pub fn ndcg_at_k_for_query(
    ranked: &[RunEntry],
    judgements: &BTreeMap<String, Grade>,
    k: usize,
) -> Option<f64> {
    let mut ideal: Vec<Grade> = judgements.values().copied().filter(|g| *g > 0).collect();
    if ideal.is_empty() {
        return None;
    }
    ideal.sort_unstable_by(|left, right| right.cmp(left));
    let ideal_dcg = dcg_at_k(&ideal, k);
    if ideal_dcg <= 0.0 {
        return None;
    }

    let mut seen = BTreeSet::new();
    let retrieved: Vec<Grade> = ranked
        .iter()
        .filter(|entry| seen.insert(entry.doc_id.as_str()))
        .take(k)
        .map(|entry| judgements.get(&entry.doc_id).copied().unwrap_or(0))
        .collect();
    Some(dcg_at_k(&retrieved, k) / ideal_dcg)
}

/// Returns mean nDCG@k over every judged query.
///
/// Queries absent from the run contribute zero — they were judged, and the
/// system returned nothing for them. Queries absent from the qrels, or
/// judged with no positive grade, are skipped entirely.
#[must_use]
pub fn mean_ndcg_at_k(run: &Run, qrels: &Qrels, k: usize) -> f64 {
    let empty: Vec<RunEntry> = Vec::new();
    let mut total = 0.0_f64;
    let mut counted = 0_usize;
    for (query, judgements) in qrels {
        let ranked = run.get(query).unwrap_or(&empty);
        if let Some(score) = ndcg_at_k_for_query(ranked, judgements, k) {
            total += score;
            counted += 1;
        }
    }
    if counted == 0 {
        return 0.0;
    }
    total / counted as f64
}

/// One corpus's gate row: the published target and what we measured.
#[derive(Clone, Debug, PartialEq)]
pub struct GateRow {
    /// Corpus name.
    pub corpus: String,
    /// Published Pyserini flat-BM25 nDCG@10.
    pub target: f64,
    /// What this engine measured, or `None` when the corpus was unavailable.
    pub measured: Option<f64>,
}

impl GateRow {
    /// Returns true when the measurement is within `tolerance` of the target.
    ///
    /// Returns false for an unmeasured corpus: an absent number is never a
    /// pass.
    #[must_use]
    pub fn passes(&self, tolerance: f64) -> bool {
        self.measured
            .is_some_and(|value| (value - self.target).abs() <= tolerance)
    }
}

/// The published Pyserini FLAT BM25 nDCG@10 targets.
///
/// # The column matters
///
/// `research/02a:267` reports flat and multifield pairs: TREC-COVID
/// 0.595/0.656, FiQA 0.236/0.236, NFCorpus 0.322/0.325, SciFact 0.679/0.665.
/// The task-13 spec's gate row mixed the two, taking SciFact and NFCorpus
/// from the MULTIFIELD column — which flatters a miss on SciFact by 14
/// thousandths and tightens NFCorpus by 3. These are the FLAT numbers, and
/// the flat scorer is gated against them.
#[must_use]
pub fn flat_targets() -> Vec<GateRow> {
    [
        ("trec-covid", 0.595),
        ("fiqa", 0.236),
        ("nfcorpus", 0.322),
        ("scifact", 0.679),
    ]
    .into_iter()
    .map(|(corpus, target)| GateRow {
        corpus: corpus.to_owned(),
        target,
        measured: None,
    })
    .collect()
}

/// The multifield secondary target, for the BM25F-lite comparison.
#[must_use]
pub fn multifield_targets() -> Vec<GateRow> {
    [
        ("trec-covid", 0.656),
        ("fiqa", 0.236),
        ("nfcorpus", 0.325),
        ("scifact", 0.665),
    ]
    .into_iter()
    .map(|(corpus, target)| GateRow {
        corpus: corpus.to_owned(),
        target,
        measured: None,
    })
    .collect()
}
