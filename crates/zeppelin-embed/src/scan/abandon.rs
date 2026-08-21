//! Conservative block-granular abandonment over dimension-major PDX blocks.
//!
//! Dimensions use their fixed stored coordinate order. A future segment may
//! supply a cheap variance-statistic permutation, but no PCA rotation or other
//! trained artifact is part of this scan path.

use std::ops::Range;

use crate::kernels;
use crate::quant::{Bit4Factors, Bit4Query, est_dot_bit4};

use super::pdx::{Bit4ColumnExtrema, PdxMatrix};
use super::topk::BoundedTopK;
use super::{PartitionScan, ScanCandidate, ScanError, ScanQuery, ScanRequest, ScanRows};

pub(crate) const BASELINE_DIMENSIONS_PER_SLAB: usize = 32;
const STRATIFIED_SAMPLE_ROWS: usize = 512;
const LEADING_BLOCK_SAMPLE_ROWS: usize = 16;
const F32_DOT_BACKWARD_ERROR_EPSILON: f64 = 1.0e-5;

#[derive(Clone, Copy, Debug)]
struct Bit4FactorExtrema {
    minimum: f64,
    maximum: f64,
}

#[derive(Debug)]
enum SeedKind {
    F32,
    Bit4 {
        factor_extrema: Vec<Bit4FactorExtrema>,
    },
}

#[derive(Debug)]
pub(crate) struct AbandonmentSeed {
    pub(crate) dims_touched: u64,
    pub(crate) bytes_read: u64,
    threshold: Option<f32>,
    kind: SeedKind,
}

pub(crate) fn prepare_f32_pdx_seed(
    query: &[f32],
    matrix: &PdxMatrix,
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
) -> Result<AbandonmentSeed, ScanError> {
    validate_f32_pdx(query, matrix)?;
    let sampled_rows = sampled_rows(matrix, k)?;
    let mut selected = BoundedTopK::new(k.min(matrix.row_count()));
    let mut dims_touched = 0_u64;
    let mut bytes_read = 0_u64;
    for row_id in sampled_rows {
        if !row_is_allowed(row_mask, row_id) {
            continue;
        }
        let row = decode_sample_f32_row(matrix, row_id)?;
        let score = crate::kernels::dot_f32(query, &row);
        if !score.is_finite() {
            return Err(ScanError::NonFiniteScore { row_id });
        }
        let backward_error = query
            .iter()
            .zip(&row)
            .map(|(&query_value, &row_value)| {
                f64::from(query_value).abs() * f64::from(row_value).abs()
            })
            .sum::<f64>()
            * F32_DOT_BACKWARD_ERROR_EPSILON;
        dims_touched = dims_touched
            .checked_add(u64::try_from(query.len()).map_err(|_| ScanError::ArithmeticOverflow)?)
            .ok_or(ScanError::ArithmeticOverflow)?;
        bytes_read = bytes_read
            .checked_add(
                u64::try_from(query.len().saturating_mul(size_of::<f32>()))
                    .map_err(|_| ScanError::ArithmeticOverflow)?,
            )
            .ok_or(ScanError::ArithmeticOverflow)?;
        selected.push(ScanCandidate {
            row_id,
            // The seed is threshold-only. Lower the horizontal-order score by
            // the fixed Task 03 backward-error bound so it cannot sit above
            // the score produced by the vertical accumulation order.
            score: conservative_f32_lower_bound(f64::from(score) - backward_error),
        });
    }
    Ok(AbandonmentSeed {
        dims_touched,
        bytes_read,
        threshold: selected
            .is_full()
            .then(|| selected.worst())
            .flatten()
            .map(|hit| hit.score),
        kind: SeedKind::F32,
    })
}

