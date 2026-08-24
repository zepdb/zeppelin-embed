//! The lexical index: per-segment postings and store-wide statistics.
//!
//! # Global statistics are the whole point
//!
//! [`LexicalIndex`] owns every segment, and every statistic the scorer needs
//! — `N`, `avgdl`, and per-term `df` — is derived across all of them. A
//! segment cannot answer a scoring question by itself and is not asked to.
//!
//! Segment-local IDF is a documented real-world failure (Milvus Lite) and
//! the prime suspect for the prior engine's ~30% nDCG@10 gap
//! (`research/02a:256`). The structure here makes it unconstructible: the
//! only public entry points that produce statistics hang off the store-wide
//! index, and `prop_engine_bm25_equals_model` randomizes seal boundaries so
//! any per-segment statistic diverges from the brute-force model on the
//! first multi-segment case.
//!
//! # Field handling (BM25F-lite)
//!
//! A document is a map from field id to text. Postings are keyed by
//! `(term, field)`, so the flat scorer sums a term's contribution across
//! fields with weight one — reproducing the "flat" BEIR column — and the
//! multifield scorer applies per-field weights to term frequencies and
//! lengths. Both run the same code path with a different weight table.
//!
//! # No truncation, ever
//!
//! Task 13's guardrail is absolute: documents are never truncated before
//! indexing, and no candidate cap exists outside task 14's bounds. An
//! oversized field is a typed rejection at the tokenizer boundary, not a
//! silent cut.

use std::collections::{BTreeMap, HashMap};

use super::bm25::{Bm25Error, CorpusStats};
use super::postings::{Posting, PostingList, PostingsError};
use super::sealed::SealedSegment;
use super::tokenizer::{Analyzer, Token};

/// A field identifier within a document schema.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FieldId(pub u16);

/// The default field, used when a caller indexes plain text.
pub const DEFAULT_FIELD: FieldId = FieldId(0);

/// The key a posting list hangs off: term first, so prefix scans work.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TermKey {
    /// The analyzed term.
    pub term: Vec<u8>,
    /// The field the term occurred in.
    pub field: FieldId,
}

/// An index construction failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexError {
    /// A posting list rejected an append.
    Postings(PostingsError),
    /// The corpus had no documents or no tokens.
    Stats(Bm25Error),
    /// Documents were added out of row order.
    RowsNotAscending {
        /// The offending row.
        row: u32,
    },
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Postings(error) => write!(formatter, "postings rejected: {error}"),
            Self::Stats(error) => write!(formatter, "corpus statistics unavailable: {error}"),
            Self::RowsNotAscending { row } => {
                write!(formatter, "documents must arrive in row order; got {row}")
            }
        }
    }
}

impl std::error::Error for IndexError {}

impl From<PostingsError> for IndexError {
    fn from(error: PostingsError) -> Self {
        Self::Postings(error)
    }
}

impl From<Bm25Error> for IndexError {
    fn from(error: Bm25Error) -> Self {
        Self::Stats(error)
    }
}

/// One document's text, by field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Document {
    fields: BTreeMap<FieldId, String>,
}

impl Document {
    /// Creates an empty document.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a single-field document in [`DEFAULT_FIELD`].
    #[must_use]
    pub fn with_text(text: &str) -> Self {
        let mut document = Self::new();
        document.set(DEFAULT_FIELD, text);
        document
    }

    /// Sets one field's text, replacing any previous value.
    pub fn set(&mut self, field: FieldId, text: &str) -> &mut Self {
        self.fields.insert(field, text.to_owned());
        self
    }

    /// Returns the fields, ascending by id.
    pub fn fields(&self) -> impl Iterator<Item = (FieldId, &str)> {
        self.fields
            .iter()
            .map(|(field, text)| (*field, text.as_str()))
    }
}

/// One field's analyzed token counts, dense by row.
///
/// `lengths` is exactly `row_count` long at all times: a field absent from
/// a document still occupies its row, carrying zero. That invariant is what
/// lets the scorer borrow the array wholesale instead of rebuilding it.
#[derive(Clone, Debug)]
struct FieldLengths {
    field: FieldId,
    lengths: Vec<u32>,
}

/// Hashes term keys with xxh3, the one permitted hash family.
#[derive(Clone, Debug, Default)]
struct Xxh3State;

