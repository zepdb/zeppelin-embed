//! Phrase matching over the positions task 13 stored.
//!
//! # Slop, pinned (task 15 D1)
//!
//! Slop is famously underspecified: Lucene, tantivy, and FTS5 `NEAR` all
//! differ. This engine's definition, fixed here and depended on by the
//! property tests:
//!
//! > A phrase `t0..t(n-1)` matches a document when there exist positions
//! > `q0 <= q1 <= ... <= q(n-1)`, with `qi` an occurrence of `ti`, whose
//! > total displacement `SUM |(qi - q0) - i|` is at most the slop.
//!
//! Consequences, each pinned by a fixture:
//!
//! - **Slop 0 is exact adjacency in order**: `qi = q0 + i`.
//! - **Slop 1 admits one single-position gap** (`the quick fox` matches
//!   `quick ... fox` with one word between) **or one adjacent
//!   transposition** (`fox quick` matches the query `quick fox` at cost 1
//!   for each of the two displaced terms — so a transposition needs slop 2
//!   under this metric, which is stated rather than hidden).
//! - **Positions are non-decreasing, not strictly increasing.** That is what
//!   lets a phrase cross a synonym stack for free: a stacked variant
//!   occupies the same position as the token that produced it, so
//!   `put if match` phrase-matches `put_if_match`.
//!
//! # Cost
//!
//! Matching is exact rather than greedy: a greedy walk picks a locally
//! closest occurrence and misses matches a later term would have made
//! cheaper. The search below fixes `q0` and runs a small dynamic program
//! over the remaining terms, which is `O(|p0| * n * P)` for `P` occurrences
//! per term. Documents have few occurrences of any one term, and
//! `phrase_matches_iff_naive_token_stream_scan_matches` checks the result
//! against brute force rather than trusting the shortcut.

use super::index::{FieldId, SegmentIndex};

pub(crate) enum PhraseReadError<Control> {
    Storage(super::sealed::SealedSegmentError),
    Control(Control),
}

struct SegmentPhrase<'index> {
    readers: Vec<Option<super::postings::PostingsReader<'index>>>,
    order: Vec<usize>,
    rows: u32,
}

/// Prepared metadata is query-owned; packed payloads remain borrowed from the
/// pinned immutable index. Only rows surviving term intersection read positions.
pub(crate) struct PreparedPhrase<'index> {
    segments: Vec<SegmentPhrase<'index>>,
    slop: u32,
    bytes: usize,
}

