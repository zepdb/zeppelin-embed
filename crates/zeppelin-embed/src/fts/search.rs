//! The exhaustive-OR scorer: task 13's deliverable and task 14's oracle.
//!
//! This walks every posting of every query term and scores every document
//! that matches at least one of them. It is deliberately the simplest
//! correct thing. Task 14 adds block-max pruning that must return exactly
//! what this returns — same ids, same scores, same tie-break — and this
//! module stays forever as the reference that claim is checked against.
//!
//! # Tie-break, pinned
//!
//! Results are ordered by descending score, then by ascending global
//! document id. The tie-break is part of the contract because task 14's
//! equivalence property compares result vectors element-wise: an unpinned
//! tie-break would make pruning look wrong when it was merely differently
//! ordered, or hide a real reordering bug behind a sort.
//!
//! # Global document ids
//!
//! A [`GlobalDocId`] pairs a segment ordinal with a segment-local row. Row
//! ids are dense and segment-local by task 07 invariant, so a store-wide
//! identity has to carry both. Ordering is by segment then row, which is
//! insertion order, which is time order.

use std::borrow::Cow;

use super::bm25::{Bm25Params, Df, DocLen, TermScorer, Tf};
use super::index::{FieldId, IndexError, LexicalIndex};
use super::sealed::{SealedSegment, TermStream};

/// A store-wide document identity.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GlobalDocId {
    /// Index of the segment in seal order.
    pub segment: u32,
    /// Dense segment-local row id.
    pub row: u32,
}

/// One scored result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoredDoc {
    /// The document.
    pub doc: GlobalDocId,
    /// Its BM25 score. Higher is better; never negative.
    pub score: f64,
}

/// Per-field weight applied to term frequencies and lengths.
///
/// # One entry per field, ascending
///
/// The table is normalized at construction: sorted by field id, with a
/// repeated field collapsed to its first weight. Without that invariant a
/// caller who names a field twice makes the scorer count that field's length
/// twice, which is a silently wrong score rather than a rejected query, and
/// it makes iterating the table differ from looking each field up by id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldWeights {
    weights: Vec<(FieldId, u32)>,
}

impl FieldWeights {
    /// Equal weight over the supplied fields: the flat scorer.
    ///
    /// This is what reproduces the flat BEIR column.
    #[must_use]
    pub fn flat(fields: &[FieldId]) -> Self {
        Self::normalized(fields.iter().map(|field| (*field, 1_000)).collect())
    }

    /// Explicit per-field weights, in thousandths.
    ///
    /// Integer thousandths rather than floats so the weight table is exactly
    /// comparable and a configuration digest over it is stable.
    #[must_use]
    pub fn new(weights: &[(FieldId, u32)]) -> Self {
        Self::normalized(weights.to_vec())
    }

    /// Sorts by field and collapses a repeated field to its first weight.
    fn normalized(mut weights: Vec<(FieldId, u32)>) -> Self {
        // A stable sort keeps "first weight wins" meaning first as the
        // caller wrote it, which is what `weight` has always returned.
        weights.sort_by_key(|(field, _)| *field);
        weights.dedup_by_key(|(field, _)| *field);
        Self { weights }
    }

    /// Returns the fields carrying weight.
    ///
    /// This allocates. Hot paths iterate [`Self::iter`] instead.
    #[must_use]
    pub fn fields(&self) -> Vec<FieldId> {
        self.weights.iter().map(|(field, _)| *field).collect()
    }

    /// Returns the `(field, weight)` pairs, ascending by field.
    ///
    /// The allocation-free counterpart of [`Self::fields`]. Because the
    /// table holds one entry per field, `iter` and [`Self::weight`] cannot
    /// disagree.
    pub fn iter(&self) -> impl Iterator<Item = (FieldId, u32)> + '_ {
        self.weights.iter().copied()
    }

    /// Returns one field's weight in thousandths.
    #[must_use]
    pub fn weight(&self, field: FieldId) -> u32 {
        self.weights
            .iter()
            .find(|(candidate, _)| *candidate == field)
            .map_or(0, |(_, weight)| *weight)
    }

    /// Returns true when every named field carries unit weight.
    ///
    /// Unit weight is 1,000 thousandths, the identity: a field weighted
    /// this way contributes its length and its term frequencies unchanged.
    #[must_use]
    pub fn is_unit(&self) -> bool {
        !self.weights.is_empty() && self.weights.iter().all(|(_, weight)| *weight == 1_000)
    }

    /// Returns true when every weight is equal.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        let mut iterator = self.weights.iter().map(|(_, weight)| *weight);
        let Some(first) = iterator.next() else {
            return true;
        };
        iterator.all(|weight| weight == first)
    }
}

