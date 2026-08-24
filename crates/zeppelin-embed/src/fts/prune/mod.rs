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
use crate::fts::sealed::TermStream;
use crate::fts::search::{GlobalDocId, ScoredDoc, SearchCounters, SearchResult, TermQuery};

pub use bounds::{BlockBound, build_block_bounds, impact_of, term_upper_bound};

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

/// Term count at and above which the rule prefers WAND. Three terms or
/// fewer go to MAXSCORE; see [`select_strategy`] for the measurement.
pub const MIN_TERMS_FOR_WAND: usize = 4;

/// Returns the strategy the measured rule selects.
///
/// **MAXSCORE at three terms or fewer, WAND at four or more, at any `k`.**
///
/// # Provenance: measured here, and re-derived when the engine changed
///
/// The original rule — WAND when `terms <= 4 && k <= 10` — came from GOV2
/// on x86 at 25 million documents (`research/02a:282`). Recalibrating it
/// on this engine's own counters first moved the boundary to two terms;
/// fixing MAXSCORE's probe order (descending by bound, with a
/// metadata-only block bound tested before every seek) then made MAXSCORE
/// uniformly cheaper and moved the crossover again, from three terms to
/// four. This is exactly why
/// `strategy_rule_matches_the_counters_it_was_calibrated_from` re-derives
/// the rule on every run rather than trusting a number in a comment.
///
/// What the counters say now, at 100,000 documents:
///
/// - **At two and three terms MAXSCORE wins**: 23,594 cost units against
///   WAND's 26,819 at three terms, `k` 10. With few cursors the pivot has
///   little to choose between, and MAXSCORE no longer pays a seek per
///   cursor per candidate.
/// - **At four terms and above WAND wins** at the corpus sizes that
///   matter, on the posting counter that is the inner loop.
/// - **`k` still does not discriminate at scale.** The 2,000-document
///   corpus at `k` of 100 prefers MAXSCORE at every term count, but both
///   strategies cost about 2,000 units there — sub-millisecond either way
///   — and a rule fitted to that regime would misroute the large corpora
///   where the choice is worth something.
///
/// **Both strategies return identical results** — `prune_equivalence`
/// gates that — so this is a cost decision with no quality risk.
///
/// **Owed:** confirmation on BEIR. This corpus is synthetic, with uniform
/// term frequencies and lengths. Replacing a constant measured on someone
/// else's hardware and collection with one measured on ours is a strict
/// improvement, but it is not the same as measuring the real thing.
#[must_use]
pub const fn select_strategy(term_count: usize, k: usize) -> Strategy {
    // `k` is accepted and deliberately unused: the counters say it does not
    // discriminate, and dropping the parameter would be a breaking API
    // change made for a cosmetic reason.
    let _ = k;
    if term_count >= MIN_TERMS_FOR_WAND {
        Strategy::BlockMaxWand
    } else {
        Strategy::BlockMaxMaxscore
    }
}

/// One query term prepared for pruning within one segment.
///
/// The cursor is a view over the sealed streams, not a copy of them. It
/// holds no entry array: `advance` and `seek` walk block metadata and
/// decode only the blocks the traversal actually lands in.
#[derive(Clone, Debug)]
pub struct TermCursor<'segment> {
    /// The merged sealed postings for this term across the weighted fields.
    pub stream: TermStream<'segment>,
    /// Store-wide document frequency.
    pub df: Df,
    /// This term's hoisted scoring constants.
    ///
    /// Built once per (term, segment) so the inner loops never recompute
    /// `idf` or `avgdl`. See [`crate::fts::bm25::TermScorer`].
    pub scorer: TermScorer,
    /// Largest contribution this term can make to any document.
    pub upper_bound: f64,
}