impl<'index> PreparedPhrase<'index> {
    pub(crate) fn allocation_bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn new<Control>(
        index: &'index super::index::LexicalIndex,
        query: &super::query::LexicalQuery,
        mut reserve: impl FnMut(Option<usize>) -> Result<(), Control>,
        mut checkpoint: impl FnMut() -> Result<(), Control>,
    ) -> Result<Option<Self>, PhraseReadError<Control>> {
        let super::query::LexicalQuery::Phrase { terms, slop, field } = query else {
            return Ok(None);
        };
        let mut bytes = index
            .segments()
            .len()
            .checked_mul(std::mem::size_of::<SegmentPhrase<'_>>());
        for segment in index.segments() {
            checkpoint().map_err(PhraseReadError::Control)?;
            bytes = bytes.and_then(|bytes| {
                terms
                    .len()
                    .checked_mul(
                        std::mem::size_of::<Option<super::postings::PostingsReader<'_>>>()
                            + std::mem::size_of::<usize>(),
                    )
                    .and_then(|extra| bytes.checked_add(extra))
            });
            for term in terms {
                bytes = bytes.and_then(|bytes| {
                    segment
                        .position_reader_bytes(term, *field)
                        .and_then(|extra| bytes.checked_add(extra))
                });
            }
        }
        reserve(bytes).map_err(PhraseReadError::Control)?;
        let bytes = bytes.ok_or(PhraseReadError::Storage(
            super::sealed::SealedSegmentError::Geometry("phrase allocation size overflow"),
        ))?;
        let mut segments = Vec::with_capacity(index.segments().len());
        for segment in index.segments() {
            let mut readers = Vec::with_capacity(terms.len());
            for term in terms {
                checkpoint().map_err(PhraseReadError::Control)?;
                readers.push(
                    segment
                        .position_reader(term, *field)
                        .map_err(PhraseReadError::Storage)?,
                );
            }
            let mut order = (0..terms.len()).collect::<Vec<_>>();
            order.sort_unstable_by_key(|slot| {
                readers
                    .get(*slot)
                    .and_then(Option::as_ref)
                    .map_or(0, |reader| reader.document_frequency())
            });
            segments.push(SegmentPhrase {
                readers,
                order,
                rows: segment.row_count(),
            });
        }
        Ok(Some(Self {
            segments,
            slop: *slop,
            bytes,
        }))
    }

    pub(crate) fn matches<Control>(
        &self,
        doc: super::search::GlobalDocId,
        mut reserve: impl FnMut(Option<usize>) -> Result<(), Control>,
        mut checkpoint: impl FnMut() -> Result<(), Control>,
    ) -> Result<bool, PhraseReadError<Control>> {
        let invalid = || {
            PhraseReadError::Storage(super::sealed::SealedSegmentError::Geometry(
                "phrase candidate is outside its pinned segment",
            ))
        };
        let segment = self
            .segments
            .get(doc.segment as usize)
            .ok_or_else(invalid)?;
        if doc.row >= segment.rows {
            return Err(invalid());
        }
        let views_bytes = segment
            .readers
            .len()
            .checked_mul(std::mem::size_of::<Option<super::postings::RowPositions<'_>>>());
        let scratch = views_bytes.and_then(|bytes| self.bytes.checked_add(bytes));
        reserve(scratch).map_err(PhraseReadError::Control)?;
        let mut views = vec![None; segment.readers.len()];
        let mut positions = 0_usize;
        let mut maximum = 1_usize;
        // Check membership from the rarest term first; do not decode any
        // positions until the row is in every required term/field stream.
        for slot in &segment.order {
            checkpoint().map_err(PhraseReadError::Control)?;
            let Some(reader) = segment.readers.get(*slot).and_then(Option::as_ref) else {
                return Ok(false);
            };
            let Some(view) = reader
                .positions(doc.row)
                .map_err(|error| PhraseReadError::Storage(error.into()))?
            else {
                return Ok(false);
            };
            positions = positions.checked_add(view.len()).ok_or_else(invalid)?;
            maximum = maximum.max(view.len());
            *views.get_mut(*slot).ok_or_else(invalid)? = Some(view);
        }
        // Original-order position vectors plus the existing alignment DP's
        // two simultaneous arrays. Its arrays have exact reserved capacities.
        let bytes = scratch
            .and_then(|bytes| {
                segment
                    .readers
                    .len()
                    .checked_mul(std::mem::size_of::<Vec<u32>>())
                    .and_then(|extra| bytes.checked_add(extra))
            })
            .and_then(|bytes| {
                positions
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|extra| bytes.checked_add(extra))
            })
            .and_then(|bytes| {
                maximum
                    .checked_mul(2 * std::mem::size_of::<(u32, u32)>())
                    .and_then(|extra| bytes.checked_add(extra))
            });
        reserve(bytes).map_err(PhraseReadError::Control)?;
        let mut streams = Vec::with_capacity(views.len());
        for view in views {
            checkpoint().map_err(PhraseReadError::Control)?;
            streams.push(
                view.ok_or_else(invalid)?
                    .decode()
                    .map_err(|error| PhraseReadError::Storage(error.into()))?,
            );
        }
        Ok(streams_match(&streams, self.slop))
    }
}

/// A phrase query over already-analyzed terms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhraseQuery {
    /// The analyzed terms, in order.
    pub terms: Vec<Vec<u8>>,
    /// Maximum total positional displacement.
    pub slop: u32,
    /// The field to match within.
    pub field: FieldId,
}

/// A rejected phrase query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhraseError {
    /// The phrase had no terms.
    Empty,
}

impl std::fmt::Display for PhraseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a phrase query needs at least one term")
    }
}

impl std::error::Error for PhraseError {}