/// A structured lexical query.
///
/// Terms are already analyzed. There is no string query path into the
/// engine: parsing text into a structured query is a bindings-layer
/// convenience with its own typed errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TermQuery {
    /// Analyzed query terms.
    pub terms: Vec<Vec<u8>>,
    /// Field weights to score over.
    pub fields: FieldWeights,
}

impl TermQuery {
    /// Builds a flat single-field query over already-analyzed terms.
    #[must_use]
    pub fn flat(terms: Vec<Vec<u8>>, fields: &[FieldId]) -> Self {
        Self {
            terms,
            fields: FieldWeights::flat(fields),
        }
    }
}

/// Deterministic counters, extended by task 14's pruning contracts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchCounters {
    /// Documents whose score was actually computed.
    pub docs_evaluated: u64,
    /// Posting entries decoded.
    pub postings_decoded: u64,
    /// Blocks whose contents were decoded.
    pub blocks_decoded: u64,
    /// Blocks skipped without decoding. Always zero for the exhaustive path.
    pub blocks_skipped: u64,
}

/// A search outcome with its counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchResult {
    /// Scored documents, best first.
    pub hits: Vec<ScoredDoc>,
    /// Counters for this search.
    pub counters: SearchCounters,
}

/// The weighted analyzed length of one row.
#[must_use]
pub fn row_length(segment: &SealedSegment, row: u32, weights: &FieldWeights) -> u32 {
    weighted_length(segment, row, weights)
}

/// The weighted analyzed length of every row in one segment.
///
/// # Why this borrows
///
/// The flat single-field scorer asks for `length * 1000 / 1000`, which is
/// the field's own dense array unchanged. Returning it borrowed makes the
/// per-query length preparation free rather than `O(row_count)`, and the
/// identity is exact for every `u32` length, not approximate.
///
/// Any other weighting materializes the array once per segment per query,
/// which is still one allocation instead of one lookup per scored posting.
#[must_use]
pub fn weighted_lengths<'segment>(
    segment: &'segment SealedSegment,
    weights: &FieldWeights,
) -> Cow<'segment, [u32]> {
    let row_count = usize::try_from(segment.row_count()).unwrap_or(0);
    let mut entries = weights.iter();
    if let (Some((field, 1_000)), None) = (entries.next(), entries.next())
        && let Some(lengths) = segment.field_lengths(field)
        && lengths.len() == row_count
    {
        return Cow::Borrowed(lengths);
    }
    // The flat MULTI-field case, which is what every BEIR run uses: unit
    // weight over a set of fields covering the segment. The weighted length
    // is then just the document's total length, which does not depend on
    // the query at all and is precomputed once at seal.
    //
    // Without this the scorer rebuilt a row-count-long array on EVERY
    // query -- 171,332 entries on TREC-COVID, each costing a linear scan of
    // the field table per field.
    if weights.is_unit()
        && segment.fields().all(|field| weights.weight(field) == 1_000)
        && segment.total_lengths().len() == row_count
    {
        return Cow::Borrowed(segment.total_lengths());
    }
    Cow::Owned(
        (0..row_count)
            .map(|row| weighted_length(segment, u32::try_from(row).unwrap_or(u32::MAX), weights))
            .collect(),
    )
}

