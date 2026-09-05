//! Explicit quantized candidate selection followed by exact selected-row scores.

/// Controls the candidate frontier of each active or sealed segment.
///
/// The frontier is `min(eligible_rows, k * oversample)`, plus all coarse-score
/// boundary ties. The limit bounds exact row reads per segment and vector
/// producer invocation, including ties; exceeding it is a typed error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanRescoreOptions {
    oversample: usize,
    max_candidates_per_segment: usize,
}

impl ScanRescoreOptions {
    /// Creates explicit controls; neither value may be zero.
    pub const fn new(
        oversample: usize,
        max_candidates_per_segment: usize,
    ) -> Result<Self, crate::quant::RescoreError> {
        if oversample == 0 {
            return Err(crate::quant::RescoreError::ZeroOversample);
        }
        if max_candidates_per_segment == 0 {
            return Err(crate::quant::RescoreError::ZeroCandidateLimit);
        }
        Ok(Self {
            oversample,
            max_candidates_per_segment,
        })
    }

    /// Requested coarse frontier multiplier before boundary ties.
    #[must_use]
    pub const fn oversample(self) -> usize {
        self.oversample
    }

    /// Hard exact-row-read limit per segment and vector producer invocation.
    #[must_use]
    pub const fn max_candidates_per_segment(self) -> usize {
        self.max_candidates_per_segment
    }

    pub(crate) fn frontier(self, k: usize, eligible: u64) -> Result<usize, super::QueryError> {
        let requested = k
            .checked_mul(self.oversample)
            .ok_or_else(|| rescore_error(crate::quant::RescoreError::ArithmeticOverflow))?;
        let eligible = usize::try_from(eligible)
            .map_err(|_| rescore_error(crate::quant::RescoreError::ArithmeticOverflow))?;
        let frontier = requested.min(eligible);
        self.check_count(frontier)?;
        Ok(frontier)
    }

    fn check_count(self, requested: usize) -> Result<(), super::QueryError> {
        if requested > self.max_candidates_per_segment {
            return Err(rescore_error(
                crate::quant::RescoreError::CandidateLimitExceeded {
                    requested,
                    maximum: self.max_candidates_per_segment,
                },
            ));
        }
        Ok(())
    }
}

fn rescore_error(error: crate::quant::RescoreError) -> super::QueryError {
    super::QueryError::Scan(crate::scan::ScanError::Rescore(error))
}

pub(crate) fn accumulate(
    total: &mut crate::diag::ScanRescoreCounters,
    next: crate::diag::ScanRescoreCounters,
) -> Result<(), super::QueryError> {
    let add = |a: u64, b: u64| {
        a.checked_add(b).ok_or(super::QueryError::Scan(
            crate::scan::ScanError::ArithmeticOverflow,
        ))
    };
    total.coarse_rows = add(total.coarse_rows, next.coarse_rows)?;
    total.eligible_rows = add(total.eligible_rows, next.eligible_rows)?;
    total.candidates_rescored = add(total.candidates_rescored, next.candidates_rescored)?;
    total.coarse_bytes = add(total.coarse_bytes, next.coarse_bytes)?;
    total.rescore_bytes = add(total.rescore_bytes, next.rescore_bytes)?;
    Ok(())
}

/// Local candidate storage accepted by the shared selected-row scorer.
pub(crate) trait RescoreCandidate {
    fn row_id(&self) -> usize;
    fn score(&self) -> f32;
    fn set_score(&mut self, score: f32);
}

impl RescoreCandidate for crate::scan::ScanCandidate {
    fn row_id(&self) -> usize {
        self.row_id
    }
    fn score(&self) -> f32 {
        self.score
    }
    fn set_score(&mut self, score: f32) {
        self.score = score;
    }
}