/// Returns the smallest total displacement of any alignment, if one exists.
///
/// `streams[i]` is the ascending list of positions at which term `i` occurs.
/// Returns `None` when no non-decreasing alignment exists at all.
#[must_use]
pub fn minimum_displacement(streams: &[Vec<u32>]) -> Option<u32> {
    let first = streams.first()?;
    if streams.len() == 1 {
        return first
            .is_empty()
            .then_some(0)
            .or(Some(0))
            .filter(|_| !first.is_empty());
    }
    if streams.iter().any(Vec::is_empty) {
        return None;
    }

    let mut best: Option<u32> = None;
    for anchor in first {
        // costs[j] = least displacement using occurrence j of the current
        // term, given the anchor. Seeded from the anchor itself.
        let mut previous: Vec<(u32, u32)> = vec![(*anchor, 0)];
        for (offset, stream) in streams.iter().enumerate().skip(1) {
            let expected = u32::try_from(offset).unwrap_or(u32::MAX);
            let mut current: Vec<(u32, u32)> = Vec::with_capacity(stream.len());
            for candidate in stream {
                // Only alignments that stay non-decreasing are legal.
                let Some(best_previous) = previous
                    .iter()
                    .filter(|(position, _)| *position <= *candidate)
                    .map(|(_, cost)| *cost)
                    .min()
                else {
                    continue;
                };
                let relative = candidate.saturating_sub(*anchor);
                let displacement = relative.abs_diff(expected);
                current.push((*candidate, best_previous.saturating_add(displacement)));
            }
            if current.is_empty() {
                previous.clear();
                break;
            }
            previous = current;
        }
        if let Some(cost) = previous.iter().map(|(_, cost)| *cost).min() {
            best = Some(best.map_or(cost, |current: u32| current.min(cost)));
        }
    }
    best
}

/// Returns true when the streams admit an alignment within `slop`.
#[must_use]
pub fn streams_match(streams: &[Vec<u32>], slop: u32) -> bool {
    minimum_displacement(streams).is_some_and(|cost| cost <= slop)
}

