//! Deterministic two-stage coarse selection and exact f32 rescoring.

use std::convert::Infallible;

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
    /// A per-segment candidate limit was zero.
    ZeroCandidateLimit,
    /// The requested frontier or its boundary ties exceeded the explicit limit.
    CandidateLimitExceeded {
        /// Candidate rows that would require exact scoring.
        requested: usize,
        /// Caller-supplied maximum per segment.
        maximum: usize,
    },
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
            Self::ZeroCandidateLimit => {
                formatter.write_str("rescore candidate limit must not be zero")
            }
            Self::CandidateLimitExceeded { requested, maximum } => write!(
                formatter,
                "rescore candidate count {requested} exceeds per-segment limit {maximum}"
            ),
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RescoreCheckError<E> {
    Rescore(RescoreError),
    Check(E),
}

impl<E> From<RescoreError> for RescoreCheckError<E> {
    fn from(error: RescoreError) -> Self {
        Self::Rescore(error)
    }
}

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
    match rescore_top_k_with_check(query, rows, dimension, pool, k, |_, _| {
        Ok::<(), Infallible>(())
    }) {
        Ok(result) => Ok(result),
        Err(RescoreCheckError::Rescore(error)) => Err(error),
        Err(RescoreCheckError::Check(error)) => match error {},
    }
}

pub(crate) fn rescore_top_k_with_check<E>(
    query: &[f32],
    rows: &[f32],
    dimension: usize,
    pool: RescorePool<'_>,
    k: usize,
    check: impl FnMut(usize, bool) -> Result<(), E>,
) -> Result<RescoreResult, RescoreCheckError<E>> {
    rescore_top_k_reusing(query, rows, dimension, pool, k, check, &mut NoScoreReuse)
}

/// The owner binds this cache to one query, physical source, revision, epoch
/// and metric. Values retain f64 precision through ranking.
pub(crate) trait ExactScoreReuse<E> {
    fn get(&mut self, row: usize) -> Result<Option<f64>, E>;
    fn insert(&mut self, row: usize, score: f64) -> Result<(), E>;
}

struct NoScoreReuse;

impl<E> ExactScoreReuse<E> for NoScoreReuse {
    fn get(&mut self, _row: usize) -> Result<Option<f64>, E> {
        Ok(None)
    }