pub(crate) fn prepare_bit4_pdx_seed(
    query: &Bit4Query,
    matrix: &PdxMatrix,
    factors: &[Bit4Factors],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
) -> Result<AbandonmentSeed, ScanError> {
    if matrix.dimension() != query.len() {
        return Err(ScanError::DimensionMismatch {
            query: query.len(),
            rows: matrix.dimension(),
        });
    }
    if matrix.row_count() != factors.len() {
        return Err(ScanError::FactorCount {
            expected: matrix.row_count(),
            actual: factors.len(),
        });
    }
    let factor_extrema = build_bit4_factor_extrema(matrix, factors)?;
    let sampled_rows = sampled_rows(matrix, k)?;
    let mut selected = BoundedTopK::new(k.min(matrix.row_count()));
    let mut dims_touched = 0_u64;
    let mut bytes_read = 0_u64;
    for row_id in sampled_rows {
        if !row_is_allowed(row_mask, row_id) {
            continue;
        }
        let row = decode_sample_bit4_row(matrix, row_id)?;
        let factor = factors
            .get(row_id)
            .copied()
            .ok_or(ScanError::ArithmeticOverflow)?;
        let score = est_dot_bit4(query, &row, factor)?;
        if !score.is_finite() {
            return Err(ScanError::NonFiniteScore { row_id });
        }
        dims_touched = dims_touched
            .checked_add(u64::try_from(query.len()).map_err(|_| ScanError::ArithmeticOverflow)?)
            .ok_or(ScanError::ArithmeticOverflow)?;
        bytes_read = bytes_read
            .checked_add(
                u64::try_from(query.len().div_ceil(2))
                    .map_err(|_| ScanError::ArithmeticOverflow)?,
            )
            .ok_or(ScanError::ArithmeticOverflow)?;
        selected.push(ScanCandidate { row_id, score });
    }
    Ok(AbandonmentSeed {
        dims_touched,
        bytes_read,
        threshold: selected
            .is_full()
            .then(|| selected.worst())
            .flatten()
            .map(|hit| hit.score),
        kind: SeedKind::Bit4 { factor_extrema },
    })
}

pub(crate) fn validate_f32_pdx(query: &[f32], matrix: &PdxMatrix) -> Result<(), ScanError> {
    if query.is_empty() {
        return Err(ScanError::ZeroDimension);
    }
    if matrix.dimension() != query.len() {
        return Err(ScanError::DimensionMismatch {
            query: query.len(),
            rows: matrix.dimension(),
        });
    }
    if let Some((index, _)) = query
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(ScanError::NonFiniteInput { index });
    }
    if let Some(index) = matrix.f32_first_non_finite() {
        let index = query
            .len()
            .checked_add(index)
            .ok_or(ScanError::ArithmeticOverflow)?;
        return Err(ScanError::NonFiniteInput { index });
    }
    Ok(())
}

fn sampled_rows(matrix: &PdxMatrix, k: usize) -> Result<Vec<usize>, ScanError> {
    if k == 0 || matrix.row_count() == 0 {
        return Ok(Vec::new());
    }
    let baseline_sample_count = STRATIFIED_SAMPLE_ROWS.min(matrix.row_count().div_ceil(2));
    let sample_count = baseline_sample_count.max(k).min(matrix.row_count());
    let mut sampled_rows = Vec::with_capacity(sample_count.saturating_add(k));
    append_stratified_rows(&mut sampled_rows, 0, matrix.row_count(), sample_count)?;
    if let Some(first_block) = matrix.blocks().first() {
        let block_rows = first_block.row_count() as usize;
        let leading_count = LEADING_BLOCK_SAMPLE_ROWS.max(k).min(block_rows);
        append_stratified_rows(&mut sampled_rows, 0, block_rows, leading_count)?;
    }
    sampled_rows.sort_unstable();
    sampled_rows.dedup();
    Ok(sampled_rows)
}

fn append_stratified_rows(
    output: &mut Vec<usize>,
    start: usize,
    length: usize,
    count: usize,
) -> Result<(), ScanError> {
    if count == 0 {
        return Ok(());
    }
    let count = count.min(length);
    let base_width = length / count;
    let wider_buckets = length % count;
    let mut bucket_start = start;
    for bucket in 0..count {
        let width = base_width + usize::from(bucket < wider_buckets);
        output.push(
            bucket_start
                .checked_add(width / 2)
                .ok_or(ScanError::ArithmeticOverflow)?,
        );
        bucket_start = bucket_start
            .checked_add(width)
            .ok_or(ScanError::ArithmeticOverflow)?;
    }
    Ok(())
}

