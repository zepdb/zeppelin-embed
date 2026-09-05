//! Structured lexical queries and their deterministic expansion plan.
//!
//! Persisted source text is deliberately not interpreted here. `StoredText`
//! remains a lossless row-addressed storage primitive; this module compiles
//! query behavior from the indexed vocabulary. Adding another operator (for
//! example regex) therefore does not require a segment-format change.

use super::vocabulary::Vocabulary;

use super::fuzzy;
use super::index::FieldId;
use super::phonetic;
#[cfg(test)]
use super::phrase;
use super::search::{FieldWeights, TermQuery};
use super::snippet::Highlight;
#[cfg(test)]
use super::tokenizer::Analyzer;

/// Expansion order and one set of pinned BM25 statistics, shared by the
/// combined top-k producer and exact scoring of supplied hybrid candidates.
pub(crate) struct PreparedWeightedQuery {
    expansions: Vec<LexicalExpansion>,
    scoring: super::search::PreparedTermQuery,
}

impl PreparedWeightedQuery {
    pub(crate) fn new(
        index: &super::index::LexicalIndex,
        expansions: Vec<LexicalExpansion>,
        fields: FieldWeights,
    ) -> Result<Self, super::index::IndexError> {
        let query = TermQuery {
            terms: expansions.iter().map(|entry| entry.term.clone()).collect(),
            fields,
        };
        Ok(Self {
            scoring: super::search::PreparedTermQuery::from_owned(
                index,
                query,
                super::bm25::Bm25Params::beir(),
            )?,
            expansions,
        })
    }

    pub(crate) fn allocation_bytes(
        expansions: &Vec<LexicalExpansion>,
        fields: &FieldWeights,
    ) -> Option<usize> {
        let mut bytes = expansions
            .capacity()
            .checked_mul(std::mem::size_of::<LexicalExpansion>())?;
        for entry in expansions {
            bytes = bytes.checked_add(entry.term.capacity())?.checked_add(
                super::search::PreparedTermQuery::single_allocation_bytes(
                    &FieldWeights::flat(&[]),
                    entry.term.len(),
                )?,
            )?;
        }
        bytes.checked_add(
            fields
                .iter()
                .count()
                .checked_mul(std::mem::size_of::<(FieldId, u32)>())?,
        )
    }

    pub(crate) fn expansions(&self) -> &[LexicalExpansion] {
        &self.expansions
    }
    pub(crate) fn take_expansions(&mut self) -> Vec<LexicalExpansion> {
        std::mem::take(&mut self.expansions)
    }
    pub(crate) fn scoring(&self) -> &super::search::PreparedTermQuery {
        &self.scoring
    }
}

/// A structured lexical query over already-analyzed terms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LexicalQuery {
    /// Existing OR-of-terms semantics.
    Term(TermQuery),
    /// Terms must occur in positional phrase order within the declared slop.
    Phrase {
        /// Analyzed phrase terms in order.
        terms: Vec<Vec<u8>>,
        /// Maximum total positional displacement.
        slop: u32,
        /// Field to match.
        field: FieldId,
    },
    /// Every indexed term beginning with this analyzed byte prefix.
    Prefix {
        /// Non-empty analyzed prefix.
        prefix: Vec<u8>,
        /// Field to match.
        field: FieldId,
    },
    /// Every indexed term within a bounded edit distance.
    Fuzzy {
        /// Analyzed source term.
        term: Vec<u8>,
        /// Requested edit-distance ceiling, at most two.
        max_distance: u32,
        /// Field to match.
        field: FieldId,
    },
    /// Terms sharing the pinned phonetic code.
    Phonetic {
        /// UTF-8 term whose primary phonetic code is used.
        term: Vec<u8>,
        /// Field to match.
        field: FieldId,
    },
}

impl LexicalQuery {
    /// Wraps the compatibility term query.
    #[must_use]
    pub const fn term(query: TermQuery) -> Self {
        Self::Term(query)
    }

    /// Builds a phrase query.
    #[must_use]
    pub const fn phrase(terms: Vec<Vec<u8>>, slop: u32, field: FieldId) -> Self {
        Self::Phrase { terms, slop, field }
    }

    /// Builds a prefix query.
    #[must_use]
    pub const fn prefix(prefix: Vec<u8>, field: FieldId) -> Self {
        Self::Prefix { prefix, field }
    }

    /// Builds a bounded fuzzy query.
    #[must_use]
    pub const fn fuzzy(term: Vec<u8>, max_distance: u32, field: FieldId) -> Self {
        Self::Fuzzy {
            term,
            max_distance,
            field,
        }
    }

