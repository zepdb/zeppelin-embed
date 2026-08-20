//! Deterministic two-stage coarse selection and exact f32 rescoring.

use crate::kernels::dot_f32;

/// Stored-data bytes touched by one two-stage query.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchByteCounts {
    /// Encoded row bytes and per-row factor bytes read by the coarse scan.
    pub coarse: usize,
    /// Full-precision row bytes read for exact candidate rescoring.
    pub rescore: usize,
    total: usize,
}

impl SearchByteCounts {
    /// Returns coarse plus exact-rescore bytes.
    #[must_use]
    pub const fn total(self) -> usize {
        self.total
    }
}

/// One exact hit returned from the rescore stage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RescoreHit {
    /// Zero-based row position in the supplied matrix.
    pub row_index: usize,
    /// Exact task-03 f32 dot-product score.
    pub score: f32,
}

/// Exact top-k hits, candidate count, and byte accounting for both stages.
#[derive(Clone, Debug, PartialEq)]
pub struct RescoreResult {
    /// Exact hits ordered by descending score and then ascending row index.
    pub hits: Vec<RescoreHit>,
    /// Number of coarse candidates read from full-precision storage.
    pub candidates_rescored: usize,
    /// Stored-data byte counters for this query.
    pub bytes: SearchByteCounts,
}

/// Typed failure from the two-stage rescore helper.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RescoreError {
    /// The declared dimension was zero.
    ZeroDimension,
    /// The query width differed from the declared row width.
    QueryDimension {
        /// Declared row width.
        expected: usize,
        /// Supplied query width.
        actual: usize,
    },
    /// The flat row matrix was not an exact multiple of the dimension.
    RowDataLength {
        /// Declared row width.
        dimension: usize,
        /// Supplied scalar count.
        actual: usize,
    },
    /// The coarse score count differed from the matrix row count.
    CoarseScoreCount {
        /// Number of matrix rows.
        expected: usize,
        /// Supplied coarse score count.
        actual: usize,
    },
    /// Exact top-k must request at least one hit.
    ZeroK,
    /// Oversampling must retain at least `k` coarse candidates.
    ZeroOversample,
    /// A coarse score was NaN or infinite.
    NonFiniteCoarseScore {
        /// Zero-based row position of the invalid score.
        index: usize,
    },
    /// A candidate or byte-count multiplication overflowed `usize`.
    ArithmeticOverflow,
}

impl std::fmt::Display for RescoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDimension => formatter.write_str("rescore dimension must not be zero"),
            Self::QueryDimension { expected, actual } => write!(
                formatter,
                "rescore query dimension mismatch: expected {expected}, got {actual}"
            ),
            Self::RowDataLength { dimension, actual } => write!(
                formatter,
                "rescore row data length {actual} is not divisible by dimension {dimension}"
            ),
            Self::CoarseScoreCount { expected, actual } => write!(
                formatter,
                "rescore coarse score count mismatch: expected {expected}, got {actual}"
            ),
            Self::ZeroK => formatter.write_str("rescore top-k must not be zero"),
            Self::ZeroOversample => formatter.write_str("rescore oversample must not be zero"),
            Self::NonFiniteCoarseScore { index } => {
                write!(
                    formatter,
                    "rescore coarse score is non-finite at row {index}"
                )
            }
            Self::ArithmeticOverflow => formatter.write_str("rescore byte count overflowed usize"),
        }
    }
}

impl std::error::Error for RescoreError {}

/// Selects a coarse frontier and exactly rescores its best `k` rows.
///
/// `rows` is a contiguous row-major f32 matrix. `coarse_scores` contains one
/// larger-is-better score per row. The frontier has
/// `min(row_count, k * oversample)` entries, with row index as the deterministic
/// tie-breaker. Exact rescoring calls the runtime-dispatched task-03 f32 kernel
/// once per retained row.
///
/// Byte counters measure stored row data: the coarse stage touches
/// `row_count * coarse_bytes_per_row`; the exact stage touches
/// `frontier * dimension * 4`. Query bytes and output metadata are deliberately
/// excluded because they are shared across schemes and are not corpus-row I/O.
///
/// # Errors
///
/// Returns [`RescoreError`] for invalid shapes, zero controls, non-finite
/// coarse scores, or arithmetic overflow.
#[allow(clippy::too_many_arguments)]
pub fn rescore_top_k(
    query: &[f32],
    rows: &[f32],
    dimension: usize,
    coarse_scores: &[f32],
    k: usize,
    oversample: usize,
    coarse_bytes_per_row: usize,
) -> Result<RescoreResult, RescoreError> {
    if dimension == 0 {
        return Err(RescoreError::ZeroDimension);
    }
    if query.len() != dimension {
        return Err(RescoreError::QueryDimension {
            expected: dimension,
            actual: query.len(),
        });
    }
    if !rows.len().is_multiple_of(dimension) {
        return Err(RescoreError::RowDataLength {
            dimension,
            actual: rows.len(),
        });
    }
    let row_count = rows.len() / dimension;
    if coarse_scores.len() != row_count {
        return Err(RescoreError::CoarseScoreCount {
            expected: row_count,
            actual: coarse_scores.len(),
        });
    }
    if k == 0 {
        return Err(RescoreError::ZeroK);
    }
    if oversample == 0 {
        return Err(RescoreError::ZeroOversample);
    }
    if let Some((index, _)) = coarse_scores
        .iter()
        .enumerate()
        .find(|(_, score)| !score.is_finite())
    {
        return Err(RescoreError::NonFiniteCoarseScore { index });
    }

    let requested = k
        .checked_mul(oversample)
        .ok_or(RescoreError::ArithmeticOverflow)?;
    let candidate_count = row_count.min(requested);
    let mut candidates = coarse_scores
        .iter()
        .copied()
        .enumerate()
        .collect::<Vec<_>>();
    candidates.sort_unstable_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    candidates.truncate(candidate_count);

    let mut hits = Vec::with_capacity(candidate_count);
    for (row_index, _) in candidates {
        let start = row_index
            .checked_mul(dimension)
            .ok_or(RescoreError::ArithmeticOverflow)?;
        let end = start
            .checked_add(dimension)
            .ok_or(RescoreError::ArithmeticOverflow)?;
        let row = rows.get(start..end).ok_or(RescoreError::RowDataLength {
            dimension,
            actual: rows.len(),
        })?;
        hits.push(RescoreHit {
            row_index,
            score: dot_f32(query, row),
        });
    }
    hits.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.row_index.cmp(&right.row_index))
    });
    hits.truncate(k.min(hits.len()));

    let coarse = row_count
        .checked_mul(coarse_bytes_per_row)
        .ok_or(RescoreError::ArithmeticOverflow)?;
    let rescore = candidate_count
        .checked_mul(dimension)
        .and_then(|values| values.checked_mul(std::mem::size_of::<f32>()))
        .ok_or(RescoreError::ArithmeticOverflow)?;
    let total = coarse
        .checked_add(rescore)
        .ok_or(RescoreError::ArithmeticOverflow)?;

    Ok(RescoreResult {
        hits,
        candidates_rescored: candidate_count,
        bytes: SearchByteCounts {
            coarse,
            rescore,
            total,
        },
    })
}