fn sample_location(matrix: &PdxMatrix, row_id: usize) -> Result<(usize, usize), ScanError> {
    matrix
        .blocks()
        .iter()
        .copied()
        .enumerate()
        .find_map(|(block_index, block)| {
            let first = usize::try_from(block.first_row()).ok()?;
            let end = first.checked_add(block.row_count() as usize)?;
            (row_id >= first && row_id < end).then_some((block_index, row_id - first))
        })
        .ok_or(ScanError::ArithmeticOverflow)
}

fn decode_sample_f32_row(matrix: &PdxMatrix, row_id: usize) -> Result<Vec<f32>, ScanError> {
    let (block_index, local_row) = sample_location(matrix, row_id)?;
    let byte_start = local_row
        .checked_mul(size_of::<f32>())
        .ok_or(ScanError::ArithmeticOverflow)?;
    let byte_end = byte_start
        .checked_add(size_of::<f32>())
        .ok_or(ScanError::ArithmeticOverflow)?;
    let mut row = Vec::with_capacity(matrix.dimension());
    for column in 0..matrix.dimension() {
        let bytes = matrix
            .f32_column(block_index, column)?
            .get(byte_start..byte_end)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let array = <[u8; 4]>::try_from(bytes)
            .map_err(|_| ScanError::Pdx(super::pdx::PdxError::CorruptBlock))?;
        row.push(f32::from_bits(u32::from_le_bytes(array)));
    }
    Ok(row)
}

fn decode_sample_bit4_row(matrix: &PdxMatrix, row_id: usize) -> Result<Vec<u8>, ScanError> {
    let (block_index, local_row) = sample_location(matrix, row_id)?;
    let mut row = Vec::with_capacity(matrix.dimension().div_ceil(2));
    for column in 0..matrix.dimension().div_ceil(2) {
        let end = column.checked_add(1).ok_or(ScanError::ArithmeticOverflow)?;
        let byte = matrix
            .column_range(block_index, column..end)?
            .get(local_row)
            .copied()
            .ok_or(ScanError::ArithmeticOverflow)?;
        row.push(byte);
    }
    Ok(row)
}

fn build_bit4_factor_extrema(
    matrix: &PdxMatrix,
    factors: &[Bit4Factors],
) -> Result<Vec<Bit4FactorExtrema>, ScanError> {
    let mut extrema = Vec::with_capacity(matrix.blocks().len());
    for block in matrix.blocks().iter().copied() {
        let first =
            usize::try_from(block.first_row()).map_err(|_| ScanError::ArithmeticOverflow)?;
        let end = first
            .checked_add(block.row_count() as usize)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let block_factors = factors
            .get(first..end)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let (minimum, maximum) = block_factors.iter().fold(
            (f64::INFINITY, f64::NEG_INFINITY),
            |(minimum, maximum), factor| {
                let correction = factor.correction();
                (minimum.min(correction), maximum.max(correction))
            },
        );
        extrema.push(Bit4FactorExtrema { minimum, maximum });
    }
    Ok(extrema)
}

pub(crate) fn scan_pdx_with_abandonment(
    request: ScanRequest<'_>,
    k: usize,
    range: Range<usize>,
    seed: &AbandonmentSeed,
) -> Result<PartitionScan, ScanError> {
    match (request.query, request.rows, &seed.kind) {
        (ScanQuery::F32(query), ScanRows::F32Pdx(matrix), SeedKind::F32) => {
            scan_f32_pdx(query, matrix, request.row_mask, k, range, seed)
        }
        (
            ScanQuery::Bit4(query),
            ScanRows::Bit4Pdx { codes, factors },
            SeedKind::Bit4 { factor_extrema },
        ) => scan_bit4_pdx(
            Bit4ScanInputs {
                query,
                matrix: codes,
                factors,
                factor_extrema,
                row_mask: request.row_mask,
            },
            k,
            range,
            seed,
        ),
        _ => Err(ScanError::EarlyAbandonmentUnsupported),
    }
}