    /// Builds a phonetic query.
    #[must_use]
    pub const fn phonetic(term: Vec<u8>, field: FieldId) -> Self {
        Self::Phonetic { term, field }
    }

    pub(crate) fn needs_vocabulary(&self) -> bool {
        matches!(
            self,
            Self::Prefix { .. } | Self::Fuzzy { .. } | Self::Phonetic { .. }
        )
    }

    pub(crate) fn fields(&self) -> FieldWeights {
        match self {
            Self::Term(query) => query.fields.clone(),
            Self::Phrase { field, .. }
            | Self::Prefix { field, .. }
            | Self::Fuzzy { field, .. }
            | Self::Phonetic { field, .. } => FieldWeights::flat(&[*field]),
        }
    }

    #[cfg(test)]
    pub(crate) fn phrase_constraint(&self) -> Option<(&[Vec<u8>], u32)> {
        match self {
            Self::Phrase { terms, slop, .. } => Some((terms, *slop)),
            _ => None,
        }
    }
}

/// Why one analyzed term entered a structured query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LexicalMatchKind {
    /// The caller supplied this exact term.
    Term,
    /// The term is one member of a phrase.
    Phrase,
    /// The term expanded from a prefix.
    Prefix,
    /// The term expanded by edit distance.
    Fuzzy {
        /// Exact Wagner-Fischer edit distance.
        distance: u32,
    },
    /// The term shares the query's pinned phonetic code.
    Phonetic,
}

/// One reported scoring expansion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalExpansion {
    /// Indexed analyzed term.
    pub term: Vec<u8>,
    /// Multiplicative BM25 boost in thousandths.
    pub boost_thousandths: u16,
    /// Expansion provenance.
    pub kind: LexicalMatchKind,
}

/// Owned snippet text with absolute source ranges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedLexicalSnippet {
    /// UTF-8-valid text copied from `source`.
    pub text: String,
    /// Absolute byte range within the persisted source text.
    pub source: Highlight,
    /// Absolute matched byte ranges within the persisted source text.
    pub highlights: Vec<Highlight>,
}

/// A malformed structured lexical query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LexicalQueryError {
    /// A term, phrase, or prefix that must be non-empty was empty.
    Empty,
    /// Fuzzy edit distance exceeded the pinned maximum.
    FuzzyDistance {
        /// Requested edit-distance ceiling.
        requested: u32,
        /// Engine maximum.
        maximum: u32,
    },
    /// A phonetic query was not UTF-8.
    InvalidPhoneticUtf8,
    /// The phonetic query contained no encodable letters.
    EmptyPhoneticCode,
}

impl std::fmt::Display for LexicalQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("structured lexical query must not be empty"),
            Self::FuzzyDistance { requested, maximum } => write!(
                formatter,
                "fuzzy distance {requested} exceeds maximum {maximum}"
            ),
            Self::InvalidPhoneticUtf8 => formatter.write_str("phonetic query must be UTF-8"),
            Self::EmptyPhoneticCode => {
                formatter.write_str("phonetic query has no encodable letters")
            }
        }
    }
}

impl std::error::Error for LexicalQueryError {}

#[cfg(test)]
fn vocabulary<'a>(query: &LexicalQuery, terms: impl Iterator<Item = &'a [u8]>) -> Vocabulary {
    if query.needs_vocabulary() {
        terms.map(<[u8]>::to_vec).collect()
    } else {
        Vocabulary::empty()
    }
}

