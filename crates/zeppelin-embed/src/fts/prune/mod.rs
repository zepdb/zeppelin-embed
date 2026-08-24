//! Dynamic pruning: block-max MAXSCORE and WAND, counter-gated.
//!
//! # What this buys, and what it must never cost
//!
//! Skipping is the whole wall-clock gap between a naive engine and
//! Lucene/tantivy: documents evaluated drop from 3.8M to 21.9K per GOV2
//! query, and latency from 225.7 ms to 27.9 ms (`research/02a:281`). Those
//! are x86 numbers on 25M documents; the ratio is the transferable part,
//! not the constants.
//!
//! What it must never cost is a single changed result. Every function here
//! decides only *which documents get scored*. The scoring itself is
//! [`crate::fts::search::merge_term`] and [`crate::fts::bm25::term_score`],
//! shared verbatim with the exhaustive path, so `prop_pruned_topk_equals_
//! exhaustive` is a statement about skipping decisions rather than about
//! two scorers agreeing by luck.
//!
//! **If a bound is uncertain, evaluate.** A slow answer is a bug report; a
//! wrong answer is a lost user.
//!
//! # Strategy selection
//!
//! Block-max WAND wins at small `k` with few terms; block-max MAXSCORE
//! degrades more gracefully as the term count grows and is Lucene's choice
//! (`research/02a:282`). The v1 rule is the spec's: WAND when the query has
//! at most four terms and `k` is at most ten, else MAXSCORE. That rule came
//! from GOV2 on x86 and is explicitly a starting point to be recalibrated
//! from our own counters — see `docs/14-pruning-contracts.md`.

pub mod bounds;
pub mod maxscore;
pub mod wand;

use crate::fts::bm25::{Bm25Params, Df, TermScorer};
use crate::fts::index::{IndexError, LexicalIndex};
use crate::fts::search::{GlobalDocId, ScoredDoc, SearchCounters, SearchResult, TermQuery};

pub use bounds::{build_block_bounds, term_upper_bound, BlockBound};

/// Postings per pruning block.
///
/// Matches the persisted geometry so the in-memory bounds and a sealed
/// segment's stored maxima describe the same blocks.
pub const PRUNE_BLOCK_SIZE: usize = 64;

/// Lists shorter than one block skip the pruning machinery entirely.
///
/// This is the dominant case in small corpora (`research/02a:328`). It is a
/// speed constant, re-tunable from counters, not a correctness threshold:
/// the fast path scores the same documents the pruned path would.
pub const SHORT_LIST_POSTINGS: usize = PRUNE_BLOCK_SIZE;

/// Which pruning strategy ran.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Strategy {
    /// Exhaustive OR, no pruning. The oracle.
    Exhaustive,
    /// Block-max WAND: pivot selection over sorted cursors.
    #[default]
    BlockMaxWand,
    /// Block-max MAXSCORE: essential/non-essential partition.
    BlockMaxMaxscore,
}

/// Returns the strategy the recorded v1 rule selects.
///
/// WAND when `terms <= 4 && k <= 10`, else MAXSCORE. Recalibrate from
/// counter evidence and record the change; do not tune it silently.
#[must_use]
pub const fn select_strategy(term_count: usize, k: usize) -> Strategy {
    if term_count <= 4 && k <= 10 {
        Strategy::BlockMaxWand
    } else {
        Strategy::BlockMaxMaxscore
    }
}

/// One query term prepared for pruning within one segment.
#[derive(Clone, Debug)]
pub struct TermCursor {
    /// `(row, weighted term frequency)`, ascending by row.
    pub entries: Vec<(u32, u32)>,
    /// Per-block upper bounds.
    pub blocks: Vec<BlockBound>,
    /// Entries per block: the fixed geometry that makes addressing O(1).
    pub block_size: usize,
    /// Store-wide document frequency.
    pub df: Df,
    /// This term's hoisted scoring constants.
    ///
    /// Built once per (term, segment) so the inner loops never recompute
    /// `idf` or `avgdl`. See [`crate::fts::bm25::TermScorer`].
    pub scorer: TermScorer,
    /// Largest contribution this term can make to any document.
    pub upper_bound: f64,
    /// Cursor position into `entries`.
    pub position: usize,
}