/// Scores every matching document exhaustively and returns the top `k`.
///
/// # Errors
///
/// Returns [`IndexError::Stats`] when the index holds no documents, because
/// `avgdl` is undefined and every score would be a division artefact.
pub fn search(
    index: &LexicalIndex,
    query: &TermQuery,
    k: usize,
    params: Bm25Params,
) -> Result<SearchResult, IndexError> {
    let stats = index.corpus_stats()?;
    let fields = query.fields.fields();
    let mut counters = SearchCounters::default();
    let mut accumulator: Vec<(GlobalDocId, f64)> = Vec::new();

    // Document frequency is store-wide, computed once per term.
    let mut frequencies: Vec<(usize, u32)> = Vec::with_capacity(query.terms.len());
    for (position, term) in query.terms.iter().enumerate() {
        frequencies.push((position, index.document_frequency(term, &fields)));
    }

    for (segment_ordinal, segment) in index.segments().iter().enumerate() {
        let segment_index = u32::try_from(segment_ordinal).unwrap_or(u32::MAX);
        // Row ids are dense, so a flat array indexed by row is both the
        // fastest accumulator and the one that keeps per-document summation
        // in query-term order — which task 14's pruning must reproduce to
        // the last bit. `touched` keeps the sweep proportional to matches
        // rather than to corpus size.
        let row_count = usize::try_from(segment.row_count()).unwrap_or(0);
        let mut row_scores: Vec<f64> = vec![0.0; row_count];
        let mut touched: Vec<u32> = Vec::new();
        // Prepared once per segment, borrowed outright in the flat
        // single-field case, instead of once per scored posting.
        let lengths = weighted_lengths(segment, &query.fields);

        for (position, term) in query.terms.iter().enumerate() {
            let df = frequencies
                .iter()
                .find(|(slot, _)| *slot == position)
                .map_or(0, |(_, value)| *value);
            if df == 0 {
                continue;
            }
            let Some(mut stream) = TermStream::open(segment, term, &query.fields) else {
                continue;
            };
            // `idf` costs a `ln()` and `avgdl` costs a division; both are
            // constant across this term's postings, so they are computed
            // once here rather than once per posting.
            let scorer = TermScorer::new(Df(df), &stats, params);
            // Blocks arrive from the sealed stream, decoded on demand into
            // reused scratch buffers. Nothing is materialized.
            while let Some(row) = stream.current_row() {
                counters.postings_decoded = counters.postings_decoded.saturating_add(1);
                let tf = stream.current_tf().unwrap_or(0);
                let length = usize::try_from(row)
                    .ok()
                    .and_then(|slot| lengths.get(slot).copied())
                    .unwrap_or(0);
                let score = scorer.score(Tf(tf), DocLen(length));
                if let Ok(slot) = usize::try_from(row)
                    && let Some(entry) = row_scores.get_mut(slot)
                {
                    if *entry == 0.0 {
                        touched.push(row);
                    }
                    *entry += score;
                }
                stream.advance();
            }
            counters.blocks_decoded = counters
                .blocks_decoded
                .saturating_add(stream.blocks_decoded());
        }

        touched.sort_unstable();
        touched.dedup();
        for row in touched {
            let score = usize::try_from(row)
                .ok()
                .and_then(|slot| row_scores.get(slot).copied())
                .unwrap_or(0.0);
            counters.docs_evaluated = counters.docs_evaluated.saturating_add(1);
            accumulator.push((
                GlobalDocId {
                    segment: segment_index,
                    row,
                },
                score,
            ));
        }
    }

    // Descending score, then ascending document id. Pinned; see module docs.
    accumulator.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    accumulator.truncate(k);

    Ok(SearchResult {
        hits: accumulator
            .into_iter()
            .map(|(doc, score)| ScoredDoc { doc, score })
            .collect(),
        counters,
    })
}