impl std::hash::BuildHasher for Xxh3State {
    type Hasher = xxhash_rust::xxh3::Xxh3;

    fn build_hasher(&self) -> Self::Hasher {
        xxhash_rust::xxh3::Xxh3::new()
    }
}

/// One segment's postings and lengths, addressed by dense row id.
///
/// # Why lengths are dense arrays and not a map
///
/// Rows are dense and ascending by the task 07 invariant, so a
/// `BTreeMap<(row, field), u32>` spends a pointer-chasing descent — about
/// seventeen levels at 171,000 documents — on what an array answers with one
/// index. The scorer performs that lookup once per scored posting, so the
/// descent was among the largest per-posting costs in the engine.
///
/// # Why the term table is a hash map and not a tree
///
/// Accumulation looks a term up once per (document, term, field): tens of
/// millions of probes on a real corpus, against a table with millions of
/// entries. A B-tree descent is ~20 pointer-chasing key compares per
/// probe; an xxh3 probe is one hash and usually one compare. The sorted
/// view the seal needs is materialized ONCE, in [`Self::postings`],
/// instead of being maintained on every insert.
#[derive(Clone, Debug, Default)]
pub struct SegmentIndex {
    /// Term key to slot in `lists`.
    terms: HashMap<TermKey, usize, Xxh3State>,
    /// Posting lists, in first-seen order; `terms` holds the slots.
    lists: Vec<PostingList>,
    /// Per-field analyzed token counts, ascending by field id.
    lengths: Vec<FieldLengths>,
    row_count: u32,
}

impl SegmentIndex {
    /// Creates an empty segment index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of rows.
    #[must_use]
    pub const fn row_count(&self) -> u32 {
        self.row_count
    }

    /// Returns true when the segment holds no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Returns the posting lists, ordered by term then field.
    ///
    /// The sorted view is built here, once per call, rather than being
    /// maintained on every insert; seal is its one hot caller.
    pub fn postings(&self) -> impl Iterator<Item = (&TermKey, &PostingList)> {
        let mut entries: Vec<(&TermKey, usize)> =
            self.terms.iter().map(|(key, slot)| (key, *slot)).collect();
        entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
        entries
            .into_iter()
            .filter_map(|(key, slot)| self.lists.get(slot).map(|list| (key, list)))
    }

    /// Returns one term's posting list within one field.
    #[must_use]
    pub fn posting_list(&self, term: &[u8], field: FieldId) -> Option<&PostingList> {
        let slot = *self.terms.get(&TermKey {
            term: term.to_vec(),
            field,
        })?;
        self.lists.get(slot)
    }

    /// Returns the analyzed token count of one row's field.
    #[must_use]
    pub fn field_length(&self, row: u32, field: FieldId) -> u32 {
        self.field_lengths(field)
            .and_then(|lengths| {
                usize::try_from(row)
                    .ok()
                    .and_then(|slot| lengths.get(slot).copied())
            })
            .unwrap_or(0)
    }

