//! Structured lexical queries and their deterministic expansion plan.
//!
//! Persisted source text is deliberately not interpreted here. `StoredText`
//! remains a lossless row-addressed storage primitive; this module compiles
//! query behavior from the indexed vocabulary. Adding another operator (for
//! example regex) therefore does not require a segment-format change.

use std::collections::BTreeSet;

use super::fuzzy;
use super::index::FieldId;
use super::phonetic;
use super::phrase;
use super::search::{FieldWeights, TermQuery};
use super::snippet::Highlight;
use super::tokenizer::Analyzer;

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

    pub(crate) fn fields(&self) -> FieldWeights {
        match self {
            Self::Term(query) => query.fields.clone(),
            Self::Phrase { field, .. }
            | Self::Prefix { field, .. }
            | Self::Fuzzy { field, .. }
            | Self::Phonetic { field, .. } => FieldWeights::flat(&[*field]),
        }
    }

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

pub(crate) fn vocabulary<'a>(terms: impl Iterator<Item = &'a [u8]>) -> BTreeSet<Vec<u8>> {
    terms.map(<[u8]>::to_vec).collect()
}

pub(crate) fn expand(
    query: &LexicalQuery,
    vocabulary: &BTreeSet<Vec<u8>>,
) -> Result<Vec<LexicalExpansion>, LexicalQueryError> {
    let expansions = match query {
        LexicalQuery::Term(query) => {
            if query.terms.is_empty() || query.terms.iter().any(Vec::is_empty) {
                return Err(LexicalQueryError::Empty);
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
                return Err(LexicalQueryError::Empty);
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
                return Err(LexicalQueryError::Empty);
            }
            vocabulary
                .iter()
                .filter(|term| term.starts_with(prefix))
                .cloned()
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
                return Err(LexicalQueryError::Empty);
            }
            if *max_distance > fuzzy::MAX_EDIT_DISTANCE {
                return Err(LexicalQueryError::FuzzyDistance {
                    requested: *max_distance,
                    maximum: fuzzy::MAX_EDIT_DISTANCE,
                });
            }
            vocabulary
                .iter()
                .filter_map(|candidate| {
                    let distance = wagner_fischer(term, candidate);
                    (distance <= *max_distance).then(|| LexicalExpansion {
                        term: candidate.clone(),
                        boost_thousandths: match distance {
                            0 => 1_000,
                            1 => 500,
                            _ => 250,
                        },
                        kind: LexicalMatchKind::Fuzzy { distance },
                    })
                })
                .collect()
        }
        LexicalQuery::Phonetic { term, .. } => {
            let term =
                std::str::from_utf8(term).map_err(|_| LexicalQueryError::InvalidPhoneticUtf8)?;
            let code = phonetic::encode(term);
            if code.is_empty() {
                return Err(LexicalQueryError::EmptyPhoneticCode);
            }
            vocabulary
                .iter()
                .filter(|candidate| {
                    std::str::from_utf8(candidate)
                        .is_ok_and(|candidate| phonetic::encode(candidate) == code)
                })
                .cloned()
                .map(|term| LexicalExpansion {
                    term,
                    boost_thousandths: 250,
                    kind: LexicalMatchKind::Phonetic,
                })
                .collect()
        }
    };
    Ok(expansions)
}

pub(crate) fn phrase_matches(
    analyzer: &Analyzer,
    text: &str,
    terms: &[Vec<u8>],
    slop: u32,
) -> bool {
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

fn wagner_fischer(left: &[u8], right: &[u8]) -> u32 {
    let mut previous = (0..=right.len())
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
        .collect::<Vec<_>>();
    for (row, left_byte) in left.iter().enumerate() {
        let mut current = Vec::with_capacity(right.len() + 1);
        current.push(u32::try_from(row + 1).unwrap_or(u32::MAX));
        for (column, right_byte) in right.iter().enumerate() {
            let deletion = previous[column + 1].saturating_add(1);
            let insertion = current[column].saturating_add(1);
            let substitution = previous[column].saturating_add(u32::from(left_byte != right_byte));
            current.push(deletion.min(insertion).min(substitution));
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(u32::MAX)
}
