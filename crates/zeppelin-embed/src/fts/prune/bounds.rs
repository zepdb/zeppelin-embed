//! Per-block upper bounds: the only thing that makes skipping sound.
//!
//! # The one invariant
//!
//! A block's bound must be at or above every true score inside it. If it is
//! ever below, pruning drops a document that belonged in the top-k, and that
//! is a wrong answer rather than a slow one — the exact failure task 14's
//! equivalence property exists to catch.
//!
//! # The bound is an impact pair, evaluated late
//!
//! A block is summarized by two integers: the largest term frequency it
//! holds and the shortest document it touches. Neither carries a statistic,
//! so neither can go stale. The bound is `term_score(max_tf, min_len)` under
//! whatever `avgdl` and `(k1, b)` the caller is scoring with right now.
//!
//! Soundness is monotonicity: BM25 rises with `tf` and falls with `len`, so
//! that pair dominates every posting in the block. The two extremes need not
//! come from the same document; pairing them is looser than the true maximum
//! and therefore still an upper bound.
//!
//! # Why not the exact per-block maximum
//!
//! Because it is not a summary — it is a score, and a score is a number
//! computed under one set of statistics. `score / ceiling` cancels `idf` but
//! not `avgdl` or `(k1, b)`, so a stored maximum sealed when documents were
//! short falls **below** a true score once longer documents arrive: 15.21%
//! below on an `avgdl` rise from 10 to 100, 19.56% below on a
//! `beir()`-to-`anserini()` change. `tests/block_max_soundness.rs` gates it.
//!
//! The pair is looser, and a looser bound skips less. That is the price of
//! an answer that is right under drift, and it is the one direction of error
//! that is safe: a loose bound costs time, a stale one costs a result.
//!
//! # Cost
//!
//! Building bounds this way costs one `term_score` call per BLOCK. The
//! previous form scored every posting in order to bound it, which is why the
//! pruned path was slower than the scan it exists to avoid.

use crate::fts::bm25::TermScorer;
use crate::fts::bm25::{DocLen, Tf};
use crate::fts::postings::BlockImpact;

/// One block's bound over a run of `(row, weighted tf)` entries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockBound {
    /// Index of the first entry the block covers.
    pub start: usize,
    /// Index one past the last entry the block covers.
    pub end: usize,
    /// Largest row id in the block; the skip key.
    pub last_row: u32,
    /// The block's impact pair, as a sealed segment would store it.
    pub impact: BlockImpact,
    /// Upper bound on any posting's contribution under the live scorer.
    pub max_score: f64,
}

/// Summarizes one run of `(row, weighted tf)` entries as an impact pair.
///
/// `lengths` supplies the weighted analyzed length of each row. A row beyond
/// the array contributes the shortest possible length, which pushes the
/// bound up rather than down.
#[must_use]
pub fn impact_of(entries: &[(u32, u32)], lengths: &[u32]) -> BlockImpact {
    let mut max_tf = 0_u32;
    let mut min_len = u32::MAX;
    for (row, tf) in entries {
        max_tf = max_tf.max(*tf);
        let length = usize::try_from(*row)
            .ok()
            .and_then(|slot| lengths.get(slot).copied())
            .unwrap_or(1);
        min_len = min_len.min(length);
    }
    BlockImpact {
        max_tf,
        // Saturating DOWN; see `BlockImpact::min_len`.
        min_len: u16::try_from(min_len).unwrap_or(u16::MAX),
    }
}

