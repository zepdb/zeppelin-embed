//! Selectivity-driven exact lexical filtering.

use crate::fts::bm25::Bm25Params;
use crate::fts::index::{IndexError, LexicalIndex};
use crate::fts::prune::{search_pruned_filtered, select_strategy};
use crate::fts::search::{SearchResult, TermQuery, search_allow_list_driven};
use crate::meta::DocBitmap;

/// Corpus divisor below which the allow-list drives lexical iteration.
///
/// PLACEHOLDER -- NOT YET MEASURED. Both branches are exact; a poor value is
/// a latency defect only.
pub const LEXICAL_ALLOW_LIST_DIVISOR: u64 = 64;

/// Exact lexical filter execution branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LexicalBranch {
    /// Enumerate allow-listed rows and seek terms to each row.
    AllowListDrive,
    /// Preserve pruning traversal and post-check every scored candidate.
    PostCheck,
}

/// Filtered lexical result and the branch that produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct LexicalSearchOutcome {
    /// Exact BM25 hits and counters.
    pub result: SearchResult,
    /// Actual exact filter branch.
    pub branch: LexicalBranch,
}

/// Typed lexical filter planning or scoring failure.
#[derive(Debug)]
pub enum LexicalFilterError {
    /// One bitmap per lexical segment is required.
    SegmentCount {
        /// Lexical segment count.
        expected: usize,
        /// Supplied bitmap count.
        actual: usize,
    },
    /// An allow-list row exceeded its segment's dense row space.
    RowOutOfRange {
        /// Segment ordinal.
        segment: usize,
        /// Invalid row.
        row: u32,
        /// Segment row count.
        row_count: u32,
    },
    /// The lexical scorer rejected the index or query.
    Index(IndexError),
}

impl std::fmt::Display for LexicalFilterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SegmentCount { expected, actual } => write!(
                formatter,
                "lexical filter supplied {actual} segment bitmaps, expected {expected}"
            ),
            Self::RowOutOfRange {
                segment,
                row,
                row_count,
            } => write!(
                formatter,
                "lexical filter row {row} is outside segment {segment} row count {row_count}"
            ),
            Self::Index(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LexicalFilterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Index(error) => Some(error),
            Self::SegmentCount { .. } | Self::RowOutOfRange { .. } => None,
        }
    }
}

impl From<IndexError> for LexicalFilterError {
    fn from(error: IndexError) -> Self {
        Self::Index(error)
    }
}

/// Plans and executes one exact lexical query over segment-local allow-lists.
pub fn search_lexical_filtered(
    index: &LexicalIndex,
    query: &TermQuery,
    k: usize,
    params: Bm25Params,
    allow_lists: &[DocBitmap],
    forced: Option<LexicalBranch>,
) -> Result<LexicalSearchOutcome, LexicalFilterError> {
    let references = allow_lists.iter().collect::<Vec<_>>();
    search_lexical_filtered_refs(index, query, k, params, &references, forced)
}

pub(crate) fn search_lexical_filtered_refs(
    index: &LexicalIndex,
    query: &TermQuery,
    k: usize,
    params: Bm25Params,
    allow_lists: &[&DocBitmap],
    forced: Option<LexicalBranch>,
) -> Result<LexicalSearchOutcome, LexicalFilterError> {
    validate_allow_lists(index, allow_lists)?;
    let allowed = allow_lists
        .iter()
        .map(|allow_list| allow_list.cardinality())
        .fold(0_u64, u64::saturating_add);
    let corpus = index.document_count();
    let branch = forced.unwrap_or_else(|| {
        if allowed.saturating_mul(LEXICAL_ALLOW_LIST_DIVISOR) <= corpus {
            LexicalBranch::AllowListDrive
        } else {
            LexicalBranch::PostCheck
        }
    });
    let result = match branch {
        LexicalBranch::AllowListDrive => {
            search_allow_list_driven(index, query, k, params, allow_lists)?
        }
        LexicalBranch::PostCheck => search_pruned_filtered(
            index,
            query,
            k,
            params,
            select_strategy(query.terms.len(), k),
            allow_lists,
        )?,
    };
    Ok(LexicalSearchOutcome { result, branch })
}

// Keep this loop's stack layout independent of the larger query caller.
#[inline(never)]
fn validate_allow_lists(
    index: &LexicalIndex,
    allow_lists: &[&DocBitmap],
) -> Result<(), LexicalFilterError> {
    if index.segments().len() != allow_lists.len() {
        return Err(LexicalFilterError::SegmentCount {
            expected: index.segments().len(),
            actual: allow_lists.len(),
        });
    }
    for (segment, (sealed, allow_list)) in index.segments().iter().zip(allow_lists).enumerate() {
        if let Some(row) = allow_list.iter().find(|row| *row >= sealed.row_count()) {
            return Err(LexicalFilterError::RowOutOfRange {
                segment,
                row,
                row_count: sealed.row_count(),
            });
        }
    }
    Ok(())
}