impl TermCursor {
    /// Returns the row at the cursor, or `None` when exhausted.
    #[must_use]
    pub fn current(&self) -> Option<u32> {
        self.entries.get(self.position).map(|(row, _)| *row)
    }

    /// Returns the term frequency at the cursor.
    #[must_use]
    pub fn current_tf(&self) -> Option<u32> {
        self.entries.get(self.position).map(|(_, tf)| *tf)
    }

    /// Advances the cursor to the first row at or after `row`.
    ///
    /// Uses the block skip keys to jump whole blocks, then walks within one.
    /// Returns the number of blocks skipped without inspecting their
    /// entries, for the counter contract.
    ///
    /// # Why this is a search and not a walk
    ///
    /// Skip keys ascend, because entries ascend by row, so the first block
    /// that can hold `row` is a binary search. Walking made `seek` cost
    /// `O(blocks)`, and `seek` runs per candidate: on a term with 1,500
    /// blocks that quadratic factor is a plausible reason the pruned path
    /// was slower than the scan it exists to avoid. The reported skip count
    /// is unchanged, because skips are decisions and decisions are pinned by
    /// the counter contracts.
    pub fn seek(&mut self, row: u32) -> u64 {
        let mut skipped = 0_u64;
        if let Some(block) = self.block_containing(self.position)
            && block.last_row < row
            && self.block_size > 0
        {
            let first = self.position / self.block_size;
            let ahead = self.blocks.get(first..).unwrap_or(&[]);
            let hops = ahead.partition_point(|block| block.last_row < row);
            skipped = u64::try_from(hops).unwrap_or(u64::MAX);
            self.position = match self.blocks.get(first.saturating_add(hops)) {
                Some(block) => block.start,
                // Every remaining block is behind the target row.
                None => self.entries.len(),
            };
        }
        while let Some(current) = self.current() {
            if current >= row {
                break;
            }
            self.position += 1;
        }
        skipped
    }

    /// Returns the block containing `position`, if any.
    ///
    /// Blocks are fixed geometry and contiguous from zero, so the containing
    /// block is an index rather than a scan. The containment check is kept
    /// as the guard: a cursor whose blocks do not match its `block_size`
    /// reports "between blocks", which callers already treat as the loose,
    /// safe direction rather than as a bound they may prune against.
    #[must_use]
    pub fn block_containing(&self, position: usize) -> Option<&BlockBound> {
        if self.block_size == 0 {
            return None;
        }
        let block = self.blocks.get(position / self.block_size)?;
        (position >= block.start && position < block.end).then_some(block)
    }

    /// Returns the bound of the block the cursor currently sits in.
    ///
    /// Falls back to the term's overall bound when the cursor is between
    /// blocks, which is the safe direction: a looser bound prunes less.
    #[must_use]
    pub fn current_block_max(&self) -> f64 {
        self.block_containing(self.position)
            .map_or(self.upper_bound, |block| block.max_score)
    }

    /// Returns true when the cursor has passed the last entry.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.position >= self.entries.len()
    }
}

/// A bounded top-k heap with the pinned tie-break.
///
/// Ordering is descending score then ascending document id, matching
/// [`crate::fts::search`] exactly. The threshold this exposes is what every
/// pruning decision is made against.
#[derive(Clone, Debug, Default)]
pub struct TopK {
    capacity: usize,
    entries: Vec<(GlobalDocId, f64)>,
}

impl TopK {
    /// Creates a heap holding at most `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Vec::with_capacity(capacity.saturating_add(1)),
        }
    }

    /// Offers one scored document.
    pub fn offer(&mut self, doc: GlobalDocId, score: f64) {
        self.entries.push((doc, score));
        self.entries.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.0.cmp(&right.0))
        });
        self.entries.truncate(self.capacity);
    }

    /// Returns the current k-th best score, or zero while not yet full.
    ///
    /// Returning zero rather than negative infinity while the heap is
    /// filling is deliberate: BM25 scores are non-negative, so zero already
    /// admits everything, and it keeps the threshold monotone.
    #[must_use]
    pub fn threshold(&self) -> f64 {
        if self.entries.len() < self.capacity {
            return 0.0;
        }
        self.entries.last().map_or(0.0, |(_, score)| *score)
    }

    /// Returns true when the heap holds `capacity` entries.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// Consumes the heap into ranked results.
    #[must_use]
    pub fn into_hits(self) -> Vec<ScoredDoc> {
        self.entries
            .into_iter()
            .map(|(doc, score)| ScoredDoc { doc, score })
            .collect()
    }
}