fn scan_f32_pdx(
    query: &[f32],
    matrix: &PdxMatrix,
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
    range: Range<usize>,
    seed: &AbandonmentSeed,
) -> Result<PartitionScan, ScanError> {
    validate_f32_pdx(query, matrix)?;
    validate_range(matrix, &range)?;
    let mut selected = BoundedTopK::new(k.min(range.end.saturating_sub(range.start)));
    let mut accumulators = vec![0.0_f32; matrix.rows_per_block()];
    let mut slab_bounds = vec![0.0_f64; query.len().div_ceil(BASELINE_DIMENSIONS_PER_SLAB)];
    let mut stats = PartialStats::default();
    for (block_index, block) in matrix.blocks().iter().copied().enumerate() {
        let Some((block_first, block_rows, scan_start, scan_end)) =
            super::block_scan_window(block, &range)?
        else {
            continue;
        };
        let block_accumulators = accumulators
            .get_mut(..block_rows)
            .ok_or(ScanError::ArithmeticOverflow)?;
        block_accumulators.fill(0.0);
        let extrema = matrix.f32_extrema(block_index)?;
        let bound = F32BlockBound::new(query, extrema, &mut slab_bounds);
        if let Some(threshold) = abandonment_threshold(&selected, seed.threshold)
            && bound.supports_abandonment
            && conservative_score_upper(0.0, bound.total_remaining, bound.rounding_guard)
                < f64::from(threshold)
        {
            stats.skip_block(row_mask, scan_start..scan_end)?;
            continue;
        }

        let mut remaining = bound.total_remaining;
        let mut exited = false;
        let mut slab_start = 0_usize;
        let mut slab_index = 0_usize;
        while slab_start < query.len() {
            let slab_end = slab_start
                .saturating_add(BASELINE_DIMENSIONS_PER_SLAB)
                .min(query.len());
            let query_slab = query
                .get(slab_start..slab_end)
                .ok_or(ScanError::ArithmeticOverflow)?;
            let columns = matrix.column_range(block_index, slab_start..slab_end)?;
            kernels::vertical_f32(query_slab, columns, block_rows, block_accumulators);
            stats.add_work(block_rows, slab_end - slab_start, columns.len())?;
            remaining -= slab_bounds
                .get(slab_index)
                .copied()
                .ok_or(ScanError::ArithmeticOverflow)?;
            if let Some(threshold) = abandonment_threshold(&selected, seed.threshold)
                && bound.supports_abandonment
                && conservative_score_upper(
                    f64::from(kernels::max_f32(block_accumulators)),
                    remaining,
                    bound.rounding_guard,
                ) < f64::from(threshold)
            {
                stats.abandon_rows(row_mask, scan_start..scan_end)?;
                exited = true;
                break;
            }
            slab_start = slab_end;
            slab_index = slab_index
                .checked_add(1)
                .ok_or(ScanError::ArithmeticOverflow)?;
        }
        if !exited {
            super::emit_f32_block(
                block_accumulators,
                block_first,
                scan_start,
                scan_end,
                row_mask,
                &mut selected,
            )?;
        }
    }
    Ok(stats.finish(selected))
}

struct Bit4ScanInputs<'a> {
    query: &'a Bit4Query,
    matrix: &'a PdxMatrix,
    factors: &'a [Bit4Factors],
    factor_extrema: &'a [Bit4FactorExtrema],
    row_mask: Option<&'a roaring::RoaringBitmap>,
}

