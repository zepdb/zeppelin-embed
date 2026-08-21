//! Scalar reference kernels: the oracle for every dispatched variant.

use super::{InstructionTier, KernelArm, KernelTable, MAX_DOT_I8_DIMENSION};

pub(super) fn table() -> KernelTable {
    KernelTable {
        arm: KernelArm::Scalar,
        tier: InstructionTier::Scalar,
        dot_i8,
        dot_f32,
        dot_f16,
        hamming_u1,
        dot_i8_batch,
        hamming_u1_batch,
        dot_bit4,
        dot_bit4_prepared,
        dot_bit4_batch,
        score_bit4_prepared_batch,
        vertical_f32,
        vertical_f16,
        vertical_i8,
        vertical_bit4,
        f32_extrema_slab_bounds,
        max_f32,
        max_i32,
        vertical_rows_per_tile: super::BASELINE_KERNEL_CONFIG.vertical_rows_per_tile,
    }
}

pub(super) fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    debug_assert!(
        a.len() <= MAX_DOT_I8_DIMENSION,
        "i8 dimension must be pre-validated"
    );
    a.iter()
        .zip(b)
        .map(|(&left, &right)| i32::from(left) * i32::from(right))
        .sum()
}

pub(super) fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    a.iter()
        .zip(b)
        .fold(0.0_f32, |sum, (&left, &right)| sum + left * right)
}

pub(super) fn dot_f16(a: &[u16], b: &[u16]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    a.iter().zip(b).fold(0.0_f32, |sum, (&left, &right)| {
        sum + f16_to_f32(left) * f16_to_f32(right)
    })
}

pub(super) fn hamming_u1(a: &[u8], b: &[u8]) -> u32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    a.iter()
        .zip(b)
        .map(|(&left, &right)| (left ^ right).count_ones())
        .sum()
}

pub(super) fn dot_i8_batch(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    debug_assert_eq!(q.len(), d, "query dimension must be pre-validated");
    debug_assert!(
        d <= MAX_DOT_I8_DIMENSION,
        "i8 dimension must be pre-validated"
    );
    debug_assert_eq!(
        rows.len(),
        d.saturating_mul(out.len()),
        "batch shape must be pre-validated"
    );
    if d == 0 {
        out.fill(0);
        return;
    }
    for (row, result) in rows.chunks_exact(d).zip(out.iter_mut()) {
        *result = dot_i8(q, row);
    }
}

pub(super) fn hamming_u1_batch(q: &[u8], rows: &[u8], d_bytes: usize, out: &mut [u32]) {
    debug_assert_eq!(q.len(), d_bytes, "query dimension must be pre-validated");
    debug_assert_eq!(
        rows.len(),
        d_bytes.saturating_mul(out.len()),
        "batch shape must be pre-validated"
    );
    if d_bytes == 0 {
        out.fill(0);
        return;
    }
    for (row, result) in rows.chunks_exact(d_bytes).zip(out.iter_mut()) {
        *result = hamming_u1(q, row);
    }
}

pub(super) fn dot_bit4(q: &[i8], codes: &[u8]) -> i32 {
    const SHIFTS: [u32; 2] = [4, 0];
    debug_assert_eq!(codes.len(), q.len().div_ceil(2));
    debug_assert!(q.len() <= MAX_DOT_I8_DIMENSION);
    codes
        .iter()
        .zip(q.chunks(2))
        .map(|(&packed, query)| {
            query
                .iter()
                .zip(SHIFTS)
                .map(|(&query_value, shift)| {
                    let code = i32::from((packed >> shift) & 0x0f);
                    i32::from(query_value) * (2 * code - 15)
                })
                .sum::<i32>()
        })
        .sum()
}

pub(super) fn dot_bit4_prepared(q: &[i8], query_sum: i32, codes: &[u8]) -> i32 {
    2 * dot_bit4_prepared_unsigned(q, codes) - 15 * query_sum
}

