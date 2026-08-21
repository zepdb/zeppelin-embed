//! Conservative partial-dot-product abandonment over f32 PDX blocks.
//!
//! Dimensions use their fixed stored coordinate order. A future segment may
//! supply a cheap variance-statistic permutation, but no PCA rotation or other
//! trained artifact is part of this scan path.

use std::ops::Range;

use super::pdx::PdxMatrix;
use super::topk::BoundedTopK;
use super::{PartitionScan, ScanCandidate, ScanError};

pub(crate) const BASELINE_DIMENSIONS_PER_SLAB: usize = 32;
const STRATIFIED_SAMPLE_ROWS: usize = 512;
const LEADING_BLOCK_SAMPLE_ROWS: usize = 16;

#[derive(Debug)]
pub(crate) struct AbandonmentSeed {
    pub(crate) candidates: Vec<ScanCandidate>,
    pub(crate) dims_touched: u64,
    sampled_rows: Vec<usize>,
    threshold: Option<f32>,
}

pub(crate) fn prepare_f32_pdx_seed(
    query: &[f32],
    matrix: &PdxMatrix,
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
) -> Result<AbandonmentSeed, ScanError> {
    validate_f32_pdx(query, matrix)?;
    if k == 0 || matrix.row_count() == 0 {
        return Ok(AbandonmentSeed {
            candidates: Vec::new(),
            dims_touched: 0,
            sampled_rows: Vec::new(),
            threshold: None,
        });
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

    let mut selected = BoundedTopK::new(k.min(matrix.row_count()));
    let mut dims_touched = 0_u64;
    for &row_id in &sampled_rows {
        if !row_is_allowed(row_mask, row_id) {
            continue;
        }
        let row = decode_sample_row(matrix, row_id)?;
        let score = crate::kernels::dot_f32(query, &row);
        if !score.is_finite() {
            return Err(ScanError::NonFiniteScore { row_id });
        }
        dims_touched = dims_touched
            .checked_add(u64::try_from(query.len()).map_err(|_| ScanError::ArithmeticOverflow)?)
            .ok_or(ScanError::ArithmeticOverflow)?;
        selected.push(ScanCandidate { row_id, score });
    }
    let threshold = selected
        .is_full()
        .then(|| selected.worst())
        .flatten()
        .map(|hit| hit.score);
    Ok(AbandonmentSeed {
        candidates: selected.into_sorted(),
        dims_touched,
        sampled_rows,
        threshold,
    })
}

fn validate_f32_pdx(query: &[f32], matrix: &PdxMatrix) -> Result<(), ScanError> {
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
        let row_id = bucket_start
            .checked_add(width / 2)
            .ok_or(ScanError::ArithmeticOverflow)?;
        output.push(row_id);
        bucket_start = bucket_start
            .checked_add(width)
            .ok_or(ScanError::ArithmeticOverflow)?;
    }
    Ok(())
}