fn scan_bit4_pdx(
    inputs: Bit4ScanInputs<'_>,
    k: usize,
    range: Range<usize>,
    seed: &AbandonmentSeed,
) -> Result<PartitionScan, ScanError> {
    let Bit4ScanInputs {
        query,
        matrix,
        factors,
        factor_extrema,
        row_mask,
    } = inputs;
    validate_range(matrix, &range)?;
    let mut selected = BoundedTopK::new(k.min(range.end.saturating_sub(range.start)));
    let mut accumulators = vec![0_i32; matrix.rows_per_block()];
    let mut slab_bounds = vec![0_i64; query.len().div_ceil(BASELINE_DIMENSIONS_PER_SLAB)];
    let mut stats = PartialStats::default();
    for (block_index, block) in matrix.blocks().iter().copied().enumerate() {
        let Some((block_first, block_rows, scan_start, scan_end)) =
            super::block_scan_window(block, &range)?
        else {
            continue;
        };
        let block_accumulators = accumulators
            .get_mut(..block_rows)
            .ok_or(ScanError::ArithmeticOverflow)?;
        block_accumulators.fill(0);
        let core_bound = Bit4CoreBound::new(
            query.coordinate_codes(),
            matrix.bit4_extrema(block_index)?,
            &mut slab_bounds,
        )?;
        let factor_bound = factor_extrema
            .get(block_index)
            .copied()
            .ok_or(ScanError::ArithmeticOverflow)?;
        let bound =
            Bit4BlockBound::new(core_bound.absolute_upper, factor_bound, query.scale_half());
        if let Some(threshold) = abandonment_threshold(&selected, seed.threshold)
            && bound.supports_abandonment
            && bound.score_upper(0, core_bound.total_remaining) < f64::from(threshold)
        {
            stats.skip_block(row_mask, scan_start..scan_end)?;
            continue;
        }

        let mut remaining = core_bound.total_remaining;
        let mut exited = false;
        let mut slab_start = 0_usize;
        let mut slab_index = 0_usize;
        while slab_start < query.len() {
            let slab_end = slab_start
                .saturating_add(BASELINE_DIMENSIONS_PER_SLAB)
                .min(query.len());
            let query_slab = query
                .coordinate_codes()
                .get(slab_start..slab_end)
                .ok_or(ScanError::ArithmeticOverflow)?;
            let columns = matrix.column_range(block_index, slab_start / 2..slab_end.div_ceil(2))?;
            kernels::vertical_bit4(query_slab, columns, block_rows, block_accumulators);
            stats.add_work(block_rows, slab_end - slab_start, columns.len())?;
            remaining = remaining
                .checked_sub(
                    slab_bounds
                        .get(slab_index)
                        .copied()
                        .ok_or(ScanError::ArithmeticOverflow)?,
                )
                .ok_or(ScanError::ArithmeticOverflow)?;
            if let Some(threshold) = abandonment_threshold(&selected, seed.threshold)
                && bound.supports_abandonment
                && bound.score_upper(i64::from(kernels::max_i32(block_accumulators)), remaining)
                    < f64::from(threshold)
            {
                stats.abandon_rows(row_mask, scan_start..scan_end)?;
                exited = true;
                break;
            }
            slab_start = slab_end;
            slab_index = slab_index
                .checked_add(1)
                .ok_or(ScanError::ArithmeticOverflow)?;
        }
        if exited {
            continue;
        }
        for (local_row, &integer_dot) in block_accumulators.iter().enumerate() {
            let row_id = block_first
                .checked_add(local_row)
                .ok_or(ScanError::ArithmeticOverflow)?;
            if row_id < scan_start || row_id >= scan_end || !row_is_allowed(row_mask, row_id) {
                continue;
            }
            let factor = factors
                .get(row_id)
                .copied()
                .ok_or(ScanError::ArithmeticOverflow)?;
            let score = kernels::score_bit4_integer(integer_dot, factor, query.scale_half());
            if !score.is_finite() {
                return Err(ScanError::NonFiniteScore { row_id });
            }
            selected.push(ScanCandidate { row_id, score });
        }
    }
    Ok(stats.finish(selected))
}

fn validate_range(matrix: &PdxMatrix, range: &Range<usize>) -> Result<(), ScanError> {
    if range.start > range.end || range.end > matrix.row_count() {
        return Err(ScanError::Pdx(super::pdx::PdxError::CorruptBlock));
    }
    Ok(())
}

fn abandonment_threshold(selected: &BoundedTopK, seed_threshold: Option<f32>) -> Option<f32> {
    let local = selected
        .is_full()
        .then(|| selected.worst())
        .flatten()
        .map(|hit| hit.score);
    match (seed_threshold, local) {
        (Some(seed_score), Some(local_score)) => Some(seed_score.max(local_score)),
        (Some(seed_score), None) => Some(seed_score),
        (None, Some(local_score)) => Some(local_score),
        (None, None) => None,
    }
}