    /// Returns the fields this segment recorded lengths for, ascending.
    pub fn fields(&self) -> impl Iterator<Item = FieldId> + '_ {
        self.lengths.iter().map(|entry| entry.field)
    }

    /// Returns one field's dense length array, if the field was ever set.
    ///
    /// The slice is exactly [`Self::row_count`] long, so a caller scoring a
    /// single unweighted field can borrow it rather than rebuild it. A
    /// linear scan over the field table is right here: a schema has a
    /// handful of fields, and a map would trade an index for a descent.
    #[must_use]
    pub fn field_lengths(&self, field: FieldId) -> Option<&[u32]> {
        self.lengths
            .iter()
            .find(|entry| entry.field == field)
            .map(|entry| entry.lengths.as_slice())
    }

    /// Returns the analyzed token count of one row across every field.
    #[must_use]
    pub fn document_length(&self, row: u32) -> u32 {
        let Ok(slot) = usize::try_from(row) else {
            return 0;
        };
        self.lengths
            .iter()
            .filter_map(|entry| entry.lengths.get(slot).copied())
            .fold(0_u32, u32::saturating_add)
    }

    /// Returns the segment's total analyzed token count.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.lengths
            .iter()
            .flat_map(|entry| entry.lengths.iter())
            .map(|length| u64::from(*length))
            .sum()
    }

    /// Records one row's analyzed length for one field.
    ///
    /// Pads the field's array with zeros for any earlier row that did not
    /// carry the field, which is what keeps every array dense.
    fn set_field_length(&mut self, row: u32, field: FieldId, length: u32) {
        let Ok(slot) = usize::try_from(row) else {
            return;
        };
        let position = match self.lengths.iter().position(|entry| entry.field == field) {
            Some(position) => position,
            None => {
                // Fields stay ascending so the table has a stable order.
                let position = self
                    .lengths
                    .iter()
                    .position(|entry| entry.field > field)
                    .unwrap_or(self.lengths.len());
                self.lengths.insert(
                    position,
                    FieldLengths {
                        field,
                        lengths: Vec::new(),
                    },
                );
                position
            }
        };
        let Some(entry) = self.lengths.get_mut(position) else {
            return;
        };
        entry.lengths.resize(slot, 0);
        entry.lengths.push(length);
    }

    /// Extends every field array to cover `rows` rows.
    fn pad_lengths_to(&mut self, rows: usize) {
        for entry in &mut self.lengths {
            entry.lengths.resize(rows, 0);
        }
    }

    /// Appends one analyzed document as the next dense row.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Postings`] when the analyzer produces a token
    /// stream a posting list refuses, which would be a tokenizer contract
    /// violation rather than bad input.
    pub fn push_document(
        &mut self,
        analyzer: &Analyzer,
        document: &Document,
    ) -> Result<u32, IndexError> {
        let row = self.row_count;
        for (field, text) in document.fields() {
            let tokens = analyzer.analyze(text);
            self.accumulate(row, field, &tokens)?;
        }
        self.finish_row();
        Ok(row)
    }

    /// Appends many documents, analyzing them in parallel.
    ///
    /// The result is identical to pushing the same documents one at a time
    /// in slice order: analysis is independent per document and runs on
    /// explicit scoped threads, while accumulation happens strictly in row
    /// order on the calling thread — so nothing about the outcome depends
    /// on scheduling, and the single-writer invariant is untouched.
    ///
    /// `threads` is the caller's policy; one or fewer runs sequentially.
    /// Analysis is batched so the in-flight token working set stays
    /// proportional to the batch, never to the corpus.
    ///
    /// # Errors
    ///
    /// As [`Self::push_document`]. On an error, documents before the
    /// failing one remain indexed, exactly as a sequential loop would
    /// leave them.
    pub fn push_documents(
        &mut self,
        analyzer: &Analyzer,
        documents: &[Document],
        threads: usize,
    ) -> Result<(), IndexError> {
        let workers = threads.max(1).min(documents.len().max(1));
        if workers <= 1 {
            for document in documents {
                self.push_document(analyzer, document)?;
            }
            return Ok(());
        }
        let batch_size = workers.saturating_mul(32).max(1);
        for batch in documents.chunks(batch_size) {
            let mut analyzed: Vec<Vec<(FieldId, Vec<Token>)>> = Vec::new();
            analyzed.resize_with(batch.len(), Vec::new);
            let stride = batch.len().div_ceil(workers);
            std::thread::scope(|scope| {
                let mut rest = analyzed.as_mut_slice();
                let mut offset = 0_usize;
                while !rest.is_empty() {
                    let take = stride.min(rest.len());
                    let (mine, tail) = rest.split_at_mut(take);
                    rest = tail;
                    let end = offset.saturating_add(take);
                    let docs = batch.get(offset..end).unwrap_or(&[]);
                    offset = end;
                    scope.spawn(move || {
                        for (slot, document) in mine.iter_mut().zip(docs) {
                            *slot = document
                                .fields()
                                .map(|(field, text)| (field, analyzer.analyze(text)))
                                .collect();
                        }
                    });
                }
            });
            for fields in &analyzed {
                let row = self.row_count;
                for (field, tokens) in fields {
                    self.accumulate(row, *field, tokens)?;
                }
                self.finish_row();
            }
        }
        Ok(())
    }

    /// Folds one field's analyzed tokens into the postings for one row.
    fn accumulate(&mut self, row: u32, field: FieldId, tokens: &[Token]) -> Result<(), IndexError> {
        // Position count is the analyzed length: stacked variants share
        // a position and must not inflate the document length, or avgdl
        // stops matching the unit being scored.
        let length = tokens
            .iter()
            .map(|token| token.position)
            .max()
            .map_or(0, |highest| highest.saturating_add(1));
        self.set_field_length(row, field, length);

        // Sort-based grouping over borrowed terms replaces the per-document
        // `BTreeMap<Vec<u8>, Vec<u32>>`: no map to build and tear down per
        // document, no allocation per (document, term). `str` compares
        // byte-wise, so groups arrive in exactly the order the map's keys
        // did, and a group's positions arrive already sorted.
        let mut scratch: Vec<(&str, u32)> = tokens
            .iter()
            .map(|token| (token.term.as_str(), token.position))
            .collect();
        scratch.sort_unstable();

        // One reusable probe key per field: lookups borrow it, and only a
        // term new to the segment pays an owned copy.
        let mut probe = TermKey {
            term: Vec::new(),
            field,
        };
        let mut start = 0_usize;
        while let Some((term, _)) = scratch.get(start).copied() {
            let mut end = start.saturating_add(1);
            while scratch.get(end).is_some_and(|(next, _)| *next == term) {
                end = end.saturating_add(1);
            }
            let group = scratch.get(start..end).unwrap_or(&[]);
            let mut positions: Vec<u32> = Vec::with_capacity(group.len());
            for (_, position) in group {
                if positions.last() != Some(position) {
                    positions.push(*position);
                }
            }
            let tf = u32::try_from(positions.len()).unwrap_or(u32::MAX);
            probe.term.clear();
            probe.term.extend_from_slice(term.as_bytes());
            let posting = Posting {
                docid: row,
                tf,
                positions,
            };
            match self.terms.get(&probe).copied() {
                Some(slot) => {
                    // The map only ever hands out slots minted below, so
                    // the lookup cannot miss; the `if let` is the panic-free
                    // spelling of that impossibility.
                    if let Some(list) = self.lists.get_mut(slot) {
                        list.push(posting)?;
                    }
                }
                None => {
                    let mut list = PostingList::new();
                    list.push(posting)?;
                    let slot = self.lists.len();
                    self.lists.push(list);
                    self.terms.insert(
                        TermKey {
                            term: probe.term.clone(),
                            field,
                        },
                        slot,
                    );
                }
            }
            start = end;
        }
        Ok(())
    }

    /// Seals the row: every field array is padded to cover it.
    fn finish_row(&mut self) {
        self.row_count = self.row_count.saturating_add(1);
        self.pad_lengths_to(usize::try_from(self.row_count).unwrap_or(usize::MAX));
    }
}