pub(crate) fn expand_with_phonetic<E: From<LexicalQueryError>>(
    query: &LexicalQuery,
    vocabulary: &Vocabulary,
    phonetic_lookup: impl FnOnce(&str) -> Result<Vec<LexicalExpansion>, E>,
) -> Result<Vec<LexicalExpansion>, E> {
    #[cfg(any(test, feature = "test-support"))]
    super::preparation_observer::expansion();
    let expansions = match query {
        LexicalQuery::Term(query) => {
            if query.terms.is_empty() || query.terms.iter().any(Vec::is_empty) {
                return Err(LexicalQueryError::Empty.into());
            }
            query
                .terms
                .iter()
                .cloned()
                .map(|term| LexicalExpansion {
                    term,
                    boost_thousandths: 1_000,
                    kind: LexicalMatchKind::Term,
                })
                .collect()
        }
        LexicalQuery::Phrase { terms, .. } => {
            if terms.is_empty() || terms.iter().any(Vec::is_empty) {
                return Err(LexicalQueryError::Empty.into());
            }
            terms
                .iter()
                .cloned()
                .map(|term| LexicalExpansion {
                    term,
                    boost_thousandths: 1_000,
                    kind: LexicalMatchKind::Phrase,
                })
                .collect()
        }
        LexicalQuery::Prefix { prefix, .. } => {
            if prefix.is_empty() {
                return Err(LexicalQueryError::Empty.into());
            }
            vocabulary
                .prefix(prefix)
                .map(<[u8]>::to_vec)
                .map(|term| LexicalExpansion {
                    term,
                    boost_thousandths: 1_000,
                    kind: LexicalMatchKind::Prefix,
                })
                .collect()
        }
        LexicalQuery::Fuzzy {
            term, max_distance, ..
        } => {
            if term.is_empty() {
                return Err(LexicalQueryError::Empty.into());
            }
            if *max_distance > fuzzy::MAX_EDIT_DISTANCE {
                return Err(LexicalQueryError::FuzzyDistance {
                    requested: *max_distance,
                    maximum: fuzzy::MAX_EDIT_DISTANCE,
                }
                .into());
            }
            let mut scratch = fuzzy::BoundedDistance::new();
            let mut matches = vocabulary
                .fuzzy_candidates(term.len(), *max_distance)
                .filter_map(|candidate| {
                    scratch
                        .distance(term, candidate, *max_distance)
                        .map(|distance| LexicalExpansion {
                            term: candidate.to_vec(),
                            boost_thousandths: match distance {
                                0 => 1_000,
                                1 => 500,
                                _ => 250,
                            },
                            kind: LexicalMatchKind::Fuzzy { distance },
                        })
                })
                .collect::<Vec<_>>();
            matches.sort_unstable_by(|left, right| left.term.cmp(&right.term));
            matches
        }
        LexicalQuery::Phonetic { term, .. } => {
            let term =
                std::str::from_utf8(term).map_err(|_| LexicalQueryError::InvalidPhoneticUtf8)?;
            let code = phonetic::encode(term);
            if code.is_empty() {
                return Err(LexicalQueryError::EmptyPhoneticCode.into());
            }
            phonetic_lookup(&code)?
        }
    };
    Ok(expansions)
}

pub(crate) fn phonetic_expansions(
    vocabulary: &Vocabulary,
    index: &super::phonetic_index::PhoneticIndex,
    code: &str,
) -> Vec<LexicalExpansion> {
    vocabulary
        .select(index.terms(code))
        .map(|term| LexicalExpansion {
            term: term.to_vec(),
            boost_thousandths: 250,
            kind: LexicalMatchKind::Phonetic,
        })
        .collect()
}

#[cfg(test)]
fn expand(
    query: &LexicalQuery,
    vocabulary: &Vocabulary,
) -> Result<Vec<LexicalExpansion>, LexicalQueryError> {
    // Existing small query tests retain the independent full encoder-scan control.
    expand_with_phonetic(query, vocabulary, |code| {
        Ok(vocabulary
            .iter()
            .filter(|term| {
                std::str::from_utf8(term).is_ok_and(|term| phonetic::encode(term) == code)
            })
            .map(|term| LexicalExpansion {
                term: term.to_vec(),
                boost_thousandths: 250,
                kind: LexicalMatchKind::Phonetic,
            })
            .collect())
    })
}

