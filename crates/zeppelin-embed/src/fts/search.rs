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
//! A [`GlobalDocId`](crate::fts::search::GlobalDocId) pairs a segment ordinal
//! with a segment-local row. Row ids are dense and segment-local by task 07
//! invariant, so a store-wide identity has to carry both. Ordering is by
//! segment then row, which is insertion order, which is time order.

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

    pub(crate) fn fields_controlled<E>(
        &self,
        work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<Vec<FieldId>, E> {
        work.check_now()?;
        let mut fields = Vec::with_capacity(self.weights.len());
        for (field, _) in &self.weights {
            work.step()?;
            fields.push(*field);
        }
        work.check_now()?;
        Ok(fields)
    }

    pub(crate) fn clone_controlled<E>(
        &self,
        work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<Self, E> {
        work.check_now()?;
        let mut weights = Vec::with_capacity(self.weights.len());
        for entry in &self.weights {
            work.step()?;
            #[cfg(test)]
            PREPARED_FIELD_COPIES.with(|count| count.set(count.get() + 1));
            weights.push(*entry);
        }
        work.check_now()?;
        // The source already has normalized order and first-weight semantics.
        Ok(Self { weights })
    }

    /// Returns the `(field, weight)` pairs, ascending by field.
    ///
    /// The allocation-free counterpart of [`Self::fields`]. Because the
    /// table holds one entry per field, `iter` and [`Self::weight`] cannot
    /// disagree.
    pub fn iter(&self) -> impl Iterator<Item = (FieldId, u32)> + '_ {
        self.weights.iter().copied()
    }

    pub(crate) fn allocation_bytes(&self) -> Option<usize> {
        self.weights
            .len()
            .checked_mul(std::mem::size_of::<(FieldId, u32)>())
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
    let mut work = super::control::WorkCheck::new(|| Ok::<(), std::convert::Infallible>(()));
    match weighted_lengths_controlled(segment, weights, &mut work) {
        Ok(lengths) => lengths,
        Err(never) => match never {},
    }
}

pub(crate) fn weighted_lengths_controlled<'segment, E>(
    segment: &'segment SealedSegment,
    weights: &FieldWeights,
    work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<Cow<'segment, [u32]>, E> {
    work.check_now()?;
    let row_count = usize::try_from(segment.row_count()).unwrap_or(0);
    let mut entries = weights.iter();
    if let (Some((field, 1_000)), None) = (entries.next(), entries.next())
        && let Some(lengths) = segment.field_lengths_controlled(field, work)?
        && lengths.len() == row_count
    {
        return Ok(Cow::Borrowed(lengths));
    }
    // The flat MULTI-field case, which is what every BEIR run uses: unit
    // weight over a set of fields covering the segment. The weighted length
    // is then just the document's total length, which does not depend on
    // the query at all and is precomputed once at seal.
    //
    // Without this the scorer rebuilt a row-count-long array on EVERY
    // query -- 171,332 entries on TREC-COVID, each costing a linear scan of
    // the field table per field.
    let mut all_unit = !weights.weights.is_empty();
    for (_, weight) in weights.iter() {
        work.step()?;
        if weight != 1_000 {
            all_unit = false;
            break;
        }
    }
    if all_unit {
        for field in segment.fields() {
            work.step()?;
            let mut found = false;
            for (candidate, _) in weights.iter() {
                work.step()?;
                if candidate == field {
                    found = true;
                    break;
                }
            }
            if !found {
                all_unit = false;
                break;
            }
        }
        if all_unit && segment.total_lengths().len() == row_count {
            return Ok(Cow::Borrowed(segment.total_lengths()));
        }
    }
    let mut lengths = Vec::with_capacity(row_count);
    for row in 0..row_count {
        work.step()?;
        lengths.push(row_length_controlled(
            segment,
            u32::try_from(row).unwrap_or(u32::MAX),
            weights,
            work,
        )?);
    }
    work.check_now()?;
    Ok(Cow::Owned(lengths))
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
        frequencies.push((position, index.prepared_document_frequency(term, &fields)?));
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

/// Scores only allow-listed rows by driving iteration from those row ids.
///
/// The caller validates one bitmap per lexical segment and bounds every row
/// before entering this crate-internal exact branch.
pub(crate) fn search_allow_list_driven(
    index: &LexicalIndex,
    query: &TermQuery,
    k: usize,
    params: Bm25Params,
    allow_lists: &[&crate::meta::DocBitmap],
) -> Result<SearchResult, IndexError> {
    match search_allow_list_driven_controlled(index, query, k, params, allow_lists, || {
        Ok::<(), std::convert::Infallible>(())
    }) {
        Ok(result) => Ok(result),
        Err(ControlledSearchError::Index(error)) => Err(error),
        Err(ControlledSearchError::Control(error)) => match error {},
    }
}

pub(crate) enum ControlledSearchError<Control> {
    Index(IndexError),
    Control(Control),
}

impl<Control> From<IndexError> for ControlledSearchError<Control> {
    fn from(error: IndexError) -> Self {
        Self::Index(error)
    }
}

impl ControlledSearchError<std::convert::Infallible> {
    pub(crate) fn into_index_error(self) -> IndexError {
        match self {
            Self::Index(error) => error,
            Self::Control(never) => match never {},
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CandidateScoringWork {
    pub(crate) streams_opened: u64,
    pub(crate) scorers_prepared: u64,
    pub(crate) seeks: u64,
    pub(crate) rows_visited: u64,
}

pub(crate) struct CandidateScores {
    /// One explicit match/nonmatch in each original caller position.
    pub(crate) scores: Vec<Option<f64>>,
    pub(crate) counters: SearchCounters,
}

/// Owned analyzed terms and corpus-dependent constants for one pinned query.
/// All three term arrays are constructed together and remain immutable.
pub(crate) struct PreparedTermQuery {
    query: TermQuery,
    stats: super::bm25::CorpusStats,
    frequencies: Vec<u32>,
    scorers: Vec<TermScorer>,
    params: Bm25Params,
}

#[cfg(test)]
std::thread_local! {
    static PREPARED_TERM_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PREPARED_FIELD_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PREPARED_SIZE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl PreparedTermQuery {
    pub(crate) fn new_controlled<E>(
        index: &LexicalIndex,
        query: &TermQuery,
        params: Bm25Params,
        mut check: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, ControlledSearchError<E>> {
        let mut work =
            super::control::WorkCheck::new(|| check().map_err(ControlledSearchError::Control));
        work.check_now()?;
        let mut terms = Vec::with_capacity(query.terms.len());
        for term in &query.terms {
            work.step()?;
            #[cfg(test)]
            PREPARED_TERM_COPIES.with(|count| count.set(count.get() + 1));
            terms.push(term.clone());
        }
        let fields = query.fields.clone_controlled(&mut work)?;
        Self::from_owned_controlled(index, TermQuery { terms, fields }, params, check)
    }

    pub(crate) fn from_owned_controlled<E>(
        index: &LexicalIndex,
        query: TermQuery,
        params: Bm25Params,
        mut check: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, ControlledSearchError<E>> {
        check().map_err(ControlledSearchError::Control)?;
        let stats = index.corpus_stats()?;
        let fields = query
            .fields
            .fields_controlled(&mut super::control::WorkCheck::new(|| {
                check().map_err(ControlledSearchError::Control)
            }))?;
        let frequencies = query
            .terms
            .iter()
            .map(|term| index.prepared_document_frequency_controlled(term, &fields, &mut check))
            .collect::<Result<Vec<_>, _>>()?;
        let mut scorers = Vec::with_capacity(frequencies.len());
        let mut work =
            super::control::WorkCheck::new(|| check().map_err(ControlledSearchError::Control));
        for df in &frequencies {
            work.step()?;
            scorers.push(TermScorer::new(Df(*df), &stats, params));
        }
        work.check_now()?;
        Ok(Self {
            query,
            stats,
            frequencies,
            scorers,
            params,
        })
    }

    #[cfg(test)]
    pub(crate) fn new(
        index: &LexicalIndex,
        query: &TermQuery,
        params: Bm25Params,
    ) -> Result<Self, IndexError> {
        Self::from_owned(index, query.clone(), params)
    }

    #[cfg(test)]
    pub(crate) fn from_owned(
        index: &LexicalIndex,
        query: TermQuery,
        params: Bm25Params,
    ) -> Result<Self, IndexError> {
        let stats = index.corpus_stats()?;
        let fields = query.fields.fields();
        let frequencies: Vec<_> = query
            .terms
            .iter()
            .map(|term| index.prepared_document_frequency(term, &fields))
            .collect::<Result<Vec<_>, _>>()?;
        let scorers = frequencies
            .iter()
            .map(|df| TermScorer::new(Df(*df), &stats, params))
            .collect();
        Ok(Self {
            query,
            stats,
            frequencies,
            scorers,
            params,
        })
    }

    pub(crate) fn query(&self) -> &TermQuery {
        &self.query
    }
    pub(crate) fn stats(&self) -> super::bm25::CorpusStats {
        self.stats
    }
    pub(crate) fn frequencies(&self) -> &[u32] {
        &self.frequencies
    }
    pub(crate) fn scorer(&self, slot: usize) -> Option<TermScorer> {
        self.scorers.get(slot).copied()
    }
    pub(crate) fn params(&self) -> Bm25Params {
        self.params
    }

    #[cfg(test)]
    pub(crate) fn allocation_bytes(query: &TermQuery) -> Option<usize> {
        let mut work = super::control::WorkCheck::new(|| Ok::<(), std::convert::Infallible>(()));
        match Self::allocation_bytes_controlled(query, &mut work) {
            Ok(bytes) => bytes,
            Err(never) => match never {},
        }
    }

    pub(crate) fn allocation_bytes_controlled<E>(
        query: &TermQuery,
        work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<Option<usize>, E> {
        work.check_now()?;
        let mut term_bytes = 0_usize;
        for term in &query.terms {
            work.step()?;
            #[cfg(test)]
            PREPARED_SIZE_VISITS.with(|count| count.set(count.get() + 1));
            let Some(sum) = term_bytes.checked_add(term.len()) else {
                return Ok(None);
            };
            term_bytes = sum;
        }
        work.check_now()?;
        Ok(query
            .terms
            .len()
            .checked_mul(
                std::mem::size_of::<Vec<u8>>()
                    + std::mem::size_of::<u32>()
                    + std::mem::size_of::<TermScorer>(),
            )
            .and_then(|bytes| bytes.checked_add(term_bytes))
            .and_then(|bytes| {
                bytes.checked_add(
                    query
                        .fields
                        .weights
                        .len()
                        .checked_mul(std::mem::size_of::<(FieldId, u32)>())?,
                )
            }))
    }

    pub(crate) fn single_allocation_bytes(
        fields: &FieldWeights,
        term_bytes: usize,
    ) -> Option<usize> {
        (std::mem::size_of::<Vec<u8>>()
            + std::mem::size_of::<u32>()
            + std::mem::size_of::<TermScorer>())
        .checked_add(term_bytes)?
        .checked_add(
            fields
                .weights
                .len()
                .checked_mul(std::mem::size_of::<(FieldId, u32)>())?,
        )
    }
}

/// Frozen query statistics shared by candidate batches across segments.
/// Eligibility is a caller contract; row bounds are checked before scoring.
pub(crate) struct CandidateScoring<'query> {
    query: &'query TermQuery,
    stats: super::bm25::CorpusStats,
    frequencies: Cow<'query, [u32]>,
    params: Bm25Params,
    scorers: Option<&'query [TermScorer]>,
}

impl<'query> CandidateScoring<'query> {
    pub(crate) fn new_controlled<E>(
        index: &LexicalIndex,
        query: &'query TermQuery,
        params: Bm25Params,
        mut check: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, ControlledSearchError<E>> {
        check().map_err(ControlledSearchError::Control)?;
        let stats = index.corpus_stats()?;
        let fields = query
            .fields
            .fields_controlled(&mut super::control::WorkCheck::new(|| {
                check().map_err(ControlledSearchError::Control)
            }))?;
        let frequencies = query
            .terms
            .iter()
            .map(|term| index.prepared_document_frequency_controlled(term, &fields, &mut check))
            .collect::<Result<Vec<_>, _>>()?;
        check().map_err(ControlledSearchError::Control)?;
        Ok(Self {
            query,
            stats,
            frequencies: Cow::Owned(frequencies),
            params,
            scorers: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn new(
        index: &LexicalIndex,
        query: &'query TermQuery,
        params: Bm25Params,
    ) -> Result<Self, IndexError> {
        let stats = index.corpus_stats()?;
        let fields = query.fields.fields();
        let frequencies = query
            .terms
            .iter()
            .map(|term| index.prepared_document_frequency(term, &fields))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            query,
            stats,
            frequencies: Cow::Owned(frequencies),
            params,
            scorers: None,
        })
    }

    pub(crate) fn prepared(prepared: &'query PreparedTermQuery) -> Self {
        Self {
            query: &prepared.query,
            stats: prepared.stats,
            frequencies: Cow::Borrowed(&prepared.frequencies),
            params: prepared.params,
            scorers: Some(&prepared.scorers),
        }
    }

    fn term_scorer(&self, slot: usize, df: u32) -> TermScorer {
        self.scorers
            .and_then(|scorers| scorers.get(slot))
            .copied()
            .unwrap_or_else(|| TermScorer::new(Df(df), &self.stats, self.params))
    }

    pub(crate) fn score_rows<Control>(
        &self,
        ordinal: usize,
        segment: &SealedSegment,
        rows: &[u32],
        work: &mut CandidateScoringWork,
        checkpoint: impl FnMut(&CandidateScoringWork) -> Result<(), Control>,
    ) -> Result<CandidateScores, ControlledSearchError<Control>> {
        self.score_rows_with(ordinal, segment, rows, work, checkpoint, |_, score| score)
    }

    pub(crate) fn score_weighted_rows<Control>(
        &self,
        ordinal: usize,
        segment: &SealedSegment,
        rows: &[u32],
        work: &mut CandidateScoringWork,
        checkpoint: impl FnMut(&CandidateScoringWork) -> Result<(), Control>,
        expansions: &[super::query::LexicalExpansion],
    ) -> Result<CandidateScores, ControlledSearchError<Control>> {
        self.score_rows_with(ordinal, segment, rows, work, checkpoint, |slot, score| {
            score
                * expansions
                    .get(slot)
                    .map_or(0.0, |entry| f64::from(entry.boost_thousandths) / 1_000.0)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn score_rows_with<Control>(
        &self,
        ordinal: usize,
        segment: &SealedSegment,
        rows: &[u32],
        work: &mut CandidateScoringWork,
        mut checkpoint: impl FnMut(&CandidateScoringWork) -> Result<(), Control>,
        contribution: impl Fn(usize, f64) -> f64,
    ) -> Result<CandidateScores, ControlledSearchError<Control>> {
        checkpoint(work).map_err(ControlledSearchError::Control)?;
        // Validate the whole input before any cursor or score is produced.
        for (position, &row) in rows.iter().enumerate() {
            if row >= segment.row_count() {
                return Err(ControlledSearchError::Index(
                    IndexError::LiveRowOutOfRange {
                        segment: ordinal,
                        row,
                        row_count: segment.row_count(),
                    },
                ));
            }
            if position % 64 == 63 {
                checkpoint(work).map_err(ControlledSearchError::Control)?;
            }
        }
        let lengths = {
            let mut nested = super::control::WorkCheck::new(|| {
                checkpoint(work).map_err(ControlledSearchError::Control)
            });
            weighted_lengths_controlled(segment, &self.query.fields, &mut nested)?
        };
        let routing = {
            let mut nested = super::control::WorkCheck::new(|| {
                checkpoint(work).map_err(ControlledSearchError::Control)
            });
            let mut routing = Vec::with_capacity(rows.len());
            for (position, &row) in rows.iter().enumerate() {
                nested.step()?;
                routing.push((row, position));
            }
            super::control::sort_by(
                &mut routing,
                |left, right| {
                    #[cfg(test)]
                    CANDIDATE_ROUTING_COMPARISONS.with(|value| value.set(value.get() + 1));
                    left.0.cmp(&right.0)
                },
                &mut nested,
            )?;
            routing
        };
        let mut scores = vec![None; rows.len()];
        let mut counters = SearchCounters::default();
        if rows.is_empty() {
            return Ok(CandidateScores { scores, counters });
        }
        let mut streams = Vec::with_capacity(self.query.terms.len());
        // Preserve query-term occurrences and order, including repeated terms.
        for (slot, (term, &df)) in self
            .query
            .terms
            .iter()
            .zip(self.frequencies.iter())
            .enumerate()
        {
            if df != 0 {
                let mut nested = super::control::WorkCheck::new(|| {
                    checkpoint(work).map_err(ControlledSearchError::Control)
                });
                if let Some(stream) =
                    TermStream::open_controlled(segment, term, &self.query.fields, &mut nested)?
                {
                    work.streams_opened = work.streams_opened.saturating_add(1);
                    let scorer = self.term_scorer(slot, df);
                    work.scorers_prepared = work
                        .scorers_prepared
                        .saturating_add(u64::from(self.scorers.is_none()));
                    streams.push((stream, scorer, slot));
                }
            }
            if slot % 64 == 63 {
                checkpoint(work).map_err(ControlledSearchError::Control)?;
            }
        }
        let mut group_start = 0;
        while let Some(&(row, _)) = routing.get(group_start) {
            let mut group_end = group_start + 1;
            {
                let mut nested = super::control::WorkCheck::new(|| {
                    checkpoint(work).map_err(ControlledSearchError::Control)
                });
                while routing.get(group_end).is_some_and(|&(next, _)| next == row) {
                    nested.step()?;
                    group_end += 1;
                }
            }
            let length = usize::try_from(row)
                .ok()
                .and_then(|slot| lengths.get(slot).copied())
                .ok_or(ControlledSearchError::Index(
                    IndexError::LiveLengthMissing {
                        segment: ordinal,
                        row,
                    },
                ))?;
            let mut total = 0.0_f64;
            let mut matched = false;
            for (stream, scorer, slot) in &mut streams {
                {
                    let mut nested = super::control::WorkCheck::new(|| {
                        checkpoint(work).map_err(ControlledSearchError::Control)
                    });
                    stream.seek_controlled(row, &mut nested)?;
                }
                work.seeks = work.seeks.saturating_add(1);
                if work.seeks.is_multiple_of(64) {
                    checkpoint(work).map_err(ControlledSearchError::Control)?;
                }
                if stream.current_row() == Some(row) {
                    counters.postings_decoded = counters.postings_decoded.saturating_add(1);
                    total += contribution(
                        *slot,
                        scorer.score(
                            Tf({
                                let mut nested = super::control::WorkCheck::new(|| {
                                    checkpoint(work).map_err(ControlledSearchError::Control)
                                });
                                stream.current_tf_controlled(&mut nested)?.unwrap_or(0)
                            }),
                            DocLen(length),
                        ),
                    );
                    matched = true;
                }
            }
            work.rows_visited = work.rows_visited.saturating_add(1);
            if work.rows_visited.is_multiple_of(64) {
                checkpoint(work).map_err(ControlledSearchError::Control)?;
            }
            if matched {
                counters.docs_evaluated = counters.docs_evaluated.saturating_add(1);
            }
            let mut nested = super::control::WorkCheck::new(|| {
                checkpoint(work).map_err(ControlledSearchError::Control)
            });
            // These private boundaries come only from successful routing.get
            // probes above, so both splits remain inside the owned slice.
            let group = routing
                .split_at(group_start)
                .1
                .split_at(group_end - group_start)
                .0;
            for &(_, position) in group {
                nested.step()?;
                #[cfg(test)]
                CANDIDATE_DUPLICATE_COPIES.with(|value| value.set(value.get() + 1));
                // Every position was generated from this exact output length.
                if let Some(output) = scores.get_mut(position) {
                    *output = matched.then_some(total);
                }
            }
            group_start = group_end;
        }
        // Reused cursors own cumulative block receipts; count each once.
        for (slot, (stream, _, _)) in streams.into_iter().enumerate() {
            if slot % 64 == 0 {
                checkpoint(work).map_err(ControlledSearchError::Control)?;
            }
            counters.blocks_decoded = counters
                .blocks_decoded
                .saturating_add(stream.blocks_decoded());
            counters.blocks_skipped = counters
                .blocks_skipped
                .saturating_add(stream.blocks_skipped());
        }
        checkpoint(work).map_err(ControlledSearchError::Control)?;
        Ok(CandidateScores { scores, counters })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AllowListScoringBranch {
    RowDriven,
    PostingDriven,
}

fn allow_list_scoring_branch(allowed: u64, corpus: u64) -> AllowListScoringBranch {
    if allowed.saturating_mul(crate::planner::LEXICAL_ALLOW_LIST_DIVISOR) <= corpus {
        AllowListScoringBranch::RowDriven
    } else {
        AllowListScoringBranch::PostingDriven
    }
}

/// Allow-list scorer with deterministic cancellation checkpoints at entry,
/// exit, segment boundaries, and every 64 posting/document work units.
pub(crate) fn search_allow_list_driven_controlled<Control>(
    index: &LexicalIndex,
    query: &TermQuery,
    k: usize,
    params: Bm25Params,
    allow_lists: &[&crate::meta::DocBitmap],
    checkpoint: impl FnMut() -> Result<(), Control>,
) -> Result<SearchResult, ControlledSearchError<Control>> {
    let allowed = allow_lists
        .iter()
        .map(|allow_list| allow_list.cardinality())
        .fold(0_u64, u64::saturating_add);
    let branch = allow_list_scoring_branch(allowed, index.document_count());
    search_allow_list_driven_controlled_with_branch(
        index,
        query,
        k,
        params,
        allow_lists,
        branch,
        checkpoint,
        None,
    )
}

pub(crate) fn search_allow_list_prepared_controlled<Control>(
    index: &LexicalIndex,
    prepared: &PreparedTermQuery,
    k: usize,
    allow_lists: &[&crate::meta::DocBitmap],
    checkpoint: impl FnMut() -> Result<(), Control>,
) -> Result<SearchResult, ControlledSearchError<Control>> {
    let allowed = allow_lists
        .iter()
        .map(|list| list.cardinality())
        .fold(0_u64, u64::saturating_add);
    let branch = allow_list_scoring_branch(allowed, index.document_count());
    search_allow_list_driven_controlled_with_branch(
        index,
        prepared.query(),
        k,
        prepared.params,
        allow_lists,
        branch,
        checkpoint,
        Some(prepared),
    )
}

#[allow(clippy::too_many_arguments)]
fn search_allow_list_driven_controlled_with_branch<Control>(
    index: &LexicalIndex,
    query: &TermQuery,
    k: usize,
    params: Bm25Params,
    allow_lists: &[&crate::meta::DocBitmap],
    branch: AllowListScoringBranch,
    mut checkpoint: impl FnMut() -> Result<(), Control>,
    prepared_query: Option<&PreparedTermQuery>,
) -> Result<SearchResult, ControlledSearchError<Control>> {
    checkpoint().map_err(ControlledSearchError::Control)?;
    let prepared = match prepared_query {
        Some(prepared) => CandidateScoring::prepared(prepared),
        None => CandidateScoring::new_controlled(index, query, params, &mut checkpoint)?,
    };
    let frequencies = &prepared.frequencies;
    let mut counters = SearchCounters::default();
    let mut accumulator = Vec::<(GlobalDocId, f64)>::new();
    let mut work_units = 0_u64;

    match branch {
        AllowListScoringBranch::RowDriven => {
            for (ordinal, segment) in index.segments().iter().enumerate() {
                checkpoint().map_err(ControlledSearchError::Control)?;
                let Some(allow_list) = allow_lists.get(ordinal) else {
                    continue;
                };
                let segment_index = u32::try_from(ordinal).unwrap_or(u32::MAX);
                let rows = allow_list.iter().collect::<Vec<_>>();
                let batch = prepared.score_rows(
                    ordinal,
                    segment,
                    &rows,
                    &mut CandidateScoringWork::default(),
                    |_| checkpoint(),
                )?;
                counters.docs_evaluated = counters
                    .docs_evaluated
                    .saturating_add(batch.counters.docs_evaluated);
                counters.postings_decoded = counters
                    .postings_decoded
                    .saturating_add(batch.counters.postings_decoded);
                counters.blocks_decoded = counters
                    .blocks_decoded
                    .saturating_add(batch.counters.blocks_decoded);
                counters.blocks_skipped = counters
                    .blocks_skipped
                    .saturating_add(batch.counters.blocks_skipped);
                for (row, score) in rows.into_iter().zip(batch.scores) {
                    if let Some(score) = score {
                        accumulator.push((
                            GlobalDocId {
                                segment: segment_index,
                                row,
                            },
                            score,
                        ));
                    }
                }
            }
        }
        AllowListScoringBranch::PostingDriven => {
            let largest_segment = index
                .segments()
                .iter()
                .filter_map(|segment| usize::try_from(segment.row_count()).ok())
                .max()
                .unwrap_or(0);
            let mut row_scores = vec![0.0_f64; largest_segment];
            let mut touched = Vec::<u32>::new();

            for (ordinal, segment) in index.segments().iter().enumerate() {
                checkpoint().map_err(ControlledSearchError::Control)?;
                let Some(allow_list) = allow_lists.get(ordinal) else {
                    continue;
                };
                let segment_index = u32::try_from(ordinal).unwrap_or(u32::MAX);
                let lengths = {
                    let mut nested = super::control::WorkCheck::new(|| {
                        checkpoint().map_err(ControlledSearchError::Control)
                    });
                    weighted_lengths_controlled(segment, &query.fields, &mut nested)?
                };

                for (slot, term) in query.terms.iter().enumerate() {
                    let df = frequencies.get(slot).copied().unwrap_or(0);
                    if df == 0 {
                        continue;
                    }
                    let mut nested = super::control::WorkCheck::new(|| {
                        checkpoint().map_err(ControlledSearchError::Control)
                    });
                    let Some(mut stream) =
                        TermStream::open_controlled(segment, term, &query.fields, &mut nested)?
                    else {
                        continue;
                    };
                    let scorer = prepared.term_scorer(slot, df);
                    while let Some(row) = stream.current_row() {
                        work_units = work_units.saturating_add(1);
                        if work_units.is_multiple_of(64) {
                            nested.check_now()?;
                        }
                        counters.postings_decoded = counters.postings_decoded.saturating_add(1);
                        if allow_list.contains(row) {
                            let tf = stream.current_tf_controlled(&mut nested)?.unwrap_or(0);
                            let length = usize::try_from(row)
                                .ok()
                                .and_then(|position| lengths.get(position).copied())
                                .unwrap_or(0);
                            let score = scorer.score(Tf(tf), DocLen(length));
                            if let Ok(position) = usize::try_from(row)
                                && let Some(entry) = row_scores.get_mut(position)
                            {
                                if *entry == 0.0 {
                                    touched.push(row);
                                }
                                *entry += score;
                            }
                        }
                        stream.advance_controlled(&mut nested)?;
                    }
                    counters.blocks_decoded = counters
                        .blocks_decoded
                        .saturating_add(stream.blocks_decoded());
                    counters.blocks_skipped = counters
                        .blocks_skipped
                        .saturating_add(stream.blocks_skipped());
                }

                touched.sort_unstable();
                touched.dedup();
                for row in touched.drain(..) {
                    let score = usize::try_from(row)
                        .ok()
                        .and_then(|position| row_scores.get_mut(position))
                        .map(|entry| {
                            let score = *entry;
                            *entry = 0.0;
                            score
                        })
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
        }
    }
    accumulator.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    accumulator.truncate(k);
    checkpoint().map_err(ControlledSearchError::Control)?;
    Ok(SearchResult {
        hits: accumulator
            .into_iter()
            .map(|(doc, score)| ScoredDoc { doc, score })
            .collect(),
        counters,
    })
}

#[cfg(test)]
thread_local! {
    static WEIGHTED_LENGTH_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CANDIDATE_ROUTING_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CANDIDATE_DUPLICATE_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn weighted_length(segment: &SealedSegment, row: u32, weights: &FieldWeights) -> u32 {
    let mut work = super::control::WorkCheck::new(|| Ok::<(), std::convert::Infallible>(()));
    match row_length_controlled(segment, row, weights, &mut work) {
        Ok(length) => length,
        Err(never) => match never {},
    }
}

pub(crate) fn row_length_controlled<E>(
    segment: &SealedSegment,
    row: u32,
    weights: &FieldWeights,
    work: &mut super::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<u32, E> {
    #[cfg(test)]
    WEIGHTED_LENGTH_ROWS.with(|count| count.set(count.get() + 1));
    let mut total = 0_u64;
    for (field, weight) in weights.iter() {
        work.step()?;
        let length = segment
            .field_lengths_controlled(field, work)?
            .and_then(|lengths| usize::try_from(row).ok().and_then(|slot| lengths.get(slot)))
            .copied()
            .unwrap_or(0);
        total = total.saturating_add(u64::from(length) * u64::from(weight));
    }
    Ok(u32::try_from(total / 1_000).unwrap_or(u32::MAX))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use std::convert::Infallible;

    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};

    use super::*;

    fn prepared_copy_case(fields: bool) {
        let index = index_of(&["alpha"]);
        let terms = vec![b"alpha".to_vec(); if fields { 1 } else { 4_096 }];
        let query = TermQuery {
            terms,
            fields: FieldWeights::flat(
                &(0..if fields { 4_096 } else { 1 })
                    .map(FieldId)
                    .collect::<Vec<_>>(),
            ),
        };
        PREPARED_TERM_COPIES.with(|count| count.set(0));
        PREPARED_FIELD_COPIES.with(|count| count.set(0));
        let copied = || {
            if fields {
                PREPARED_FIELD_COPIES.with(std::cell::Cell::get)
            } else {
                PREPARED_TERM_COPIES.with(std::cell::Cell::get)
            }
        };
        let result =
            PreparedTermQuery::new_controlled(&index, &query, Bm25Params::default(), || {
                if copied() >= 64 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });
        println!("prepared copy fields={fields}, copied={}", copied());
        assert!(matches!(
            result,
            Err(ControlledSearchError::Control("cancelled"))
        ));
        assert!(
            (64..=128).contains(&copied()),
            "owned copying must check within the query"
        );
        let clean =
            PreparedTermQuery::new_controlled(&index, &query, Bm25Params::default(), || {
                Ok::<(), Infallible>(())
            })
            .unwrap_or_else(|_| panic!("fresh control failed"));
        assert_eq!(clean.query().terms, query.terms);
        assert_eq!(clean.query().fields, query.fields);
    }

    #[test]
    fn astra_18_prepared_term_copy_cancels_before_statistics() {
        prepared_copy_case(false);
    }

    #[test]
    fn astra_18_prepared_field_copy_cancels_before_statistics() {
        prepared_copy_case(true);
    }

    #[test]
    fn astra_18_prepared_term_size_cancels_inside_walk() {
        let query = TermQuery::flat(vec![b"alpha".to_vec(); 4_096], &[DEFAULT_FIELD]);
        PREPARED_SIZE_VISITS.with(|count| count.set(0));
        let mut work = super::super::control::WorkCheck::new(|| {
            if PREPARED_SIZE_VISITS.with(std::cell::Cell::get) >= 64 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        let result = PreparedTermQuery::allocation_bytes_controlled(&query, &mut work);
        let visited = PREPARED_SIZE_VISITS.with(std::cell::Cell::get);
        println!("prepared size terms visited={visited}");
        assert_eq!(result, Err("cancelled"));
        assert!((64..=128).contains(&visited));
        let mut clean = super::super::control::WorkCheck::new(|| Ok::<(), Infallible>(()));
        // Fixed 64-bit fixture: 4096 owned five-byte terms, their Vec headers,
        // u32 frequencies and 32-byte scorers, plus one eight-byte field entry.
        assert_eq!(
            PreparedTermQuery::allocation_bytes_controlled(&query, &mut clean),
            Ok(Some(266_248))
        );
    }
    use crate::fts::index::{DEFAULT_FIELD, Document, SegmentIndex};
    use crate::fts::tokenizer::{Analyzer, Profile};
    use crate::meta::DocBitmap;

    #[test]
    fn astra_18_weighted_length_preparation_is_cancelable() {
        let index = index_of(&vec!["alpha beta"; 4_096]);
        let segment = index.segments().first().unwrap();
        let weights = FieldWeights::new(&[(DEFAULT_FIELD, 500)]);
        let expected = vec![1_u32; 4_096];
        assert_eq!(weighted_lengths(segment, &weights).as_ref(), expected);
        WEIGHTED_LENGTH_ROWS.with(|count| count.set(0));
        let mut checks = 0;
        let mut work = super::super::control::WorkCheck::new(|| {
            checks += 1;
            if checks == 3 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        let result = weighted_lengths_controlled(segment, &weights, &mut work);
        let rows = WEIGHTED_LENGTH_ROWS.with(std::cell::Cell::get);
        println!("checks={checks}, length_rows={rows}");
        assert_eq!(result, Err("cancelled"));
        assert!(
            rows > 0 && rows <= 128,
            "preparation must stop inside the row loop"
        );
        let mut work = super::super::control::WorkCheck::new(|| Ok::<(), ()>(()));
        assert_eq!(
            weighted_lengths_controlled(segment, &weights, &mut work)
                .unwrap()
                .as_ref(),
            expected
        );
    }

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

    fn candidate_batch(
        index: &LexicalIndex,
        query: &TermQuery,
        ordinal: usize,
        rows: &[u32],
    ) -> (CandidateScores, CandidateScoringWork) {
        let prepared =
            CandidateScoring::new(index, query, Bm25Params::default()).expect("frozen statistics");
        let mut work = CandidateScoringWork::default();
        let batch =
            match prepared.score_rows(ordinal, &index.segments()[ordinal], rows, &mut work, |_| {
                Ok::<(), Infallible>(())
            }) {
                Ok(batch) => batch,
                Err(ControlledSearchError::Index(error)) => panic!("candidate scoring: {error}"),
                Err(ControlledSearchError::Control(never)) => match never {},
            };
        (batch, work)
    }

    #[test]
    fn astra_18_candidate_routing_cancels_inside_sort() {
        let index = index_of(&["alpha", "omega"]);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let prepared =
            CandidateScoring::new(&index, &query, Bm25Params::default()).expect("statistics");
        let rows = (0..4_096)
            .map(|i| ((i * 7_919) % 2) as u32)
            .collect::<Vec<_>>();
        CANDIDATE_ROUTING_COMPARISONS.with(|value| value.set(0));
        let mut work = CandidateScoringWork::default();
        let result = prepared.score_rows(0, &index.segments()[0], &rows, &mut work, |_| {
            if CANDIDATE_ROUTING_COMPARISONS.with(std::cell::Cell::get) >= 64 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        let comparisons = CANDIDATE_ROUTING_COMPARISONS.with(std::cell::Cell::get);
        println!(
            "candidate routing comparisons={comparisons}, streams={}",
            work.streams_opened
        );
        assert!(matches!(
            result,
            Err(ControlledSearchError::Control("cancelled"))
        ));
        assert!(
            (64..=128).contains(&comparisons),
            "routing sort must stop within its comparisons"
        );
        assert_eq!(work.streams_opened, 0);
        let (clean, _) = candidate_batch(&index, &query, 0, &rows);
        assert_eq!(clean.scores.len(), rows.len());
        for (&row, score) in rows.iter().zip(clean.scores) {
            if row == 0 {
                assert!((score.expect("matching literal row") - 2.0_f64.ln()).abs() < 1e-12);
            } else {
                assert_eq!(score, None);
            }
        }
    }

    #[test]
    fn astra_18_candidate_duplicates_cancel_inside_output_walk() {
        let index = index_of(&["alpha", "omega"]);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let prepared =
            CandidateScoring::new(&index, &query, Bm25Params::default()).expect("statistics");
        let rows = vec![0; 4_096];
        CANDIDATE_DUPLICATE_COPIES.with(|value| value.set(0));
        let mut work = CandidateScoringWork::default();
        let result = prepared.score_rows(0, &index.segments()[0], &rows, &mut work, |_| {
            if CANDIDATE_DUPLICATE_COPIES.with(std::cell::Cell::get) >= 64 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        let copies = CANDIDATE_DUPLICATE_COPIES.with(std::cell::Cell::get);
        println!(
            "candidate duplicate copies={copies}, rows_scored={}",
            work.rows_visited
        );
        assert!(matches!(
            result,
            Err(ControlledSearchError::Control("cancelled"))
        ));
        assert!(
            (64..=128).contains(&copies),
            "one repeated row must not hide an unbounded output walk"
        );
        assert_eq!(work.rows_visited, 1);
        let (clean, clean_work) = candidate_batch(&index, &query, 0, &rows);
        assert_eq!(clean.scores.len(), rows.len());
        assert_eq!(clean_work.rows_visited, 1);
        assert!(
            clean
                .scores
                .iter()
                .all(|score| score.is_some_and(|score| (score - 2.0_f64.ln()).abs() < 1e-12))
        );
    }

    #[test]
    fn astra_02_candidate_bm25_opens_one_stream_per_term_segment() {
        let index = index_of(&vec!["alpha beta"; 1_000]);
        let query = TermQuery::flat(vec![b"alpha".to_vec(), b"beta".to_vec()], &[DEFAULT_FIELD]);
        let (batch, work) = candidate_batch(&index, &query, 0, &[999, 7, 511, 42]);
        assert!(batch.scores.iter().all(Option::is_some));
        assert_eq!(
            work.streams_opened, 2,
            "prepare once per term, not once per row"
        );
        assert_eq!(work.scorers_prepared, 2);
        assert_eq!(work.seeks, 8);
    }

    #[test]
    fn astra_02_candidate_nonmatch_is_explicit() {
        let index = index_of(&["alpha", "omega"]);
        let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
        let (batch, _) = candidate_batch(&index, &query, 0, &[1, 0, 1]);
        assert_eq!(batch.scores.len(), 3);
        assert_eq!(batch.scores[0], None);
        assert!(batch.scores[1].is_some());
        assert_eq!(batch.scores[2], None);
        let prepared =
            CandidateScoring::new(&index, &query, Bm25Params::default()).expect("prepare");
        let mut work = CandidateScoringWork::default();
        let result = prepared.score_rows(7, &index.segments()[0], &[0, 2], &mut work, |_| {
            Ok::<(), Infallible>(())
        });
        assert!(matches!(
            result,
            Err(ControlledSearchError::Index(
                IndexError::LiveRowOutOfRange {
                    segment: 7,
                    row: 2,
                    row_count: 2
                }
            ))
        ));
        assert_eq!(
            work.streams_opened, 0,
            "reject the entire request before scoring a valid prefix"
        );
    }

    #[test]
    fn astra_02_candidate_scoring_cancels_during_sparse_seeks() {
        let index = index_of(&["alpha", "omega"]);
        let query = TermQuery::flat(vec![b"alpha".to_vec(); 128], &[DEFAULT_FIELD]);
        let prepared =
            CandidateScoring::new(&index, &query, Bm25Params::default()).expect("prepare");
        let mut work = CandidateScoringWork::default();
        let result = prepared.score_rows(0, &index.segments()[0], &[1], &mut work, |receipt| {
            if receipt.seeks >= 64 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            result,
            Err(ControlledSearchError::Control("cancelled"))
        ));
        assert_eq!(
            work.seeks, 64,
            "unsuccessful seeks must reach cancellation checkpoints"
        );
        let (clean, clean_work) = candidate_batch(&index, &query, 0, &[1]);
        assert_eq!(clean.scores, vec![None]);
        assert_eq!(clean_work.seeks, 128);
    }

    #[test]
    fn astra_02_candidate_work_screen() {
        let index = index_of(&vec!["alpha beta"; 1_000]);
        let query = TermQuery::flat(vec![b"alpha".to_vec(), b"beta".to_vec()], &[DEFAULT_FIELD]);
        for count in [1, 10, 50, 400] {
            let rows = (0..count).map(|row| row * 2).collect::<Vec<_>>();
            let (batch, work) = candidate_batch(&index, &query, 0, &rows);
            assert_eq!(batch.scores.len(), count as usize);
            assert!(batch.scores.iter().all(Option::is_some));
            println!(
                "candidate_rows={count} work={work:?} counters={:?}",
                batch.counters
            );
        }
    }

    fn astra_weighted_live_fixture() -> (LexicalIndex, TermQuery) {
        let mut index = LexicalIndex::new();
        for (documents, live) in [
            (
                vec![
                    ("alpha alpha", "beta gamma"),
                    ("alpha deleted", "beta beta"),
                    ("beta", "alpha alpha"),
                ],
                vec![0, 2],
            ),
            (
                vec![
                    ("gamma", "alpha beta"),
                    ("alpha beta", "beta beta"),
                    ("omega", "omega"),
                    ("alpha", "beta beta beta"),
                ],
                vec![0, 1, 3],
            ),
        ] {
            let mut segment = SegmentIndex::new();
            for (title, body) in documents {
                let mut document = Document::new();
                document.set(FieldId(0), title);
                document.set(FieldId(1), body);
                segment
                    .push_document(&analyzer(), &document)
                    .expect("literal fields");
            }
            index
                .push_sealed_with_live_rows(
                    SealedSegment::seal(&segment).expect("sealed fixture"),
                    &DocBitmap::from_ids(live),
                )
                .expect("frozen live statistics");
        }
        let query = TermQuery {
            terms: vec![b"alpha".to_vec(), b"beta".to_vec(), b"alpha".to_vec()],
            fields: FieldWeights::new(&[
                (FieldId(0), 2_000),
                (FieldId(1), 500),
                (FieldId(0), 9_000),
            ]),
        };
        (index, query)
    }

    #[test]
    fn astra_02_candidate_bm25_matches_exhaustive_scores() {
        let (index, query) = astra_weighted_live_fixture();
        let exhaustive =
            search(&index, &query, usize::MAX, Bm25Params::default()).expect("exhaustive scoring");
        let expected = exhaustive
            .hits
            .iter()
            .map(|hit| (hit.doc, hit.score.to_bits()))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (ordinal, rows) in [vec![2, 0, 2], vec![3, 0, 1, 3]].iter().enumerate() {
            let (batch, work) = candidate_batch(&index, &query, ordinal, rows);
            for (&row, score) in rows.iter().zip(batch.scores) {
                assert_eq!(
                    score.map(f64::to_bits),
                    expected
                        .get(&GlobalDocId {
                            segment: ordinal as u32,
                            row
                        })
                        .copied()
                );
            }
            assert_eq!(
                work.streams_opened, 3,
                "preserve repeated query-term occurrences"
            );
            assert_eq!(
                work.rows_visited,
                [2, 3][ordinal],
                "route duplicate caller rows once"
            );
        }
    }

    #[test]
    fn astra_02_candidate_scoring_preserves_repeated_terms_and_fields() {
        let (index, query) = astra_weighted_live_fixture();
        assert_eq!(
            index.corpus_stats().expect("live stats").document_count(),
            5
        );
        assert_eq!(index.corpus_stats().expect("live stats").total_tokens(), 18);
        // Literal live rows have weighted (length, alpha tf, beta tf) below.
        // Both terms occur in all five live documents before tf weighting.
        // N=5, raw avgdl=18/5, title weight=2, body weight=1/2 (floor after
        // merging). Query order is alpha, beta, alpha; no product scorer is
        // used by this independent BM25 formula.
        let idf = (1.0_f64 + 0.5 / 5.5).ln();
        for (ordinal, row, length, alpha, beta) in [
            (0, 0, 5, 4, 0),
            (0, 2, 3, 1, 2),
            (1, 0, 3, 0, 0),
            (1, 1, 5, 2, 3),
            (1, 3, 3, 2, 1),
        ] {
            let mut expected = 0.0;
            let mut matched = false;
            for tf in [alpha, beta, alpha] {
                if tf != 0 {
                    let tf = f64::from(tf);
                    expected += idf * (tf * 2.2)
                        / (tf + 1.2 * (0.25 + 0.75 * f64::from(length) / (18.0 / 5.0)));
                    matched = true;
                }
            }
            let (batch, _) = candidate_batch(&index, &query, ordinal, &[row]);
            assert_eq!(
                batch.scores[0].map(f64::to_bits),
                matched.then_some(expected.to_bits()),
                "segment {ordinal} row {row}"
            );
        }
    }

    #[test]
    fn astra_09_prepared_lexical_paths_keep_weighted_repeated_term_scores() {
        let (index, query) = astra_weighted_live_fixture();
        let prepared =
            PreparedTermQuery::new(&index, &query, Bm25Params::beir()).expect("prepared query");
        let idf = (1.0_f64 + 0.5 / 5.5).ln();
        let mut expected = Vec::new();
        let mut work = CandidateScoringWork::default();
        for (ordinal, row, length, alpha, beta) in [
            (0, 0, 5, 4, 0),
            (0, 2, 3, 1, 2),
            (1, 0, 3, 0, 0),
            (1, 1, 5, 2, 3),
            (1, 3, 3, 2, 1),
        ] {
            let mut score = 0.0_f64;
            let mut matched = false;
            for tf in [alpha, beta, alpha] {
                if tf != 0 {
                    let tf = f64::from(tf);
                    score += idf * (tf * 2.2)
                        / (tf + 1.2 * (0.25 + 0.75 * f64::from(length) / (18.0 / 5.0)));
                    matched = true;
                }
            }
            let batch = CandidateScoring::prepared(&prepared)
                .score_rows(
                    ordinal,
                    &index.segments()[ordinal],
                    &[row],
                    &mut work,
                    |_| Ok::<(), Infallible>(()),
                )
                .unwrap_or_else(|error| match error {
                    ControlledSearchError::Index(error) => {
                        panic!("prepared scoring failed: {error}")
                    }
                    ControlledSearchError::Control(never) => match never {},
                });
            assert_eq!(
                batch.scores[0].map(f64::to_bits),
                matched.then_some(score.to_bits())
            );
            if matched {
                expected.push(ScoredDoc {
                    doc: GlobalDocId {
                        segment: ordinal as u32,
                        row,
                    },
                    score,
                });
            }
        }
        assert_eq!(
            work.scorers_prepared, 0,
            "all constants came from query preparation"
        );
        expected.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
        let allow = [
            crate::meta::DocBitmap::from_ids([0, 2]),
            crate::meta::DocBitmap::from_ids([0, 1, 3]),
        ];
        let allow = [&allow[0], &allow[1]];
        for branch in [
            AllowListScoringBranch::RowDriven,
            AllowListScoringBranch::PostingDriven,
        ] {
            let result = search_allow_list_driven_controlled_with_branch(
                &index,
                prepared.query(),
                10,
                Bm25Params::beir(),
                &allow,
                branch,
                || Ok::<(), Infallible>(()),
                Some(&prepared),
            )
            .unwrap_or_else(|error| match error {
                ControlledSearchError::Index(error) => panic!("prepared scoring failed: {error}"),
                ControlledSearchError::Control(never) => match never {},
            });
            assert_eq!(result.hits, expected);
        }
        for strategy in [
            crate::fts::prune::Strategy::BlockMaxWand,
            crate::fts::prune::Strategy::BlockMaxMaxscore,
        ] {
            for k in [1, 3, 10] {
                let result = crate::fts::prune::search_pruned_prepared_filtered(
                    &index, &prepared, k, strategy, &allow,
                )
                .expect("fresh wider cursors");
                assert_eq!(result.hits, expected[..k.min(expected.len())]);
            }
        }
    }

    #[test]
    #[ignore = "explicit paired release-process candidate screen"]
    fn astra_02_selective_batch_screen() {
        let documents = (0..65_536)
            .map(|row| ["alpha beta", "alpha gamma", "beta gamma", "omega"][row % 4])
            .collect::<Vec<_>>();
        let index = index_of(&documents);
        let queries = [
            vec!["alpha", "beta"],
            vec!["alpha", "alpha", "beta"],
            vec!["omega", "beta"],
            vec!["gamma"],
        ]
        .into_iter()
        .map(|terms| {
            TermQuery::flat(
                terms
                    .into_iter()
                    .map(|term| term.as_bytes().to_vec())
                    .collect(),
                &[DEFAULT_FIELD],
            )
        })
        .collect::<Vec<_>>();
        let references = queries
            .iter()
            .map(|query| {
                search(&index, query, usize::MAX, Bm25Params::default())
                    .expect("exhaustive reference")
                    .hits
                    .into_iter()
                    .map(|hit| (hit.doc.row, hit.score.to_bits()))
                    .collect::<std::collections::BTreeMap<_, _>>()
            })
            .collect::<Vec<_>>();
        println!(
            "astra02_fixture,v1,rows=65536,queries=64,warm=20,seed=0x5eed,core_candidate_only=true"
        );
        for position in 0..84 {
            let case = position % 64;
            let count = [1, 10, 50, 400][case % 4];
            // Fixed arithmetic routing, including unsorted sparse inputs. No
            // random generator or qrels affect this exact-scoring treatment.
            let rows = (0..count)
                .rev()
                .map(|row| ((row * (65_536 / count) + case * 37 + 0x5eed) % 65_536) as u32)
                .collect::<Vec<_>>();
            let started = std::time::Instant::now();
            let (batch, work) = candidate_batch(&index, &queries[case % queries.len()], 0, &rows);
            let elapsed = started.elapsed().as_nanos();
            for (row, score) in rows.iter().zip(&batch.scores) {
                assert_eq!(
                    score.map(f64::to_bits),
                    references[case % queries.len()].get(row).copied()
                );
            }
            if position >= 20 {
                println!(
                    "astra02_sample,{case},{count},{elapsed},{},{},{},{},{},{}",
                    work.streams_opened,
                    work.scorers_prepared,
                    work.seeks,
                    work.rows_visited,
                    batch.counters.blocks_decoded,
                    batch.counters.docs_evaluated
                );
            }
        }
    }

    fn forced_allow_list_search(
        index: &LexicalIndex,
        query: &TermQuery,
        k: usize,
        allow_lists: &[&DocBitmap],
        branch: AllowListScoringBranch,
    ) -> SearchResult {
        match search_allow_list_driven_controlled_with_branch(
            index,
            query,
            k,
            Bm25Params::default(),
            allow_lists,
            branch,
            || Ok::<(), Infallible>(()),
            None,
        ) {
            Ok(result) => result,
            Err(ControlledSearchError::Index(error)) => {
                panic!("forced allow-list search failed: {error:?}")
            }
            Err(ControlledSearchError::Control(error)) => match error {},
        }
    }

    #[test]
    fn dense_allow_list_search_is_posting_driven() {
        let analyzer = analyzer();
        let mut segment = SegmentIndex::new();
        for _ in 0..256 {
            segment
                .push_document(&analyzer, &Document::with_text("alpha beta"))
                .expect("indexable");
        }
        let mut index = LexicalIndex::new();
        index.push_segment(segment).expect("seals");
        let query = TermQuery::flat(vec![b"alpha".to_vec(), b"beta".to_vec()], &[DEFAULT_FIELD]);
        let allow_list = DocBitmap::full(256);
        let result =
            search_allow_list_driven(&index, &query, 256, Bm25Params::default(), &[&allow_list])
                .expect("scores");

        assert_eq!(result.hits.len(), 256);
        assert_eq!(result.counters.blocks_decoded, 8);
    }

    #[test]
    fn posting_driven_matches_row_driven_hit_for_hit() {
        let name = "fts::search::tests::posting_driven_matches_row_driven_hit_for_hit";
        let mut seeded = crate::test_support::seeded_rng(name);
        let mut runner = TestRunner::new(Config {
            cases: 128,
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        });
        let cases = (
            prop::collection::vec((0_u8..16, any::<bool>()), 2..130),
            prop::collection::vec(0_u8..6, 1..7),
            any::<u16>(),
            any::<u8>(),
        );

        let result = runner.run(&cases, |(rows, query_terms, cut_seed, k_seed)| {
            let analyzer = analyzer();
            let cut = 1 + usize::from(cut_seed) % rows.len().saturating_sub(1);
            let mut first = SegmentIndex::new();
            let mut second = SegmentIndex::new();

            for (position, (mask, _)) in rows.iter().enumerate() {
                let mut words = Vec::new();
                for (bit, term) in [(1_u8, "alpha"), (2, "beta"), (4, "gamma"), (8, "delta")] {
                    if mask & bit != 0 {
                        words.push(term);
                        if position.is_multiple_of(3) {
                            words.push(term);
                        }
                    }
                }
                if words.is_empty() {
                    words.push("filler");
                }
                let document = Document::with_text(&words.join(" "));
                if position < cut {
                    first
                        .push_document(&analyzer, &document)
                        .expect("indexable first segment");
                } else {
                    second
                        .push_document(&analyzer, &document)
                        .expect("indexable second segment");
                }
            }

            let mut index = LexicalIndex::new();
            index.push_segment(first).expect("seals first segment");
            index.push_segment(second).expect("seals second segment");
            let terms = query_terms
                .iter()
                .map(|term| match term {
                    0 => b"alpha".to_vec(),
                    1 => b"beta".to_vec(),
                    2 => b"gamma".to_vec(),
                    3 => b"delta".to_vec(),
                    4 => b"missing".to_vec(),
                    _ => b"alpha".to_vec(),
                })
                .collect();
            let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
            let k = usize::from(k_seed) % rows.len().saturating_add(1);

            for dense in [false, true] {
                let mut first_allowed = Vec::new();
                let mut second_allowed = Vec::new();
                for (position, (_, selected)) in rows.iter().enumerate() {
                    let allowed = if dense {
                        *selected || !position.is_multiple_of(4)
                    } else {
                        *selected && position.is_multiple_of(16)
                    };
                    if allowed && position < cut {
                        first_allowed.push(u32::try_from(position).unwrap_or(u32::MAX));
                    } else if allowed {
                        second_allowed
                            .push(u32::try_from(position.saturating_sub(cut)).unwrap_or(u32::MAX));
                    }
                }
                let first_allow_list = DocBitmap::from_ids(first_allowed);
                let second_allow_list = DocBitmap::from_ids(second_allowed);
                let allow_lists = [&first_allow_list, &second_allow_list];
                let row_driven = forced_allow_list_search(
                    &index,
                    &query,
                    k,
                    &allow_lists,
                    AllowListScoringBranch::RowDriven,
                );
                let posting_driven = forced_allow_list_search(
                    &index,
                    &query,
                    k,
                    &allow_lists,
                    AllowListScoringBranch::PostingDriven,
                );

                prop_assert_eq!(row_driven.hits.len(), posting_driven.hits.len());
                for (row_hit, posting_hit) in row_driven.hits.iter().zip(&posting_driven.hits) {
                    prop_assert_eq!(row_hit.doc, posting_hit.doc);
                    prop_assert_eq!(row_hit.score.to_bits(), posting_hit.score.to_bits());
                }
            }
            Ok(())
        });
        assert!(result.is_ok(), "property result: {result:?}");
    }

    #[test]
    fn divisor_rule_keeps_the_selective_boundary_row_driven() {
        assert_eq!(
            allow_list_scoring_branch(1, 64),
            AllowListScoringBranch::RowDriven
        );
        assert_eq!(
            allow_list_scoring_branch(2, 128),
            AllowListScoringBranch::RowDriven
        );
        assert_eq!(
            allow_list_scoring_branch(2, 127),
            AllowListScoringBranch::PostingDriven
        );
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