#[derive(Default)]
struct PartialStats {
    dims_touched: u64,
    rows_abandoned: u64,
    blocks_skipped: u64,
    bytes_read: u64,
}

impl PartialStats {
    fn add_work(&mut self, rows: usize, dimensions: usize, bytes: usize) -> Result<(), ScanError> {
        super::add_pdx_work(
            &mut self.dims_touched,
            &mut self.bytes_read,
            rows,
            dimensions,
            bytes,
        )
    }

    fn skip_block(
        &mut self,
        row_mask: Option<&roaring::RoaringBitmap>,
        rows: Range<usize>,
    ) -> Result<(), ScanError> {
        self.blocks_skipped = self
            .blocks_skipped
            .checked_add(1)
            .ok_or(ScanError::ArithmeticOverflow)?;
        self.abandon_rows(row_mask, rows)
    }

    fn abandon_rows(
        &mut self,
        row_mask: Option<&roaring::RoaringBitmap>,
        rows: Range<usize>,
    ) -> Result<(), ScanError> {
        self.rows_abandoned = self
            .rows_abandoned
            .checked_add(
                u64::try_from(super::allowed_row_count(row_mask, rows))
                    .map_err(|_| ScanError::ArithmeticOverflow)?,
            )
            .ok_or(ScanError::ArithmeticOverflow)?;
        Ok(())
    }

    fn finish(self, selected: BoundedTopK) -> PartitionScan {
        PartitionScan {
            candidates: selected.into_sorted(),
            dims_touched: self.dims_touched,
            rows_abandoned: self.rows_abandoned,
            blocks_skipped: self.blocks_skipped,
            bytes_read: self.bytes_read,
        }
    }
}

#[derive(Debug)]
struct F32BlockBound {
    total_remaining: f64,
    rounding_guard: f64,
    supports_abandonment: bool,
}

impl F32BlockBound {
    fn new(query: &[f32], extrema: &[kernels::F32Extrema], slab_bounds: &mut [f64]) -> Self {
        let totals = kernels::f32_extrema_slab_bounds(
            query,
            extrema,
            BASELINE_DIMENSIONS_PER_SLAB,
            slab_bounds,
        );
        let rounding_guard = kernel_rounding_guard(totals.absolute_contribution, query.len());
        let supports_abandonment = totals.absolute_contribution <= f64::from(f32::MAX)
            && totals.maximum_contribution.is_finite()
            && rounding_guard.is_finite()
            && slab_bounds.iter().all(|value| value.is_finite());
        Self {
            total_remaining: totals.maximum_contribution,
            rounding_guard,
            supports_abandonment,
        }
    }
}

#[derive(Debug)]
struct Bit4CoreBound {
    total_remaining: i64,
    absolute_upper: i64,
}

impl Bit4CoreBound {
    fn new(
        query: &[i8],
        extrema: &[Bit4ColumnExtrema],
        slab_bounds: &mut [i64],
    ) -> Result<Self, ScanError> {
        if extrema.len() != query.len().div_ceil(2)
            || slab_bounds.len() != query.len().div_ceil(BASELINE_DIMENSIONS_PER_SLAB)
        {
            return Err(ScanError::Pdx(super::pdx::PdxError::CorruptBlock));
        }
        slab_bounds.fill(0);
        let mut total_remaining = 0_i64;
        let mut absolute_upper = 0_i64;
        for (column, (query_pair, bounds)) in query.chunks(2).zip(extrema).enumerate() {
            let even = query_pair
                .first()
                .copied()
                .ok_or(ScanError::ArithmeticOverflow)?;
            let mut contribution =
                bit4_coordinate_upper(even, bounds.high_minimum, bounds.high_maximum);
            if let Some(&odd) = query_pair.get(1) {
                contribution = contribution
                    .checked_add(bit4_coordinate_upper(
                        odd,
                        bounds.low_minimum,
                        bounds.low_maximum,
                    ))
                    .ok_or(ScanError::ArithmeticOverflow)?;
            }
            let contribution = i64::from(contribution);
            let mut absolute = i64::from(bit4_coordinate_absolute(
                even,
                bounds.high_minimum,
                bounds.high_maximum,
            ));
            if let Some(&odd) = query_pair.get(1) {
                absolute = absolute
                    .checked_add(i64::from(bit4_coordinate_absolute(
                        odd,
                        bounds.low_minimum,
                        bounds.low_maximum,
                    )))
                    .ok_or(ScanError::ArithmeticOverflow)?;
            }
            absolute_upper = absolute_upper
                .checked_add(absolute)
                .ok_or(ScanError::ArithmeticOverflow)?;
            total_remaining = total_remaining
                .checked_add(contribution)
                .ok_or(ScanError::ArithmeticOverflow)?;
            let coordinate = column.checked_mul(2).ok_or(ScanError::ArithmeticOverflow)?;
            let slab = slab_bounds
                .get_mut(coordinate / BASELINE_DIMENSIONS_PER_SLAB)
                .ok_or(ScanError::ArithmeticOverflow)?;
            *slab = slab
                .checked_add(contribution)
                .ok_or(ScanError::ArithmeticOverflow)?;
        }
        Ok(Self {
            total_remaining,
            absolute_upper,
        })
    }
}

