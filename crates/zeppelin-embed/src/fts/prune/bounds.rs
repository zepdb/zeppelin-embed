//! Per-block upper bounds: the only thing that makes skipping sound.
//!
//! # The one invariant
//!
//! A block's stored bound must be at or above every true score inside it.
//! If it is ever below, pruning drops a document that belonged in the
//! top-k, and that is a wrong answer rather than a slow one — the exact
//! failure task 14's equivalence property exists to catch.
//!
//! Two things push the bound upward and neither may be reversed:
//!
//! 1. **Quantization rounds up.** `quantize_block_max` takes a ceiling, so
//!    a `u8` slot always over-states rather than under-states.
//! 2. **The shortest document in the block sets the length term.** BM25
//!    scores rise as a document gets shorter, so the block's minimum length
//!    paired with its maximum term frequency bounds every posting in it.
//!    Those two need not come from the same document; using the pair is
//!    looser than the true maximum and therefore still an upper bound.

use crate::fts::bm25::{Bm25Params, CorpusStats, Df, DocLen, Tf, term_score, term_score_ceiling};
use crate::fts::postings::{dequantize_block_max, quantize_block_max};

/// One block's bound over a run of `(row, weighted tf)` entries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockBound {
    /// Index of the first entry the block covers.
    pub start: usize,
    /// Index one past the last entry the block covers.
    pub end: usize,
    /// Largest row id in the block; the skip key.
    pub last_row: u32,
    /// Upper bound on any posting's contribution, after quantization.
    pub max_score: f64,
}

/// Builds per-block bounds for one term's merged postings.
///
/// `lengths` supplies the weighted analyzed length of each row. `scale` is
/// the quantization scale, normally the term's absolute score ceiling, so
/// the `u8` slot spans the range the term can actually reach.
///
/// The bound returned is the DEQUANTIZED value, so it is exactly what a
/// sealed segment would hand back. Computing bounds from exact scores here
/// and quantized ones on disk would let the property pass in memory and fail
/// against a real segment.
#[must_use]
pub fn build_block_bounds(
    entries: &[(u32, u32)],
    lengths: &[u32],
    block_size: usize,
    df: Df,
    stats: &CorpusStats,
    params: Bm25Params,
) -> Vec<BlockBound> {
    if block_size == 0 || entries.is_empty() {
        return Vec::new();
    }
    let scale = term_score_ceiling(df, stats, params);
    let mut bounds = Vec::with_capacity(entries.len().div_ceil(block_size));
    let mut start = 0_usize;
    while start < entries.len() {
        let end = (start + block_size).min(entries.len());
        let mut best = 0.0_f64;
        let mut last_row = 0_u32;
        for index in start..end {
            let Some((row, tf)) = entries.get(index).copied() else {
                continue;
            };
            last_row = row;
            let length = usize::try_from(row)
                .ok()
                .and_then(|slot| lengths.get(slot).copied())
                .unwrap_or(1);
            let score = term_score(Tf(tf), df, DocLen(length), stats, params);
            if score > best {
                best = score;
            }
        }
        // Round-trip through the persisted representation so the bound used
        // here is exactly the bound a sealed segment would provide.
        let quantized = quantize_block_max(best, scale);
        let restored = dequantize_block_max(quantized, scale);
        bounds.push(BlockBound {
            start,
            end,
            last_row,
            // Guard against a scale of zero making the bound collapse.
            max_score: if restored >= best { restored } else { best },
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

    fn stats() -> CorpusStats {
        CorpusStats::new(1_000, 40_000).expect("valid stats")
    }

    #[test]
    fn quantized_block_max_is_never_below_any_true_score_in_its_block() {
        let stats = stats();
        let params = Bm25Params::default();
        let lengths: Vec<u32> = (0..200_u32).map(|row| 1 + row % 97).collect();
        let entries: Vec<(u32, u32)> = (0..200_u32).map(|row| (row, 1 + row % 13)).collect();

        for df in [1_u32, 5, 200, 999] {
            for block_size in [1_usize, 2, 7, 64, 128] {
                let bounds = build_block_bounds(
                    &entries,
                    &lengths,
                    block_size,
                    Df(df),
                    &stats,
                    params,
                );
                for block in &bounds {
                    for entry in entries.iter().take(block.end).skip(block.start) {
                        let (row, tf) = *entry;
                        let length = lengths[row as usize];
                        let truth =
                            term_score(Tf(tf), Df(df), DocLen(length), &stats, params);
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
    fn the_term_upper_bound_dominates_every_block_bound() {
        let stats = stats();
        let lengths: Vec<u32> = (0..64_u32).map(|row| 1 + row).collect();
        let entries: Vec<(u32, u32)> = (0..64_u32).map(|row| (row, 1 + row % 5)).collect();
        let bounds = build_block_bounds(
            &entries,
            &lengths,
            8,
            Df(10),
            &stats,
            Bm25Params::default(),
        );
        let overall = term_upper_bound(&bounds);
        for block in &bounds {
            assert!(block.max_score <= overall + 1e-12);
        }
    }

    #[test]
    fn blocks_partition_the_entries_exactly_once() {
        let stats = stats();
        let lengths: Vec<u32> = vec![10; 100];
        let entries: Vec<(u32, u32)> = (0..100_u32).map(|row| (row, 2)).collect();
        for block_size in [1_usize, 3, 64, 200] {
            let bounds = build_block_bounds(
                &entries,
                &lengths,
                block_size,
                Df(4),
                &stats,
                Bm25Params::default(),
            );
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
        let lengths: Vec<u32> = vec![5; 40];
        let entries: Vec<(u32, u32)> = (0..10_u32).map(|row| (row * 3, 1)).collect();
        let bounds = build_block_bounds(
            &entries,
            &lengths,
            4,
            Df(2),
            &stats,
            Bm25Params::default(),
        );
        assert_eq!(bounds.len(), 3);
        assert_eq!(bounds[0].last_row, 9);
        assert_eq!(bounds[1].last_row, 21);
        assert_eq!(bounds[2].last_row, 27);
    }

    #[test]
    fn degenerate_inputs_produce_no_bounds_rather_than_panicking() {
        let stats = stats();
        assert!(
            build_block_bounds(&[], &[], 8, Df(1), &stats, Bm25Params::default()).is_empty()
        );
        assert!(
            build_block_bounds(&[(0, 1)], &[5], 0, Df(1), &stats, Bm25Params::default())
                .is_empty()
        );
        assert_eq!(term_upper_bound(&[]), 0.0);
    }

    #[test]
    fn a_row_with_no_recorded_length_still_yields_a_sound_bound() {
        // A length array shorter than the highest row must not silently
        // produce a bound below the truth.
        let stats = stats();
        let entries = vec![(0_u32, 3_u32), (500, 3)];
        let lengths = vec![10_u32];
        let bounds =
            build_block_bounds(&entries, &lengths, 8, Df(2), &stats, Bm25Params::default());
        assert_eq!(bounds.len(), 1);
        // The fallback length of 1 is the shortest possible, so its score is
        // the largest possible: the bound stays an upper bound.
        let fallback = term_score(Tf(3), Df(2), DocLen(1), &stats, Bm25Params::default());
        assert!(bounds[0].max_score >= fallback - 1e-12);
    }
}
