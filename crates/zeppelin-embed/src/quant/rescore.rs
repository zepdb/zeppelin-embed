//! Deterministic two-stage coarse selection and exact f32 rescoring.

use crate::kernels::dot_f32;
use crate::scan::ScanCandidate;
use crate::scan::topk::BoundedTopK;

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
    /// Exact larger-is-better score under the requested full-precision metric.
    pub score: f64,
}

/// Full-precision ranking applied to the retained coarse pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RescoreMetric {
    /// Runtime-dispatched task-03 f32 inner product.
    InnerProduct,
    /// Negative squared-L2 over the stored f32 row; larger remains better.
    SquaredL2,
}

#[derive(Clone, Copy, Debug)]
enum PoolWidth {
    DenseOversample(usize),
    Retained,
}

/// Describes either dense coarse scores or a graph traversal's retained sparse pool.
#[derive(Clone, Copy, Debug)]
pub struct RescorePool<'a> {
    row_indices: Option<&'a [u32]>,
    coarse_scores: &'a [f32],
    width: PoolWidth,
    metric: RescoreMetric,
    coarse_rows_touched: usize,
    coarse_bytes_per_row: usize,
    prefetch: bool,
}

impl<'a> RescorePool<'a> {
    /// Selects `k * oversample` rows from one dense coarse score per stored row.
    #[must_use]
    pub const fn dense(
        coarse_scores: &'a [f32],
        oversample: usize,
        coarse_bytes_per_row: usize,
    ) -> Self {
        Self {
            row_indices: None,
            coarse_scores,
            width: PoolWidth::DenseOversample(oversample),
            metric: RescoreMetric::InnerProduct,
            coarse_rows_touched: coarse_scores.len(),
            coarse_bytes_per_row,
            prefetch: false,
        }
    }

    /// Rescores every row retained by a sparse graph traversal.
    #[must_use]
    pub const fn retained(
        row_indices: &'a [u32],
        coarse_scores: &'a [f32],
        metric: RescoreMetric,
        coarse_rows_touched: usize,
        coarse_bytes_per_row: usize,
    ) -> Self {
        Self {
            row_indices: Some(row_indices),
            coarse_scores,
            width: PoolWidth::Retained,
            metric,
            coarse_rows_touched,
            coarse_bytes_per_row,
            prefetch: false,
        }
    }

    /// Selects whether exact-row reads issue the platform prefetch hint.
    #[must_use]
    pub const fn with_prefetch(mut self, prefetch: bool) -> Self {
        self.prefetch = prefetch;
        self
    }
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
    /// A retained graph pool supplied a different number of ids and scores.
    CandidateRowCount {
        /// Number of retained coarse scores.
        expected: usize,
        /// Number of retained row ids.
        actual: usize,
    },
    /// A retained graph-pool row id was outside the f32 matrix.
    CandidateRowOutOfRange {
        /// Position inside the retained pool.
        position: usize,
        /// Invalid stored row index.
        row_index: usize,
        /// Available stored rows.
        row_count: usize,
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
    /// Exact f32-row scoring produced NaN or infinity.
    NonFiniteExactScore {
        /// Stored row whose exact score was invalid.
        row_index: usize,
    },
    /// The supplied pool held fewer than `k` rows.
    InsufficientCandidates {
        /// Requested result count.
        k: usize,
        /// Rows available for exact rescore.
        candidates: usize,
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
            Self::CandidateRowCount { expected, actual } => write!(
                formatter,
                "rescore retained row id count mismatch: expected {expected}, got {actual}"
            ),
            Self::CandidateRowOutOfRange {
                position,
                row_index,
                row_count,
            } => write!(
                formatter,
                "rescore retained row at position {position} is {row_index}, outside {row_count} rows"
            ),
            Self::ZeroK => formatter.write_str("rescore top-k must not be zero"),
            Self::ZeroOversample => formatter.write_str("rescore oversample must not be zero"),
            Self::NonFiniteCoarseScore { index } => {
                write!(
                    formatter,
                    "rescore coarse score is non-finite at row {index}"
                )
            }
            Self::NonFiniteExactScore { row_index } => {
                write!(
                    formatter,
                    "rescore exact score is non-finite at row {row_index}"
                )
            }
            Self::InsufficientCandidates { k, candidates } => write!(
                formatter,
                "rescore pool has {candidates} candidates, fewer than requested top-k {k}"
            ),
            Self::ArithmeticOverflow => formatter.write_str("rescore byte count overflowed usize"),
        }
    }
}

