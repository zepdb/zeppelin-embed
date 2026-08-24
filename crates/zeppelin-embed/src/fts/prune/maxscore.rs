//! Block-max MAXSCORE.
//!
//! # The mechanism
//!
//! Sort the query terms by their upper bound, ascending. Walk that order
//! accumulating bounds: the longest prefix whose bounds sum to at most the
//! current top-k threshold can never, on its own, lift a document into the
//! results. Those terms are **non-essential** — they are still scored, but
//! they no longer drive iteration. Only the essential suffix supplies
//! candidate documents.
//!
//! As the threshold rises the essential set shrinks, so a query that starts
//! by touching everything ends by touching only its rarest term. That is
//! why MAXSCORE degrades gracefully as terms are added, and why Lucene
//! chose it (`research/02a:282`).
//!
//! # Where correctness lives
//!
//! A document reached only through non-essential terms cannot beat the
//! threshold, because the sum of their bounds is at most the threshold by
//! construction. Every other document is enumerated, and either fully
//! scored — with the non-essential terms added back in — or abandoned on a
//! sum of exact contributions and upper bounds that proves it cannot reach
//! the threshold. Nothing is dropped on an estimate: every drop is a proof.

use crate::fts::bm25::DocLen;
use crate::fts::bm25::Tf;
use crate::fts::search::{GlobalDocId, SearchCounters};

use super::{TermCursor, TopK};

/// Returns the weighted length of one row, defaulting to one token.
fn length_of(lengths: &[u32], row: u32) -> u32 {
    usize::try_from(row)
        .ok()
        .and_then(|slot| lengths.get(slot).copied())
        .unwrap_or(1)
}