/// Returns the rows of one segment matching the phrase.
///
/// # Errors
///
/// Returns [`PhraseError::Empty`] for a phrase with no terms. An empty
/// phrase is not "match everything"; it is a caller mistake.
pub fn search_segment(
    segment: &SegmentIndex,
    query: &PhraseQuery,
) -> Result<Vec<u32>, PhraseError> {
    if query.terms.is_empty() {
        return Err(PhraseError::Empty);
    }
    // Candidate rows are those carrying the first term; every match must
    // contain every term, so intersecting from the rarest would be faster
    // but never changes the answer.
    let Some(first) = query.terms.first() else {
        return Err(PhraseError::Empty);
    };
    let Some(anchor_list) = segment.posting_list(first, query.field) else {
        return Ok(Vec::new());
    };

    let mut matches = Vec::new();
    for posting in anchor_list.postings() {
        let mut streams: Vec<Vec<u32>> = Vec::with_capacity(query.terms.len());
        let mut complete = true;
        for term in &query.terms {
            let Some(list) = segment.posting_list(term, query.field) else {
                complete = false;
                break;
            };
            let Some(entry) = list
                .postings()
                .iter()
                .find(|candidate| candidate.docid == posting.docid)
            else {
                complete = false;
                break;
            };
            streams.push(entry.positions.clone());
        }
        if complete && streams_match(&streams, query.slop) {
            matches.push(posting.docid);
        }
    }
    Ok(matches)
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
    use crate::fts::index::{DEFAULT_FIELD, Document};
    use crate::fts::tokenizer::{Analyzer, Profile, TokenizerConfig, Vocabulary};

    #[test]
    fn astra_11_phrase_slop_repetition_and_field_boundaries_match_oracle() {
        use crate::fts::index::LexicalIndex;
        use crate::fts::query::LexicalQuery;
        use crate::fts::sealed::SealedSegment;
        use crate::fts::search::GlobalDocId;
        let texts = [
            "alpha beta alpha",
            "alpha filler beta alpha",
            "beta alpha",
            "alpha alpha beta",
            "alpha the beta",
        ];
        // Literal positions include the stop-word gap in row four. A second
        // field reverses order, and must never contribute to field zero.
        let literal = [
            [vec![0, 2], vec![1]],
            [vec![0, 3], vec![2]],
            [vec![1], vec![0]],
            [vec![0, 1], vec![2]],
            [vec![0], vec![2]],
        ];
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
        let mut active = SegmentIndex::new();
        for text in texts {
            let mut doc = Document::with_text(text);
            doc.set(FieldId(1), "beta alpha");
            active.push_document(&analyzer, &doc).expect("document");
        }
        let bytes = SealedSegment::seal(&active)
            .expect("seal")
            .encode_region()
            .expect("persist");
        let mut index = LexicalIndex::new();
        index
            .push_sealed(SealedSegment::decode_region(&bytes).expect("reopen persisted positions"));
        fn enumerate(streams: &[Vec<u32>], chosen: &mut Vec<u32>, slop: u32) -> bool {
            if chosen.len() == streams.len() {
                let first = chosen[0];
                return chosen
                    .iter()
                    .enumerate()
                    .map(|(i, value)| value.saturating_sub(first).abs_diff(i as u32))
                    .sum::<u32>()
                    <= slop;
            }
            for value in &streams[chosen.len()] {
                if chosen.last().is_none_or(|last| value >= last) {
                    chosen.push(*value);
                    if enumerate(streams, chosen, slop) {
                        return true;
                    }
                    chosen.pop();
                }
            }
            false
        }
        for field in [DEFAULT_FIELD, FieldId(1), FieldId(2)] {
            for words in [vec![0, 1], vec![0, 0], vec![1, 0], vec![0, 1, 0]] {
                let terms = words
                    .iter()
                    .map(|word| {
                        if *word == 0 {
                            b"alpha".to_vec()
                        } else {
                            b"beta".to_vec()
                        }
                    })
                    .collect::<Vec<_>>();
                for slop in [0, 1, 2, 6] {
                    let query = LexicalQuery::phrase(terms.clone(), slop, field);
                    let prepared =
                        PreparedPhrase::new(&index, &query, |_| Ok::<_, ()>(()), || Ok(()))
                            .unwrap_or_else(|_| panic!("prepare"))
                            .expect("phrase");
                    for (row, source) in literal.iter().enumerate() {
                        let streams = words
                            .iter()
                            .map(|word| match field.0 {
                                0 => source[*word].clone(),
                                1 => vec![if *word == 0 { 1 } else { 0 }],
                                _ => vec![],
                            })
                            .collect::<Vec<_>>();
                        let expected = enumerate(&streams, &mut Vec::new(), slop);
                        let actual = prepared
                            .matches(
                                GlobalDocId {
                                    segment: 0,
                                    row: row as u32,
                                },
                                |_| Ok::<_, ()>(()),
                                || Ok(()),
                            )
                            .unwrap_or_else(|_| panic!("positions"));
                        assert_eq!(
                            actual, expected,
                            "row={row} field={field:?} words={words:?} slop={slop}"
                        );
                        if field.0 < 2 {
                            assert_eq!(
                                actual,
                                crate::fts::query::phrase_matches(
                                    &analyzer,
                                    if field.0 == 0 {
                                        texts[row]
                                    } else {
                                        "beta alpha"
                                    },
                                    &terms,
                                    slop
                                ),
                                "old text matcher secondary control"
                            );
                        }
                    }
                }
            }
        }
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

    fn code_analyzer() -> Analyzer {
        Analyzer::new(Profile::Code.config()).expect("valid config")
    }

    fn phrase(terms: &[&str], slop: u32) -> PhraseQuery {
        PhraseQuery {
            terms: terms.iter().map(|term| term.as_bytes().to_vec()).collect(),
            slop,
            field: DEFAULT_FIELD,
        }
    }

    #[test]
    fn slop_zero_and_slop_one_semantics_match_the_pinned_fixtures() {
        let analyzer = code_analyzer();
        let segment = segment_of(
            &analyzer,
            &[
                "quick brown fox", // row 0: adjacent, in order
                "quick red brown", // row 1: one word between quick and brown
                "brown quick",     // row 2: reversed
                "quick",           // row 3: missing the second term
            ],
        );

        // Slop 0 is exact adjacency in order.
        assert_eq!(
            search_segment(&segment, &phrase(&["quick", "brown"], 0)).expect("valid"),
            vec![0]
        );
        // Slop 1 admits the single-position gap in row 1.
        assert_eq!(
            search_segment(&segment, &phrase(&["quick", "brown"], 1)).expect("valid"),
            vec![0, 1]
        );
        // Row 2 is a transposition: brown at 0, quick at 1. Aligning
        // (quick, brown) needs q0=1, q1=0, which is not non-decreasing, so
        // the only legal alignment uses a later brown — there is none.
        assert!(
            !search_segment(&segment, &phrase(&["quick", "brown"], 1))
                .expect("valid")
                .contains(&2)
        );
    }

    #[test]
    fn a_single_term_phrase_is_just_a_term_query() {
        let analyzer = code_analyzer();
        let segment = segment_of(&analyzer, &["alpha beta", "gamma"]);
        assert_eq!(
            search_segment(&segment, &phrase(&["alpha"], 0)).expect("valid"),
            vec![0]
        );
    }

    #[test]
    fn an_empty_phrase_is_a_typed_error() {
        let analyzer = code_analyzer();
        let segment = segment_of(&analyzer, &["alpha"]);
        assert_eq!(
            search_segment(&segment, &phrase(&[], 0)),
            Err(PhraseError::Empty)
        );
    }

    #[test]
    fn synonym_stacked_phrase_matches_the_underscored_identifier() {
        // The task-12 investment paying out: a vocabulary entry makes
        // "put if match" and put_if_match the same thing, and the phrase
        // crosses the stack because stacked variants share a position.
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare(
                "put_if_match",
                &[&["put", "if", "match"][..], &["putifmatch"][..]],
            )
            .expect("valid declaration");
        let analyzer = Analyzer::new(Profile::Code.config().with_vocabulary(vocabulary))
            .expect("valid config");
        let segment = segment_of(&analyzer, &["call put_if_match now"]);

        // The decomposed parts sit at consecutive positions.
        assert_eq!(
            search_segment(&segment, &phrase(&["put", "if", "match"], 0)).expect("valid"),
            vec![0]
        );
        // And the canonical term is stacked at the run's first position.
        assert_eq!(
            search_segment(&segment, &phrase(&["put_if_match"], 0)).expect("valid"),
            vec![0]
        );
    }

    #[test]
    fn a_phrase_crossing_a_stopword_gap_needs_the_slop_the_gap_costs() {
        // "state of the art" analyzes to state@0, art@3 under the default
        // profile: "of" and "the" are stopwords whose positions remain
        // spent. The phrase therefore needs slop 2, and the fixture pins it.
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("valid config");
        let segment = segment_of(&analyzer, &["state of the art"]);
        assert!(
            search_segment(&segment, &phrase(&["state", "art"], 1))
                .expect("valid")
                .is_empty()
        );
        assert_eq!(
            search_segment(&segment, &phrase(&["state", "art"], 2)).expect("valid"),
            vec![0]
        );
    }

    #[test]
    fn displacement_is_exact_not_greedy() {
        // Term A at 0 and 5; term B at 6. A greedy walk anchors on A=0 and
        // reports displacement 5; the true minimum anchors on A=5 for
        // displacement 0.
        let streams = vec![vec![0_u32, 5], vec![6]];
        assert_eq!(minimum_displacement(&streams), Some(0));
        assert!(streams_match(&streams, 0));
    }

    #[test]
    fn a_missing_term_never_matches() {
        assert_eq!(minimum_displacement(&[vec![1_u32], Vec::new()]), None);
        assert!(!streams_match(&[vec![1_u32], Vec::new()], 100));
    }

    #[test]
    fn stacked_positions_cost_nothing_to_cross() {
        // Two terms sharing position 0, then a third at 1: a stack.
        let streams = vec![vec![0_u32], vec![0], vec![1]];
        // Expected offsets are 0,1,2 but actual are 0,0,1, so the
        // displacement is |0-1| + |1-2| = 2 under the pinned metric.
        assert_eq!(minimum_displacement(&streams), Some(2));
    }

    #[test]
    fn an_empty_stream_list_yields_no_displacement() {
        assert_eq!(minimum_displacement(&[]), None);
        assert_eq!(minimum_displacement(&[Vec::new()]), None);
    }
}