/// The weighted analyzed length of one row.
fn weighted_length(segment: &SealedSegment, row: u32, weights: &FieldWeights) -> u32 {
    let mut total = 0_u64;
    for (field, weight) in weights.iter() {
        total =
            total.saturating_add(u64::from(segment.field_length(row, field)) * u64::from(weight));
    }
    u32::try_from(total / 1_000).unwrap_or(u32::MAX)
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
    use crate::fts::index::{DEFAULT_FIELD, Document, SegmentIndex};
    use crate::fts::tokenizer::{Analyzer, Profile};

    fn analyzer() -> Analyzer {
        Analyzer::new(Profile::Code.config()).expect("valid config")
    }

    fn index_of(texts: &[&str]) -> LexicalIndex {
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
    fn a_repeated_field_is_collapsed_rather_than_counted_twice() {
        // Weight tables are normalized at construction, so iterating the
        // table and looking each field up by id cannot disagree. Before
        // normalization a repeated field added its length to the document
        // twice, which lowered every score for that document.
        let weights = FieldWeights::flat(&[FieldId(1), FieldId(1), FieldId(0)]);
        assert_eq!(weights.fields(), vec![FieldId(0), FieldId(1)]);
        let table: Vec<(FieldId, u32)> = weights.iter().collect();
        assert_eq!(table, vec![(FieldId(0), 1_000), (FieldId(1), 1_000)]);
        for (field, weight) in weights.iter() {
            assert_eq!(weight, weights.weight(field));
        }
        let explicit = FieldWeights::new(&[(FieldId(2), 7), (FieldId(2), 9)]);
        assert_eq!(explicit.iter().count(), 1);
        assert_eq!(explicit.weight(FieldId(2)), 7);
    }

    #[test]
    fn the_flat_single_field_length_array_is_borrowed_not_rebuilt() {
        // The flat scorer asks for `length * 1000 / 1000`, which is the
        // field's own dense array. Borrowing it is what makes the per-query
        // length preparation free; the identity must be exact, not close.
        let index = index_of(&["a b c", "d", "e f"]);
        let segment = index.segments().first().expect("one segment");
        let weights = FieldWeights::flat(&[DEFAULT_FIELD]);
        let lengths = weighted_lengths(segment, &weights);
        assert!(
            matches!(lengths, std::borrow::Cow::Borrowed(_)),
            "the flat single-field case must borrow"
        );
        assert_eq!(lengths.as_ref(), &[3, 1, 2]);
        for row in 0..segment.row_count() {
            assert_eq!(
                lengths.get(row as usize).copied(),
                Some(row_length(segment, row, &weights))
            );
        }
        // The flat MULTI-field case must borrow too: it is what every BEIR
        // run uses, and rebuilding it per query cost a row-count-long pass
        // on every one of them.
        let mut two = SegmentIndex::new();
        let analyzer = analyzer();
        for (title, body) in [("alpha one", "body two three"), ("beta", "more body text")] {
            let mut document = Document::new();
            document.set(FieldId(0), title);
            document.set(FieldId(1), body);
            two.push_document(&analyzer, &document).expect("indexable");
        }
        let two = crate::fts::sealed::SealedSegment::seal(&two).expect("seals");
        let flat = FieldWeights::flat(&[FieldId(0), FieldId(1)]);
        let borrowed = weighted_lengths(&two, &flat);
        assert!(
            matches!(borrowed, std::borrow::Cow::Borrowed(_)),
            "the flat multi-field case must borrow, not rebuild per query"
        );
        for row in 0..two.row_count() {
            assert_eq!(
                borrowed.get(row as usize).copied(),
                Some(row_length(&two, row, &flat)),
                "the borrowed total must equal the weighted length it replaces"
            );
        }
        // A field the segment has but the query does not name must NOT be
        // folded into the borrowed total.
        let partial = FieldWeights::flat(&[FieldId(1)]);
        let only_body = weighted_lengths(&two, &partial);
        for row in 0..two.row_count() {
            assert_eq!(
                only_body.get(row as usize).copied(),
                Some(row_length(&two, row, &partial))
            );
        }

        // A weight other than unity, or a second field, must materialize.
        let scaled = FieldWeights::new(&[(DEFAULT_FIELD, 500)]);
        assert!(matches!(
            weighted_lengths(segment, &scaled),
            std::borrow::Cow::Owned(_)
        ));
        assert_eq!(weighted_lengths(segment, &scaled).as_ref(), &[1, 0, 1]);
    }

    #[test]
    fn a_query_returns_only_matching_documents_best_first() {
        let index = index_of(&["alpha beta", "alpha alpha gamma", "delta"]);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let result = search(&index, &query, 10, Bm25Params::default()).expect("scores");
        assert_eq!(result.hits.len(), 2);
        // Row 1 has two occurrences of alpha, so it must rank first.
        assert_eq!(result.hits[0].doc, GlobalDocId { segment: 0, row: 1 });
        assert_eq!(result.hits[1].doc, GlobalDocId { segment: 0, row: 0 });
        assert!(result.hits[0].score > result.hits[1].score);
    }

    #[test]
    fn top_k_truncates_without_reordering() {
        let index = index_of(&["a x", "a a x", "a a a x"]);
        let query = TermQuery::flat(vec![b"a".to_vec()], &[DEFAULT_FIELD]);
        let full = search(&index, &query, 10, Bm25Params::default()).expect("scores");
        let truncated = search(&index, &query, 2, Bm25Params::default()).expect("scores");
        assert_eq!(truncated.hits, full.hits[..2]);
    }

    #[test]
    fn the_tie_break_is_ascending_document_id() {
        // Three identical documents must come back in row order.
        let index = index_of(&["same", "same", "same"]);
        let query = TermQuery::flat(vec![b"same".to_vec()], &[DEFAULT_FIELD]);
        let result = search(&index, &query, 10, Bm25Params::default()).expect("scores");
        let rows: Vec<u32> = result.hits.iter().map(|hit| hit.doc.row).collect();
        assert_eq!(rows, vec![0, 1, 2]);
    }

    #[test]
    fn scores_are_comparable_across_segments() {
        // The same document text in two different segments must score
        // identically. If any statistic were segment-local it would not.
        let analyzer = analyzer();
        let mut first = SegmentIndex::new();
        first
            .push_document(&analyzer, &Document::with_text("engine tuning"))
            .expect("indexable");
        first
            .push_document(&analyzer, &Document::with_text("unrelated text here"))
            .expect("indexable");
        let mut second = SegmentIndex::new();
        second
            .push_document(&analyzer, &Document::with_text("engine tuning"))
            .expect("indexable");
        second
            .push_document(&analyzer, &Document::with_text("unrelated text here"))
            .expect("indexable");

        let mut index = LexicalIndex::new();
        index.push_segment(first).expect("seals");
        index.push_segment(second).expect("seals");

        let query = TermQuery::flat(vec![b"engine".to_vec()], &[DEFAULT_FIELD]);
        let result = search(&index, &query, 10, Bm25Params::default()).expect("scores");
        assert_eq!(result.hits.len(), 2);
        assert!(
            (result.hits[0].score - result.hits[1].score).abs() < 1e-12,
            "identical documents scored differently across segments: {:?}",
            result.hits
        );
    }

    #[test]
    fn an_unknown_term_matches_nothing_rather_than_everything() {
        let index = index_of(&["alpha", "beta"]);
        let query = TermQuery::flat(vec![b"omega".to_vec()], &[DEFAULT_FIELD]);
        let result = search(&index, &query, 10, Bm25Params::default()).expect("scores");
        assert!(result.hits.is_empty());
        assert_eq!(result.counters.docs_evaluated, 0);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let index = index_of(&["alpha"]);
        let query = TermQuery::flat(Vec::new(), &[DEFAULT_FIELD]);
        let result = search(&index, &query, 10, Bm25Params::default()).expect("scores");
        assert!(result.hits.is_empty());
    }

    #[test]
    fn an_empty_index_is_a_typed_error_not_a_division_by_zero() {
        let index = LexicalIndex::new();
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        assert!(search(&index, &query, 10, Bm25Params::default()).is_err());
    }

    #[test]
    fn the_exhaustive_path_never_skips_a_block() {
        let index = index_of(&["alpha beta", "alpha gamma"]);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let result = search(&index, &query, 10, Bm25Params::default()).expect("scores");
        assert_eq!(result.counters.blocks_skipped, 0);
        assert!(result.counters.postings_decoded >= 2);
    }

    #[test]
    fn multiple_query_terms_sum_their_contributions() {
        let index = index_of(&["alpha", "alpha beta"]);
        let query = TermQuery::flat(vec![b"alpha".to_vec(), b"beta".to_vec()], &[DEFAULT_FIELD]);
        let result = search(&index, &query, 10, Bm25Params::default()).expect("scores");
        // The document matching both terms must outrank the one matching one.
        assert_eq!(result.hits[0].doc.row, 1);
    }

    #[test]
    fn field_weights_change_ranking_and_flat_weights_do_not() {
        let analyzer = analyzer();
        let mut segment = SegmentIndex::new();
        let mut first = Document::new();
        first.set(FieldId(0), "engine");
        first.set(FieldId(1), "unrelated");
        segment.push_document(&analyzer, &first).expect("indexable");
        let mut second = Document::new();
        second.set(FieldId(0), "unrelated");
        second.set(FieldId(1), "engine");
        segment
            .push_document(&analyzer, &second)
            .expect("indexable");
        let mut index = LexicalIndex::new();
        index.push_segment(segment).expect("seals");

        let flat = TermQuery {
            terms: vec![b"engine".to_vec()],
            fields: FieldWeights::flat(&[FieldId(0), FieldId(1)]),
        };
        assert!(flat.fields.is_flat());
        let flat_result = search(&index, &flat, 10, Bm25Params::default()).expect("scores");
        assert!(
            (flat_result.hits[0].score - flat_result.hits[1].score).abs() < 1e-12,
            "flat weights must not prefer a field"
        );

        let weighted = TermQuery {
            terms: vec![b"engine".to_vec()],
            fields: FieldWeights::new(&[(FieldId(0), 4_000), (FieldId(1), 1_000)]),
        };
        assert!(!weighted.fields.is_flat());
        let weighted_result = search(&index, &weighted, 10, Bm25Params::default()).expect("scores");
        assert_eq!(
            weighted_result.hits[0].doc.row, 0,
            "the heavier field must win"
        );
    }

    #[test]
    fn field_weight_lookup_reports_zero_for_unweighted_fields() {
        let weights = FieldWeights::new(&[(FieldId(1), 2_000)]);
        assert_eq!(weights.weight(FieldId(1)), 2_000);
        assert_eq!(weights.weight(FieldId(7)), 0);
        assert_eq!(weights.fields(), vec![FieldId(1)]);
        assert!(FieldWeights::new(&[]).is_flat());
    }
}