fn bit4_coordinate_upper(query: i8, minimum: u8, maximum: u8) -> i32 {
    let query = i32::from(query);
    let minimum = 2 * i32::from(minimum) - 15;
    let maximum = 2 * i32::from(maximum) - 15;
    (query * minimum).max(query * maximum)
}

fn bit4_coordinate_absolute(query: i8, minimum: u8, maximum: u8) -> i32 {
    let query = i32::from(query);
    let minimum = query * (2 * i32::from(minimum) - 15);
    let maximum = query * (2 * i32::from(maximum) - 15);
    minimum.abs().max(maximum.abs())
}

#[derive(Debug)]
struct Bit4BlockBound {
    factor: Bit4FactorExtrema,
    query_scale_half: f64,
    rounding_guard: f64,
    supports_abandonment: bool,
}

impl Bit4BlockBound {
    fn new(absolute_core_upper: i64, factor: Bit4FactorExtrema, query_scale_half: f64) -> Self {
        let absolute_upper = query_scale_half.abs()
            * factor.minimum.abs().max(factor.maximum.abs())
            * absolute_core_upper as f64;
        let rounding_guard = kernel_rounding_guard(absolute_upper, 1);
        let supports_abandonment = factor.minimum >= 0.0
            && factor.minimum.is_finite()
            && factor.maximum.is_finite()
            && query_scale_half.is_finite()
            && rounding_guard.is_finite();
        Self {
            factor,
            query_scale_half,
            rounding_guard,
            supports_abandonment,
        }
    }

    fn score_upper(&self, partial: i64, remaining: i64) -> f64 {
        let core_upper = partial.saturating_add(remaining);
        let factor = if core_upper.is_negative() {
            self.factor.minimum
        } else {
            self.factor.maximum
        };
        self.query_scale_half * factor * core_upper as f64 + self.rounding_guard
    }
}

fn kernel_rounding_guard(total_absolute_upper: f64, dimensions: usize) -> f64 {
    let operations = dimensions.saturating_mul(4) as f64;
    let unit_roundoff = f64::from(f32::EPSILON) * 0.5;
    let numerator = operations * unit_roundoff;
    if numerator >= 1.0 || !total_absolute_upper.is_finite() {
        return f64::INFINITY;
    }
    let gamma = numerator / (1.0 - numerator);
    gamma * total_absolute_upper + operations * f64::from(f32::MIN_POSITIVE)
}

fn conservative_score_upper(partial: f64, remaining: f64, rounding_guard: f64) -> f64 {
    partial + remaining + rounding_guard
}

fn conservative_f32_lower_bound(value: f64) -> f32 {
    let rounded = value as f32;
    if f64::from(rounded) <= value {
        return rounded;
    }
    if rounded == f32::NEG_INFINITY {
        return rounded;
    }
    if rounded == 0.0 {
        return -f32::from_bits(1);
    }
    let bits = rounded.to_bits();
    if rounded.is_sign_positive() {
        f32::from_bits(bits - 1)
    } else {
        f32::from_bits(bits + 1)
    }
}