/// Scores every candidate exhaustively. The short-list fast path.
///
/// Used when the posting lists are too short for pruning to pay for itself;
/// it produces exactly the same documents and scores.
pub fn score_all(
    cursors: &mut [TermCursor<'_>],
    lengths: &[u32],
    segment: u32,
    heap: &mut TopK,
    counters: &mut SearchCounters,
) {
    // Kept sorted by row so accumulation is a binary search rather than a
    // linear scan. The list is bounded at SHORT_LIST_POSTINGS today, which
    // makes the quadratic form harmless -- but it is exactly the defect
    // class already found and fixed three times elsewhere in this engine,
    // and being harmless is not the same as being right.
    let mut totals: Vec<(u32, f64)> = Vec::new();
    for cursor in cursors.iter_mut() {
        cursor.reset();
        while let Some(row) = cursor.stream.current_row() {
            counters.postings_decoded = counters.postings_decoded.saturating_add(1);
            let tf = cursor.stream.current_tf().unwrap_or(0);
            let score = cursor.scorer.score(Tf(tf), DocLen(length_of(lengths, row)));
            match totals.binary_search_by_key(&row, |(candidate, _)| *candidate) {
                Ok(slot) => {
                    if let Some((_, total)) = totals.get_mut(slot) {
                        *total += score;
                    }
                }
                Err(slot) => totals.insert(slot, (row, score)),
            }
            cursor.advance();
        }
    }
    for (row, score) in totals {
        counters.docs_evaluated = counters.docs_evaluated.saturating_add(1);
        heap.offer(GlobalDocId { segment, row }, score);
    }
}

/// Runs block-max MAXSCORE over one segment's cursors.
///
/// Corpus statistics and BM25 parameters are not arguments: each cursor
/// carries its own [`crate::fts::bm25::TermScorer`], built once from exactly
/// those inputs.
pub fn run(
    cursors: &mut [TermCursor<'_>],
    lengths: &[u32],
    segment: u32,
    heap: &mut TopK,
    counters: &mut SearchCounters,
) {
    // Ascending by upper bound: cheapest terms become non-essential first.
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    order.sort_by(|left, right| {
        let a = cursors.get(*left).map_or(0.0, |c| c.upper_bound);
        let b = cursors.get(*right).map_or(0.0, |c| c.upper_bound);
        a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
    });

    for cursor in cursors.iter_mut() {
        cursor.reset();
    }

    // Both are loop-invariant in shape, and the bound total is invariant in
    // value: upper bounds never change during a run. Recomputing the sum per
    // candidate was O(terms) work and the Vec was an allocation per
    // candidate.
    let mut contributions: Vec<f64> = vec![0.0; cursors.len()];
    let total_bound: f64 = order
        .iter()
        .filter_map(|slot| cursors.get(*slot))
        .map(|cursor| cursor.upper_bound)
        .sum();

    loop {
        let threshold = heap.threshold();
        // Non-essential terms are the longest CHEAP-END PREFIX whose bounds
        // sum to at most the threshold: a document reachable only through
        // them cannot beat a full top-k, so they need not drive iteration.
        // Testing the prefix is the whole rule; testing the suffix instead
        // declares terms non-essential that can still lift a document, and
        // the equivalence property catches it as a missing result.
        let mut essential_from = 0_usize;
        let mut accumulated = 0.0_f64;
        while essential_from < order.len() {
            let bound = order
                .get(essential_from)
                .and_then(|slot| cursors.get(*slot))
                .map_or(0.0, |cursor| cursor.upper_bound);
            if heap.is_full() && accumulated + bound <= threshold {
                accumulated += bound;
                essential_from += 1;
            } else {
                break;
            }
        }
        let essential = order.get(essential_from..).unwrap_or(&[]);
        if essential.is_empty() {
            break;
        }

        // The next candidate is the smallest current row among essentials.
        let mut candidate: Option<u32> = None;
        for slot in essential {
            let Some(cursor) = cursors.get(*slot) else {
                continue;
            };
            if let Some(row) = cursor.current() {
                candidate = Some(candidate.map_or(row, |best: u32| best.min(row)));
            }
        }
        let Some(row) = candidate else {
            break;
        };

        // Score the candidate across every term, essential or not.
        //
        // Contributions are collected per TERM SLOT and summed in slot
        // order at the end, never in the bound-sorted order this loop
        // walks. Floating-point addition is not associative, so summing in
        // a different order than the exhaustive scorer differs from it by
        // an ULP — which is a changed result, and the equivalence property
        // is right to reject it.
        contributions.fill(0.0);
        let mut running = 0.0_f64;
        let mut remaining_bound = total_bound;
        let mut abandoned = false;
        // Probe DESCENDING by upper bound. The expensive terms are the
        // essential suffix, whose cursors already sit at or past the
        // candidate, so their exact contributions arrive before any cheap
        // cursor is asked to move — and the bound test below can then
        // cancel those moves outright. Probing ascending pays the long
        // lists' seeks before the test that would have spared them can
        // possibly fire; that inversion was most of MAXSCORE's posting
        // traffic.
        for slot in order.iter().rev() {
            let Some(cursor) = cursors.get_mut(*slot) else {
                continue;
            };
            if heap.is_full() && cursor.current() != Some(row) {
                // Before paying this cursor's seek: bound what it could
                // still add from the impact pair of the block that would
                // hold the row, read from metadata without decoding. Zero
                // when the cursor has passed the row, which is exact.
                let ceiling = cursor.stream.bound_for(row, &cursor.scorer);
                if running + ceiling + (remaining_bound - cursor.upper_bound) < threshold {
                    abandoned = true;
                    break;
                }
            }
            // Blocks jumped are tallied on the stream itself and collected
            // once the segment finishes.
            cursor.seek(row);
            let contribution = if cursor.current() == Some(row) {
                counters.postings_decoded = counters.postings_decoded.saturating_add(1);
                cursor.current_tf().map_or(0.0, |tf| {
                    cursor.scorer.score(Tf(tf), DocLen(length_of(lengths, row)))
                })
            } else {
                0.0
            };
            if let Some(entry) = contributions.get_mut(*slot) {
                *entry = contribution;
            }
            running += contribution;
            remaining_bound -= cursor.upper_bound;
            // Bail out only when even a perfect remainder cannot reach the
            // threshold. `remaining_bound` is a sum of upper bounds, so this
            // can never discard a document that would have qualified.
            if running + remaining_bound < threshold && heap.is_full() {
                abandoned = true;
                break;
            }
        }

        if !abandoned {
            let total: f64 = contributions.iter().sum();
            counters.docs_evaluated = counters.docs_evaluated.saturating_add(1);
            heap.offer(GlobalDocId { segment, row }, total);
        }

        // Advance past the candidate everywhere it appears.
        for cursor in cursors.iter_mut() {
            if cursor.current() == Some(row) {
                cursor.advance();
            }
        }
        if cursors.iter().all(TermCursor::exhausted) {
            break;
        }
    }
}
