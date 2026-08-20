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
        dot_bit2,
        dot_bit4,
        dot_bit2_batch,
        dot_bit4_batch,
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

pub(super) fn dot_bit2(q: &[i8], codes: &[u8]) -> i32 {
    const SHIFTS: [u32; 4] = [6, 4, 2, 0];
    debug_assert_eq!(codes.len(), q.len().div_ceil(4));
    debug_assert!(q.len() <= MAX_DOT_I8_DIMENSION);
    codes
        .iter()
        .zip(q.chunks(4))
        .map(|(&packed, query)| {
            query
                .iter()
                .zip(SHIFTS)
                .map(|(&query_value, shift)| {
                    let code = i32::from((packed >> shift) & 0x03);
                    i32::from(query_value) * (2 * code - 3)
                })
                .sum::<i32>()
        })
        .sum()
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

pub(super) fn dot_bit2_batch(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    dot_packed_batch(q, rows, d, out, 4, dot_bit2);
}

pub(super) fn dot_bit4_batch(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    dot_packed_batch(q, rows, d, out, 2, dot_bit4);
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