fn row_is_allowed(row_mask: Option<&roaring::RoaringBitmap>, row_id: usize) -> bool {
    !row_mask.is_some_and(|mask| u32::try_from(row_id).map_or(true, |id| !mask.contains(id)))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::{
        Bit4ColumnExtrema, Bit4CoreBound, F32BlockBound, bit4_coordinate_upper,
        conservative_f32_lower_bound, conservative_score_upper, kernel_rounding_guard,
    };
    use crate::kernels::F32Extrema;

    #[test]
    fn bound_mixed_signs_selects_the_maximizing_endpoint() {
        let query = [2.0_f32, -2.0];
        let extrema = [
            F32Extrema {
                minimum_bits: (-7.0_f32).to_bits(),
                maximum_bits: 3.0_f32.to_bits(),
            },
            F32Extrema {
                minimum_bits: (-7.0_f32).to_bits(),
                maximum_bits: 3.0_f32.to_bits(),
            },
        ];
        let mut slabs = [0.0_f64; 1];
        let bound = F32BlockBound::new(&query, &extrema, &mut slabs);
        assert_eq!(bound.total_remaining, 20.0);
    }

    #[test]
    fn bound_zero_query_and_identical_rows_is_exact() {
        let query = [0.0_f32, -0.0, 0.0];
        let extrema = [
            F32Extrema {
                minimum_bits: (-4.0_f32).to_bits(),
                maximum_bits: (-4.0_f32).to_bits(),
            },
            F32Extrema {
                minimum_bits: 7.0_f32.to_bits(),
                maximum_bits: 7.0_f32.to_bits(),
            },
            F32Extrema {
                minimum_bits: f32::MIN_POSITIVE.to_bits(),
                maximum_bits: f32::MIN_POSITIVE.to_bits(),
            },
        ];
        let mut slabs = [0.0_f64; 1];
        let bound = F32BlockBound::new(&query, &extrema, &mut slabs);
        assert_eq!(bound.total_remaining, 0.0);
        assert!(bound.supports_abandonment);
    }

    #[test]
    fn bound_denormals_include_an_absolute_rounding_guard() {
        let guard = kernel_rounding_guard(f64::from(f32::from_bits(1)), 8);
        assert!(guard >= 32.0 * f64::from(f32::MIN_POSITIVE));
        assert!(conservative_score_upper(0.0, 0.0, guard) > 0.0);
    }

    #[test]
    fn bound_very_large_magnitudes_disable_abandonment_before_overflow_can_hide() {
        let query = [f32::MAX, f32::MAX];
        let extrema = [
            F32Extrema {
                minimum_bits: (-2.0_f32).to_bits(),
                maximum_bits: 2.0_f32.to_bits(),
            },
            F32Extrema {
                minimum_bits: (-2.0_f32).to_bits(),
                maximum_bits: 2.0_f32.to_bits(),
            },
        ];
        let mut slabs = [0.0_f64; 1];
        let bound = F32BlockBound::new(&query, &extrema, &mut slabs);
        assert!(!bound.supports_abandonment);
    }

    #[test]
    fn bit4_bound_is_integer_exact_for_both_nibbles() {
        let query = [3_i8, -2];
        let extrema = [Bit4ColumnExtrema {
            high_minimum: 1,
            high_maximum: 9,
            low_minimum: 4,
            low_maximum: 15,
        }];
        let mut slabs = [0_i64; 1];
        let bound = Bit4CoreBound::new(&query, &extrema, &mut slabs).expect("valid bound");
        let expected =
            i64::from(bit4_coordinate_upper(3, 1, 9)) + i64::from(bit4_coordinate_upper(-2, 4, 15));
        assert_eq!(bound.total_remaining, expected);
        assert_eq!(slabs[0], expected);
    }

    #[test]
    fn f32_seed_lower_bound_never_rounds_up() {
        for value in [
            -1.0e30_f64,
            -1.000_000_01,
            -f64::from(f32::from_bits(1)),
            0.0,
            f64::from(f32::from_bits(1)),
            1.000_000_01,
            1.0e30,
        ] {
            assert!(f64::from(conservative_f32_lower_bound(value)) <= value);
        }
    }
}