impl std::error::Error for RescoreError {}

fn validate_rescore_request(
    query: &[f32],
    rows: &[f32],
    dimension: usize,
    pool: &RescorePool<'_>,
    k: usize,
) -> Result<(usize, usize), RescoreError> {
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
    match pool.row_indices {
        None if pool.coarse_scores.len() != row_count => {
            return Err(RescoreError::CoarseScoreCount {
                expected: row_count,
                actual: pool.coarse_scores.len(),
            });
        }
        Some(row_indices) if row_indices.len() != pool.coarse_scores.len() => {
            return Err(RescoreError::CandidateRowCount {
                expected: pool.coarse_scores.len(),
                actual: row_indices.len(),
            });
        }
        None | Some(_) => {}
    }
    if k == 0 {
        return Err(RescoreError::ZeroK);
    }
    let candidate_count = match pool.width {
        PoolWidth::DenseOversample(0) => return Err(RescoreError::ZeroOversample),
        PoolWidth::DenseOversample(oversample) => {
            let requested = k
                .checked_mul(oversample)
                .ok_or(RescoreError::ArithmeticOverflow)?;
            pool.coarse_scores.len().min(requested)
        }
        PoolWidth::Retained => pool.coarse_scores.len(),
    };
    if candidate_count < k {
        return Err(RescoreError::InsufficientCandidates {
            k,
            candidates: candidate_count,
        });
    }
    if let Some((index, _)) = pool
        .coarse_scores
        .iter()
        .enumerate()
        .find(|(_, score)| !score.is_finite())
    {
        return Err(RescoreError::NonFiniteCoarseScore { index });
    }
    Ok((row_count, candidate_count))
}

/// Selects a coarse frontier and exactly rescores its best `k` rows.
///
/// `rows` is a contiguous row-major f32 matrix. A dense pool contains one
/// larger-is-better score per row and retains `min(row_count, k * oversample)`.
/// A sparse graph pool names the already-retained rows and rescores all of them.
/// Row index is the deterministic tie-breaker in both modes.
///
/// Byte counters measure stored row data: the coarse stage touches
/// `coarse_rows_touched * coarse_bytes_per_row`; the exact stage touches
/// `frontier * dimension * 4`. Query bytes and output metadata are deliberately
/// excluded because they are shared across schemes and are not corpus-row I/O.
///
/// # Errors
///
/// Returns [`RescoreError`] for invalid shapes, zero controls, non-finite
/// coarse scores, or arithmetic overflow.
pub fn rescore_top_k(
    query: &[f32],
    rows: &[f32],
    dimension: usize,
    pool: RescorePool<'_>,
    k: usize,
) -> Result<RescoreResult, RescoreError> {
    let (row_count, candidate_count) = validate_rescore_request(query, rows, dimension, &pool, k)?;

    let candidates = match pool.width {
        PoolWidth::DenseOversample(_) => {
            let mut coarse_top_k = BoundedTopK::new(candidate_count);
            for (row_id, &score) in pool.coarse_scores.iter().enumerate() {
                coarse_top_k.push(ScanCandidate { row_id, score });
            }
            // Dense rescore consumes exactly `candidate_count` positions, so
            // this physical drain deliberately discards boundary extras.
            Some(coarse_top_k.into_sorted())
        }
        PoolWidth::Retained => None,
    };

    let mut exact = Vec::with_capacity(candidate_count);
    for position in 0..candidate_count {
        let row_id =
            selected_row_index(candidates.as_deref(), pool.row_indices, position, row_count)?;
        let ahead_position = position.saturating_add(4);
        if pool.prefetch && ahead_position < candidate_count {
            let ahead = selected_row_index(
                candidates.as_deref(),
                pool.row_indices,
                ahead_position,
                row_count,
            )?;
            prefetch_f32_row(rows, ahead, dimension);
        }
        let start = row_id
            .checked_mul(dimension)
            .ok_or(RescoreError::ArithmeticOverflow)?;
        let end = start
            .checked_add(dimension)
            .ok_or(RescoreError::ArithmeticOverflow)?;
        let row = rows.get(start..end).ok_or(RescoreError::RowDataLength {
            dimension,
            actual: rows.len(),
        })?;
        let score = match pool.metric {
            RescoreMetric::InnerProduct => f64::from(dot_f32(query, row)),
            RescoreMetric::SquaredL2 => -squared_l2_f64(query, row),
        };
        if !score.is_finite() {
            return Err(RescoreError::NonFiniteExactScore { row_index: row_id });
        }
        exact.push(RescoreHit {
            row_index: row_id,
            score,
        });
    }
    if exact.len() > k {
        let _ = exact.select_nth_unstable_by(k, rescore_hit_best_first);
        exact.truncate(k);
    }
    exact.sort_unstable_by(rescore_hit_best_first);

    let coarse = pool
        .coarse_rows_touched
        .checked_mul(pool.coarse_bytes_per_row)
        .ok_or(RescoreError::ArithmeticOverflow)?;
    let rescore = candidate_count
        .checked_mul(dimension)
        .and_then(|values| values.checked_mul(std::mem::size_of::<f32>()))
        .ok_or(RescoreError::ArithmeticOverflow)?;
    let total = coarse
        .checked_add(rescore)
        .ok_or(RescoreError::ArithmeticOverflow)?;

    Ok(RescoreResult {
        hits: exact,
        candidates_rescored: candidate_count,
        bytes: SearchByteCounts {
            coarse,
            rescore,
            total,
        },
    })
}

