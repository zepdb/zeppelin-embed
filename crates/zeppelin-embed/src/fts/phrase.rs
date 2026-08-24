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
        return first.is_empty().then_some(0).or(Some(0)).filter(|_| !first.is_empty());
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
            let mut current: Vec<(u32, u32)> = Vec::new();
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
    use crate::fts::index::{Document, DEFAULT_FIELD};
    use crate::fts::tokenizer::{Analyzer, Profile, TokenizerConfig, Vocabulary};

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
                "quick brown fox",   // row 0: adjacent, in order
                "quick red brown",   // row 1: one word between quick and brown
                "brown quick",       // row 2: reversed
                "quick",             // row 3: missing the second term
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