impl TermCursor<'_> {
    /// Returns the row at the cursor, or `None` when exhausted.
    #[must_use]
    pub fn current(&self) -> Option<u32> {
        self.stream.current().map(|(row, _)| row)
    }

    /// Returns the merged term frequency at the cursor.
    #[must_use]
    pub fn current_tf(&self) -> Option<u32> {
        self.stream.current().map(|(_, tf)| tf)
    }

    /// Advances one merged posting.
    pub fn advance(&mut self) {
        self.stream.advance();
    }

    /// Rewinds to the first merged posting.
    pub fn reset(&mut self) {
        self.stream.reset();
    }

    /// Advances to the first row at or after `row`.
    ///
    /// Skip keys ascend, so the first block that can hold `row` is a binary
    /// search over metadata rows; the blocks between are never decoded.
    pub fn seek(&mut self, row: u32) {
        self.stream.seek(row);
    }

    /// Returns the bound of the block the cursor currently sits in.
    ///
    /// Read from the stored impact pairs and evaluated against this query's
    /// live statistics; see [`crate::fts::sealed`].
    #[must_use]
    pub fn current_block_max(&self) -> f64 {
        self.stream.block_bound(&self.scorer)
    }

    /// Returns true when the cursor has passed the last entry.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.stream.exhausted()
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

    /// The pinned ranking order: descending score, then ascending id.
    ///
    /// One definition, used by both the insertion and the tests that check
    /// it against a full sort. Two spellings of a comparator is how a
    /// tie-break drifts.
    fn rank(left: &(GlobalDocId, f64), right: &(GlobalDocId, f64)) -> std::cmp::Ordering {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    }

    /// Offers one scored document.
    ///
    /// Insertion into a sorted array, not a sort on every offer. For `k` of
    /// order one hundred that is strictly less work, and it reproduces the
    /// pinned tie-break exactly: the comparator is the same one, and the
    /// scan stops at the first entry the newcomer does not precede, which is
    /// where a stable sort would have placed it.
    pub fn offer(&mut self, doc: GlobalDocId, score: f64) {
        if self.capacity == 0 {
            return;
        }
        let entry = (doc, score);
        // A full heap whose worst entry already outranks the newcomer would
        // have sorted it into the truncated tail. Dropping it here is the
        // same decision, taken without touching the array.
        if self.entries.len() >= self.capacity
            && self
                .entries
                .last()
                .is_some_and(|last| Self::rank(&entry, last) != std::cmp::Ordering::Less)
        {
            return;
        }
        let mut slot = self.entries.len();
        while slot > 0 {
            let Some(previous) = self.entries.get(slot.saturating_sub(1)) else {
                break;
            };
            if Self::rank(&entry, previous) == std::cmp::Ordering::Less {
                slot = slot.saturating_sub(1);
            } else {
                break;
            }
        }
        self.entries.insert(slot, entry);
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

        let mut cursors: Vec<TermCursor<'_>> = Vec::with_capacity(query.terms.len());
        for (slot, term) in query.terms.iter().enumerate() {
            let df = Df(frequencies.get(slot).copied().unwrap_or(0));
            if df.0 == 0 {
                continue;
            }
            let Some(stream) = TermStream::open(segment, term, &query.fields) else {
                continue;
            };
            if stream.exhausted() {
                continue;
            }
            let scorer = TermScorer::new(df, &stats, params);
            // Read, never rebuilt. The term's bound comes from the stored
            // impact pairs, which costs O(blocks) metadata reads; the
            // previous form scored every posting in order to bound it, and
            // that is why pruning was slower than the scan it exists to
            // avoid.
            let upper_bound = stream.upper_bound(&scorer);
            cursors.push(TermCursor {
                stream,
                df,
                scorer,
                upper_bound,
            });
        }
        if cursors.is_empty() {
            continue;
        }

        // Short lists skip the pruning machinery entirely. The count comes
        // from block metadata, so asking costs no decode.
        let total: usize = cursors
            .iter()
            .map(|cursor| cursor.stream.posting_count())
            .sum();
        if total <= SHORT_LIST_POSTINGS {
            maxscore::score_all(
                &mut cursors,
                &lengths,
                segment_index,
                &mut heap,
                &mut counters,
            );
        } else {
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
        for cursor in &cursors {
            counters.blocks_decoded = counters
                .blocks_decoded
                .saturating_add(cursor.stream.blocks_decoded());
            counters.blocks_skipped = counters
                .blocks_skipped
                .saturating_add(cursor.stream.blocks_skipped());
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
        index.push_segment(segment).expect("seals");
        index
    }

    #[test]
    fn strategy_selection_follows_the_measured_rule() {
        assert_eq!(select_strategy(1, 10), Strategy::BlockMaxMaxscore);
        assert_eq!(select_strategy(3, 10), Strategy::BlockMaxMaxscore);
        assert_eq!(select_strategy(4, 10), Strategy::BlockMaxWand);
        assert_eq!(select_strategy(6, 10), Strategy::BlockMaxWand);
        // `k` no longer discriminates, at either end of the range.
        assert_eq!(select_strategy(3, 1_000), Strategy::BlockMaxMaxscore);
        assert_eq!(select_strategy(4, 1_000), Strategy::BlockMaxWand);
        assert_eq!(select_strategy(0, 0), Strategy::BlockMaxMaxscore);
    }

    #[test]
    fn short_list_query_takes_no_pruning_decision() {
        // The fast path reads its one block -- a posting cannot be scored
        // without being decoded -- but it makes no skipping decision and it
        // scores every match. `blocks_decoded` counts real decodes now that
        // the query path reads the sealed format, so "zero" would be a claim
        // about a path that no longer exists.
        let texts: Vec<String> = (0..8).map(|index| format!("alpha doc{index}")).collect();
        let index = index_of(&texts);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        for strategy in [Strategy::BlockMaxWand, Strategy::BlockMaxMaxscore] {
            let result =
                search_pruned(&index, &query, 10, Bm25Params::default(), strategy).expect("scores");
            assert_eq!(
                result.counters.blocks_decoded, 1,
                "eight postings live in exactly one block"
            );
            assert_eq!(
                result.counters.blocks_skipped, 0,
                "the short-list fast path must take no skipping decision"
            );
            assert_eq!(result.counters.docs_evaluated, 8);
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

    #[test]
    fn top_k_insertion_matches_a_full_sort_exactly() {
        // The sort-per-offer heap P1.4 replaced, as an oracle. Ties matter
        // most here, so scores are drawn from a deliberately small set: a
        // tie-break that drifts by one position is a changed result and the
        // equivalence property is right to reject it.
        use rand::Rng as _;
        let mut rng = crate::test_support::seeded_rng(
            "fts::prune::tests::top_k_insertion_matches_a_full_sort_exactly",
        );
        for capacity in [0_usize, 1, 2, 10, 100] {
            for _ in 0..64 {
                let mut heap = TopK::new(capacity);
                let mut oracle: Vec<(GlobalDocId, f64)> = Vec::new();
                for _ in 0..250 {
                    let doc = GlobalDocId {
                        segment: rng.random_range(0..3_u32),
                        row: rng.random_range(0..40_u32),
                    };
                    let score = f64::from(rng.random_range(0..5_u32));
                    heap.offer(doc, score);
                    oracle.push((doc, score));
                    oracle.sort_by(TopK::rank);
                    oracle.truncate(capacity);
                    assert_eq!(
                        heap.clone().into_hits(),
                        oracle
                            .iter()
                            .map(|(doc, score)| ScoredDoc {
                                doc: *doc,
                                score: *score
                            })
                            .collect::<Vec<_>>(),
                        "insertion diverged from a full sort at capacity {capacity}"
                    );
                    assert_eq!(
                        heap.threshold().to_bits(),
                        if oracle.len() < capacity {
                            0.0_f64
                        } else {
                            oracle.last().map_or(0.0, |(_, score)| *score)
                        }
                        .to_bits()
                    );
                }
            }
        }
    }

    /// Builds a single-term index whose rows advance by `stride`.
    fn strided_index(count: u32, stride: u32) -> LexicalIndex {
        let analyzer = analyzer();
        let mut segment = SegmentIndex::new();
        let mut row = 0_u32;
        for ordinal in 0..count {
            // Rows between the term's occurrences carry a different term, so
            // the posting list has real gaps rather than a dense run.
            while row < ordinal.saturating_mul(stride) {
                segment
                    .push_document(&analyzer, &Document::with_text("filler"))
                    .expect("indexable");
                row = row.saturating_add(1);
            }
            segment
                .push_document(&analyzer, &Document::with_text("alpha"))
                .expect("indexable");
            row = row.saturating_add(1);
        }
        let mut index = LexicalIndex::new();
        index.push_segment(segment).expect("seals");
        index
    }

    /// The rows a term occupies, as a linear oracle for `seek`.
    fn rows_of(index: &LexicalIndex) -> Vec<u32> {
        let segment = index.segments().first().expect("one segment");
        let mut stream = crate::fts::sealed::TermStream::open(
            segment,
            b"alpha",
            &crate::fts::search::FieldWeights::flat(&[DEFAULT_FIELD]),
        )
        .expect("stream");
        let mut rows = Vec::new();
        while let Some((row, _)) = stream.current() {
            rows.push(row);
            stream.advance();
        }
        rows
    }

    /// Opens a stream over the one term of a `strided_index`.
    fn stream_of(index: &LexicalIndex) -> crate::fts::sealed::TermStream<'_> {
        crate::fts::sealed::TermStream::open(
            index.segments().first().expect("one segment"),
            b"alpha",
            &crate::fts::search::FieldWeights::flat(&[DEFAULT_FIELD]),
        )
        .expect("stream")
    }

    #[test]
    fn a_sealed_seek_lands_exactly_where_a_linear_walk_would() {
        // Seeks are decisions, and decisions are pinned by the counter
        // contracts, so a binary search over skip keys must land on the same
        // row a walk would -- from every starting position, to every target.
        for count in [1_u32, 7, 64, 65, 200] {
            for stride in [1_u32, 3] {
                let index = strided_index(count, stride);
                let rows = rows_of(&index);
                assert_eq!(rows.len(), usize::try_from(count).expect("small"));
                let last = count.saturating_mul(stride).saturating_add(2);
                for start in [0_usize, 1, 5, 63, 64, 199] {
                    if start >= rows.len() {
                        continue;
                    }
                    for target in 0..=last {
                        let mut cursor = stream_of(&index);
                        for _ in 0..start {
                            cursor.advance();
                        }
                        cursor.seek(target);
                        // The oracle: the first row at or after `target`,
                        // never earlier than where the cursor already sat.
                        let expected = rows.iter().skip(start).copied().find(|row| *row >= target);
                        assert_eq!(
                            cursor.current().map(|(row, _)| row),
                            expected,
                            "seek diverged (count={count}, stride={stride}, \
                             start={start}, target={target})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_sealed_seek_skips_whole_blocks_without_decoding_them() {
        let index = strided_index(200, 1);
        let mut stream = stream_of(&index);
        let decoded = stream.blocks_decoded();
        stream.seek(150);
        assert_eq!(stream.current().map(|(row, _)| row), Some(150));
        assert!(
            stream.blocks_skipped() >= 1,
            "seeking past a whole block must report it"
        );
        assert_eq!(
            stream.blocks_decoded(),
            decoded + 1,
            "a seek decodes exactly the block it lands in"
        );
    }

    #[test]
    fn seeking_beyond_the_last_row_exhausts_the_cursor() {
        let index = strided_index(10, 1);
        let stats = index.corpus_stats().expect("stats");
        let scorer = TermScorer::new(Df(10), &stats, Bm25Params::default());
        let mut cursor = TermCursor {
            stream: stream_of(&index),
            df: Df(10),
            scorer,
            upper_bound: 1.0,
        };
        cursor.seek(9_999);
        assert!(cursor.exhausted());
        assert_eq!(cursor.current(), None);
        assert_eq!(cursor.current_tf(), None);
        // Every run is spent, so no block contributes and the bound
        // collapses to zero rather than to a stale number.
        assert!(cursor.current_block_max().abs() < 1e-12);
    }

    #[test]
    fn the_sealed_block_bound_matches_the_in_memory_oracle() {
        // `build_block_bounds` is the reference implementation of the same
        // impact-pair formula. Reading a bound out of bytes and building one
        // in memory must give the same number, or the equivalence property
        // passes against a fixture and fails against a real segment.
        let index = strided_index(200, 1);
        let stats = index.corpus_stats().expect("stats");
        let scorer = TermScorer::new(Df(200), &stats, Bm25Params::default());
        let rows = rows_of(&index);
        let entries: Vec<(u32, u32)> = rows.iter().map(|row| (*row, 1_u32)).collect();
        let segment = index.segments().first().expect("one segment");
        let lengths = segment.field_lengths(DEFAULT_FIELD).expect("lengths");
        let oracle = build_block_bounds(&entries, lengths, PRUNE_BLOCK_SIZE, &scorer);

        let mut stream = stream_of(&index);
        for block in &oracle {
            assert!(
                (stream.block_bound(&scorer) - block.max_score).abs() < 1e-15,
                "sealed and in-memory bounds diverged at block {:?}",
                block.start
            );
            for _ in block.start..block.end {
                stream.advance();
            }
        }
        let overall = term_upper_bound(&oracle);
        let fresh = stream_of(&index);
        assert!((fresh.upper_bound(&scorer) - overall).abs() < 1e-15);
    }

    #[test]
    fn maxscore_abandons_a_candidate_before_touching_the_cheap_cursor() {
        // One rare, high-impact term and one common, low-impact term. The
        // single document holding both fills the heap at k=1; after that,
        // every rare-only candidate is provably unable to reach the
        // threshold BEFORE the common cursor is asked to move, so the
        // common list's later blocks must never be decoded. A probe order
        // that seeks the cheap cursor first walks every common block and
        // fails this bound.
        let mut texts: Vec<String> = Vec::new();
        // Row 0: the top document. The rare term dominates its score.
        texts.push(format!("{} common", "rare ".repeat(30).trim()));
        // Ten long rare-only documents interleaved every twenty common
        // documents, so a seek-per-candidate traversal would land in every
        // common block in turn.
        for group in 0..10 {
            for slot in 0..20 {
                texts.push(format!("common c{group}x{slot}"));
            }
            let padding: String = (0..64).map(|word| format!("p{group}w{word} ")).collect();
            texts.push(format!("rare {padding}"));
        }
        let index = index_of(&texts);
        let query = TermQuery::flat(vec![b"rare".to_vec(), b"common".to_vec()], &[DEFAULT_FIELD]);
        let pruned = search_pruned(
            &index,
            &query,
            1,
            Bm25Params::default(),
            Strategy::BlockMaxMaxscore,
        )
        .expect("scores");
        let oracle =
            crate::fts::search::search(&index, &query, 1, Bm25Params::default()).expect("scores");
        assert_eq!(pruned.hits, oracle.hits, "pruning must not change results");
        assert!(
            pruned.counters.blocks_decoded <= 2,
            "abandonment must fire before the cheap cursor seeks; decoded {} blocks \
             (one rare block plus the common list's first is the whole budget)",
            pruned.counters.blocks_decoded
        );
    }

    #[test]
    fn the_metadata_block_bound_dominates_every_row_it_bounds() {
        // `bound_for` is what MAXSCORE abandons candidates against, so it
        // must dominate the exact contribution of any row the stream can
        // still reach — from every cursor position, to every target. A
        // bound below the truth is a dropped result, not a slow one.
        let index = strided_index(200, 3);
        let stats = index.corpus_stats().expect("stats");
        let scorer = TermScorer::new(Df(200), &stats, Bm25Params::default());
        let segment = index.segments().first().expect("one segment");
        let lengths = segment.field_lengths(DEFAULT_FIELD).expect("lengths");
        let last = rows_of(&index).last().copied().unwrap_or(0);
        for start in [0_usize, 1, 63, 64, 150] {
            for target in 0..=last.saturating_add(2) {
                let mut stream = stream_of(&index);
                for _ in 0..start {
                    stream.advance();
                }
                let bound = stream.bound_for(target, &scorer);
                let mut probe = stream.clone();
                probe.seek(target);
                if let Some((row, tf)) = probe.current()
                    && row == target
                {
                    let length = lengths.get(usize::try_from(row).expect("small")).copied();
                    let exact = scorer.score(
                        crate::fts::bm25::Tf(tf),
                        crate::fts::bm25::DocLen(length.unwrap_or(1)),
                    );
                    assert!(
                        bound + 1e-12 >= exact,
                        "bound {bound} fell below the exact score {exact} \
                         (start={start}, target={target})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_exhaustive_strategy_delegates_to_the_oracle() {
        let texts: Vec<String> = (0..5).map(|index| format!("alpha beta{index}")).collect();
        let index = index_of(&texts);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let pruned = search_pruned(
            &index,
            &query,
            3,
            Bm25Params::default(),
            Strategy::Exhaustive,
        )
        .expect("scores");
        let direct =
            crate::fts::search::search(&index, &query, 3, Bm25Params::default()).expect("scores");
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
            assert!(search_pruned(&index, &query, 5, Bm25Params::default(), strategy).is_err());
        }
    }
}