/// Builds per-block bounds for one term's merged postings.
///
/// The bound is exactly what a sealed segment hands back: the same impact
/// pair, evaluated by the same scorer. Computing bounds one way in memory
/// and another way on disk would let the equivalence property pass against a
/// fixture and fail against a real segment.
#[must_use]
pub fn build_block_bounds(
    entries: &[(u32, u32)],
    lengths: &[u32],
    block_size: usize,
    scorer: &TermScorer,
) -> Vec<BlockBound> {
    if block_size == 0 || entries.is_empty() {
        return Vec::new();
    }
    let mut bounds = Vec::with_capacity(entries.len().div_ceil(block_size));
    let mut start = 0_usize;
    while start < entries.len() {
        let end = (start + block_size).min(entries.len());
        let chunk = entries.get(start..end).unwrap_or(&[]);
        let impact = impact_of(chunk, lengths);
        bounds.push(BlockBound {
            start,
            end,
            last_row: chunk.last().map_or(0, |(row, _)| *row),
            impact,
            // One score per block, not one per posting.
            max_score: scorer.score(Tf(impact.max_tf), DocLen(u32::from(impact.min_len))),
        });
        start = end;
    }
    bounds
}

/// Returns the largest bound across every block, the term's upper bound.
#[must_use]
pub fn term_upper_bound(bounds: &[BlockBound]) -> f64 {
    bounds
        .iter()
        .map(|block| block.max_score)
        .fold(0.0_f64, f64::max)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use crate::fts::bm25::{Bm25Params, CorpusStats, Df, term_score};

    fn stats() -> CorpusStats {
        CorpusStats::new(1_000, 40_000).expect("valid stats")
    }

    #[test]
    fn a_block_bound_is_never_below_any_true_score_in_its_block() {
        let stats = stats();
        let params = Bm25Params::default();
        let lengths: Vec<u32> = (0..200_u32).map(|row| 1 + row % 97).collect();
        let entries: Vec<(u32, u32)> = (0..200_u32).map(|row| (row, 1 + row % 13)).collect();

        for df in [1_u32, 5, 200, 999] {
            let scorer = TermScorer::new(Df(df), &stats, params);
            for block_size in [1_usize, 2, 7, 64, 128] {
                let bounds = build_block_bounds(&entries, &lengths, block_size, &scorer);
                for block in &bounds {
                    for entry in entries.iter().take(block.end).skip(block.start) {
                        let (row, tf) = *entry;
                        let length = lengths[row as usize];
                        let truth = term_score(Tf(tf), Df(df), DocLen(length), &stats, params);
                        assert!(
                            block.max_score >= truth - 1e-12,
                            "bound {} is below a true score {truth} \
                             (df={df}, block={block_size}, row={row})",
                            block.max_score
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_bound_built_here_matches_one_built_from_a_sealed_impact_pair() {
        // The in-memory bound and the persisted one must be the same number,
        // or the equivalence property passes against a fixture and fails
        // against a real segment.
        let stats = stats();
        let scorer = TermScorer::new(Df(7), &stats, Bm25Params::default());
        let lengths: Vec<u32> = (0..64_u32).map(|row| 3 + row % 11).collect();
        let entries: Vec<(u32, u32)> = (0..64_u32).map(|row| (row, 1 + row % 5)).collect();
        let bounds = build_block_bounds(&entries, &lengths, 16, &scorer);
        assert_eq!(bounds.len(), 4);
        for block in &bounds {
            assert!((block.impact.bound(&scorer) - block.max_score).abs() < 1e-15);
        }
    }

    #[test]
    fn the_bound_survives_statistics_and_parameter_drift() {
        // The whole point of a pair over a stored score: the summary carries
        // no statistics, so re-evaluating it under drifted ones stays sound.
        let lengths: Vec<u32> = (0..64_u32).map(|row| 1 + row % 40).collect();
        let entries: Vec<(u32, u32)> = (0..64_u32).map(|row| (row, 1 + row % 9)).collect();
        let sealed = CorpusStats::new(100, 1_000).expect("avgdl 10");
        let sealed_scorer = TermScorer::new(Df(64), &sealed, Bm25Params::beir());
        let bounds = build_block_bounds(&entries, &lengths, 16, &sealed_scorer);

        for (docs, tokens) in [(1_000_u64, 100_000_u64), (10, 50), (5_000, 5_000)] {
            let live = CorpusStats::new(docs, tokens).expect("valid stats");
            for params in [Bm25Params::beir(), Bm25Params::anserini()] {
                let live_scorer = TermScorer::new(Df(64), &live, params);
                for block in &bounds {
                    // The reader re-evaluates the stored pair; it never
                    // reuses the number computed at seal time.
                    let bound = block.impact.bound(&live_scorer);
                    for entry in entries.iter().take(block.end).skip(block.start) {
                        let (row, tf) = *entry;
                        let truth = live_scorer.score(Tf(tf), DocLen(lengths[row as usize]));
                        assert!(bound >= truth - 1e-12, "bound {bound} below {truth}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_term_upper_bound_dominates_every_block_bound() {
        let stats = stats();
        let scorer = TermScorer::new(Df(10), &stats, Bm25Params::default());
        let lengths: Vec<u32> = (0..64_u32).map(|row| 1 + row).collect();
        let entries: Vec<(u32, u32)> = (0..64_u32).map(|row| (row, 1 + row % 5)).collect();
        let bounds = build_block_bounds(&entries, &lengths, 8, &scorer);
        let overall = term_upper_bound(&bounds);
        for block in &bounds {
            assert!(block.max_score <= overall + 1e-12);
        }
    }

    #[test]
    fn blocks_partition_the_entries_exactly_once() {
        let stats = stats();
        let scorer = TermScorer::new(Df(4), &stats, Bm25Params::default());
        let lengths: Vec<u32> = vec![10; 100];
        let entries: Vec<(u32, u32)> = (0..100_u32).map(|row| (row, 2)).collect();
        for block_size in [1_usize, 3, 64, 200] {
            let bounds = build_block_bounds(&entries, &lengths, block_size, &scorer);
            let mut cursor = 0_usize;
            for block in &bounds {
                assert_eq!(block.start, cursor, "blocks must be contiguous");
                assert!(block.end > block.start, "a block must not be empty");
                cursor = block.end;
            }
            assert_eq!(cursor, entries.len(), "blocks must cover every entry");
        }
    }

    #[test]
    fn the_last_row_of_each_block_is_its_skip_key() {
        let stats = stats();
        let scorer = TermScorer::new(Df(2), &stats, Bm25Params::default());
        let lengths: Vec<u32> = vec![5; 40];
        let entries: Vec<(u32, u32)> = (0..10_u32).map(|row| (row * 3, 1)).collect();
        let bounds = build_block_bounds(&entries, &lengths, 4, &scorer);
        assert_eq!(bounds.len(), 3);
        assert_eq!(bounds[0].last_row, 9);
        assert_eq!(bounds[1].last_row, 21);
        assert_eq!(bounds[2].last_row, 27);
    }

    #[test]
    fn degenerate_inputs_produce_no_bounds_rather_than_panicking() {
        let stats = stats();
        let scorer = TermScorer::new(Df(1), &stats, Bm25Params::default());
        assert!(build_block_bounds(&[], &[], 8, &scorer).is_empty());
        assert!(build_block_bounds(&[(0, 1)], &[5], 0, &scorer).is_empty());
        assert_eq!(term_upper_bound(&[]), 0.0);
        assert!(impact_of(&[], &[]).is_absent());
    }

    #[test]
    fn a_row_with_no_recorded_length_still_yields_a_sound_bound() {
        // A length array shorter than the highest row must not silently
        // produce a bound below the truth.
        let stats = stats();
        let scorer = TermScorer::new(Df(2), &stats, Bm25Params::default());
        let entries = vec![(0_u32, 3_u32), (500, 3)];
        let lengths = vec![10_u32];
        let bounds = build_block_bounds(&entries, &lengths, 8, &scorer);
        assert_eq!(bounds.len(), 1);
        // The fallback length of 1 is the shortest possible, so its score is
        // the largest possible: the bound stays an upper bound.
        let fallback = term_score(Tf(3), Df(2), DocLen(1), &stats, Bm25Params::default());
        assert!(bounds[0].max_score >= fallback - 1e-12);
    }

    #[test]
    fn a_length_beyond_the_slot_saturates_downward() {
        let entries = vec![(0_u32, 2_u32)];
        let lengths = vec![100_000_u32];
        assert_eq!(impact_of(&entries, &lengths).min_len, u16::MAX);
    }
}