fn selected_row_index(
    selected: Option<&[ScanCandidate]>,
    retained: Option<&[u32]>,
    position: usize,
    row_count: usize,
) -> Result<usize, RescoreError> {
    match selected {
        Some(selected) => selected
            .get(position)
            .map(|candidate| candidate.row_id)
            .ok_or(RescoreError::ArithmeticOverflow),
        None => retained_row_index(retained, position, row_count),
    }
}

fn rescore_hit_best_first(left: &RescoreHit, right: &RescoreHit) -> std::cmp::Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.row_index.cmp(&right.row_index))
}

fn retained_row_index(
    row_indices: Option<&[u32]>,
    position: usize,
    row_count: usize,
) -> Result<usize, RescoreError> {
    let row_index = match row_indices {
        None => position,
        Some(row_indices) => usize::try_from(*row_indices.get(position).ok_or(
            RescoreError::CandidateRowCount {
                expected: position.saturating_add(1),
                actual: row_indices.len(),
            },
        )?)
        .map_err(|_| RescoreError::ArithmeticOverflow)?,
    };
    if row_index >= row_count {
        return Err(RescoreError::CandidateRowOutOfRange {
            position,
            row_index,
            row_count,
        });
    }
    Ok(row_index)
}

fn squared_l2_f64(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let delta = f64::from(*left) - f64::from(*right);
            delta * delta
        })
        .sum()
}

fn prefetch_f32_row(base: &[f32], row_id: usize, dimensions: usize) {
    let Some(start) = row_id.checked_mul(dimensions) else {
        return;
    };
    let Some(value) = base.get(start) else {
        return;
    };
    let address = std::ptr::from_ref(value);
    #[cfg(target_arch = "aarch64")]
    {
        const CACHE_LINE_BYTES: usize = 128;
        let Some(row_bytes) = dimensions.checked_mul(std::mem::size_of::<f32>()) else {
            return;
        };
        let cache_line_count = row_bytes.div_ceil(CACHE_LINE_BYTES);
        for cache_line in 0..cache_line_count {
            let Some(byte_offset) = cache_line.checked_mul(CACHE_LINE_BYTES) else {
                return;
            };
            let line_address = address.cast::<u8>().wrapping_add(byte_offset);
            // SAFETY: the address points into the live immutable f32 rescore mapping.
            unsafe {
                std::arch::asm!(
                    "prfm pldl1keep, [{line_address}]",
                    line_address = in(reg) line_address,
                    options(readonly, nostack)
                );
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = address;
}
