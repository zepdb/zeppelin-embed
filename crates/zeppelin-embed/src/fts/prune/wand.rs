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
//! *block* bounds of the blocks that could hold the pivot rather than the
//! whole-term bounds, can this pivot document still qualify? If not, jump
//! past it — and past every following block the same bounds condemn, which
//! is found by walking impact pairs through the skip keys and costs no
//! decode. That is where the 3.8M-to-21.9K reduction comes from
//! (`research/02a:281`).
//!
//! Both halves of that matter. Taking the decision and then capping the
//! jump at the end of the block the pivot happened to be standing in makes
//! the traversal land in the next block, decode it, and ask the same
//! question again: `blocks_decoded` becomes the whole list at every `k`
//! and `blocks_skipped` is structurally zero. `prune_contracts::block_max_
//! wand_skips_blocks_it_can_prove_unreachable` gates that it does not.
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

        // The block-max refinement, taken on METADATA ALONE before any
        // cursor is paid to move.
        //
        // # What it decides
        //
        // The pivot rule has just proved that no row below `pivot_row` can
        // qualify. This asks the second, cheaper question about `pivot_row`
        // itself and the span above it: using the impact pairs of the
        // blocks that could hold those rows rather than the whole-term
        // bounds, can anything up there still reach the threshold? While
        // the answer is no, the span is condemned and the cursors seek
        // straight over it — over whole blocks, on skip keys, without
        // unpacking one of them.
        //
        // # Why every live cursor, and why `bound_for` rather than the
        // block the cursor is standing in
        //
        // `bound_for` reports the impact pair of the block that COULD hold
        // the row, found by binary search over skip keys, and nothing for a
        // cursor whose head has already passed the row. So it is correct
        // for a cursor that trails the pivot as well as one sitting on it,
        // which is what lets this run before the trailing cursors are
        // advanced instead of only after. Summed over every live cursor it
        // dominates the row's true score, because each term's contribution
        // is at or below its own block's pair.
        //
        // `bound_horizon_for` reports the row through which each cursor's
        // answer is unchanged; the minimum is the row through which the sum
        // is unchanged, and therefore the row through which the proof
        // holds. Extending target to just past it and asking again walks
        // condemned blocks at one metadata read each rather than decoding
        // them one at a time.
        //
        // # The bug this replaces
        //
        // The previous form capped the jump at the end of the block the
        // pivot was standing in. However many blocks the bound had just
        // condemned, the cursor landed in the very next one, decoded it,
        // asked the same question and stepped again. `blocks_decoded` was
        // the whole list at every `k` and `blocks_skipped` was structurally
        // zero: block-max WAND had become a slower way to skip documents.
        if heap.is_full() {
            let mut target = pivot_row;
            let mut condemned = false;
            loop {
                let mut bound = 0.0_f64;
                let mut edge: Option<u32> = None;
                for cursor in cursors.iter() {
                    if cursor.exhausted() {
                        continue;
                    }
                    bound += cursor.stream.bound_for(target, &cursor.scorer);
                    if let Some(found) = cursor.stream.bound_horizon_for(target) {
                        edge = Some(edge.map_or(found, |held: u32| held.min(found)));
                    }
                }
                if bound > threshold {
                    break;
                }
                // No cursor bounds the span: every one of them is spent at
                // or past `target` and can never contribute again, so there
                // is nothing left to extend over.
                let Some(edge) = edge else {
                    break;
                };
                let next = edge.saturating_add(1);
                // A span that does not move is an infinite loop.
                // `bound_horizon_for` never reports below its own argument,
                // so this advances; the guard is against a degenerate
                // cursor rather than an expected case.
                if next <= target {
                    break;
                }
                target = next;
                condemned = true;
            }
            if condemned {
                for cursor in cursors.iter_mut() {
                    if cursor.current().is_some_and(|row| row < target) {
                        // Blocks jumped are tallied on the stream itself
                        // and collected once the segment finishes.
                        cursor.seek(target);
                    }
                }
                continue;
            }
        }

        if first_row == pivot_row {
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