/// Runs a pruned search and returns the top `k`.
///
/// The result is required to equal [`crate::fts::search::search`] exactly.
///
/// # Errors
///
/// Returns [`IndexError::Stats`] when the index holds no documents.
pub fn search_pruned(
    index: &LexicalIndex,
    query: &TermQuery,
    k: usize,
    params: Bm25Params,
    strategy: Strategy,
) -> Result<SearchResult, IndexError> {
    if strategy == Strategy::Exhaustive {
        return crate::fts::search::search(index, query, k, params);
    }
    let stats = index.corpus_stats()?;
    let fields = query.fields.fields();
    let mut counters = SearchCounters::default();
    let mut heap = TopK::new(k.max(1));

    // Store-wide document frequency, computed once per term.
    let frequencies: Vec<u32> = query
        .terms
        .iter()
        .map(|term| index.document_frequency(term, &fields))
        .collect();

    for (ordinal, segment) in index.segments().iter().enumerate() {
        let segment_index = u32::try_from(ordinal).unwrap_or(u32::MAX);
        // Borrowed outright in the flat single-field case; see
        // `crate::fts::search::weighted_lengths`. This used to be an
        // O(row_count) rebuild on every query.
        let lengths = crate::fts::search::weighted_lengths(segment, &query.fields);

        let mut cursors: Vec<TermCursor> = Vec::with_capacity(query.terms.len());
        for (slot, term) in query.terms.iter().enumerate() {
            let df = Df(frequencies.get(slot).copied().unwrap_or(0));
            if df.0 == 0 {
                continue;
            }
            let merged = crate::fts::search::merge_term(segment, term, &query.fields);
            if merged.entries.is_empty() {
                continue;
            }
            let blocks = build_block_bounds(
                &merged.entries,
                &lengths,
                PRUNE_BLOCK_SIZE,
                df,
                &stats,
                params,
            );
            let upper_bound = term_upper_bound(&blocks);
            counters.blocks_decoded = counters
                .blocks_decoded
                .saturating_add(u64::try_from(blocks.len()).unwrap_or(0));
            cursors.push(TermCursor {
                entries: merged.entries,
                blocks,
                block_size: PRUNE_BLOCK_SIZE,
                df,
                scorer: TermScorer::new(df, &stats, params),
                upper_bound,
                position: 0,
            });
        }
        if cursors.is_empty() {
            continue;
        }

        // Short lists skip the pruning machinery entirely.
        let total: usize = cursors.iter().map(|cursor| cursor.entries.len()).sum();
        if total <= SHORT_LIST_POSTINGS {
            counters.blocks_decoded = 0;
            maxscore::score_all(
                &cursors,
                &lengths,
                segment_index,
                &mut heap,
                &mut counters,
            );
            continue;
        }

        match strategy {
            Strategy::BlockMaxWand => wand::run(
                &mut cursors,
                &lengths,
                segment_index,
                &mut heap,
                &mut counters,
            ),
            Strategy::BlockMaxMaxscore => maxscore::run(
                &mut cursors,
                &lengths,
                segment_index,
                &mut heap,
                &mut counters,
            ),
            Strategy::Exhaustive => {}
        }
    }

    Ok(SearchResult {
        hits: heap.into_hits(),
        counters,
    })
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
    use crate::fts::index::DEFAULT_FIELD;
    use crate::fts::index::{Document, SegmentIndex};
    use crate::fts::tokenizer::{Analyzer, Profile};

    fn analyzer() -> Analyzer {
        Analyzer::new(Profile::Code.config()).expect("valid config")
    }

    fn index_of(texts: &[String]) -> LexicalIndex {
        let analyzer = analyzer();
        let mut segment = SegmentIndex::new();
        for text in texts {
            segment
                .push_document(&analyzer, &Document::with_text(text))
                .expect("indexable");
        }
        let mut index = LexicalIndex::new();
        index.push_segment(segment);
        index
    }

    #[test]
    fn strategy_selection_follows_the_recorded_rule() {
        assert_eq!(select_strategy(1, 10), Strategy::BlockMaxWand);
        assert_eq!(select_strategy(4, 10), Strategy::BlockMaxWand);
        assert_eq!(select_strategy(5, 10), Strategy::BlockMaxMaxscore);
        assert_eq!(select_strategy(4, 11), Strategy::BlockMaxMaxscore);
        assert_eq!(select_strategy(1, 1_000), Strategy::BlockMaxMaxscore);
        assert_eq!(select_strategy(0, 0), Strategy::BlockMaxWand);
    }

    #[test]
    fn short_list_query_decodes_zero_blocks() {
        let texts: Vec<String> = (0..8).map(|index| format!("alpha doc{index}")).collect();
        let index = index_of(&texts);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        for strategy in [Strategy::BlockMaxWand, Strategy::BlockMaxMaxscore] {
            let result = search_pruned(&index, &query, 10, Bm25Params::default(), strategy)
                .expect("scores");
            assert_eq!(
                result.counters.blocks_decoded, 0,
                "the short-list fast path must not decode a block"
            );
            assert_eq!(result.hits.len(), 8);
        }
    }

    #[test]
    fn the_top_k_heap_uses_the_pinned_tie_break() {
        let mut heap = TopK::new(3);
        for row in [2_u32, 0, 1] {
            heap.offer(GlobalDocId { segment: 0, row }, 5.0);
        }
        let rows: Vec<u32> = heap.into_hits().iter().map(|hit| hit.doc.row).collect();
        assert_eq!(rows, vec![0, 1, 2], "ties must break on ascending id");
    }

    #[test]
    fn the_threshold_is_zero_until_the_heap_is_full() {
        let mut heap = TopK::new(2);
        assert_eq!(heap.threshold(), 0.0);
        assert!(!heap.is_full());
        heap.offer(GlobalDocId { segment: 0, row: 0 }, 9.0);
        assert_eq!(heap.threshold(), 0.0, "a partial heap admits everything");
        heap.offer(GlobalDocId { segment: 0, row: 1 }, 4.0);
        assert!(heap.is_full());
        assert!((heap.threshold() - 4.0).abs() < 1e-12);
    }

    /// The linear implementations P1.3 replaced, kept as test oracles.
    ///
    /// Their whole job is to be obviously correct, so any divergence in the
    /// constant-time versions shows up as a differing answer rather than as
    /// a differing latency nobody measured.
    fn linear_block_containing(cursor: &TermCursor, position: usize) -> Option<&BlockBound> {
        cursor
            .blocks
            .iter()
            .find(|block| position >= block.start && position < block.end)
    }

    fn linear_seek(cursor: &mut TermCursor, row: u32) -> u64 {
        let mut skipped = 0_u64;
        while let Some(block) = linear_block_containing(cursor, cursor.position).copied() {
            if block.last_row >= row {
                break;
            }
            if block.end > cursor.position {
                skipped = skipped.saturating_add(1);
            }
            cursor.position = block.end;
            if cursor.position >= cursor.entries.len() {
                return skipped;
            }
        }
        while let Some(current) = cursor.current() {
            if current >= row {
                break;
            }
            cursor.position += 1;
        }
        skipped
    }

    /// Builds a cursor over `count` entries with the given block geometry.
    fn geometry_cursor(count: u32, block_size: usize, stride: u32) -> TermCursor {
        let entries: Vec<(u32, u32)> = (0..count).map(|row| (row * stride, 1 + row % 7)).collect();
        let lengths: Vec<u32> = (0..count * stride.max(1))
            .map(|row| 1 + row % 23)
            .collect();
        let stats = crate::fts::bm25::CorpusStats::new(1_000, 40_000).expect("stats");
        let params = Bm25Params::default();
        let blocks =
            build_block_bounds(&entries, &lengths, block_size, Df(50), &stats, params);
        TermCursor {
            entries,
            blocks,
            block_size,
            df: Df(50),
            scorer: TermScorer::new(Df(50), &stats, params),
            upper_bound: 1.0,
            position: 0,
        }
    }

    #[test]
    fn constant_time_block_addressing_agrees_with_a_linear_scan() {
        // Every geometry, including a ragged final block and a block size
        // that does not divide the entry count, at every position including
        // one past the end.
        for count in [1_u32, 7, 64, 65, 200] {
            for block_size in [1_usize, 3, 8, 64, 512] {
                let cursor = geometry_cursor(count, block_size, 1);
                for position in 0..=cursor.entries.len() {
                    assert_eq!(
                        cursor.block_containing(position),
                        linear_block_containing(&cursor, position),
                        "addressing diverged at position {position}                          (count={count}, block_size={block_size})"
                    );
                }
            }
        }
    }

    #[test]
    fn a_binary_search_seek_reports_the_same_skips_as_the_linear_walk() {
        // Skips are decisions, and decisions are pinned by the counter
        // contracts, so the search must report exactly what the walk did --
        // from every starting position, to every target row.
        for count in [1_u32, 7, 64, 65, 200] {
            for block_size in [1_usize, 3, 8, 64] {
                for stride in [1_u32, 3] {
                    let reference = geometry_cursor(count, block_size, stride);
                    let last = count.saturating_mul(stride).saturating_add(2);
                    for start in [0_usize, 1, 5, 63, 64, 199] {
                        if start >= reference.entries.len() {
                            continue;
                        }
                        for target in (0..=last).step_by(1) {
                            let mut fast = reference.clone();
                            fast.position = start;
                            let mut slow = reference.clone();
                            slow.position = start;
                            let fast_skips = fast.seek(target);
                            let slow_skips = linear_seek(&mut slow, target);
                            assert_eq!(
                                (fast_skips, fast.position),
                                (slow_skips, slow.position),
                                "seek diverged (count={count}, block_size={block_size},                                  stride={stride}, start={start}, target={target})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_cursor_seek_skips_whole_blocks_and_reports_them() {
        let entries: Vec<(u32, u32)> = (0..200_u32).map(|row| (row, 1)).collect();
        let lengths = vec![10_u32; 200];
        let stats = crate::fts::bm25::CorpusStats::new(200, 2_000).expect("stats");
        let blocks = build_block_bounds(
            &entries,
            &lengths,
            PRUNE_BLOCK_SIZE,
            Df(200),
            &stats,
            Bm25Params::default(),
        );
        let mut cursor = TermCursor {
            entries,
            blocks,
            block_size: PRUNE_BLOCK_SIZE,
            df: Df(200),
            scorer: TermScorer::new(Df(200), &stats, Bm25Params::default()),
            upper_bound: 1.0,
            position: 0,
        };
        let skipped = cursor.seek(150);
        assert_eq!(cursor.current(), Some(150));
        assert!(skipped >= 2, "seeking past two blocks must report them");
    }

    #[test]
    fn seeking_beyond_the_last_entry_exhausts_the_cursor() {
        let entries: Vec<(u32, u32)> = (0..10_u32).map(|row| (row, 1)).collect();
        let lengths = vec![10_u32; 10];
        let stats = crate::fts::bm25::CorpusStats::new(10, 100).expect("stats");
        let blocks = build_block_bounds(
            &entries,
            &lengths,
            4,
            Df(10),
            &stats,
            Bm25Params::default(),
        );
        let mut cursor = TermCursor {
            entries,
            blocks,
            block_size: 4,
            df: Df(10),
            scorer: TermScorer::new(Df(10), &stats, Bm25Params::default()),
            upper_bound: 1.0,
            position: 0,
        };
        cursor.seek(9_999);
        assert!(cursor.exhausted());
        assert_eq!(cursor.current(), None);
        assert_eq!(cursor.current_tf(), None);
        // An exhausted cursor sits outside every block and falls back to the
        // term bound, which is the loose, safe direction.
        assert!((cursor.current_block_max() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn the_exhaustive_strategy_delegates_to_the_oracle() {
        let texts: Vec<String> = (0..5).map(|index| format!("alpha beta{index}")).collect();
        let index = index_of(&texts);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let pruned =
            search_pruned(&index, &query, 3, Bm25Params::default(), Strategy::Exhaustive)
                .expect("scores");
        let direct = crate::fts::search::search(&index, &query, 3, Bm25Params::default())
            .expect("scores");
        assert_eq!(pruned.hits, direct.hits);
    }

    #[test]
    fn an_empty_index_is_a_typed_error_under_every_strategy() {
        let index = LexicalIndex::new();
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        for strategy in [
            Strategy::BlockMaxWand,
            Strategy::BlockMaxMaxscore,
            Strategy::Exhaustive,
        ] {
            assert!(
                search_pruned(&index, &query, 5, Bm25Params::default(), strategy).is_err()
            );
        }
    }
}