    fn insert(&mut self, _row: usize, _score: f64) -> Result<(), E> {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rescore_top_k_reusing<E>(
    query: &[f32],
    rows: &[f32],
    dimension: usize,
    pool: RescorePool<'_>,
    k: usize,
    mut check: impl FnMut(usize, bool) -> Result<(), E>,
    reuse: &mut impl ExactScoreReuse<E>,
) -> Result<RescoreResult, RescoreCheckError<E>> {
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
    let mut batch: Vec<(usize, &[f32])> = Vec::with_capacity(ROW_BATCH);
    let mut scores: Vec<f64> = Vec::with_capacity(ROW_BATCH);
    let mut candidates_rescored = 0_usize;
    let mut position = 0;
    while position < candidate_count {
        let batch_end = position.saturating_add(ROW_BATCH).min(candidate_count);
        batch.clear();
        // A malformed pool is reported at the position that names the bad
        // row, exactly as the row-at-a-time loop did: the batch keeps only
        // the rows before it and the error is raised after their checks.
        let mut resolve_error = None;
        for batch_position in position..batch_end {
            match resolve_row(
                candidates.as_deref(),
                pool.row_indices,
                batch_position,
                row_count,
                rows,
                dimension,
            ) {
                Ok(resolved) => batch.push(resolved),
                Err(error) => {
                    resolve_error = Some(error);
                    break;
                }
            }
        }
        if pool.prefetch {
            let ahead_end = batch_end.saturating_add(ROW_BATCH).min(candidate_count);
            for ahead_position in batch_end..ahead_end {
                if let Ok(ahead) = selected_row_index(
                    candidates.as_deref(),
                    pool.row_indices,
                    ahead_position,
                    row_count,
                ) {
                    prefetch_f32_row(rows, ahead, dimension);
                }
            }
        }
        // Scoring is pure, so hoisting the whole batch ahead of the checks
        // is invisible: the checks, the non-finite rejection, and the pushes
        // below still run in strict position order.
        let mut prior = [None; ROW_BATCH];
        let mut missing = [(0, &[][..]); ROW_BATCH];
        let mut missing_count = 0;
        for (resolved, cached) in batch.iter().zip(&mut prior) {
            *cached = reuse.get(resolved.0).map_err(RescoreCheckError::Check)?;
            if cached.is_none() {
                *missing
                    .get_mut(missing_count)
                    .ok_or(RescoreError::ArithmeticOverflow)? = *resolved;
                missing_count += 1;
            }
        }
        let missing = missing
            .get(..missing_count)
            .ok_or(RescoreError::ArithmeticOverflow)?;
        score_batch(query, missing, pool.metric, &mut scores);
        candidates_rescored = candidates_rescored
            .checked_add(missing.len())
            .ok_or(RescoreError::ArithmeticOverflow)?;
        let mut new_scores = scores.iter();
        for (offset, ((row_id, _), cached)) in batch.iter().zip(prior).enumerate() {
            let row_id = *row_id;
            let score = match cached {
                Some(score) => score,
                None => *new_scores.next().ok_or(RescoreError::ArithmeticOverflow)?,
            };
            let scored_position = position.saturating_add(offset);
            check(row_id, scored_position.is_multiple_of(64)).map_err(RescoreCheckError::Check)?;
            if !score.is_finite() {
                return Err(RescoreError::NonFiniteExactScore { row_index: row_id }.into());
            }
            if cached.is_none() {
                reuse
                    .insert(row_id, score)
                    .map_err(RescoreCheckError::Check)?;
            }
            exact.push(RescoreHit {
                row_index: row_id,
                score,
            });
        }
        if let Some(error) = resolve_error {
            return Err(error.into());
        }
        position = batch_end;
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
    let rescore = candidates_rescored
        .checked_mul(dimension)
        .and_then(|values| values.checked_mul(std::mem::size_of::<f32>()))
        .ok_or(RescoreError::ArithmeticOverflow)?;
    let total = coarse
        .checked_add(rescore)
        .ok_or(RescoreError::ArithmeticOverflow)?;

    Ok(RescoreResult {
        hits: exact,
        candidates_rescored,
        bytes: SearchByteCounts {
            coarse,
            rescore,
            total,
        },
    })
}

/// Stream validated alive rows without a coarse-score or complete result buffer.
/// Scoring is pure; checks and the sink still execute in physical input order.
pub(crate) fn exact_squared_l2_with_sink<'a, E>(
    query: &[f32],
    mut rows: impl Iterator<Item = (usize, &'a [f32])>,
    mut check: impl FnMut(usize, bool) -> Result<(), E>,
    mut sink: impl FnMut(usize, f64) -> Result<(), E>,
) -> Result<usize, RescoreCheckError<E>> {
    let mut position = 0_usize;
    let (mut current, mut len) = next_exact_batch(&mut rows);
    loop {
        if len == 0 {
            return Ok(position);
        }
        // Read the next four eligible rows once, then reuse those references on
        // the next iteration. Lookahead and score scratch stay on the stack.
        let (next, next_len) = next_exact_batch(&mut rows);
        for (_, row) in next.iter().take(next_len) {
            prefetch_f32_row(row, 0, query.len());
        }
        let batch = current.get(..len).ok_or(RescoreError::ArithmeticOverflow)?;
        let mut scores = [0.0_f64; ROW_BATCH];
        if let [(_, first), (_, second), (_, third), (_, fourth)] = batch {
            let (a, b, c, d) = squared_l2_f64_x4(query, first, second, third, fourth);
            scores = [-a, -b, -c, -d];
        } else {
            for (score, (_, row)) in scores.iter_mut().zip(batch) {
                *score = -squared_l2_f64(query, row);
            }
        }
        for ((row_id, _), score) in batch.iter().zip(scores) {
            check(*row_id, position.is_multiple_of(64)).map_err(RescoreCheckError::Check)?;
            if !score.is_finite() {
                return Err(RescoreError::NonFiniteExactScore { row_index: *row_id }.into());
            }
            sink(*row_id, score).map_err(RescoreCheckError::Check)?;
            position += 1;
        }
        current = next;
        len = next_len;
    }
}

fn next_exact_batch<'a>(
    rows: &mut impl Iterator<Item = (usize, &'a [f32])>,
) -> ([(usize, &'a [f32]); ROW_BATCH], usize) {
    let mut batch: [(usize, &[f32]); ROW_BATCH] = [(0, &[]); ROW_BATCH];
    let mut len = 0;
    for slot in &mut batch {
        let Some(row) = rows.next() else {
            break;
        };
        *slot = row;
        len += 1;
    }
    (batch, len)
}

/// Rows scored in one call so that four independent f64 accumulator chains
/// overlap. One row's exact squared-L2 sum is a strictly sequential chain of
/// dependent `fadd`s that no compiler may reassociate, so a single row cannot
/// fill the floating-point pipeline; four rows can. Every row still
/// accumulates its own terms left to right, so a batched score is bit-identical
/// to the row-at-a-time definition.
const ROW_BATCH: usize = 4;

fn resolve_row<'a>(
    selected: Option<&[ScanCandidate]>,
    retained: Option<&[u32]>,
    position: usize,
    row_count: usize,
    rows: &'a [f32],
    dimension: usize,
) -> Result<(usize, &'a [f32]), RescoreError> {
    let row_id = selected_row_index(selected, retained, position, row_count)?;
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
    Ok((row_id, row))
}

fn score_batch(
    query: &[f32],
    batch: &[(usize, &[f32])],
    metric: RescoreMetric,
    scores: &mut Vec<f64>,
) {
    #[cfg(any(test, feature = "test-support"))]
    for (row, _) in batch {
        crate::graph::search::observe_exact_score(*row);
    }
    scores.clear();
    match metric {
        RescoreMetric::InnerProduct => {
            for (_, row) in batch {
                scores.push(f64::from(dot_f32(query, row)));
            }
        }
        RescoreMetric::SquaredL2 => {
            let mut rest = batch;
            while let [(_, first), (_, second), (_, third), (_, fourth), tail @ ..] = rest {
                let (first, second, third, fourth) =
                    squared_l2_f64_x4(query, first, second, third, fourth);
                scores.push(-first);
                scores.push(-second);
                scores.push(-third);
                scores.push(-fourth);
                rest = tail;
            }
            for (_, row) in rest {
                scores.push(-squared_l2_f64(query, row));
            }
        }
    }
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

/// The one exact squared-L2 definition. Every exact vector score in the
/// engine, including a hybrid cross-fill, comes from here so a bounded leg
/// and an unbounded one cannot disagree in the last bits.
pub(crate) fn squared_l2_f64(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let delta = f64::from(*left) - f64::from(*right);
            delta * delta
        })
        .sum()
}

/// Four rows of the one squared-L2 definition above, scored together.
///
/// Each row keeps its own accumulator and adds its own terms in the same
/// left-to-right order as `squared_l2_f64`, so every returned sum is bit
/// identical to that definition applied to that row. Nothing is reassociated
/// and nothing is contracted into a fused multiply-add: the only thing that
/// changes is that four independent dependency chains are in flight instead
/// of one, which is what lets the floating-point pipeline issue more than one
/// add per chain latency.
pub(crate) fn squared_l2_f64_x4(
    query: &[f32],
    first: &[f32],
    second: &[f32],
    third: &[f32],
    fourth: &[f32],
) -> (f64, f64, f64, f64) {
    let mut first_sum = 0.0_f64;
    let mut second_sum = 0.0_f64;
    let mut third_sum = 0.0_f64;
    let mut fourth_sum = 0.0_f64;
    for ((((query, first), second), third), fourth) in
        query.iter().zip(first).zip(second).zip(third).zip(fourth)
    {
        let query = f64::from(*query);
        let first = query - f64::from(*first);
        let second = query - f64::from(*second);
        let third = query - f64::from(*third);
        let fourth = query - f64::from(*fourth);
        first_sum += first * first;
        second_sum += second * second;
        third_sum += third * third;
        fourth_sum += fourth * fourth;
    }
    (first_sum, second_sum, third_sum, fourth_sum)
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

#[cfg(test)]
mod exact_sink_tests {
    use super::*;

    #[test]
    fn astra_04_sink_checks_precede_nonfinite_and_count_eligible_rows() {
        let mut values = [1.0_f32; 67];
        if let Some(value) = values.get_mut(65) {
            *value = f32::NAN;
        }
        for cancel in [true, false] {
            let mut checks = Vec::new();
            let mut emitted = Vec::new();
            let result = exact_squared_l2_with_sink(
                &[0.0],
                values.chunks_exact(1).enumerate(),
                |row, checkpoint| {
                    checks.push((row, checkpoint));
                    if cancel && row == 65 { Err(65) } else { Ok(()) }
                },
                |row, score| {
                    emitted.push((row, score.to_bits()));
                    Ok(())
                },
            );
            if cancel {
                assert!(matches!(result, Err(RescoreCheckError::Check(65))));
            } else {
                assert!(matches!(
                    result,
                    Err(RescoreCheckError::Rescore(
                        RescoreError::NonFiniteExactScore { row_index: 65 }
                    ))
                ));
            }
            assert_eq!(
                checks,
                (0..66).map(|row| (row, row % 64 == 0)).collect::<Vec<_>>()
            );
            assert_eq!(
                emitted,
                (0..65)
                    .map(|row| (row, (-1.0_f64).to_bits()))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod reuse_tests {
    use super::*;

    #[derive(Default)]
    struct Cache(std::collections::BTreeMap<usize, f64>);

    impl ExactScoreReuse<usize> for Cache {
        fn get(&mut self, row: usize) -> Result<Option<f64>, usize> {
            Ok(self.0.get(&row).copied())
        }
        fn insert(&mut self, row: usize, score: f64) -> Result<(), usize> {
            assert!(
                self.0.insert(row, score).is_none(),
                "only new rows are retained"
            );
            Ok(())
        }
    }

    #[test]
    fn astra_09_reused_rescores_preserve_f64_order_checks_and_new_row_work() {
        let rows = [
            1.0_f32,
            2.0_f32.powi(-13),
            1.0,
            0.0,
            2.0,
            0.0,
            3.0,
            0.0,
            4.0,
            0.0,
            5.0,
            0.0,
        ];
        let query = [0.0_f32; 2];
        let mut cache = Cache::default();
        for (ids, expected_calls) in [
            (&[0_u32, 1, 2, 3][..], 4),
            (&[0, 1, 2, 3, 4, 5][..], 2),
            (&[0, 1, 2, 3, 4, 5][..], 0),
        ] {
            let coarse = vec![0.0; ids.len()];
            let pool = RescorePool::retained(ids, &coarse, RescoreMetric::SquaredL2, ids.len(), 13);
            let mut checked = Vec::new();
            let result = rescore_top_k_reusing(
                &query,
                &rows,
                2,
                pool,
                2,
                |row, _| {
                    checked.push(row);
                    Ok(())
                },
                &mut cache,
            )
            .expect("valid retained pool");
            assert_eq!(
                checked,
                ids.iter().map(|row| *row as usize).collect::<Vec<_>>()
            );
            assert_eq!(result.candidates_rescored, expected_calls);
            assert_eq!(result.bytes.rescore, expected_calls * 2 * 4);
            assert_eq!(
                result
                    .hits
                    .iter()
                    .map(|hit| hit.row_index)
                    .collect::<Vec<_>>(),
                vec![1, 0]
            );
            assert_eq!(result.hits[0].score, -1.0);
            assert_eq!(result.hits[1].score, -1.0 - 2.0_f64.powi(-26));
            assert_eq!(
                (result.hits[0].score as f32).to_bits(),
                (result.hits[1].score as f32).to_bits(),
                "a premature f32 cache would choose the wrong row on the next round"
            );
            let fresh = rescore_top_k(&query, &rows, 2, pool, 2).expect("fresh control");
            assert_eq!(fresh.hits, result.hits);
        }
        let cancelled = rescore_top_k_reusing(
            &query,
            &rows,
            2,
            RescorePool::retained(&[0, 1], &[0.0, 0.0], RescoreMetric::SquaredL2, 2, 13),
            1,
            |row, _| if row == 1 { Err(row) } else { Ok(()) },
            &mut cache,
        );
        assert!(
            matches!(cancelled, Err(RescoreCheckError::Check(1))),
            "cached rows still run checks"
        );
        let mut checked = Vec::new();
        let malformed = rescore_top_k_reusing(
            &query,
            &rows,
            2,
            RescorePool::retained(&[0, 99], &[0.0, 0.0], RescoreMetric::SquaredL2, 2, 13),
            1,
            |row, _| {
                checked.push(row);
                Ok(())
            },
            &mut cache,
        );
        assert_eq!(checked, vec![0]);
        assert!(matches!(
            malformed,
            Err(RescoreCheckError::Rescore(
                RescoreError::CandidateRowOutOfRange {
                    position: 1,
                    row_index: 99,
                    row_count: 6
                }
            ))
        ));
    }
}
