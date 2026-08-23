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

use super::bm25::{Bm25Params, Df, DocLen, Tf, term_score};
use super::index::{FieldId, IndexError, LexicalIndex};

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
        Self {
            weights: fields.iter().map(|field| (*field, 1_000)).collect(),
        }
    }

    /// Explicit per-field weights, in thousandths.
    ///
    /// Integer thousandths rather than floats so the weight table is exactly
    /// comparable and a configuration digest over it is stable.
    #[must_use]
    pub fn new(weights: &[(FieldId, u32)]) -> Self {
        let mut weights = weights.to_vec();
        weights.sort_by_key(|(field, _)| *field);
        Self { weights }
    }

    /// Returns the fields carrying weight.
    #[must_use]
    pub fn fields(&self) -> Vec<FieldId> {
        self.weights.iter().map(|(field, _)| *field).collect()
    }

    /// Returns one field's weight in thousandths.
    #[must_use]
    pub fn weight(&self, field: FieldId) -> u32 {
        self.weights
            .iter()
            .find(|(candidate, _)| *candidate == field)
            .map_or(0, |(_, weight)| *weight)
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
        // Weighted term frequency and weighted length per row.
        let mut row_scores: Vec<(u32, f64)> = Vec::new();

        for (position, term) in query.terms.iter().enumerate() {
            let df = frequencies
                .iter()
                .find(|(slot, _)| *slot == position)
                .map_or(0, |(_, value)| *value);
            if df == 0 {
                continue;
            }
            // Accumulate the weighted term frequency per row across fields.
            let mut weighted: Vec<(u32, u64)> = Vec::new();
            for field in &fields {
                let weight = u64::from(query.fields.weight(*field));
                if weight == 0 {
                    continue;
                }
                let Some(list) = segment.posting_list(term, *field) else {
                    continue;
                };
                counters.blocks_decoded = counters.blocks_decoded.saturating_add(1);
                for posting in list.postings() {
                    counters.postings_decoded = counters.postings_decoded.saturating_add(1);
                    let contribution = u64::from(posting.tf).saturating_mul(weight);
                    match weighted.iter_mut().find(|(row, _)| *row == posting.docid) {
                        Some((_, total)) => *total = total.saturating_add(contribution),
                        None => weighted.push((posting.docid, contribution)),
                    }
                }
            }

            for (row, weighted_tf) in weighted {
                // Weights are thousandths; divide back out at the end so the
                // flat case is exactly the unweighted case.
                let tf = u32::try_from(weighted_tf / 1_000).unwrap_or(u32::MAX);
                if tf == 0 {
                    continue;
                }
                let length = weighted_length(segment, row, &query.fields);
                let score = term_score(Tf(tf), Df(df), DocLen(length), &stats, params);
                match row_scores.iter_mut().find(|(candidate, _)| *candidate == row) {
                    Some((_, total)) => *total += score,
                    None => row_scores.push((row, score)),
                }
            }
        }

        for (row, score) in row_scores {
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
fn weighted_length(
    segment: &super::index::SegmentIndex,
    row: u32,
    weights: &FieldWeights,
) -> u32 {
    let mut total = 0_u64;
    for field in weights.fields() {
        let weight = u64::from(weights.weight(field));
        total = total.saturating_add(u64::from(segment.field_length(row, field)) * weight);
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
    use crate::fts::index::{Document, SegmentIndex, DEFAULT_FIELD};
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
        index.push_segment(segment);
        index
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
        index.push_segment(first);
        index.push_segment(second);

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
        let query = TermQuery::flat(
            vec![b"alpha".to_vec(), b"beta".to_vec()],
            &[DEFAULT_FIELD],
        );
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
        segment.push_document(&analyzer, &second).expect("indexable");
        let mut index = LexicalIndex::new();
        index.push_segment(segment);

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
        let weighted_result =
            search(&index, &weighted, 10, Bm25Params::default()).expect("scores");
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
