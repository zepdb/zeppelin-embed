//! Block-max WAND.
//!
//! # The mechanism
//!
//! Keep the cursors sorted by their current document. Walk them from the
//! front accumulating upper bounds until the running sum first exceeds the
//! top-k threshold: that cursor is the **pivot**, and its current document
//! is the first one that could possibly qualify. Every document before it
//! is provably unreachable and is skipped without being scored.
//!
//! The block-max refinement then asks a second, cheaper question: using the
//! *block* bounds at the current positions rather than the whole-term
//! bounds, can this pivot document still qualify? If not, jump past it
//! without decoding anything. That is where the 3.8M-to-21.9K reduction
//! comes from (`research/02a:281`).
//!
//! WAND is strongest at small `k` with few terms, which is why the
//! selection rule prefers it there; its advantage shrinks from 3.4x at
//! k=10 to 1.4x at k=1000 (`research/02a:282`).
//!
//! # Where correctness lives
//!
//! Both bounds are upper bounds, and the pivot rule only skips documents
//! whose *total possible* score is at or below a threshold already achieved
//! by `k` documents. A skipped document could not have entered the results.
//! When the bounds are uncertain the code evaluates rather than skipping.

use crate::fts::bm25::DocLen;
use crate::fts::bm25::Tf;
use crate::fts::search::{GlobalDocId, SearchCounters};
use crate::meta::DocBitmap;

use super::{TermCursor, TopK};

fn length_of(lengths: &[u32], row: u32) -> u32 {
    usize::try_from(row)
        .ok()
        .and_then(|slot| lengths.get(slot).copied())
        .unwrap_or(1)
}