#[cfg(test)]
pub(crate) fn phrase_matches(
    analyzer: &Analyzer,
    text: &str,
    terms: &[Vec<u8>],
    slop: u32,
) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    super::preparation_observer::phrase_reanalysis(text.len());
    let analyzed = analyzer.analyze(text);
    let streams = terms
        .iter()
        .map(|term| {
            analyzed
                .iter()
                .filter(|token| token.term.as_bytes() == term)
                .map(|token| token.position)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    phrase::streams_match(&streams, slop)
}

#[cfg(test)]
fn wagner_fischer(left: &[u8], right: &[u8]) -> u32 {
    #[cfg(any(test, feature = "test-support"))]
    let (mut cells, mut allocations) = (0, 1);
    let mut previous = (0..=right.len())
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
        .collect::<Vec<_>>();
    for (row, left_byte) in left.iter().enumerate() {
        let mut current = Vec::with_capacity(right.len() + 1);
        #[cfg(any(test, feature = "test-support"))]
        {
            allocations += 1;
        }
        current.push(u32::try_from(row + 1).unwrap_or(u32::MAX));
        for (column, right_byte) in right.iter().enumerate() {
            #[cfg(any(test, feature = "test-support"))]
            {
                cells += 1;
            }
            let (Some(up), Some(left), Some(diagonal)) = (
                previous.get(column + 1).copied(),
                current.get(column).copied(),
                previous.get(column).copied(),
            ) else {
                return u32::MAX;
            };
            let deletion = up.saturating_add(1);
            let insertion = left.saturating_add(1);
            let substitution = diagonal.saturating_add(u32::from(left_byte != right_byte));
            current.push(deletion.min(insertion).min(substitution));
        }
        previous = current;
    }
    #[cfg(any(test, feature = "test-support"))]
    super::preparation_observer::fuzzy_distance(cells, allocations);
    previous.last().copied().unwrap_or(u32::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn astra_14_phonetic_expansion_cost_screen() {
        use std::time::Instant;
        let mut terms = vec![b"night".to_vec(), b"knight".to_vec()];
        for mut n in 0..4096 {
            let mut term = if n < 2048 {
                b"smithson".to_vec()
            } else {
                b"robert".to_vec()
            };
            for _ in 0..4 {
                term.push(b'a' + (n % 26) as u8);
                n /= 26;
            }
            terms.push(term);
        }
        let dictionary = terms.into_iter().collect::<Vocabulary>();
        let index = super::super::phonetic_index::PhoneticIndex::build(&dictionary, |_, _| {
            Ok::<_, std::convert::Infallible>(())
        })
        .expect("cached map");
        let cached_expand = |query: &LexicalQuery| {
            expand_with_phonetic::<LexicalQueryError>(query, &dictionary, |code| {
                Ok(phonetic_expansions(&dictionary, &index, code))
            })
        };
        for (name, term, count) in [
            ("rare", b"night".as_slice(), 2),
            ("collision", b"smithson".as_slice(), 2048),
        ] {
            let query = LexicalQuery::phonetic(term.to_vec(), field());
            for _ in 0..2 {
                drop(cached_expand(&query).expect("warm"));
            }
            super::super::preparation_observer::begin();
            let expected = cached_expand(&query).expect("probe");
            let encodings = super::super::preparation_observer::phonetic_encoding_calls();
            super::super::preparation_observer::take();
            assert_eq!(expected.len(), count);
            let mut samples = Vec::new();
            for _ in 0..8 {
                let start = Instant::now();
                let actual = cached_expand(&query).expect("measure");
                samples.push(start.elapsed().as_secs_f64() * 1e6);
                assert_eq!(actual, expected);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "PHONETIC_SCREEN {name} vocabulary=4098 warmups=2 samples=8 p50_us={} p95_us={} expansions={} encodings={encodings}",
                (samples.get(3).expect("sample") + samples.get(4).expect("sample")) / 2.0,
                samples.last().expect("sample"),
                expected.len()
            );
        }
    }

    #[test]
    fn astra_13_fuzzy_scratch_is_reused_per_query() {
        let query = LexicalQuery::fuzzy(b"abcdefgh".to_vec(), 2, field());
        let terms = (b'a'..=b'z')
            .map(|last| {
                let mut term = b"abcdefg".to_vec();
                term.push(last);
                term
            })
            .collect::<Vec<_>>();
        let dictionary = vocabulary(&query, terms.iter().map(Vec::as_slice));
        for _ in 0..2 {
            super::super::preparation_observer::begin();
            assert_eq!(expand(&query, &dictionary).expect("query").len(), 26);
            let work = super::super::preparation_observer::fuzzy_work();
            super::super::preparation_observer::take();
            assert_eq!(work.scratch_constructions, 1, "one per query: {work:?}");
            assert_eq!(work.row_allocations, 0, "stack rows: {work:?}");
            assert_eq!(work.candidates, 26);
            assert!(work.dp_cells < 26 * 8 * 8, "banded cells: {work:?}");
        }
    }

    #[test]
    fn astra_13_fuzzy_bounded_matches_full_distance_expansions() {
        use rand::RngCore;
        use std::collections::{BTreeMap, BTreeSet};
        let mut tiny = vec![Vec::new()];
        for len in 1..=4 {
            for bits in 0..(1 << len) {
                tiny.push((0..len).map(|bit| b'a' + ((bits >> bit) & 1)).collect());
            }
        }
        let dictionary = tiny.iter().cloned().collect::<Vocabulary>();
        // Primary oracle enumerates actual edit operations, with no DP recurrence.
        for term in tiny.iter().filter(|term| !term.is_empty()) {
            let mut distances = BTreeMap::from([(term.clone(), 0_u32)]);
            let mut frontier = BTreeSet::from([term.clone()]);
            for distance in 0..=2 {
                let query = LexicalQuery::fuzzy(term.clone(), distance, field());
                let expected = dictionary
                    .iter()
                    .filter_map(|candidate| {
                        distances.get(candidate).map(|&distance| LexicalExpansion {
                            term: candidate.to_vec(),
                            boost_thousandths: match distance {
                                0 => 1000,
                                1 => 500,
                                _ => 250,
                            },
                            kind: LexicalMatchKind::Fuzzy { distance },
                        })
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    expand(&query, &dictionary).expect("tiny query"),
                    expected,
                    "{query:?}"
                );
                if distance == 2 {
                    break;
                }
                let mut next = BTreeSet::new();
                for word in frontier {
                    for at in 0..=word.len() {
                        for byte in b"ab" {
                            let mut inserted = word.clone();
                            inserted.insert(at, *byte);
                            next.insert(inserted);
                            if at < word.len() {
                                let mut replaced = word.clone();
                                *replaced.get_mut(at).expect("within word") = *byte;
                                next.insert(replaced);
                            }
                        }
                        if at < word.len() {
                            let mut removed = word.clone();
                            removed.remove(at);
                            next.insert(removed);
                        }
                    }
                }
                for word in &next {
                    distances.entry(word.clone()).or_insert(distance + 1);
                }
                frontier = next;
            }
        }
        // Secondary full-distance oracle covers arbitrary bytes, lengths and mutations.
        let mut rng = crate::test_support::seeded_rng(
            "astra_13_fuzzy_bounded_matches_full_distance_expansions",
        );
        for _ in 0..256 {
            let len = 1 + (rng.next_u32() % 80) as usize;
            let term = (0..len)
                .map(|_| (rng.next_u32() % 256) as u8)
                .collect::<Vec<_>>();
            let mut terms = vec![term.clone(), Vec::new()];
            for _ in 0..12 {
                let mut candidate = term.clone();
                for _ in 0..rng.next_u32() % 5 {
                    let at = (rng.next_u32() as usize) % (candidate.len() + 1);
                    match rng.next_u32() % 3 {
                        0 => candidate.insert(at, rng.next_u32() as u8),
                        1 if at < candidate.len() => {
                            candidate.remove(at);
                        }
                        _ => {
                            if let Some(byte) = candidate.get_mut(at) {
                                *byte = rng.next_u32() as u8;
                            }
                        }
                    }
                }
                terms.push(candidate);
            }
            let dictionary = terms.into_iter().collect::<Vocabulary>();
            for maximum in 0..=2 {
                let query = LexicalQuery::fuzzy(term.clone(), maximum, field());
                let expected = dictionary
                    .iter()
                    .filter_map(|candidate| {
                        let distance = wagner_fischer(&term, candidate);
                        (distance <= maximum).then(|| LexicalExpansion {
                            term: candidate.to_vec(),
                            boost_thousandths: match distance {
                                0 => 1000,
                                1 => 500,
                                _ => 250,
                            },
                            kind: LexicalMatchKind::Fuzzy { distance },
                        })
                    })
                    .collect::<Vec<_>>();
                assert_eq!(expand(&query, &dictionary).expect("seeded query"), expected);
            }
        }
    }

    #[test]
    fn astra_13_fuzzy_distance_boundaries_preserve_boosts() {
        let dictionary = [
            b"ab".to_vec(),
            b"ba".to_vec(),
            b"xb".to_vec(),
            b"zzab".to_vec(),
            b"zzzab".to_vec(),
        ]
        .into_iter()
        .collect::<Vocabulary>();
        let query = LexicalQuery::fuzzy(b"ab".to_vec(), 2, field());
        let actual = expand(&query, &dictionary).expect("boundary query");
        assert_eq!(
            actual
                .iter()
                .map(|e| (e.term.as_slice(), e.boost_thousandths))
                .collect::<Vec<_>>(),
            vec![
                (b"ab".as_slice(), 1000),
                (b"ba".as_slice(), 250),
                (b"xb".as_slice(), 500),
                (b"zzab".as_slice(), 250)
            ]
        );
        let utf8 = [
            b"e".to_vec(),
            "é".as_bytes().to_vec(),
            "ê".as_bytes().to_vec(),
        ]
        .into_iter()
        .collect::<Vocabulary>();
        let query = LexicalQuery::fuzzy("é".as_bytes().to_vec(), 1, FieldId(99));
        let actual = expand(&query, &utf8).expect("byte distance, global fields");
        assert_eq!(
            actual
                .iter()
                .map(|e| (e.term.as_slice(), e.boost_thousandths))
                .collect::<Vec<_>>(),
            vec![("é".as_bytes(), 1000), ("ê".as_bytes(), 500)]
        );
        assert_eq!(
            expand(&LexicalQuery::fuzzy(Vec::new(), 2, field()), &utf8),
            Err(LexicalQueryError::Empty)
        );
        assert_eq!(
            expand(&LexicalQuery::fuzzy(b"ab".to_vec(), 3, field()), &utf8),
            Err(LexicalQueryError::FuzzyDistance {
                requested: 3,
                maximum: 2
            })
        );
    }

    #[test]
    fn astra_13_fuzzy_length_filter_avoids_impossible_dp_work() {
        let query = LexicalQuery::fuzzy(b"ab".to_vec(), 2, field());
        let terms = [vec![b'x'; 64], vec![b'y'; 128]];
        let dictionary = vocabulary(&query, terms.iter().map(Vec::as_slice));
        super::super::preparation_observer::begin();
        let actual = expand(&query, &dictionary).expect("valid fuzzy query");
        let work = super::super::preparation_observer::fuzzy_work();
        super::super::preparation_observer::take();
        assert!(actual.is_empty());
        assert_eq!(
            work.dp_cells, 0,
            "impossible byte lengths performed DP: {work:?}"
        );
        assert_eq!(work.row_allocations, 0);
        assert_eq!(work.candidates, 0);
    }

    #[test]
    fn astra_13_fuzzy_expansion_cost_screen() {
        use std::time::Instant;
        let terms = (0..4096)
            .map(|mut n| {
                let mut term = vec![b'a'; 60];
                for _ in 0..4 {
                    term.push(b'a' + (n % 26) as u8);
                    n /= 26;
                }
                term
            })
            .collect::<Vec<_>>();
        let seed_query = LexicalQuery::fuzzy(vec![b'a'; 64], 2, field());
        let dictionary = vocabulary(&seed_query, terms.iter().map(Vec::as_slice));
        for (name, term) in [
            ("length_mismatch", vec![b'z'; 4]),
            ("same_length_far", vec![b'z'; 64]),
            ("same_length_near", vec![b'a'; 64]),
        ] {
            let query = LexicalQuery::fuzzy(term, 2, field());
            for _ in 0..2 {
                drop(expand(&query, &dictionary).expect("warm"));
            }
            super::super::preparation_observer::begin();
            let expected = expand(&query, &dictionary).expect("work probe");
            let work = super::super::preparation_observer::fuzzy_work();
            super::super::preparation_observer::take();
            if name != "same_length_near" {
                assert!(expected.is_empty());
            }
            let mut samples = Vec::new();
            for _ in 0..8 {
                let start = Instant::now();
                let actual = expand(&query, &dictionary).expect("timed expansion");
                samples.push(start.elapsed().as_secs_f64() * 1e6);
                assert_eq!(actual, expected);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "FUZZY_SCREEN {name} vocabulary=4096 warmups=2 samples=8 p50_us={} p95_us={} expansions={} work={work:?}",
                (samples.get(3).expect("sample") + samples.get(4).expect("sample")) / 2.0,
                samples.last().expect("sample"),
                expected.len()
            );
        }
    }

    #[test]
    fn astra_12_vocabulary_groups_memberships_with_linear_work() {
        let terms = (0..10_000)
            .map(|n| format!("unique{n:05}").into_bytes())
            .collect::<Vec<_>>();
        super::super::preparation_observer::begin();
        let dictionary = vocabulary(
            &LexicalQuery::prefix(b"unique".to_vec(), field()),
            terms.iter().map(Vec::as_slice),
        );
        let checks = super::super::preparation_observer::vocabulary_group_checks();
        super::super::preparation_observer::take();
        assert_eq!(dictionary.iter().count(), terms.len());
        assert!(
            checks <= 2 * terms.len(),
            "group boundary comparisons: {checks}"
        );
    }

    #[test]
    fn astra_12_prefix_visits_only_matching_vocabulary_range() {
        let mut terms = (0..20_000)
            .map(|n| format!("{}{n:05}", if n < 10_000 { "aaa" } else { "zzz" }).into_bytes())
            .collect::<Vec<_>>();
        terms.extend([
            b"middlea".to_vec(),
            b"middleb".to_vec(),
            b"middlec".to_vec(),
        ]);
        let query = LexicalQuery::prefix(b"middle".to_vec(), field());
        let dictionary = vocabulary(&query, terms.iter().map(Vec::as_slice));
        super::super::preparation_observer::begin();
        let actual = expand(&query, &dictionary).expect("expand");
        let work = super::super::preparation_observer::vocabulary_work();
        super::super::preparation_observer::take();
        let expected = [b"middlea", b"middleb", b"middlec"]
            .into_iter()
            .map(|term| LexicalExpansion {
                term: term.to_vec(),
                boost_thousandths: 1000,
                kind: LexicalMatchKind::Prefix,
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(
            work.terms_visited <= 4,
            "prefix walked unrelated vocabulary: {work:?}"
        );
        assert!(work.seek_steps <= 16, "lower-bound work: {work:?}");
    }

    fn field() -> FieldId {
        FieldId(7)
    }

    #[test]
    fn astra_12_prefix_field_membership_and_byte_edges_preserve_expansion_order() {
        use std::collections::{BTreeMap, BTreeSet};
        let source = [
            (b"alpha".as_slice(), FieldId(7)),
            (b"alpine", FieldId(8)),
            (b"alpha", FieldId(8)),
            (b"alpha", FieldId(7)),
            (&[0], FieldId(3)),
            (&[0, 255], FieldId(4)),
            (&[255], FieldId(7)),
            (&[255, 255], FieldId(8)),
            (&[255, 255, 0], FieldId(7)),
            ("éclair".as_bytes(), FieldId(8)),
        ];
        let mut oracle = BTreeMap::<Vec<u8>, BTreeSet<FieldId>>::new();
        for (term, field) in source {
            oracle.entry(term.to_vec()).or_default().insert(field);
        }
        let dictionary =
            Vocabulary::build(source.into_iter(), |_, _| Ok::<_, ()>(())).expect("dictionary");
        assert_eq!(
            dictionary
                .entries()
                .map(|(term, fields)| (term.to_vec(), fields.to_vec()))
                .collect::<Vec<_>>(),
            oracle
                .iter()
                .map(|(term, fields)| (term.clone(), fields.iter().copied().collect()))
                .collect::<Vec<_>>()
        );
        for prefix in [
            b"al".as_slice(),
            &[0],
            &[255],
            &[255, 255],
            &[255, 255, 255],
            &[0xc3],
            b"missing",
        ] {
            for field in [FieldId(7), FieldId(8), FieldId(99)] {
                // Vocabulary visibility is global across fields, just as before;
                // field restrictions belong to scoring, not expansion policy.
                let query = LexicalQuery::prefix(prefix.to_vec(), field);
                let expected = oracle
                    .keys()
                    .filter(|term| term.starts_with(prefix))
                    .map(|term| LexicalExpansion {
                        term: term.clone(),
                        boost_thousandths: 1000,
                        kind: LexicalMatchKind::Prefix,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    expand(&query, &dictionary).expect("expand arbitrary bytes"),
                    expected
                );
            }
        }
    }

    #[cfg(feature = "allocation-audit")]
    #[test]
    fn term_query_expansion_builds_no_vocabulary() {
        let source_terms = [
            b"cat".to_vec(),
            b"cats".to_vec(),
            b"cut".to_vec(),
            b"dog".to_vec(),
            b"night".to_vec(),
            b"nite".to_vec(),
        ];
        let query = LexicalQuery::term(TermQuery::flat(vec![b"cat".to_vec()], &[field()]));
        let empty_vocabulary = Vocabulary::empty();
        drop(expand(&query, &empty_vocabulary).expect("warm term expansion"));
        let (_, baseline) =
            crate::allocation_audit::audit_engine_path(|| expand(&query, &empty_vocabulary));

        let (expansions, report) = crate::allocation_audit::audit_engine_path(|| {
            let vocabulary = vocabulary(&query, source_terms.iter().map(Vec::as_slice));
            expand(&query, &vocabulary)
        });

        assert_eq!(
            expansions.expect("term expansion"),
            vec![LexicalExpansion {
                term: b"cat".to_vec(),
                boost_thousandths: 1_000,
                kind: LexicalMatchKind::Term,
            }]
        );
        assert_eq!(
            report.allocations, baseline.allocations,
            "Term expansion built the index vocabulary"
        );
    }

    #[test]
    fn vocabulary_expansions_preserve_terms_and_order() {
        let terms = [
            b"cat".as_slice(),
            b"cats",
            b"cut",
            b"dog",
            b"nite",
            b"night",
        ];
        let prefix = LexicalQuery::prefix(b"ca".to_vec(), field());
        let prefix_vocabulary = vocabulary(&prefix, terms.iter().copied());

        assert_eq!(
            expand(&prefix, &prefix_vocabulary).expect("prefix expansion"),
            vec![
                LexicalExpansion {
                    term: b"cat".to_vec(),
                    boost_thousandths: 1_000,
                    kind: LexicalMatchKind::Prefix,
                },
                LexicalExpansion {
                    term: b"cats".to_vec(),
                    boost_thousandths: 1_000,
                    kind: LexicalMatchKind::Prefix,
                },
            ]
        );
        let fuzzy = LexicalQuery::fuzzy(b"cat".to_vec(), 1, field());
        let fuzzy_vocabulary = vocabulary(&fuzzy, terms.iter().copied());
        assert_eq!(
            expand(&fuzzy, &fuzzy_vocabulary).expect("fuzzy expansion"),
            vec![
                LexicalExpansion {
                    term: b"cat".to_vec(),
                    boost_thousandths: 1_000,
                    kind: LexicalMatchKind::Fuzzy { distance: 0 },
                },
                LexicalExpansion {
                    term: b"cats".to_vec(),
                    boost_thousandths: 500,
                    kind: LexicalMatchKind::Fuzzy { distance: 1 },
                },
                LexicalExpansion {
                    term: b"cut".to_vec(),
                    boost_thousandths: 500,
                    kind: LexicalMatchKind::Fuzzy { distance: 1 },
                },
            ]
        );
        let phonetic = LexicalQuery::phonetic(b"night".to_vec(), field());
        let phonetic_vocabulary = vocabulary(&phonetic, terms.iter().copied());
        assert_eq!(
            expand(&phonetic, &phonetic_vocabulary).expect("phonetic expansion"),
            vec![
                LexicalExpansion {
                    term: b"night".to_vec(),
                    boost_thousandths: 250,
                    kind: LexicalMatchKind::Phonetic,
                },
                LexicalExpansion {
                    term: b"nite".to_vec(),
                    boost_thousandths: 250,
                    kind: LexicalMatchKind::Phonetic,
                },
            ]
        );
    }

    #[test]
    fn structured_query_validation_and_expansion_are_exhaustive() {
        let vocabulary = [
            b"cat".as_slice(),
            b"cats",
            b"cut",
            b"dog",
            b"nite",
            b"night",
        ]
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
        let term = LexicalQuery::term(TermQuery::flat(vec![b"cat".to_vec()], &[field()]));
        assert_eq!(term.fields(), FieldWeights::flat(&[field()]));
        assert!(term.phrase_constraint().is_none());
        assert_eq!(expand(&term, &vocabulary).expect("term expansion").len(), 1);

        let phrase = LexicalQuery::phrase(vec![b"cat".to_vec(), b"dog".to_vec()], 1, field());
        assert_eq!(
            phrase.phrase_constraint(),
            Some((&[b"cat".to_vec(), b"dog".to_vec()][..], 1))
        );
        assert_eq!(
            expand(&phrase, &vocabulary)
                .expect("phrase expansion")
                .len(),
            2
        );
        assert_eq!(
            expand(&LexicalQuery::prefix(b"ca".to_vec(), field()), &vocabulary)
                .expect("prefix expansion")
                .len(),
            2
        );
        let fuzzy = expand(
            &LexicalQuery::fuzzy(b"cat".to_vec(), 1, field()),
            &vocabulary,
        )
        .expect("fuzzy expansion");
        assert!(fuzzy.iter().any(|entry| entry.boost_thousandths == 1_000));
        assert!(fuzzy.iter().any(|entry| entry.boost_thousandths == 500));
        assert!(
            !expand(
                &LexicalQuery::phonetic(b"night".to_vec(), field()),
                &vocabulary,
            )
            .expect("phonetic expansion")
            .is_empty()
        );

        for query in [
            LexicalQuery::term(TermQuery::flat(Vec::new(), &[field()])),
            LexicalQuery::phrase(vec![Vec::new()], 0, field()),
            LexicalQuery::prefix(Vec::new(), field()),
            LexicalQuery::fuzzy(Vec::new(), 1, field()),
        ] {
            assert_eq!(expand(&query, &vocabulary), Err(LexicalQueryError::Empty));
        }
        let distance = expand(
            &LexicalQuery::fuzzy(b"cat".to_vec(), 3, field()),
            &vocabulary,
        )
        .expect_err("distance ceiling");
        assert!(distance.to_string().contains("exceeds maximum"));
        let invalid = expand(&LexicalQuery::phonetic(vec![0xff], field()), &vocabulary)
            .expect_err("invalid phonetic UTF-8");
        assert_eq!(invalid, LexicalQueryError::InvalidPhoneticUtf8);
        assert_eq!(invalid.to_string(), "phonetic query must be UTF-8");
        let empty_code = expand(
            &LexicalQuery::phonetic(b"123".to_vec(), field()),
            &vocabulary,
        )
        .expect_err("empty phonetic code");
        assert_eq!(empty_code, LexicalQueryError::EmptyPhoneticCode);
        assert_eq!(
            LexicalQueryError::Empty.to_string(),
            "structured lexical query must not be empty"
        );
        assert_eq!(wagner_fischer(b"kitten", b"sitting"), 3);
    }
}