/// The store-wide lexical index across every segment.
///
/// # Segments here are SEALED
///
/// [`push_segment`](Self::push_segment) encodes the active segment into the
/// persisted posting layout and keeps only that. There is therefore exactly
/// one scoring path in the engine — over bytes — rather than an in-memory
/// one that ships and a persisted one that is only ever tested.
#[derive(Clone, Debug, Default)]
pub struct LexicalIndex {
    segments: Vec<SealedSegment>,
}

impl LexicalIndex {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seals one active segment and appends it.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Postings`] when the encoder rejects the
    /// segment's geometry or refuses to read back what it just wrote. Both
    /// are broken-writer conditions, and they surface here rather than
    /// becoming a decode fault inside a later query.
    pub fn push_segment(&mut self, segment: SegmentIndex) -> Result<(), IndexError> {
        self.segments.push(SealedSegment::seal(&segment)?);
        Ok(())
    }

    /// Appends an already-sealed segment.
    pub fn push_sealed(&mut self, segment: SealedSegment) {
        self.segments.push(segment);
    }

    /// Returns the segments in seal order.
    #[must_use]
    pub fn segments(&self) -> &[SealedSegment] {
        &self.segments
    }

    /// Returns the store-wide document count.
    #[must_use]
    pub fn document_count(&self) -> u64 {
        self.segments
            .iter()
            .map(|segment| u64::from(segment.row_count()))
            .sum()
    }

    /// Returns the store-wide analyzed token count.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.segments.iter().map(SealedSegment::total_tokens).sum()
    }

    /// Returns the store-wide corpus statistics.
    ///
    /// This is the ONLY source of `N` and `avgdl` for scoring. There is
    /// deliberately no per-segment equivalent.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Stats`] when the index holds no documents or
    /// no tokens, because `avgdl` is undefined then.
    pub fn corpus_stats(&self) -> Result<CorpusStats, IndexError> {
        Ok(CorpusStats::new(
            self.document_count(),
            self.total_tokens(),
        )?)
    }

    /// Returns the store-wide document frequency of one term.
    ///
    /// Summed across every segment and every requested field. A document
    /// containing the term in two fields counts once, which is what makes
    /// this a *document* frequency rather than a posting count. Each
    /// segment answers from its sealed dictionary, where the cross-field
    /// union was resolved at seal time, so this costs no decode.
    #[must_use]
    pub fn document_frequency(&self, term: &[u8], fields: &[FieldId]) -> u32 {
        self.segments
            .iter()
            .map(|segment| segment.document_frequency(term, fields))
            .fold(0_u32, u32::saturating_add)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {

    #[test]
    fn parallel_analysis_is_identical_to_the_sequential_loop() {
        // push_documents claims scheduling independence; this holds it to
        // the sequential oracle exactly -- every posting, every position,
        // every length array -- across thread counts and a document count
        // chosen to leave a ragged final batch.
        let analyzer = analyzer();
        let documents: Vec<Document> = (0..97)
            .map(|index| {
                let mut document = Document::new();
                document.set(
                    FieldId(0),
                    &format!("alpha beta{} gamma-{index} 401k", index % 7),
                );
                document.set(FieldId(1), &format!("delta epsilon{} the quick", index % 5));
                document
            })
            .collect();
        let mut sequential = SegmentIndex::new();
        for document in &documents {
            sequential
                .push_document(&analyzer, document)
                .expect("indexable");
        }
        for threads in [1_usize, 3, 8] {
            let mut parallel = SegmentIndex::new();
            parallel
                .push_documents(&analyzer, &documents, threads)
                .expect("indexable");
            assert_eq!(parallel.row_count(), sequential.row_count());
            let ours: Vec<_> = parallel.postings().collect();
            let oracle: Vec<_> = sequential.postings().collect();
            assert_eq!(
                ours.len(),
                oracle.len(),
                "term count diverged at {threads} threads"
            );
            for ((key, list), (oracle_key, oracle_list)) in ours.iter().zip(&oracle) {
                assert_eq!(key, oracle_key);
                assert_eq!(
                    list.postings(),
                    oracle_list.postings(),
                    "postings diverged for {:?} at {threads} threads",
                    String::from_utf8_lossy(&key.term)
                );
            }
            for field in [FieldId(0), FieldId(1)] {
                assert_eq!(
                    parallel.field_lengths(field),
                    sequential.field_lengths(field),
                    "lengths diverged at {threads} threads"
                );
            }
        }
    }

    use super::*;
    use crate::fts::tokenizer::{Profile, TokenizerConfig};

    fn analyzer() -> Analyzer {
        Analyzer::new(TokenizerConfig::text_default()).expect("valid config")
    }

    fn code_analyzer() -> Analyzer {
        Analyzer::new(Profile::Code.config()).expect("valid config")
    }

    fn segment_of(analyzer: &Analyzer, texts: &[&str]) -> SegmentIndex {
        let mut segment = SegmentIndex::new();
        for text in texts {
            segment
                .push_document(analyzer, &Document::with_text(text))
                .expect("indexable");
        }
        segment
    }

    #[test]
    fn rows_are_dense_and_ascending() {
        let analyzer = analyzer();
        let mut segment = SegmentIndex::new();
        for (expected, text) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
            let row = segment
                .push_document(&analyzer, &Document::with_text(text))
                .expect("indexable");
            assert_eq!(row, u32::try_from(expected).expect("small"));
        }
        assert_eq!(segment.row_count(), 3);
    }

    #[test]
    fn document_frequency_is_summed_across_segments_not_per_segment() {
        let analyzer = code_analyzer();
        let mut index = LexicalIndex::new();
        index
            .push_segment(segment_of(&analyzer, &["engine", "engine"]))
            .expect("seals");
        index
            .push_segment(segment_of(&analyzer, &["engine", "other"]))
            .expect("seals");
        assert_eq!(index.document_count(), 4);
        // Three of the four documents contain "engine", across two segments.
        assert_eq!(index.document_frequency(b"engine", &[DEFAULT_FIELD]), 3);
    }

    #[test]
    fn a_term_in_two_fields_of_one_document_counts_once() {
        let analyzer = code_analyzer();
        let mut segment = SegmentIndex::new();
        let mut document = Document::new();
        document.set(FieldId(0), "engine");
        document.set(FieldId(1), "engine");
        segment
            .push_document(&analyzer, &document)
            .expect("indexable");
        let mut index = LexicalIndex::new();
        index.push_segment(segment).expect("seals");
        assert_eq!(
            index.document_frequency(b"engine", &[FieldId(0), FieldId(1)]),
            1
        );
    }

    #[test]
    fn corpus_stats_span_every_segment() {
        let analyzer = code_analyzer();
        let mut index = LexicalIndex::new();
        index
            .push_segment(segment_of(&analyzer, &["a b c"]))
            .expect("seals");
        index
            .push_segment(segment_of(&analyzer, &["d e f g h"]))
            .expect("seals");
        let stats = index.corpus_stats().expect("two documents");
        assert_eq!(stats.document_count(), 2);
        assert_eq!(stats.total_tokens(), 8);
        assert!((stats.average_document_length() - 4.0).abs() < 1e-12);
    }

    #[test]
    fn an_empty_index_refuses_to_produce_statistics() {
        let index = LexicalIndex::new();
        assert!(index.corpus_stats().is_err());
    }

    #[test]
    fn document_length_counts_positions_not_tokens() {
        // Stacked variants share a position, so an identifier that emits
        // three tokens at one position must not count as three tokens of
        // length, or avgdl stops matching the unit being scored.
        let analyzer = code_analyzer();
        let mut segment = SegmentIndex::new();
        segment
            .push_document(&analyzer, &Document::with_text("put_if_match"))
            .expect("indexable");
        // Positions 0, 1, 2 for put / if / match.
        assert_eq!(segment.document_length(0), 3);
    }

    #[test]
    fn field_lengths_are_tracked_separately_and_summed() {
        let analyzer = code_analyzer();
        let mut segment = SegmentIndex::new();
        let mut document = Document::new();
        document.set(FieldId(0), "one two");
        document.set(FieldId(1), "three four five");
        segment
            .push_document(&analyzer, &document)
            .expect("indexable");
        assert_eq!(segment.field_length(0, FieldId(0)), 2);
        assert_eq!(segment.field_length(0, FieldId(1)), 3);
        assert_eq!(segment.document_length(0), 5);
        assert_eq!(segment.total_tokens(), 5);
        assert_eq!(segment.field_length(0, FieldId(9)), 0);
    }

    #[test]
    fn posting_lists_carry_positions() {
        let analyzer = code_analyzer();
        let segment = segment_of(&analyzer, &["alpha beta alpha"]);
        let list = segment
            .posting_list(b"alpha", DEFAULT_FIELD)
            .expect("alpha present");
        let posting = list.postings().first().expect("one posting");
        assert_eq!(posting.tf, 2);
        assert_eq!(posting.positions, vec![0, 2]);
    }

    #[test]
    fn an_absent_term_has_no_posting_list_and_zero_frequency() {
        let analyzer = code_analyzer();
        let segment = segment_of(&analyzer, &["alpha"]);
        assert!(segment.posting_list(b"omega", DEFAULT_FIELD).is_none());
        let mut index = LexicalIndex::new();
        index.push_segment(segment).expect("seals");
        assert_eq!(index.document_frequency(b"omega", &[DEFAULT_FIELD]), 0);
    }

    #[test]
    fn errors_render_useful_text() {
        assert!(
            IndexError::RowsNotAscending { row: 3 }
                .to_string()
                .contains("row order")
        );
        assert!(
            IndexError::Stats(Bm25Error::EmptyCorpus)
                .to_string()
                .contains("statistics")
        );
        assert!(
            IndexError::Postings(PostingsError::ZeroTermFrequency)
                .to_string()
                .contains("postings")
        );
    }
}