pub(super) fn dot_bit4_prepared_unsigned(q: &[i8], codes: &[u8]) -> i32 {
    debug_assert_eq!(codes.len(), q.len().div_ceil(2));
    debug_assert!(q.len() <= MAX_DOT_I8_DIMENSION);
    let mut sum = 0_i32;
    let mut query_base = 0_usize;
    let mut code_base = 0_usize;
    while query_base < q.len() {
        let block_len = (q.len() - query_base).min(32);
        let even_count = block_len.div_ceil(2);
        let code_count = block_len.div_ceil(2);
        let Some(query_block) = q.get(query_base..query_base + block_len) else {
            return sum;
        };
        let Some(code_block) = codes.get(code_base..code_base + code_count) else {
            return sum;
        };
        for (field, (&packed, &even)) in code_block
            .iter()
            .zip(query_block.iter().take(even_count))
            .enumerate()
        {
            let high = i32::from(packed >> 4);
            sum += i32::from(even) * high;
            if let Some(&odd) = query_block.get(even_count + field) {
                let low = i32::from(packed & 0x0f);
                sum += i32::from(odd) * low;
            }
        }
        query_base += block_len;
        code_base += code_count;
    }
    sum
}

pub(super) fn dot_bit4_batch(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    dot_packed_batch(q, rows, d, out, 2, dot_bit4);
}

pub(super) fn score_bit4_prepared_batch(
    q: &[i8],
    query_sum: i32,
    query_scale_half: f64,
    rows: &[u8],
    d: usize,
    factors: &[crate::quant::Bit4Factors],
    out: &mut [f32],
) {
    debug_assert_eq!(q.len(), d, "query dimension must be pre-validated");
    debug_assert!(d <= MAX_DOT_I8_DIMENSION);
    debug_assert_eq!(factors.len(), out.len());
    let row_bytes = d.div_ceil(2);
    debug_assert_eq!(rows.len(), row_bytes.saturating_mul(out.len()));
    for ((row, &factor), score) in rows
        .chunks_exact(row_bytes)
        .zip(factors)
        .zip(out.iter_mut())
    {
        let integer_dot = dot_bit4_prepared(q, query_sum, row);
        *score = bit4_score(integer_dot, factor, query_scale_half);
    }
}

pub(super) fn bit4_score(
    integer_dot: i32,
    factor: crate::quant::Bit4Factors,
    query_scale_half: f64,
) -> f32 {
    let (scale, normalized_correction) = factor.scoring_parts();
    if scale == 0.0 {
        return 0.0;
    }
    ((f64::from(scale) * f64::from(normalized_correction))
        * query_scale_half
        * f64::from(integer_dot)) as f32
}

pub(super) fn vertical_f32(query: &[f32], columns: &[u8], rows: usize, out: &mut [f32]) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(
        columns.len(),
        query.len().saturating_mul(rows).saturating_mul(4)
    );
    let column_width = rows.saturating_mul(4);
    for (&query_value, column) in query.iter().zip(columns.chunks_exact(column_width)) {
        for (bytes, accumulator) in column.chunks_exact(4).zip(out.iter_mut()) {
            let Some(array) = bytes.try_into().ok() else {
                continue;
            };
            *accumulator += query_value * f32::from_bits(u32::from_le_bytes(array));
        }
    }
}

pub(super) fn vertical_f16(query: &[u16], columns: &[u8], rows: usize, out: &mut [f32]) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(
        columns.len(),
        query.len().saturating_mul(rows).saturating_mul(2)
    );
    let column_width = rows.saturating_mul(2);
    for (&query_bits, column) in query.iter().zip(columns.chunks_exact(column_width)) {
        let query_value = f16_to_f32(query_bits);
        for (bytes, accumulator) in column.chunks_exact(2).zip(out.iter_mut()) {
            let Some(array) = bytes.try_into().ok() else {
                continue;
            };
            *accumulator += query_value * f16_to_f32(u16::from_le_bytes(array));
        }
    }
}

pub(super) fn vertical_i8(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(columns.len(), query.len().saturating_mul(rows));
    for (&query_value, column) in query.iter().zip(columns.chunks_exact(rows)) {
        for (&row_value, accumulator) in column.iter().zip(out.iter_mut()) {
            *accumulator += i32::from(query_value) * i32::from(row_value as i8);
        }
    }
}