/// Runs block-max WAND over one segment's cursors.
///
/// Corpus statistics and BM25 parameters are not arguments: each cursor
/// carries its own [`crate::fts::bm25::TermScorer`], built once from exactly
/// those inputs, so there is no way for this loop to score against different
/// statistics than the bounds were built from.
pub fn run(
    cursors: &mut [TermCursor<'_>],
    lengths: &[u32],
    segment: u32,
    heap: &mut TopK,
    counters: &mut SearchCounters,
    allow_list: Option<&DocBitmap>,
) {
    for cursor in cursors.iter_mut() {
        cursor.reset();
    }

    // Hoisted to the query frame. This buffer used to be allocated and freed
    // once per pivot iteration, which on a single-term query is once per
    // posting -- the dominant allocation on the pruned path.
    //
    // Each entry caches the cursor's current row beside its slot. Rows are
    // read once per iteration instead of through the cursor table on every
    // comparison, and no cursor moves between the fill and the last read,
    // so the cache cannot go stale.
    let mut live: Vec<(u32, usize)> = Vec::with_capacity(cursors.len());

    loop {
        // Order live cursors by current document.
        live.clear();
        for (slot, cursor) in cursors.iter().enumerate() {
            if let Some(row) = cursor.current() {
                live.push((row, slot));
            }
        }
        if live.is_empty() {
            break;
        }
        // Tuples order by row then slot, and slots are unique, so this is
        // exactly the stable row sort the previous form did -- cursors
        // sharing a row keep slot order and the pivot choice stays
        // reproducible -- without a closure through the cursor table per
        // comparison.
        live.sort_unstable();

        let threshold = heap.threshold();

        // Find the pivot: the first cursor at which the accumulated
        // whole-term bounds exceed the threshold.
        let mut accumulated = 0.0_f64;
        let mut pivot: Option<usize> = None;
        for (position, (_, slot)) in live.iter().enumerate() {
            let Some(cursor) = cursors.get(*slot) else {
                continue;
            };
            accumulated += cursor.upper_bound;
            if accumulated > threshold || !heap.is_full() {
                pivot = Some(position);
                break;
            }
        }
        let Some(pivot_position) = pivot else {
            // No document can reach the threshold: the query is finished.
            break;
        };
        let Some((pivot_row, _)) = live.get(pivot_position).copied() else {
            break;
        };
        let Some((first_row, _)) = live.first().copied() else {
            break;
        };

        if first_row == pivot_row {
            // Every cursor up to the pivot is already on the pivot document.
            // Refine with block maxima before paying to score it.
            let mut block_bound = 0.0_f64;
            for (row, slot) in &live {
                if *row != pivot_row {
                    continue;
                }
                let Some(cursor) = cursors.get(*slot) else {
                    continue;
                };
                block_bound += cursor.current_block_max();
            }

            if heap.is_full() && block_bound <= threshold {
                // Nothing in these blocks can qualify. Jump the whole span
                // they cover rather than stepping one posting.
                //
                // # Why the span is safe, and where it ends
                //
                // Two things bound how far this may go.
                //
                // `horizon` is the first block boundary any cursor on the
                // pivot crosses. Up to it, every one of those cursors is
                // still inside the block whose impact pair `block_bound`
                // was built from, so `block_bound` still dominates.
                //
                // `next_row` is the first row any OTHER live cursor sits
                // at. Below it, no cursor outside the pivot set contains
                // the row at all, so none of them can add anything that
                // `block_bound` failed to account for.
                //
                // For every row in `[pivot_row, target - 1]` the total is
                // therefore at or below `block_bound`, which is at or below
                // a threshold already achieved by `k` documents. None of
                // them could have entered the results.
                let mut horizon: Option<u32> = None;
                for (row, slot) in &live {
                    if *row != pivot_row {
                        continue;
                    }
                    let Some(cursor) = cursors.get(*slot) else {
                        continue;
                    };
                    horizon = match (horizon, cursor.stream.block_horizon()) {
                        (Some(held), Some(found)) => Some(held.min(found)),
                        (None, found) => found,
                        (held, None) => held,
                    };
                }
                // Rows are sorted ascending, so the first one past the
                // pivot is the minimum the previous form scanned for.
                let next_row = live
                    .iter()
                    .map(|(row, _)| *row)
                    .find(|row| *row > pivot_row);
                let target = match (horizon, next_row) {
                    (Some(edge), Some(next)) => edge.saturating_add(1).min(next),
                    (Some(edge), None) => edge.saturating_add(1),
                    (None, Some(next)) => next,
                    (None, None) => pivot_row.saturating_add(1),
                }
                // A span that does not move is an infinite loop. Both
                // candidates are strictly above the pivot, so this only
                // guards against a degenerate cursor.
                .max(pivot_row.saturating_add(1));

                for cursor in cursors.iter_mut() {
                    if cursor.current().is_some_and(|row| row < target) {
                        // Blocks jumped are tallied on the stream itself
                        // and collected once the segment finishes.
                        cursor.seek(target);
                    }
                }
                continue;
            }

            let mut total = 0.0_f64;
            for cursor in cursors.iter_mut() {
                if cursor.current() != Some(pivot_row) {
                    continue;
                }
                counters.postings_decoded = counters.postings_decoded.saturating_add(1);
                if let Some(tf) = cursor.current_tf() {
                    total += cursor
                        .scorer
                        .score(Tf(tf), DocLen(length_of(lengths, pivot_row)));
                }
                cursor.advance();
            }
            counters.docs_evaluated = counters.docs_evaluated.saturating_add(1);
            if allow_list.is_none_or(|allowed| allowed.contains(pivot_row)) {
                heap.offer(
                    GlobalDocId {
                        segment,
                        row: pivot_row,
                    },
                    total,
                );
            }
        } else {
            // Advance the cursors that trail the pivot straight to it.
            for (row, slot) in live.iter().take(pivot_position.saturating_add(1)) {
                if *row >= pivot_row {
                    continue;
                }
                let Some(cursor) = cursors.get_mut(*slot) else {
                    continue;
                };
                // Blocks jumped are tallied on the stream itself and
                // collected once the segment finishes.
                cursor.seek(pivot_row);
            }
        }
    }
}