fn decode_sample_row(matrix: &PdxMatrix, row_id: usize) -> Result<Vec<f32>, ScanError> {
    let (block_index, block_first) = matrix
        .blocks()
        .iter()
        .copied()
        .enumerate()
        .find_map(|(block_index, block)| {
            let first = usize::try_from(block.first_row()).ok()?;
            let end = first.checked_add(block.row_count() as usize)?;
            (row_id >= first && row_id < end).then_some((block_index, first))
        })
        .ok_or(ScanError::ArithmeticOverflow)?;
    let local_row = row_id
        .checked_sub(block_first)
        .ok_or(ScanError::ArithmeticOverflow)?;
    let byte_start = local_row
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or(ScanError::ArithmeticOverflow)?;
    let byte_end = byte_start
        .checked_add(std::mem::size_of::<f32>())
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

pub(crate) fn scan_f32_pdx(
    query: &[f32],
    matrix: &PdxMatrix,
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
    range: Range<usize>,
    seed: &AbandonmentSeed,
) -> Result<PartitionScan, ScanError> {
    validate_f32_pdx(query, matrix)?;
    if range.start > range.end || range.end > matrix.row_count() {
        return Err(ScanError::Pdx(super::pdx::PdxError::CorruptBlock));
    }

    let mut selected = BoundedTopK::new(k.min(range.end - range.start));
    let mut dims_touched = 0_u64;
    let mut rows_abandoned = 0_u64;
    for (block_index, block) in matrix.blocks().iter().copied().enumerate() {
        let block_first =
            usize::try_from(block.first_row()).map_err(|_| ScanError::ArithmeticOverflow)?;
        let block_rows = block.row_count() as usize;
        let block_end = block_first
            .checked_add(block_rows)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let scan_start = block_first.max(range.start);
        let scan_end = block_end.min(range.end);
        if scan_start >= scan_end {
            continue;
        }

        if abandonment_threshold(&selected, seed).is_none() {
            let decoded = matrix.decode_f32_range(scan_start..scan_end)?;
            score_decoded_rows(
                query,
                &decoded,
                scan_start,
                row_mask,
                seed,
                &mut selected,
                &mut dims_touched,
            )?;
            continue;
        }

        let extrema = matrix.f32_extrema(block_index)?;
        let bound = BlockBound::new(query, extrema)?;
        if !bound.supports_abandonment {
            let decoded = matrix.decode_f32_range(scan_start..scan_end)?;
            score_decoded_rows(
                query,
                &decoded,
                scan_start,
                row_mask,
                seed,
                &mut selected,
                &mut dims_touched,
            )?;
            continue;
        }

        let scratch_len = block_rows
            .checked_mul(query.len())
            .ok_or(ScanError::ArithmeticOverflow)?;
        let mut row_scratch = vec![0.0_f32; scratch_len];
        let mut partial = vec![0.0_f64; block_rows];
        let mut integer_exact = vec![true; block_rows];
        let mut integer_absolute_sum = vec![0.0_f64; block_rows];
        let mut active = Vec::with_capacity(block_rows);
        for local_row in 0..block_rows {
            let row_id = block_first
                .checked_add(local_row)
                .ok_or(ScanError::ArithmeticOverflow)?;
            active.push(
                row_id >= scan_start
                    && row_id < scan_end
                    && !seed.is_sampled(row_id)
                    && row_is_allowed(row_mask, row_id),
            );
        }

        let mut slab_start = 0_usize;
        while slab_start < query.len() {
            let slab_end = slab_start
                .saturating_add(BASELINE_DIMENSIONS_PER_SLAB)
                .min(query.len());
            for column in slab_start..slab_end {
                let query_value = *query.get(column).ok_or(ScanError::ArithmeticOverflow)?;
                let column_bytes = matrix.f32_column(block_index, column)?;
                for (local_row, bytes) in column_bytes
                    .chunks_exact(std::mem::size_of::<f32>())
                    .enumerate()
                {
                    if !active.get(local_row).copied().unwrap_or(false) {
                        continue;
                    }
                    let array = <[u8; 4]>::try_from(bytes)
                        .map_err(|_| ScanError::Pdx(super::pdx::PdxError::CorruptBlock))?;
                    let value = f32::from_bits(u32::from_le_bytes(array));
                    let target = local_row
                        .checked_mul(query.len())
                        .and_then(|offset| offset.checked_add(column))
                        .ok_or(ScanError::ArithmeticOverflow)?;
                    let output = row_scratch
                        .get_mut(target)
                        .ok_or(ScanError::ArithmeticOverflow)?;
                    *output = value;
                    let accumulator = partial
                        .get_mut(local_row)
                        .ok_or(ScanError::ArithmeticOverflow)?;
                    let product = f64::from(query_value) * f64::from(value);
                    *accumulator += product;
                    let absolute_sum = integer_absolute_sum
                        .get_mut(local_row)
                        .ok_or(ScanError::ArithmeticOverflow)?;
                    *absolute_sum += product.abs();
                    let exact = integer_exact
                        .get_mut(local_row)
                        .ok_or(ScanError::ArithmeticOverflow)?;
                    *exact = *exact
                        && query_value.fract() == 0.0
                        && value.fract() == 0.0
                        && product.abs() <= 16_777_216.0
                        && *absolute_sum <= 16_777_216.0;
                    dims_touched = dims_touched
                        .checked_add(1)
                        .ok_or(ScanError::ArithmeticOverflow)?;
                }
            }

            if let Some(threshold) = abandonment_threshold(&selected, seed) {
                let remaining = bound.remaining_after(slab_end)?;
                for (local_row, is_active) in active.iter_mut().enumerate() {
                    if !*is_active {
                        continue;
                    }
                    let prefix = *partial
                        .get(local_row)
                        .ok_or(ScanError::ArithmeticOverflow)?;
                    let rounding_guard = if slab_end == query.len()
                        && integer_exact.get(local_row).copied().unwrap_or(false)
                    {
                        0.0
                    } else {
                        bound.rounding_guard
                    };
                    let upper = conservative_score_upper(prefix, remaining, rounding_guard);
                    if upper < f64::from(threshold) {
                        *is_active = false;
                        rows_abandoned = rows_abandoned
                            .checked_add(1)
                            .ok_or(ScanError::ArithmeticOverflow)?;
                    }
                }
            }
            slab_start = slab_end;
        }

        for (local_row, is_active) in active.into_iter().enumerate() {
            if !is_active {
                continue;
            }
            let row_id = block_first
                .checked_add(local_row)
                .ok_or(ScanError::ArithmeticOverflow)?;
            let start = local_row
                .checked_mul(query.len())
                .ok_or(ScanError::ArithmeticOverflow)?;
            let end = start
                .checked_add(query.len())
                .ok_or(ScanError::ArithmeticOverflow)?;
            let row = row_scratch
                .get(start..end)
                .ok_or(ScanError::ArithmeticOverflow)?;
            let score = crate::kernels::dot_f32(query, row);
            if !score.is_finite() {
                return Err(ScanError::NonFiniteScore { row_id });
            }
            dims_touched = dims_touched
                .checked_add(u64::try_from(query.len()).map_err(|_| ScanError::ArithmeticOverflow)?)
                .ok_or(ScanError::ArithmeticOverflow)?;
            selected.push(ScanCandidate { row_id, score });
        }
    }
    Ok(PartitionScan {
        candidates: selected.into_sorted(),
        dims_touched,
        rows_abandoned,
    })
}

fn score_decoded_rows(
    query: &[f32],
    rows: &[f32],
    first_row: usize,
    row_mask: Option<&roaring::RoaringBitmap>,
    seed: &AbandonmentSeed,
    selected: &mut BoundedTopK,
    dims_touched: &mut u64,
) -> Result<(), ScanError> {
    for (local_row, row) in rows.chunks_exact(query.len()).enumerate() {
        let row_id = first_row
            .checked_add(local_row)
            .ok_or(ScanError::ArithmeticOverflow)?;
        if seed.is_sampled(row_id) || !row_is_allowed(row_mask, row_id) {
            continue;
        }
        let score = crate::kernels::dot_f32(query, row);
        if !score.is_finite() {
            return Err(ScanError::NonFiniteScore { row_id });
        }
        *dims_touched = dims_touched
            .checked_add(u64::try_from(query.len()).map_err(|_| ScanError::ArithmeticOverflow)?)
            .ok_or(ScanError::ArithmeticOverflow)?;
        selected.push(ScanCandidate { row_id, score });
    }
    Ok(())
}

fn abandonment_threshold(selected: &BoundedTopK, seed: &AbandonmentSeed) -> Option<f32> {
    let local = selected
        .is_full()
        .then(|| selected.worst())
        .flatten()
        .map(|hit| hit.score);
    match (seed.threshold, local) {
        (Some(seed_score), Some(local_score)) => Some(seed_score.max(local_score)),
        (Some(seed_score), None) => Some(seed_score),
        (None, Some(local_score)) => Some(local_score),
        (None, None) => None,
    }
}

impl AbandonmentSeed {
    fn is_sampled(&self, row_id: usize) -> bool {
        self.sampled_rows.binary_search(&row_id).is_ok()
    }
}

fn row_is_allowed(row_mask: Option<&roaring::RoaringBitmap>, row_id: usize) -> bool {
    !row_mask.is_some_and(|mask| u32::try_from(row_id).map_or(true, |id| !mask.contains(id)))
}

#[derive(Debug)]
struct BlockBound {
    remaining_upper: Vec<f64>,
    rounding_guard: f64,
    supports_abandonment: bool,
}

impl BlockBound {
    fn new(query: &[f32], extrema_bits: &[(u32, u32)]) -> Result<Self, ScanError> {
        if extrema_bits.len() != query.len() {
            return Err(ScanError::Pdx(super::pdx::PdxError::CorruptBlock));
        }
        let length = query
            .len()
            .checked_add(1)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let mut remaining_upper = vec![0.0_f64; length];
        let mut total_absolute_upper = 0.0_f64;
        for column in (0..query.len()).rev() {
            let query_value = *query.get(column).ok_or(ScanError::ArithmeticOverflow)?;
            let &(minimum_bits, maximum_bits) = extrema_bits
                .get(column)
                .ok_or(ScanError::ArithmeticOverflow)?;
            let minimum = f32::from_bits(minimum_bits);
            let maximum = f32::from_bits(maximum_bits);
            let contribution = maximum_remaining_contribution(query_value, minimum, maximum);
            let absolute = (f64::from(query_value) * f64::from(minimum))
                .abs()
                .max((f64::from(query_value) * f64::from(maximum)).abs());
            total_absolute_upper += absolute;
            let suffix = *remaining_upper
                .get(column + 1)
                .ok_or(ScanError::ArithmeticOverflow)?;
            let output = remaining_upper
                .get_mut(column)
                .ok_or(ScanError::ArithmeticOverflow)?;
            *output = contribution + suffix;
        }
        let rounding_guard = kernel_rounding_guard(total_absolute_upper, query.len());
        let supports_abandonment = total_absolute_upper <= f64::from(f32::MAX)
            && rounding_guard.is_finite()
            && remaining_upper.iter().all(|value| value.is_finite());
        Ok(Self {
            remaining_upper,
            rounding_guard,
            supports_abandonment,
        })
    }

    fn remaining_after(&self, dimensions: usize) -> Result<f64, ScanError> {
        self.remaining_upper
            .get(dimensions)
            .copied()
            .ok_or(ScanError::ArithmeticOverflow)
    }
}

fn maximum_remaining_contribution(query: f32, minimum: f32, maximum: f32) -> f64 {
    let endpoint = if query.is_sign_negative() {
        minimum
    } else {
        maximum
    };
    f64::from(query) * f64::from(endpoint)
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::{
        BlockBound, conservative_score_upper, kernel_rounding_guard, maximum_remaining_contribution,
    };

    #[test]
    fn bound_mixed_signs_selects_the_maximizing_endpoint() {
        assert_eq!(maximum_remaining_contribution(2.0, -7.0, 3.0), 6.0);
        assert_eq!(maximum_remaining_contribution(-2.0, -7.0, 3.0), 14.0);
        assert_eq!(maximum_remaining_contribution(-0.0, -7.0, 3.0), -0.0);
    }

    #[test]
    fn bound_zero_query_and_identical_rows_is_exact() {
        let query = [0.0_f32, -0.0, 0.0];
        let extrema = [
            ((-4.0_f32).to_bits(), (-4.0_f32).to_bits()),
            (7.0_f32.to_bits(), 7.0_f32.to_bits()),
            (f32::MIN_POSITIVE.to_bits(), f32::MIN_POSITIVE.to_bits()),
        ];
        let bound = BlockBound::new(&query, &extrema).expect("valid bound");
        assert_eq!(bound.remaining_after(0).expect("full suffix"), 0.0);
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
            ((-2.0_f32).to_bits(), 2.0_f32.to_bits()),
            ((-2.0_f32).to_bits(), 2.0_f32.to_bits()),
        ];
        let bound = BlockBound::new(&query, &extrema).expect("representable metadata");
        assert!(!bound.supports_abandonment);
    }

    #[test]
    fn bound_partial_plus_remaining_never_drops_an_equal_threshold() {
        let upper = conservative_score_upper(-5.0, 7.0, 0.0);
        assert_eq!(upper, 2.0);
        assert!(upper >= 2.0);
    }
}