/// Reuses the shared exact scorer on only the selected, validated row slices.
/// Candidate storage is updated in place; the caller owns its charge through
/// the subsequent identity join and merge.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rescore<C: RescoreCandidate>(
    candidates: &mut Vec<C>,
    stats: &mut crate::scan::ScanStats,
    vectors: &[f32],
    query: &[f32],
    k: usize,
    eligible: u64,
    options: ScanRescoreOptions,
    cancellation: &super::QueryCancellation<'_>,
    accounting: &std::sync::Arc<super::stats::Accounting>,
    memory: &mut super::ExactScanMemory,
) -> Result<(crate::diag::ScanRescoreCounters, bool, Option<f32>), super::QueryError> {
    use super::{
        QueryError, StoreError,
        stats::{Accounted, AllocationComponent},
    };
    use crate::scan::ScanError;
    cancellation.check_graph().map_err(super::map_scan_error)?;
    let dimension = query.len();
    if dimension == 0 {
        return Err(QueryError::Scan(ScanError::ZeroDimension));
    }
    if !vectors.len().is_multiple_of(dimension) {
        return Err(QueryError::Scan(ScanError::RowDataLength {
            dimension,
            actual: vectors.len(),
        }));
    }
    let count = candidates.len();
    options.check_count(count)?;
    memory
        .candidates
        .set(
            candidates
                .capacity()
                .checked_mul(std::mem::size_of::<C>())
                .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?,
        )
        .map_err(QueryError::Store)?;
    let mut rows = Accounted::try_with_capacity(accounting, count, AllocationComponent::Temporary)
        .map_err(QueryError::Store)?;
    for (position, candidate) in candidates.iter().enumerate() {
        if position.is_multiple_of(64) {
            cancellation.check_graph().map_err(super::map_scan_error)?;
        }
        let start = candidate
            .row_id()
            .checked_mul(dimension)
            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
        let end = start
            .checked_add(dimension)
            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
        let vector = vectors.get(start..end).ok_or_else(|| {
            rescore_error(crate::quant::RescoreError::CandidateRowOutOfRange {
                position,
                row_index: candidate.row_id(),
                row_count: vectors.len() / dimension,
            })
        })?;
        rows.push((candidate.row_id(), vector))
            .map_err(QueryError::Store)?;
    }
    let mut position = 0;
    let mut worst: Option<f32> = None;
    let mut narrowing: Option<(usize, f64)> = None;
    let scored = crate::quant::exact_squared_l2_with_sink(
        query,
        rows.iter().copied(),
        |_, checkpoint| {
            if checkpoint {
                cancellation.check_graph().map_err(super::map_scan_error)?;
            }
            Ok(())
        },
        |row_id, exact| {
            let candidate = candidates
                .get_mut(position)
                .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
            position += 1;
            let score = exact as f32;
            if !score.is_finite() {
                if narrowing.is_none_or(|(row, previous)| {
                    exact.total_cmp(&previous).is_gt()
                        || (exact.total_cmp(&previous).is_eq() && row_id < row)
                }) {
                    narrowing = Some((row_id, exact));
                }
            } else {
                candidate.set_score(score);
                if worst.is_none_or(|previous| score.total_cmp(&previous).is_lt()) {
                    worst = Some(score);
                }
            }
            Ok(())
        },
    )
    .map_err(|error| match error {
        crate::quant::RescoreCheckError::Rescore(error) => super::map_l2_rescore_error(error),
        crate::quant::RescoreCheckError::Check(error) => error,
    })?;
    cancellation.check_graph().map_err(super::map_scan_error)?;
    if let Some((row_id, _)) = narrowing {
        return Err(QueryError::Scan(ScanError::NonFiniteScore { row_id }));
    }
    let scored =
        u64::try_from(scored).map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let coordinates = scored
        .checked_mul(dimension as u64)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let bytes = coordinates
        .checked_mul(4)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let counters = crate::diag::ScanRescoreCounters {
        coarse_rows: stats.dims_touched / dimension as u64,
        eligible_rows: eligible,
        candidates_rescored: scored,
        coarse_bytes: stats.bytes_read,
        rescore_bytes: bytes,
    };
    stats.dims_touched = stats
        .dims_touched
        .checked_add(coordinates)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    stats.bytes_read = stats
        .bytes_read
        .checked_add(bytes)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let caller = std::thread::current().id();
    if scored > 0 && !stats.worker_thread_ids.contains(&caller) {
        let needed = stats
            .worker_thread_ids
            .len()
            .checked_add(1)
            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
        memory
            .worker_ids
            .set(
                needed
                    .max(stats.worker_thread_ids.capacity())
                    .checked_mul(std::mem::size_of::<std::thread::ThreadId>())
                    .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?,
            )
            .map_err(QueryError::Store)?;
        stats.worker_thread_ids.try_reserve_exact(1).map_err(|_| {
            QueryError::Store(StoreError::AllocationFailed {
                needed: memory.worker_ids.bytes(),
                component: "rescore worker IDs",
            })
        })?;
        stats.worker_thread_ids.push(caller);
    }
    stats.threads_used = stats.worker_thread_ids.len();
    let exhaustive = scored == eligible;

    candidates.sort_unstable_by(|a, b| {
        b.score()
            .total_cmp(&a.score())
            .then(a.row_id().cmp(&b.row_id()))
    });
    crate::scan::truncate_to_k_with_score_ties(candidates, k, |candidate| candidate.score());
    Ok((counters, exhaustive, if exhaustive { worst } else { None }))
}