pub(super) fn vertical_bit4(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(columns.len(), query.len().div_ceil(2).saturating_mul(rows));
    for (query_pair, column) in query.chunks(2).zip(columns.chunks_exact(rows)) {
        let Some(&even_query) = query_pair.first() else {
            continue;
        };
        let odd_query = query_pair.get(1).copied();
        for (&packed, accumulator) in column.iter().zip(out.iter_mut()) {
            let high = 2 * i32::from(packed >> 4) - 15;
            let low = 2 * i32::from(packed & 0x0f) - 15;
            *accumulator += i32::from(even_query) * high
                + odd_query.map_or(0, |query_value| i32::from(query_value) * low);
        }
    }
}

pub(super) fn f32_extrema_slab_bounds(
    query: &[f32],
    extrema: &[super::F32Extrema],
    dimensions_per_slab: usize,
    slab_bounds: &mut [f64],
) -> super::F32BoundTotals {
    debug_assert_eq!(query.len(), extrema.len());
    debug_assert!(dimensions_per_slab > 0);
    debug_assert_eq!(slab_bounds.len(), query.len().div_ceil(dimensions_per_slab));
    slab_bounds.fill(0.0);
    let mut maximum_contribution = 0.0_f64;
    let mut absolute_contribution = 0.0_f64;
    for (dimension, (&query_value, bounds)) in query.iter().zip(extrema).enumerate() {
        let minimum = f32::from_bits(bounds.minimum_bits);
        let maximum = f32::from_bits(bounds.maximum_bits);
        let query_value = f64::from(query_value);
        let minimum_product = query_value * f64::from(minimum);
        let maximum_product = query_value * f64::from(maximum);
        let contribution = minimum_product.max(maximum_product);
        maximum_contribution += contribution;
        absolute_contribution += minimum_product.abs().max(maximum_product.abs());
        if let Some(slab) = slab_bounds.get_mut(dimension / dimensions_per_slab) {
            *slab += contribution;
        }
    }
    super::F32BoundTotals {
        maximum_contribution,
        absolute_contribution,
    }
}

pub(super) fn max_f32(values: &[f32]) -> f32 {
    values.iter().copied().fold(f32::NEG_INFINITY, f32::max)
}

pub(super) fn max_i32(values: &[i32]) -> i32 {
    values.iter().copied().max().unwrap_or(i32::MIN)
}

fn dot_packed_batch(
    q: &[i8],
    rows: &[u8],
    d: usize,
    out: &mut [i32],
    fields_per_byte: usize,
    dot: fn(&[i8], &[u8]) -> i32,
) {
    debug_assert_eq!(q.len(), d, "query dimension must be pre-validated");
    debug_assert!(d <= MAX_DOT_I8_DIMENSION);
    let row_bytes = d.div_ceil(fields_per_byte);
    debug_assert_eq!(
        rows.len(),
        row_bytes.saturating_mul(out.len()),
        "batch shape must be pre-validated"
    );
    if row_bytes == 0 {
        out.fill(0);
        return;
    }
    for (row, result) in rows.chunks_exact(row_bytes).zip(out.iter_mut()) {
        *result = dot(q, row);
    }
}

pub(super) fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = (bits >> 10) & 0x1f;
    let fraction = bits & 0x03ff;
    match exponent {
        0 if fraction == 0 => f32::from_bits(sign),
        0 => {
            // Every f16 subnormal is fraction * 2^-24. The integer conversion
            // and power-of-two scaling are exact in f32.
            let magnitude = f32::from(fraction) * 2.0_f32.powi(-24);
            if sign == 0 { magnitude } else { -magnitude }
        }
        0x1f => f32::from_bits(sign | 0x7f80_0000 | (u32::from(fraction) << 13)),
        _ => f32::from_bits(
            sign | (u32::from(exponent + (127 - 15)) << 23) | (u32::from(fraction) << 13),
        ),
    }
}
