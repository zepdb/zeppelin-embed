//! AArch64 NEON kernels selected only after runtime feature detection.

use std::arch::aarch64::*;
use std::arch::asm;

use super::{
    InstructionTier, KernelArm, KernelFeatures, KernelTable, MAX_DOT_I8_DIMENSION, scalar,
};

const I8_LANES: usize = 16;
// Retained by tasks/evidence/opt-ledger/B1.md iterations 1–5.
const I8_UNROLL: usize = 4;
const I8_BLOCK: usize = I8_LANES * I8_UNROLL;
const F32_LANES: usize = 4;
const FLOAT_UNROLL: usize = 4; // f16/FHM remained untested after B1 stagnation.
const FLOAT_ACCUMULATORS: usize = 4; // f16/FHM remained untested after B1 stagnation.
const FLOAT_BLOCK: usize = F32_LANES * FLOAT_UNROLL;
const _: [(); FLOAT_UNROLL] = [(); FLOAT_ACCUMULATORS];
const HAMMING_LANES: usize = 16;
const HAMMING_UNROLL: usize = 4; // u1 variants remained untested after B1 stagnation.
const HAMMING_BLOCK: usize = HAMMING_LANES * HAMMING_UNROLL;
const PACKED_LANES: usize = 16;
const BIT4_DIMENSIONS_PER_BLOCK: usize = PACKED_LANES * 2;
// Retained by tasks/evidence/opt-ledger/B1-bit4.md iterations 1-10.
const BIT4_RAW_DOTPROD_BLOCK: usize = BIT4_DIMENSIONS_PER_BLOCK * 4;
const BIT4_PREPARED_DOTPROD_BLOCK: usize = BIT4_DIMENSIONS_PER_BLOCK * 4;
const WIDE_STREAM_LANES: usize = 16;
const WIDE_STREAM_UNROLL: usize = 4;
const WIDE_STREAM_BLOCK: usize = WIDE_STREAM_LANES * WIDE_STREAM_UNROLL;

pub(super) fn table(features: KernelFeatures) -> KernelTable {
    if features.dotprod {
        dotprod_table(features)
    } else {
        widen_table(features)
    }
}

pub(super) fn dotprod_table(features: KernelFeatures) -> KernelTable {
    KernelTable {
        arm: KernelArm::Neon,
        tier: InstructionTier::NeonDotprod,
        dot_i8: dot_i8_dotprod,
        dot_f32,
        dot_f16: f16_kernel(features),
        hamming_u1,
        dot_i8_batch: dot_i8_batch_dotprod,
        hamming_u1_batch,
        dot_bit4: dot_bit4_dotprod,
        dot_bit4_prepared: dot_bit4_prepared_dotprod,
        dot_bit4_batch: dot_bit4_batch_dotprod,
        score_bit4_prepared_batch: score_bit4_prepared_batch_dotprod,
        vertical_f32: vertical_f32_r32,
        vertical_f16: vertical_f16_kernel(features, 32),
        vertical_i8: vertical_i8_dotprod_r32,
        vertical_bit4: vertical_bit4_dotprod_r32,
        f32_extrema_slab_bounds,
        max_f32,
        max_i32,
        vertical_rows_per_tile: 32,
    }
}

pub(super) fn i8mm_table(features: KernelFeatures) -> KernelTable {
    KernelTable {
        arm: KernelArm::Neon,
        tier: InstructionTier::NeonI8mmReserved,
        dot_i8: dot_i8_dotprod,
        dot_f32,
        dot_f16: f16_kernel(features),
        hamming_u1,
        dot_i8_batch: dot_i8_batch_i8mm,
        hamming_u1_batch,
        dot_bit4: dot_bit4_dotprod,
        dot_bit4_prepared: dot_bit4_prepared_dotprod,
        dot_bit4_batch: dot_bit4_batch_dotprod,
        score_bit4_prepared_batch: score_bit4_prepared_batch_dotprod,
        vertical_f32: vertical_f32_r32,
        vertical_f16: vertical_f16_kernel(features, 32),
        vertical_i8: vertical_i8_dotprod_r32,
        vertical_bit4: vertical_bit4_dotprod_r32,
        f32_extrema_slab_bounds,
        max_f32,
        max_i32,
        vertical_rows_per_tile: 32,
    }
}

pub(super) fn dotprod_u2_table(features: KernelFeatures) -> KernelTable {
    dotprod_shape_table(features, dot_i8_dotprod_u2, dot_i8_batch_dotprod_u2)
}

pub(super) fn dotprod_u6_table(features: KernelFeatures) -> KernelTable {
    dotprod_shape_table(features, dot_i8_dotprod_u6, dot_i8_batch_dotprod_u6)
}

pub(super) fn dotprod_u8_table(features: KernelFeatures) -> KernelTable {
    dotprod_shape_table(features, dot_i8_dotprod_u8, dot_i8_batch_dotprod_u8)
}

pub(super) fn dotprod_prefetch_table(features: KernelFeatures) -> KernelTable {
    dotprod_shape_table(features, dot_i8_dotprod, dot_i8_batch_dotprod_prefetch)
}

fn dotprod_shape_table(
    features: KernelFeatures,
    dot_i8: super::DotI8Fn,
    dot_i8_batch: super::DotI8BatchFn,
) -> KernelTable {
    KernelTable {
        arm: KernelArm::Neon,
        tier: InstructionTier::NeonDotprod,
        dot_i8,
        dot_f32,
        dot_f16: f16_kernel(features),
        hamming_u1,
        dot_i8_batch,
        hamming_u1_batch,
        dot_bit4: dot_bit4_dotprod,
        dot_bit4_prepared: dot_bit4_prepared_dotprod,
        dot_bit4_batch: dot_bit4_batch_dotprod,
        score_bit4_prepared_batch: score_bit4_prepared_batch_dotprod,
        vertical_f32: vertical_f32_r32,
        vertical_f16: vertical_f16_kernel(features, 32),
        vertical_i8: vertical_i8_dotprod_r32,
        vertical_bit4: vertical_bit4_dotprod_r32,
        f32_extrema_slab_bounds,
        max_f32,
        max_i32,
        vertical_rows_per_tile: 32,
    }
}

pub(super) fn widen_table(features: KernelFeatures) -> KernelTable {
    KernelTable {
        arm: KernelArm::Neon,
        tier: InstructionTier::NeonWiden,
        dot_i8: dot_i8_widen,
        dot_f32,
        dot_f16: f16_kernel(features),
        hamming_u1,
        dot_i8_batch: dot_i8_batch_widen,
        hamming_u1_batch,
        dot_bit4: dot_bit4_widen,
        dot_bit4_prepared: dot_bit4_prepared_widen,
        dot_bit4_batch: dot_bit4_batch_widen,
        score_bit4_prepared_batch: scalar::score_bit4_prepared_batch,
        vertical_f32: vertical_f32_r32,
        vertical_f16: vertical_f16_kernel(features, 32),
        vertical_i8: vertical_i8_widen_r32,
        vertical_bit4: vertical_bit4_widen_r32,
        f32_extrema_slab_bounds,
        max_f32,
        max_i32,
        vertical_rows_per_tile: 32,
    }
}

pub(super) fn vertical_r16_table(features: KernelFeatures) -> KernelTable {
    let mut table = if features.dotprod {
        dotprod_table(features)
    } else {
        widen_table(features)
    };
    table.vertical_f32 = vertical_f32_r16;
    table.vertical_f16 = vertical_f16_kernel(features, 16);
    table.vertical_i8 = if features.dotprod {
        vertical_i8_dotprod_r16
    } else {
        vertical_i8_widen_r16
    };
    table.vertical_bit4 = if features.dotprod {
        vertical_bit4_dotprod_r16
    } else {
        vertical_bit4_widen_r16
    };
    table.vertical_rows_per_tile = 16;
    table
}

#[inline]
fn dot_i8_dotprod(a: &[i8], b: &[i8]) -> i32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    debug_assert!(
        a.len() <= MAX_DOT_I8_DIMENSION,
        "i8 dimension must be pre-validated"
    );
    // SAFETY: dispatch installs this function only after runtime DotProd
    // detection; slice lengths and the supported dimension are asserted above.
    unsafe { dot_i8_dotprod_inner(a, b) }
}

#[target_feature(enable = "dotprod")]
unsafe fn dot_i8_dotprod_inner(a: &[i8], b: &[i8]) -> i32 {
    // SAFETY: complete I8_BLOCK iterations keep all four unaligned 16-byte
    // loads in both live slices. DotProd execution was checked by the wrapper.
    unsafe {
        let mut acc0 = vdupq_n_s32(0);
        let mut acc1 = vdupq_n_s32(0);
        let mut acc2 = vdupq_n_s32(0);
        let mut acc3 = vdupq_n_s32(0);
        let processed = a.len() / I8_BLOCK * I8_BLOCK;
        let mut base = 0_usize;
        while base < processed {
            let a_ptr = a.as_ptr().wrapping_add(base);
            let b_ptr = b.as_ptr().wrapping_add(base);
            acc0 = dotprod_mac(acc0, vld1q_s8(a_ptr), vld1q_s8(b_ptr));
            acc1 = dotprod_mac(
                acc1,
                vld1q_s8(a_ptr.wrapping_add(I8_LANES)),
                vld1q_s8(b_ptr.wrapping_add(I8_LANES)),
            );
            acc2 = dotprod_mac(
                acc2,
                vld1q_s8(a_ptr.wrapping_add(I8_LANES * 2)),
                vld1q_s8(b_ptr.wrapping_add(I8_LANES * 2)),
            );
            acc3 = dotprod_mac(
                acc3,
                vld1q_s8(a_ptr.wrapping_add(I8_LANES * 3)),
                vld1q_s8(b_ptr.wrapping_add(I8_LANES * 3)),
            );
            base += I8_BLOCK;
        }
        let vectors = vaddq_s32(vaddq_s32(acc0, acc1), vaddq_s32(acc2, acc3));
        let sum = vaddvq_s32(vectors);
        add_i8_tail(sum, a, b, processed)
    }
}

fn dot_i8_dotprod_u2(a: &[i8], b: &[i8]) -> i32 {
    dot_i8_dotprod_shape::<2>(a, b)
}

fn dot_i8_dotprod_u6(a: &[i8], b: &[i8]) -> i32 {
    dot_i8_dotprod_shape::<6>(a, b)
}

fn dot_i8_dotprod_u8(a: &[i8], b: &[i8]) -> i32 {
    dot_i8_dotprod_shape::<8>(a, b)
}

fn dot_i8_dotprod_shape<const ACCUMULATORS: usize>(a: &[i8], b: &[i8]) -> i32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    debug_assert!(
        a.len() <= MAX_DOT_I8_DIMENSION,
        "i8 dimension must be pre-validated"
    );
    // SAFETY: each generated table is installed only after runtime DotProd
    // detection; equal-length live slices bound every complete vector load.
    unsafe { dot_i8_dotprod_shape_inner::<ACCUMULATORS>(a, b) }
}

#[target_feature(enable = "dotprod")]
unsafe fn dot_i8_dotprod_shape_inner<const ACCUMULATORS: usize>(a: &[i8], b: &[i8]) -> i32 {
    debug_assert!(ACCUMULATORS > 0);
    let block = I8_LANES * ACCUMULATORS;
    let processed = a.len() / block * block;
    let mut accumulators = [vdupq_n_s32(0); ACCUMULATORS];
    let mut base = 0_usize;
    while base < processed {
        for (index, accumulator) in accumulators.iter_mut().enumerate() {
            let offset = base + index * I8_LANES;
            // SAFETY: offset is within a complete `block` in both live slices.
            let left = unsafe { vld1q_s8(a.as_ptr().add(offset)) };
            let right = unsafe { vld1q_s8(b.as_ptr().add(offset)) };
            *accumulator = unsafe { dotprod_mac(*accumulator, left, right) };
        }
        base += block;
    }
    let vectors = accumulators
        .into_iter()
        .fold(vdupq_n_s32(0), |sum, accumulator| {
            vaddq_s32(sum, accumulator)
        });
    add_i8_tail(vaddvq_s32(vectors), a, b, processed)
}

#[target_feature(enable = "dotprod")]
unsafe fn dotprod_mac(mut acc: int32x4_t, a: int8x16_t, b: int8x16_t) -> int32x4_t {
    // SAFETY: the caller established FEAT_DotProd and supplies initialized
    // vector registers. The instruction touches no memory and preserves the
    // exact signed-byte SDOT semantics of unstable `vdotq_s32` on stable Rust.
    unsafe {
        asm!(
            "sdot {acc:v}.4s, {a:v}.16b, {b:v}.16b",
            acc = inout(vreg) acc,
            a = in(vreg) a,
            b = in(vreg) b,
            options(pure, nomem, nostack)
        );
    }
    acc
}

#[inline]
fn dot_i8_widen(a: &[i8], b: &[i8]) -> i32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    debug_assert!(
        a.len() <= MAX_DOT_I8_DIMENSION,
        "i8 dimension must be pre-validated"
    );
    // SAFETY: dispatch installs this function only after runtime NEON
    // detection; slice lengths and the supported dimension are asserted above.
    unsafe { dot_i8_widen_inner(a, b) }
}

#[target_feature(enable = "neon")]
unsafe fn dot_i8_widen_inner(a: &[i8], b: &[i8]) -> i32 {
    // SAFETY: complete I8_BLOCK iterations keep all four unaligned 16-byte
    // loads within both live slices. The wrapper established NEON support.
    unsafe {
        let mut acc0 = vdupq_n_s32(0);
        let mut acc1 = vdupq_n_s32(0);
        let mut acc2 = vdupq_n_s32(0);
        let mut acc3 = vdupq_n_s32(0);
        let processed = a.len() / I8_BLOCK * I8_BLOCK;
        let mut base = 0_usize;
        while base < processed {
            let a_ptr = a.as_ptr().wrapping_add(base);
            let b_ptr = b.as_ptr().wrapping_add(base);
            acc0 = widen_mac(acc0, vld1q_s8(a_ptr), vld1q_s8(b_ptr));
            acc1 = widen_mac(
                acc1,
                vld1q_s8(a_ptr.wrapping_add(I8_LANES)),
                vld1q_s8(b_ptr.wrapping_add(I8_LANES)),
            );
            acc2 = widen_mac(
                acc2,
                vld1q_s8(a_ptr.wrapping_add(I8_LANES * 2)),
                vld1q_s8(b_ptr.wrapping_add(I8_LANES * 2)),
            );
            acc3 = widen_mac(
                acc3,
                vld1q_s8(a_ptr.wrapping_add(I8_LANES * 3)),
                vld1q_s8(b_ptr.wrapping_add(I8_LANES * 3)),
            );
            base += I8_BLOCK;
        }
        let vectors = vaddq_s32(vaddq_s32(acc0, acc1), vaddq_s32(acc2, acc3));
        let sum = vaddvq_s32(vectors);
        add_i8_tail(sum, a, b, processed)
    }
}

#[target_feature(enable = "neon")]
unsafe fn widen_mac(acc: int32x4_t, a: int8x16_t, b: int8x16_t) -> int32x4_t {
    let low_products = vmull_s8(vget_low_s8(a), vget_low_s8(b));
    let high_products = vmull_s8(vget_high_s8(a), vget_high_s8(b));
    vaddq_s32(
        acc,
        vaddq_s32(vpaddlq_s16(low_products), vpaddlq_s16(high_products)),
    )
}

fn add_i8_tail(sum: i32, a: &[i8], b: &[i8], processed: usize) -> i32 {
    let Some(a_tail) = a.get(processed..) else {
        return sum;
    };
    let Some(b_tail) = b.get(processed..) else {
        return sum;
    };
    sum + a_tail
        .iter()
        .zip(b_tail)
        .map(|(&left, &right)| i32::from(left) * i32::from(right))
        .sum::<i32>()
}

#[inline]
fn dot_bit4_dotprod(q: &[i8], codes: &[u8]) -> i32 {
    packed_shape_debug_assertions(q, codes, 2);
    // SAFETY: the DotProd table is installed only after runtime feature
    // detection; complete packed blocks bound every query and code load.
    unsafe { dot_bit4_dotprod_inner(q, codes) }
}

#[target_feature(enable = "dotprod")]
unsafe fn dot_bit4_dotprod_inner(q: &[i8], codes: &[u8]) -> i32 {
    // SAFETY: each iteration consumes exactly 64 code bytes and 128 query
    // bytes. Shift/mask extraction stays in registers, and the wrapper
    // established FEAT_DotProd before any SDOT instruction is reached.
    unsafe {
        let processed = q.len() / BIT4_RAW_DOTPROD_BLOCK * BIT4_RAW_DOTPROD_BLOCK;
        let mut acc0 = vdupq_n_s32(0);
        let mut acc1 = vdupq_n_s32(0);
        let mut acc2 = vdupq_n_s32(0);
        let mut acc3 = vdupq_n_s32(0);
        let mut acc4 = vdupq_n_s32(0);
        let mut acc5 = vdupq_n_s32(0);
        let mut acc6 = vdupq_n_s32(0);
        let mut acc7 = vdupq_n_s32(0);
        let mut base = 0_usize;
        while base < processed {
            let code_ptr = codes.as_ptr().wrapping_add(base / 2);
            let (values0, values1) = unpack_bit4_block(code_ptr);
            let (values2, values3) = unpack_bit4_block(code_ptr.wrapping_add(PACKED_LANES));
            let (values4, values5) = unpack_bit4_block(code_ptr.wrapping_add(PACKED_LANES * 2));
            let (values6, values7) = unpack_bit4_block(code_ptr.wrapping_add(PACKED_LANES * 3));
            let query_ptr = q.as_ptr().wrapping_add(base);
            acc0 = dotprod_mac(acc0, vld1q_s8(query_ptr), values0);
            acc1 = dotprod_mac(acc1, vld1q_s8(query_ptr.wrapping_add(I8_LANES)), values1);
            acc2 = dotprod_mac(
                acc2,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 2)),
                values2,
            );
            acc3 = dotprod_mac(
                acc3,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 3)),
                values3,
            );
            acc4 = dotprod_mac(
                acc4,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 4)),
                values4,
            );
            acc5 = dotprod_mac(
                acc5,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 5)),
                values5,
            );
            acc6 = dotprod_mac(
                acc6,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 6)),
                values6,
            );
            acc7 = dotprod_mac(
                acc7,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 7)),
                values7,
            );
            base += BIT4_RAW_DOTPROD_BLOCK;
        }
        let vectors = vaddq_s32(
            vaddq_s32(vaddq_s32(acc0, acc1), vaddq_s32(acc2, acc3)),
            vaddq_s32(vaddq_s32(acc4, acc5), vaddq_s32(acc6, acc7)),
        );
        packed_bit4_tail(vaddvq_s32(vectors), q, codes, processed)
    }
}

#[inline]
fn dot_bit4_prepared_dotprod(q: &[i8], query_sum: i32, codes: &[u8]) -> i32 {
    packed_shape_debug_assertions(q, codes, 2);
    // SAFETY: the DotProd table is installed only after runtime feature
    // detection; complete packed blocks bound every query and code load.
    unsafe { dot_bit4_prepared_dotprod_inner(q, query_sum, codes) }
}

#[target_feature(enable = "dotprod")]
unsafe fn dot_bit4_prepared_dotprod_inner(q: &[i8], query_sum: i32, codes: &[u8]) -> i32 {
    // SAFETY: each iteration consumes exactly 64 code bytes and 128 query
    // bytes. Query preparation groups even then odd coordinates per 32-byte
    // block, so high and low nibbles can feed SDOT without per-row ZIPs.
    unsafe {
        let processed = q.len() / BIT4_PREPARED_DOTPROD_BLOCK * BIT4_PREPARED_DOTPROD_BLOCK;
        let mut acc0 = vdupq_n_s32(0);
        let mut acc1 = vdupq_n_s32(0);
        let mut acc2 = vdupq_n_s32(0);
        let mut acc3 = vdupq_n_s32(0);
        let mut acc4 = vdupq_n_s32(0);
        let mut acc5 = vdupq_n_s32(0);
        let mut acc6 = vdupq_n_s32(0);
        let mut acc7 = vdupq_n_s32(0);
        let mut base = 0_usize;
        while base < processed {
            let code_ptr = codes.as_ptr().wrapping_add(base / 2);
            let query_ptr = q.as_ptr().wrapping_add(base);
            let (values0, values1) = unpack_bit4_split_block(code_ptr);
            let (values2, values3) = unpack_bit4_split_block(code_ptr.wrapping_add(PACKED_LANES));
            let (values4, values5) =
                unpack_bit4_split_block(code_ptr.wrapping_add(PACKED_LANES * 2));
            let (values6, values7) =
                unpack_bit4_split_block(code_ptr.wrapping_add(PACKED_LANES * 3));
            acc0 = dotprod_mac(acc0, vld1q_s8(query_ptr), values0);
            acc1 = dotprod_mac(acc1, vld1q_s8(query_ptr.wrapping_add(I8_LANES)), values1);
            acc2 = dotprod_mac(
                acc2,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 2)),
                values2,
            );
            acc3 = dotprod_mac(
                acc3,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 3)),
                values3,
            );
            acc4 = dotprod_mac(
                acc4,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 4)),
                values4,
            );
            acc5 = dotprod_mac(
                acc5,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 5)),
                values5,
            );
            acc6 = dotprod_mac(
                acc6,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 6)),
                values6,
            );
            acc7 = dotprod_mac(
                acc7,
                vld1q_s8(query_ptr.wrapping_add(I8_LANES * 7)),
                values7,
            );
            base += BIT4_PREPARED_DOTPROD_BLOCK;
        }
        let vectors = vaddq_s32(
            vaddq_s32(vaddq_s32(acc0, acc1), vaddq_s32(acc2, acc3)),
            vaddq_s32(vaddq_s32(acc4, acc5), vaddq_s32(acc6, acc7)),
        );
        packed_bit4_prepared_tail(vaddvq_s32(vectors), q, query_sum, codes, processed)
    }
}

#[inline]
fn dot_bit4_widen(q: &[i8], codes: &[u8]) -> i32 {
    packed_shape_debug_assertions(q, codes, 2);
    // SAFETY: the NEON table is installed only after runtime feature
    // detection; complete packed blocks bound every query and code load.
    unsafe { dot_bit4_widen_inner(q, codes) }
}

#[target_feature(enable = "neon")]
unsafe fn dot_bit4_widen_inner(q: &[i8], codes: &[u8]) -> i32 {
    // SAFETY: each iteration consumes exactly 16 code bytes and 32 query
    // bytes. Shift/mask extraction and widening MACs stay in registers.
    unsafe {
        let processed = q.len() / BIT4_DIMENSIONS_PER_BLOCK * BIT4_DIMENSIONS_PER_BLOCK;
        let mut acc = vdupq_n_s32(0);
        let mut base = 0_usize;
        while base < processed {
            let code_ptr = codes.as_ptr().wrapping_add(base / 2);
            let (values0, values1) = unpack_bit4_block(code_ptr);
            let query_ptr = q.as_ptr().wrapping_add(base);
            acc = widen_mac(acc, vld1q_s8(query_ptr), values0);
            acc = widen_mac(acc, vld1q_s8(query_ptr.wrapping_add(I8_LANES)), values1);
            base += BIT4_DIMENSIONS_PER_BLOCK;
        }
        packed_bit4_tail(vaddvq_s32(acc), q, codes, processed)
    }
}

#[inline]
fn dot_bit4_prepared_widen(q: &[i8], query_sum: i32, codes: &[u8]) -> i32 {
    packed_shape_debug_assertions(q, codes, 2);
    // SAFETY: the NEON table is installed only after runtime feature
    // detection; complete packed blocks bound every query and code load.
    unsafe { dot_bit4_prepared_widen_inner(q, query_sum, codes) }
}

#[target_feature(enable = "neon")]
unsafe fn dot_bit4_prepared_widen_inner(q: &[i8], query_sum: i32, codes: &[u8]) -> i32 {
    // SAFETY: each iteration consumes one complete 16-byte packed block and
    // its matching block-interleaved 32-byte query block.
    unsafe {
        let processed = q.len() / BIT4_DIMENSIONS_PER_BLOCK * BIT4_DIMENSIONS_PER_BLOCK;
        let mut acc0 = vdupq_n_s32(0);
        let mut acc1 = vdupq_n_s32(0);
        let mut base = 0_usize;
        while base < processed {
            let code_ptr = codes.as_ptr().wrapping_add(base / 2);
            let (values0, values1) = unpack_bit4_split_block(code_ptr);
            let query_ptr = q.as_ptr().wrapping_add(base);
            acc0 = widen_mac(acc0, vld1q_s8(query_ptr), values0);
            acc1 = widen_mac(acc1, vld1q_s8(query_ptr.wrapping_add(I8_LANES)), values1);
            base += BIT4_DIMENSIONS_PER_BLOCK;
        }
        packed_bit4_prepared_tail(
            vaddvq_s32(vaddq_s32(acc0, acc1)),
            q,
            query_sum,
            codes,
            processed,
        )
    }
}

#[target_feature(enable = "neon")]
unsafe fn unpack_bit4_block(code_ptr: *const u8) -> (int8x16_t, int8x16_t) {
    // SAFETY: callers provide a pointer to a complete 16-byte packed block.
    // One byte ZIP restores high-nibble, low-nibble dimension order entirely
    // within vector registers.
    unsafe {
        let packed = vld1q_u8(code_ptr);
        let mask = vdupq_n_u8(0x0f);
        let high = vshrq_n_u8(packed, 4);
        let low = vandq_u8(packed, mask);
        (
            bit4_grid(vzip1q_u8(high, low)),
            bit4_grid(vzip2q_u8(high, low)),
        )
    }
}

#[target_feature(enable = "neon")]
unsafe fn unpack_bit4_split_block(code_ptr: *const u8) -> (int8x16_t, int8x16_t) {
    // SAFETY: callers provide a pointer to a complete 16-byte packed block.
    unsafe {
        let packed = vld1q_u8(code_ptr);
        let mask = vdupq_n_u8(0x0f);
        (
            vreinterpretq_s8_u8(vshrq_n_u8(packed, 4)),
            vreinterpretq_s8_u8(vandq_u8(packed, mask)),
        )
    }
}

#[target_feature(enable = "neon")]
unsafe fn bit4_grid(fields: uint8x16_t) -> int8x16_t {
    vreinterpretq_s8_u8(vsubq_u8(vshlq_n_u8(fields, 1), vdupq_n_u8(15)))
}

fn packed_shape_debug_assertions(q: &[i8], codes: &[u8], fields_per_byte: usize) {
    debug_assert_eq!(codes.len(), q.len().div_ceil(fields_per_byte));
    debug_assert!(q.len() <= MAX_DOT_I8_DIMENSION);
}

fn packed_bit4_tail(sum: i32, q: &[i8], codes: &[u8], processed: usize) -> i32 {
    let Some(q_tail) = q.get(processed..) else {
        return sum;
    };
    let Some(code_tail) = codes.get(processed / 2..) else {
        return sum;
    };
    sum + scalar::dot_bit4(q_tail, code_tail)
}

fn packed_bit4_prepared_tail(
    sum: i32,
    q: &[i8],
    query_sum: i32,
    codes: &[u8],
    processed: usize,
) -> i32 {
    let Some(q_tail) = q.get(processed..) else {
        return sum;
    };
    let Some(code_tail) = codes.get(processed / 2..) else {
        return sum;
    };
    let unsigned_sum = sum + scalar::dot_bit4_prepared_unsigned(q_tail, code_tail);
    2 * unsigned_sum - 15 * query_sum
}

#[inline]
fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    // SAFETY: dispatch establishes NEON support. Complete blocks bound every
    // vector load and the checked tail slices remain within both inputs.
    unsafe { dot_f32_inner(a, b) }
}

#[target_feature(enable = "neon")]
unsafe fn dot_f32_inner(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: each complete FLOAT_BLOCK iteration keeps all four unaligned
    // vector loads in both live slices. Dispatch established NEON support.
    unsafe {
        let mut acc0 = vdupq_n_f32(0.0);
        let mut acc1 = vdupq_n_f32(0.0);
        let mut acc2 = vdupq_n_f32(0.0);
        let mut acc3 = vdupq_n_f32(0.0);
        let processed = a.len() / FLOAT_BLOCK * FLOAT_BLOCK;
        let mut base = 0_usize;
        while base < processed {
            let a_ptr = a.as_ptr().wrapping_add(base);
            let b_ptr = b.as_ptr().wrapping_add(base);
            acc0 = vfmaq_f32(acc0, vld1q_f32(a_ptr), vld1q_f32(b_ptr));
            acc1 = vfmaq_f32(
                acc1,
                vld1q_f32(a_ptr.wrapping_add(F32_LANES)),
                vld1q_f32(b_ptr.wrapping_add(F32_LANES)),
            );
            acc2 = vfmaq_f32(
                acc2,
                vld1q_f32(a_ptr.wrapping_add(F32_LANES * 2)),
                vld1q_f32(b_ptr.wrapping_add(F32_LANES * 2)),
            );
            acc3 = vfmaq_f32(
                acc3,
                vld1q_f32(a_ptr.wrapping_add(F32_LANES * 3)),
                vld1q_f32(b_ptr.wrapping_add(F32_LANES * 3)),
            );
            base += FLOAT_BLOCK;
        }
        let vectors = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
        add_f32_tail(vaddvq_f32(vectors), a, b, processed)
    }
}

#[inline]
fn f16_kernel(features: KernelFeatures) -> super::DotF16Fn {
    if features.fp16 {
        dot_f16_fp16
    } else {
        dot_f16_fallback
    }
}

#[inline]
fn dot_f16_fp16(a: &[u16], b: &[u16]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    // SAFETY: dispatch installs this function only after runtime FP16
    // detection; complete blocks bound every load within both live slices.
    unsafe { dot_f16_fp16_inner(a, b) }
}

#[target_feature(enable = "neon,fp16")]
unsafe fn dot_f16_fp16_inner(a: &[u16], b: &[u16]) -> f32 {
    // SAFETY: each complete FLOAT_BLOCK iteration keeps all four 4-half loads
    // within both live slices. Runtime dispatch established FP16 support.
    unsafe {
        let mut acc0 = vdupq_n_f32(0.0);
        let mut acc1 = vdupq_n_f32(0.0);
        let mut acc2 = vdupq_n_f32(0.0);
        let mut acc3 = vdupq_n_f32(0.0);
        let processed = a.len() / FLOAT_BLOCK * FLOAT_BLOCK;
        let mut base = 0_usize;
        while base < processed {
            let a_ptr = a.as_ptr().wrapping_add(base);
            let b_ptr = b.as_ptr().wrapping_add(base);
            acc0 = f16_mac(acc0, a_ptr, b_ptr);
            acc1 = f16_mac(
                acc1,
                a_ptr.wrapping_add(F32_LANES),
                b_ptr.wrapping_add(F32_LANES),
            );
            acc2 = f16_mac(
                acc2,
                a_ptr.wrapping_add(F32_LANES * 2),
                b_ptr.wrapping_add(F32_LANES * 2),
            );
            acc3 = f16_mac(
                acc3,
                a_ptr.wrapping_add(F32_LANES * 3),
                b_ptr.wrapping_add(F32_LANES * 3),
            );
            base += FLOAT_BLOCK;
        }
        let vectors = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
        add_f16_tail(vaddvq_f32(vectors), a, b, processed)
    }
}

#[target_feature(enable = "neon,fp16")]
unsafe fn f16_mac(acc: float32x4_t, a: *const u16, b: *const u16) -> float32x4_t {
    // SAFETY: callers provide pointers to at least four initialized u16 values.
    // FP16 dispatch permits FCVTL, which converts all IEEE binary16 classes,
    // including subnormals, infinities, and NaNs, to their binary32 forms.
    unsafe { vfmaq_f32(acc, f16x4_to_f32(vld1_u16(a)), f16x4_to_f32(vld1_u16(b))) }
}

#[target_feature(enable = "neon,fp16")]
unsafe fn f16x4_to_f32(input: uint16x4_t) -> float32x4_t {
    let output: float32x4_t;
    // SAFETY: runtime FP16 detection permits FCVTL. Both operands are vector
    // registers and the instruction touches no memory. Inline assembly avoids
    // the still-unstable stable-Rust `vcvt_f32_f16` intrinsic.
    unsafe {
        asm!(
            "fcvtl {output:v}.4s, {input:v}.4h",
            output = out(vreg) output,
            input = in(vreg) input,
            options(pure, nomem, nostack)
        );
    }
    output
}

#[inline]
fn dot_f16_fallback(a: &[u16], b: &[u16]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    // SAFETY: dispatch established NEON support. Converted stack arrays contain
    // four initialized f32 values before each vector load.
    unsafe { dot_f16_fallback_inner(a, b) }
}

#[target_feature(enable = "neon")]
unsafe fn dot_f16_fallback_inner(a: &[u16], b: &[u16]) -> f32 {
    // SAFETY: complete FLOAT_BLOCK iterations bound every source access and
    // each conversion array is fully initialized before its vector load.
    unsafe {
        let mut acc0 = vdupq_n_f32(0.0);
        let mut acc1 = vdupq_n_f32(0.0);
        let mut acc2 = vdupq_n_f32(0.0);
        let mut acc3 = vdupq_n_f32(0.0);
        let processed = a.len() / FLOAT_BLOCK * FLOAT_BLOCK;
        let mut base = 0_usize;
        while base < processed {
            let a_block = convert_f16_block(a, base);
            let b_block = convert_f16_block(b, base);
            acc0 = vfmaq_f32(
                acc0,
                vld1q_f32(a_block.as_ptr()),
                vld1q_f32(b_block.as_ptr()),
            );
            acc1 = vfmaq_f32(
                acc1,
                vld1q_f32(a_block.as_ptr().wrapping_add(F32_LANES)),
                vld1q_f32(b_block.as_ptr().wrapping_add(F32_LANES)),
            );
            acc2 = vfmaq_f32(
                acc2,
                vld1q_f32(a_block.as_ptr().wrapping_add(F32_LANES * 2)),
                vld1q_f32(b_block.as_ptr().wrapping_add(F32_LANES * 2)),
            );
            acc3 = vfmaq_f32(
                acc3,
                vld1q_f32(a_block.as_ptr().wrapping_add(F32_LANES * 3)),
                vld1q_f32(b_block.as_ptr().wrapping_add(F32_LANES * 3)),
            );
            base += FLOAT_BLOCK;
        }
        let vectors = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
        add_f16_tail(vaddvq_f32(vectors), a, b, processed)
    }
}

fn convert_f16_block(values: &[u16], base: usize) -> [f32; FLOAT_BLOCK] {
    let mut converted = [0.0_f32; FLOAT_BLOCK];
    let Some(block) = values.get(base..base + FLOAT_BLOCK) else {
        return converted;
    };
    for (output, &bits) in converted.iter_mut().zip(block) {
        *output = scalar::f16_to_f32(bits);
    }
    converted
}

fn add_f32_tail(sum: f32, a: &[f32], b: &[f32], processed: usize) -> f32 {
    let Some(a_tail) = a.get(processed..) else {
        return sum;
    };
    let Some(b_tail) = b.get(processed..) else {
        return sum;
    };
    a_tail
        .iter()
        .zip(b_tail)
        .fold(sum, |acc, (&left, &right)| acc + left * right)
}

fn add_f16_tail(sum: f32, a: &[u16], b: &[u16], processed: usize) -> f32 {
    let Some(a_tail) = a.get(processed..) else {
        return sum;
    };
    let Some(b_tail) = b.get(processed..) else {
        return sum;
    };
    a_tail.iter().zip(b_tail).fold(sum, |acc, (&left, &right)| {
        acc + scalar::f16_to_f32(left) * scalar::f16_to_f32(right)
    })
}

fn vertical_f32_r16(query: &[f32], columns: &[u8], rows: usize, out: &mut [f32]) {
    vertical_f32::<4>(query, columns, rows, out);
}

fn vertical_f32_r32(query: &[f32], columns: &[u8], rows: usize, out: &mut [f32]) {
    vertical_f32::<8>(query, columns, rows, out);
}

fn vertical_f32<const ACCUMULATORS: usize>(
    query: &[f32],
    columns: &[u8],
    rows: usize,
    out: &mut [f32],
) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(
        columns.len(),
        query.len().saturating_mul(rows).saturating_mul(4)
    );
    // SAFETY: runtime dispatch established NEON support. The asserted slab
    // shape bounds every complete row-tile load and store.
    unsafe { vertical_f32_inner::<ACCUMULATORS>(query, columns, rows, out) };
}

#[target_feature(enable = "neon")]
unsafe fn vertical_f32_inner<const ACCUMULATORS: usize>(
    query: &[f32],
    columns: &[u8],
    rows: usize,
    out: &mut [f32],
) {
    let tile_rows = ACCUMULATORS * F32_LANES;
    let processed_rows = rows / tile_rows * tile_rows;
    let column_width = rows * size_of::<f32>();
    let mut row_base = 0_usize;
    while row_base < processed_rows {
        let mut accumulators = [vdupq_n_f32(0.0); ACCUMULATORS];
        for (index, accumulator) in accumulators.iter_mut().enumerate() {
            // SAFETY: row_base addresses a complete tile in `out`.
            *accumulator = unsafe { vld1q_f32(out.as_ptr().add(row_base + index * F32_LANES)) };
        }
        for (dimension, &query_value) in query.iter().enumerate() {
            let query_vector = vdupq_n_f32(query_value);
            let byte_offset = dimension * column_width + row_base * size_of::<f32>();
            let column_ptr = columns.as_ptr().wrapping_add(byte_offset).cast::<f32>();
            for (index, accumulator) in accumulators.iter_mut().enumerate() {
                // SAFETY: each vector lies in the complete row tile and the
                // byte payload contains native little-endian f32 bits.
                let values = unsafe { vld1q_f32(column_ptr.add(index * F32_LANES)) };
                *accumulator = vfmaq_f32(*accumulator, values, query_vector);
            }
        }
        for (index, accumulator) in accumulators.into_iter().enumerate() {
            // SAFETY: the matching output tile is writable.
            unsafe {
                vst1q_f32(
                    out.as_mut_ptr().add(row_base + index * F32_LANES),
                    accumulator,
                )
            };
        }
        row_base += tile_rows;
    }
    vertical_f32_row_tail(query, columns, rows, processed_rows, out);
}

fn vertical_f32_row_tail(
    query: &[f32],
    columns: &[u8],
    rows: usize,
    first_row: usize,
    out: &mut [f32],
) {
    let column_width = rows.saturating_mul(size_of::<f32>());
    for (&query_value, column) in query.iter().zip(columns.chunks_exact(column_width)) {
        let Some(tail) = column.get(first_row.saturating_mul(4)..) else {
            continue;
        };
        let Some(out_tail) = out.get_mut(first_row..) else {
            continue;
        };
        for (bytes, accumulator) in tail.chunks_exact(4).zip(out_tail) {
            let Some(array) = bytes.try_into().ok() else {
                continue;
            };
            *accumulator += query_value * f32::from_bits(u32::from_le_bytes(array));
        }
    }
}

fn vertical_f16_kernel(features: KernelFeatures, tile_rows: usize) -> super::VerticalF16Fn {
    match (features.fp16, tile_rows) {
        (true, 16) => vertical_f16_fp16_r16,
        (true, _) => vertical_f16_fp16_r32,
        (false, 16) => vertical_f16_fallback_r16,
        (false, _) => vertical_f16_fallback_r32,
    }
}

fn vertical_f16_fp16_r16(query: &[u16], columns: &[u8], rows: usize, out: &mut [f32]) {
    vertical_f16_fp16::<4>(query, columns, rows, out);
}

fn vertical_f16_fp16_r32(query: &[u16], columns: &[u8], rows: usize, out: &mut [f32]) {
    vertical_f16_fp16::<8>(query, columns, rows, out);
}

fn vertical_f16_fp16<const ACCUMULATORS: usize>(
    query: &[u16],
    columns: &[u8],
    rows: usize,
    out: &mut [f32],
) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(
        columns.len(),
        query.len().saturating_mul(rows).saturating_mul(2)
    );
    // SAFETY: the selected table established NEON and FP16 support.
    unsafe { vertical_f16_fp16_inner::<ACCUMULATORS>(query, columns, rows, out) };
}

#[target_feature(enable = "neon,fp16")]
unsafe fn vertical_f16_fp16_inner<const ACCUMULATORS: usize>(
    query: &[u16],
    columns: &[u8],
    rows: usize,
    out: &mut [f32],
) {
    let tile_rows = ACCUMULATORS * F32_LANES;
    let processed_rows = rows / tile_rows * tile_rows;
    let column_width = rows * size_of::<u16>();
    let mut row_base = 0_usize;
    while row_base < processed_rows {
        let mut accumulators = [vdupq_n_f32(0.0); ACCUMULATORS];
        for (index, accumulator) in accumulators.iter_mut().enumerate() {
            // SAFETY: row_base addresses one complete output tile.
            *accumulator = unsafe { vld1q_f32(out.as_ptr().add(row_base + index * F32_LANES)) };
        }
        for (dimension, &query_bits) in query.iter().enumerate() {
            let query_vector = vdupq_n_f32(scalar::f16_to_f32(query_bits));
            let byte_offset = dimension * column_width + row_base * size_of::<u16>();
            let column_ptr = columns.as_ptr().wrapping_add(byte_offset).cast::<u16>();
            for (index, accumulator) in accumulators.iter_mut().enumerate() {
                // SAFETY: four f16 values fit in the complete row tile.
                let bits = unsafe { vld1_u16(column_ptr.add(index * F32_LANES)) };
                let values = unsafe { f16x4_to_f32(bits) };
                *accumulator = vfmaq_f32(*accumulator, values, query_vector);
            }
        }
        for (index, accumulator) in accumulators.into_iter().enumerate() {
            // SAFETY: the matching output tile is writable.
            unsafe {
                vst1q_f32(
                    out.as_mut_ptr().add(row_base + index * F32_LANES),
                    accumulator,
                )
            };
        }
        row_base += tile_rows;
    }
    vertical_f16_row_tail(query, columns, rows, processed_rows, out);
}

fn vertical_f16_fallback_r16(query: &[u16], columns: &[u8], rows: usize, out: &mut [f32]) {
    vertical_f16_fallback_columns::<4>(query, columns, rows, out);
}

fn vertical_f16_fallback_r32(query: &[u16], columns: &[u8], rows: usize, out: &mut [f32]) {
    vertical_f16_fallback_columns::<8>(query, columns, rows, out);
}

fn vertical_f16_fallback_columns<const ACCUMULATORS: usize>(
    query: &[u16],
    columns: &[u8],
    rows: usize,
    out: &mut [f32],
) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(
        columns.len(),
        query.len().saturating_mul(rows).saturating_mul(2)
    );
    // SAFETY: runtime dispatch established NEON support; f16 conversion is
    // scalar but each four-row multiply-accumulate remains vertical SIMD.
    unsafe { vertical_f16_fallback_inner::<ACCUMULATORS>(query, columns, rows, out) };
}

#[target_feature(enable = "neon")]
unsafe fn vertical_f16_fallback_inner<const ACCUMULATORS: usize>(
    query: &[u16],
    columns: &[u8],
    rows: usize,
    out: &mut [f32],
) {
    let tile_rows = ACCUMULATORS * F32_LANES;
    let processed_rows = rows / tile_rows * tile_rows;
    let column_width = rows * size_of::<u16>();
    let mut row_base = 0_usize;
    while row_base < processed_rows {
        let mut accumulators = [vdupq_n_f32(0.0); ACCUMULATORS];
        for (index, accumulator) in accumulators.iter_mut().enumerate() {
            // SAFETY: row_base addresses one complete output tile.
            *accumulator = unsafe { vld1q_f32(out.as_ptr().add(row_base + index * F32_LANES)) };
        }
        for (dimension, &query_bits) in query.iter().enumerate() {
            let query_vector = vdupq_n_f32(scalar::f16_to_f32(query_bits));
            let byte_offset = dimension * column_width + row_base * size_of::<u16>();
            for (index, accumulator) in accumulators.iter_mut().enumerate() {
                let start = byte_offset + index * F32_LANES * size_of::<u16>();
                let Some(bytes) = columns.get(start..start + F32_LANES * size_of::<u16>()) else {
                    continue;
                };
                let mut converted = [0.0_f32; F32_LANES];
                for (pair, value) in bytes.chunks_exact(2).zip(converted.iter_mut()) {
                    let Some(array) = pair.try_into().ok() else {
                        continue;
                    };
                    *value = scalar::f16_to_f32(u16::from_le_bytes(array));
                }
                // SAFETY: converted contains four initialized f32 values.
                let values = unsafe { vld1q_f32(converted.as_ptr()) };
                *accumulator = vfmaq_f32(*accumulator, values, query_vector);
            }
        }
        for (index, accumulator) in accumulators.into_iter().enumerate() {
            // SAFETY: the matching output tile is writable.
            unsafe {
                vst1q_f32(
                    out.as_mut_ptr().add(row_base + index * F32_LANES),
                    accumulator,
                )
            };
        }
        row_base += tile_rows;
    }
    vertical_f16_row_tail(query, columns, rows, processed_rows, out);
}

fn vertical_f16_row_tail(
    query: &[u16],
    columns: &[u8],
    rows: usize,
    first_row: usize,
    out: &mut [f32],
) {
    let column_width = rows.saturating_mul(size_of::<u16>());
    for (&query_bits, column) in query.iter().zip(columns.chunks_exact(column_width)) {
        let query_value = scalar::f16_to_f32(query_bits);
        let Some(tail) = column.get(first_row.saturating_mul(2)..) else {
            continue;
        };
        let Some(out_tail) = out.get_mut(first_row..) else {
            continue;
        };
        for (bytes, accumulator) in tail.chunks_exact(2).zip(out_tail) {
            let Some(array) = bytes.try_into().ok() else {
                continue;
            };
            *accumulator += query_value * scalar::f16_to_f32(u16::from_le_bytes(array));
        }
    }
}

fn vertical_i8_dotprod_r16(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    vertical_i8_dotprod::<4>(query, columns, rows, out);
}

fn vertical_i8_dotprod_r32(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    vertical_i8_dotprod::<8>(query, columns, rows, out);
}

fn vertical_i8_dotprod<const ACCUMULATORS: usize>(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    out: &mut [i32],
) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(columns.len(), query.len().saturating_mul(rows));
    // SAFETY: the selected table established DotProd support.
    unsafe { vertical_i8_dotprod_inner::<ACCUMULATORS>(query, columns, rows, out) };
}

#[target_feature(enable = "dotprod")]
unsafe fn vertical_i8_dotprod_inner<const ACCUMULATORS: usize>(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    out: &mut [i32],
) {
    debug_assert!(ACCUMULATORS.is_multiple_of(4));
    let tile_rows = ACCUMULATORS * 4;
    let processed_rows = rows / tile_rows * tile_rows;
    let processed_dimensions = query.len() / 4 * 4;
    let mut row_base = 0_usize;
    while row_base < processed_rows {
        let mut accumulators = [vdupq_n_s32(0); ACCUMULATORS];
        for (index, accumulator) in accumulators.iter_mut().enumerate() {
            // SAFETY: row_base addresses one complete output tile.
            *accumulator = unsafe { vld1q_s32(out.as_ptr().add(row_base + index * 4)) };
        }
        let mut dimension = 0_usize;
        while dimension < processed_dimensions {
            // SAFETY: four query bytes remain by processed_dimensions.
            let query_word =
                unsafe { std::ptr::read_unaligned(query.as_ptr().add(dimension).cast::<i32>()) };
            let query_vector = vreinterpretq_s8_s32(vdupq_n_s32(query_word));
            for group in 0..ACCUMULATORS / 4 {
                let group_row = row_base + group * 16;
                // SAFETY: the tile contains 16 rows in each of four complete
                // dimension columns.
                let column0 =
                    unsafe { vld1q_s8(columns.as_ptr().add(dimension * rows + group_row).cast()) };
                let column1 = unsafe {
                    vld1q_s8(
                        columns
                            .as_ptr()
                            .add((dimension + 1) * rows + group_row)
                            .cast(),
                    )
                };
                let column2 = unsafe {
                    vld1q_s8(
                        columns
                            .as_ptr()
                            .add((dimension + 2) * rows + group_row)
                            .cast(),
                    )
                };
                let column3 = unsafe {
                    vld1q_s8(
                        columns
                            .as_ptr()
                            .add((dimension + 3) * rows + group_row)
                            .cast(),
                    )
                };
                // SAFETY: all four inputs are initialized vector registers;
                // transpose_i8_16x4 touches no memory.
                let transposed = unsafe { transpose_i8_16x4(column0, column1, column2, column3) };
                let accumulator_base = group * 4;
                for (offset, values) in transposed.into_iter().enumerate() {
                    let Some(accumulator) = accumulators.get_mut(accumulator_base + offset) else {
                        continue;
                    };
                    *accumulator = unsafe { dotprod_mac(*accumulator, values, query_vector) };
                }
            }
            dimension += 4;
        }
        for (index, accumulator) in accumulators.into_iter().enumerate() {
            // SAFETY: the matching output tile is writable.
            unsafe { vst1q_s32(out.as_mut_ptr().add(row_base + index * 4), accumulator) };
        }
        row_base += tile_rows;
    }
    vertical_i8_row_tail(
        query.get(..processed_dimensions).unwrap_or_default(),
        columns,
        rows,
        processed_rows,
        out,
    );
    let query_tail = query.get(processed_dimensions..).unwrap_or_default();
    let columns_tail = columns
        .get(processed_dimensions.saturating_mul(rows)..)
        .unwrap_or_default();
    scalar::vertical_i8(query_tail, columns_tail, rows, out);
}

#[target_feature(enable = "neon")]
unsafe fn transpose_i8_16x4(
    column0: int8x16_t,
    column1: int8x16_t,
    column2: int8x16_t,
    column3: int8x16_t,
) -> [int8x16_t; 4] {
    let pairs01_low = vzip1q_s8(column0, column1);
    let pairs01_high = vzip2q_s8(column0, column1);
    let pairs23_low = vzip1q_s8(column2, column3);
    let pairs23_high = vzip2q_s8(column2, column3);
    let low01 = vreinterpretq_s16_s8(pairs01_low);
    let high01 = vreinterpretq_s16_s8(pairs01_high);
    let low23 = vreinterpretq_s16_s8(pairs23_low);
    let high23 = vreinterpretq_s16_s8(pairs23_high);
    [
        vreinterpretq_s8_s16(vzip1q_s16(low01, low23)),
        vreinterpretq_s8_s16(vzip2q_s16(low01, low23)),
        vreinterpretq_s8_s16(vzip1q_s16(high01, high23)),
        vreinterpretq_s8_s16(vzip2q_s16(high01, high23)),
    ]
}

fn vertical_i8_widen_r16(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    vertical_i8_widen::<4>(query, columns, rows, out);
}

fn vertical_i8_widen_r32(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    vertical_i8_widen::<8>(query, columns, rows, out);
}

fn vertical_i8_widen<const ACCUMULATORS: usize>(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    out: &mut [i32],
) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(columns.len(), query.len().saturating_mul(rows));
    // SAFETY: runtime dispatch established NEON support.
    unsafe { vertical_i8_widen_inner::<ACCUMULATORS>(query, columns, rows, out) };
}

#[target_feature(enable = "neon")]
unsafe fn vertical_i8_widen_inner<const ACCUMULATORS: usize>(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    out: &mut [i32],
) {
    debug_assert!(ACCUMULATORS.is_multiple_of(4));
    let tile_rows = ACCUMULATORS * 4;
    let processed_rows = rows / tile_rows * tile_rows;
    let mut row_base = 0_usize;
    while row_base < processed_rows {
        let mut accumulators = [vdupq_n_s32(0); ACCUMULATORS];
        for (index, accumulator) in accumulators.iter_mut().enumerate() {
            // SAFETY: row_base addresses one complete output tile.
            *accumulator = unsafe { vld1q_s32(out.as_ptr().add(row_base + index * 4)) };
        }
        for (dimension, &query_value) in query.iter().enumerate() {
            for group in 0..ACCUMULATORS / 4 {
                let group_row = row_base + group * 16;
                // SAFETY: group_row starts a complete 16-row source vector.
                let values =
                    unsafe { vld1q_s8(columns.as_ptr().add(dimension * rows + group_row).cast()) };
                let query_lanes = vdup_n_s8(query_value);
                let low_products = vmull_s8(vget_low_s8(values), query_lanes);
                let high_products = vmull_s8(vget_high_s8(values), query_lanes);
                let base = group * 4;
                add_widened_16(&mut accumulators, base, low_products, high_products);
            }
        }
        for (index, accumulator) in accumulators.into_iter().enumerate() {
            // SAFETY: the matching output tile is writable.
            unsafe { vst1q_s32(out.as_mut_ptr().add(row_base + index * 4), accumulator) };
        }
        row_base += tile_rows;
    }
    vertical_i8_row_tail(query, columns, rows, processed_rows, out);
}

fn vertical_i8_row_tail(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    first_row: usize,
    out: &mut [i32],
) {
    for (&query_value, column) in query.iter().zip(columns.chunks_exact(rows)) {
        let Some(column_tail) = column.get(first_row..) else {
            continue;
        };
        let Some(out_tail) = out.get_mut(first_row..) else {
            continue;
        };
        for (&row_value, accumulator) in column_tail.iter().zip(out_tail) {
            *accumulator += i32::from(query_value) * i32::from(row_value as i8);
        }
    }
}

fn vertical_bit4_dotprod_r16(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    vertical_bit4_dotprod::<1>(query, columns, rows, out);
}

fn vertical_bit4_dotprod_r32(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    vertical_bit4_dotprod::<2>(query, columns, rows, out);
}

fn vertical_bit4_dotprod<const GROUPS_PER_TILE: usize>(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    out: &mut [i32],
) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(columns.len(), query.len().div_ceil(2).saturating_mul(rows));
    // SAFETY: the selected table established DotProd support.
    unsafe { vertical_bit4_dotprod_inner::<GROUPS_PER_TILE>(query, columns, rows, out) };
}

#[target_feature(enable = "dotprod")]
unsafe fn vertical_bit4_dotprod_inner<const GROUPS_PER_TILE: usize>(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    out: &mut [i32],
) {
    let tile_rows = GROUPS_PER_TILE * 16;
    let processed_rows = rows / tile_rows * tile_rows;
    let processed_dimensions = query.len() / 8 * 8;
    let processed_query_sum = query
        .get(..processed_dimensions)
        .unwrap_or_default()
        .iter()
        .map(|&value| i32::from(value))
        .sum::<i32>();
    let bias = vdupq_n_s32(15 * processed_query_sum);
    let mut tile_base = 0_usize;
    while tile_base < processed_rows {
        for group in 0..GROUPS_PER_TILE {
            let row_base = tile_base + group * 16;
            let mut accumulators = [vdupq_n_s32(0); 4];
            let mut unsigned_sums = [vdupq_n_s32(0); 4];
            for (index, accumulator) in accumulators.iter_mut().enumerate() {
                // SAFETY: row_base starts a complete 16-row group.
                *accumulator = unsafe { vld1q_s32(out.as_ptr().add(row_base + index * 4)) };
            }
            let mut dimension = 0_usize;
            while dimension < processed_dimensions {
                let packed_column = dimension / 2;
                // SAFETY: four packed columns and 16 rows remain.
                let packed0 =
                    unsafe { vld1q_u8(columns.as_ptr().add(packed_column * rows + row_base)) };
                let packed1 = unsafe {
                    vld1q_u8(columns.as_ptr().add((packed_column + 1) * rows + row_base))
                };
                let packed2 = unsafe {
                    vld1q_u8(columns.as_ptr().add((packed_column + 2) * rows + row_base))
                };
                let packed3 = unsafe {
                    vld1q_u8(columns.as_ptr().add((packed_column + 3) * rows + row_base))
                };
                let mask = vdupq_n_u8(0x0f);
                let high0 = vreinterpretq_s8_u8(vshrq_n_u8(packed0, 4));
                let low0 = vreinterpretq_s8_u8(vandq_u8(packed0, mask));
                let high1 = vreinterpretq_s8_u8(vshrq_n_u8(packed1, 4));
                let low1 = vreinterpretq_s8_u8(vandq_u8(packed1, mask));
                let high2 = vreinterpretq_s8_u8(vshrq_n_u8(packed2, 4));
                let low2 = vreinterpretq_s8_u8(vandq_u8(packed2, mask));
                let high3 = vreinterpretq_s8_u8(vshrq_n_u8(packed3, 4));
                let low3 = vreinterpretq_s8_u8(vandq_u8(packed3, mask));
                // SAFETY: transpose helpers touch initialized registers only.
                let rows0 = unsafe { transpose_i8_16x4(high0, low0, high1, low1) };
                // SAFETY: transpose helpers touch initialized registers only.
                let rows1 = unsafe { transpose_i8_16x4(high2, low2, high3, low3) };
                // SAFETY: eight query bytes remain by processed_dimensions.
                let query0 = unsafe {
                    std::ptr::read_unaligned(query.as_ptr().add(dimension).cast::<i32>())
                };
                // SAFETY: the second four-byte query group also remains.
                let query1 = unsafe {
                    std::ptr::read_unaligned(query.as_ptr().add(dimension + 4).cast::<i32>())
                };
                let query0 = vreinterpretq_s8_s32(vdupq_n_s32(query0));
                let query1 = vreinterpretq_s8_s32(vdupq_n_s32(query1));
                for ((sum, values0), values1) in unsigned_sums.iter_mut().zip(rows0).zip(rows1) {
                    *sum = unsafe { dotprod_mac(*sum, values0, query0) };
                    *sum = unsafe { dotprod_mac(*sum, values1, query1) };
                }
                dimension += 8;
            }
            for ((index, accumulator), unsigned_sum) in
                accumulators.iter_mut().enumerate().zip(unsigned_sums)
            {
                *accumulator =
                    vaddq_s32(*accumulator, vsubq_s32(vshlq_n_s32(unsigned_sum, 1), bias));
                // SAFETY: the matching four-row output group is writable.
                unsafe { vst1q_s32(out.as_mut_ptr().add(row_base + index * 4), *accumulator) };
            }
        }
        tile_base += tile_rows;
    }
    vertical_bit4_row_tail(
        query.get(..processed_dimensions).unwrap_or_default(),
        columns,
        rows,
        processed_rows,
        out,
    );
    let query_tail = query.get(processed_dimensions..).unwrap_or_default();
    let column_tail = columns
        .get(processed_dimensions.div_ceil(2).saturating_mul(rows)..)
        .unwrap_or_default();
    scalar::vertical_bit4(query_tail, column_tail, rows, out);
}

fn vertical_bit4_widen_r16(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    vertical_bit4_widen::<4>(query, columns, rows, out);
}

fn vertical_bit4_widen_r32(query: &[i8], columns: &[u8], rows: usize, out: &mut [i32]) {
    vertical_bit4_widen::<8>(query, columns, rows, out);
}

fn vertical_bit4_widen<const ACCUMULATORS: usize>(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    out: &mut [i32],
) {
    debug_assert_eq!(out.len(), rows);
    debug_assert_eq!(columns.len(), query.len().div_ceil(2).saturating_mul(rows));
    // SAFETY: runtime dispatch established NEON support.
    unsafe { vertical_bit4_widen_inner::<ACCUMULATORS>(query, columns, rows, out) };
}

#[target_feature(enable = "neon")]
unsafe fn vertical_bit4_widen_inner<const ACCUMULATORS: usize>(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    out: &mut [i32],
) {
    debug_assert!(ACCUMULATORS.is_multiple_of(4));
    let tile_rows = ACCUMULATORS * 4;
    let processed_rows = rows / tile_rows * tile_rows;
    let mut row_base = 0_usize;
    while row_base < processed_rows {
        let mut accumulators = [vdupq_n_s32(0); ACCUMULATORS];
        for (index, accumulator) in accumulators.iter_mut().enumerate() {
            // SAFETY: row_base addresses one complete output tile.
            *accumulator = unsafe { vld1q_s32(out.as_ptr().add(row_base + index * 4)) };
        }
        for (column_index, query_pair) in query.chunks(2).enumerate() {
            let Some(&even_query) = query_pair.first() else {
                continue;
            };
            let odd_query = query_pair.get(1).copied().unwrap_or(0);
            for group in 0..ACCUMULATORS / 4 {
                let group_row = row_base + group * 16;
                // SAFETY: group_row starts a complete packed 16-row column.
                let packed =
                    unsafe { vld1q_u8(columns.as_ptr().add(column_index * rows + group_row)) };
                // SAFETY: bit4_grid touches only initialized vector registers.
                let high = unsafe { bit4_grid(vshrq_n_u8(packed, 4)) };
                // SAFETY: bit4_grid touches only initialized vector registers.
                let low = unsafe { bit4_grid(vandq_u8(packed, vdupq_n_u8(0x0f))) };
                let high_low = vmull_s8(vget_low_s8(high), vdup_n_s8(even_query));
                let high_high = vmull_s8(vget_high_s8(high), vdup_n_s8(even_query));
                let low_low = vmull_s8(vget_low_s8(low), vdup_n_s8(odd_query));
                let low_high = vmull_s8(vget_high_s8(low), vdup_n_s8(odd_query));
                let low_products = vaddq_s16(high_low, low_low);
                let high_products = vaddq_s16(high_high, low_high);
                let base = group * 4;
                add_widened_16(&mut accumulators, base, low_products, high_products);
            }
        }
        for (index, accumulator) in accumulators.into_iter().enumerate() {
            // SAFETY: the matching output tile is writable.
            unsafe { vst1q_s32(out.as_mut_ptr().add(row_base + index * 4), accumulator) };
        }
        row_base += tile_rows;
    }
    vertical_bit4_row_tail(query, columns, rows, processed_rows, out);
}

#[target_feature(enable = "neon")]
fn add_widened_16(accumulators: &mut [int32x4_t], base: usize, low: int16x8_t, high: int16x8_t) {
    let additions = [
        vget_low_s16(low),
        vget_high_s16(low),
        vget_low_s16(high),
        vget_high_s16(high),
    ];
    let Some(group) = accumulators.get_mut(base..base.saturating_add(4)) else {
        return;
    };
    for (accumulator, addition) in group.iter_mut().zip(additions) {
        *accumulator = vaddw_s16(*accumulator, addition);
    }
}

fn vertical_bit4_row_tail(
    query: &[i8],
    columns: &[u8],
    rows: usize,
    first_row: usize,
    out: &mut [i32],
) {
    for (query_pair, column) in query.chunks(2).zip(columns.chunks_exact(rows)) {
        let Some(&even_query) = query_pair.first() else {
            continue;
        };
        let odd_query = query_pair.get(1).copied();
        let Some(column_tail) = column.get(first_row..) else {
            continue;
        };
        let Some(out_tail) = out.get_mut(first_row..) else {
            continue;
        };
        for (&packed, accumulator) in column_tail.iter().zip(out_tail) {
            let high = 2 * i32::from(packed >> 4) - 15;
            let low = 2 * i32::from(packed & 0x0f) - 15;
            *accumulator += i32::from(even_query) * high
                + odd_query.map_or(0, |query_value| i32::from(query_value) * low);
        }
    }
}

fn f32_extrema_slab_bounds(
    query: &[f32],
    extrema: &[super::F32Extrema],
    dimensions_per_slab: usize,
    slab_bounds: &mut [f64],
) -> super::F32BoundTotals {
    debug_assert_eq!(query.len(), extrema.len());
    debug_assert!(dimensions_per_slab > 0);
    debug_assert_eq!(slab_bounds.len(), query.len().div_ceil(dimensions_per_slab));
    // SAFETY: dispatch established NEON support. F32Extrema is repr(C) with
    // two adjacent u32 fields, so VLD2 deinterleaves four complete records.
    unsafe { f32_extrema_slab_bounds_inner(query, extrema, dimensions_per_slab, slab_bounds) }
}

#[target_feature(enable = "neon")]
unsafe fn f32_extrema_slab_bounds_inner(
    query: &[f32],
    extrema: &[super::F32Extrema],
    dimensions_per_slab: usize,
    slab_bounds: &mut [f64],
) -> super::F32BoundTotals {
    slab_bounds.fill(0.0);
    let mut maximum_contribution = 0.0_f64;
    let mut absolute_contribution = 0.0_f64;
    for (slab_index, (query_slab, extrema_slab)) in query
        .chunks(dimensions_per_slab)
        .zip(extrema.chunks(dimensions_per_slab))
        .enumerate()
    {
        let processed = query_slab.len() / 4 * 4;
        let mut maximum_accumulator = vdupq_n_f64(0.0);
        let mut absolute_accumulator = vdupq_n_f64(0.0);
        let mut dimension = 0_usize;
        while dimension < processed {
            // SAFETY: four query and four repr(C) extrema records remain.
            let query_values = unsafe { vld1q_f32(query_slab.as_ptr().add(dimension)) };
            let bounds = unsafe { vld2q_u32(extrema_slab.as_ptr().add(dimension).cast::<u32>()) };
            let minimum = vreinterpretq_f32_u32(bounds.0);
            let maximum = vreinterpretq_f32_u32(bounds.1);
            let query_low = vcvt_f64_f32(vget_low_f32(query_values));
            let query_high = vcvt_high_f64_f32(query_values);
            let minimum_low = vcvt_f64_f32(vget_low_f32(minimum));
            let minimum_high = vcvt_high_f64_f32(minimum);
            let maximum_low = vcvt_f64_f32(vget_low_f32(maximum));
            let maximum_high = vcvt_high_f64_f32(maximum);
            let minimum_product_low = vmulq_f64(query_low, minimum_low);
            let minimum_product_high = vmulq_f64(query_high, minimum_high);
            let maximum_product_low = vmulq_f64(query_low, maximum_low);
            let maximum_product_high = vmulq_f64(query_high, maximum_high);
            maximum_accumulator = vaddq_f64(
                maximum_accumulator,
                vaddq_f64(
                    vmaxq_f64(minimum_product_low, maximum_product_low),
                    vmaxq_f64(minimum_product_high, maximum_product_high),
                ),
            );
            absolute_accumulator = vaddq_f64(
                absolute_accumulator,
                vaddq_f64(
                    vmaxq_f64(
                        vabsq_f64(minimum_product_low),
                        vabsq_f64(maximum_product_low),
                    ),
                    vmaxq_f64(
                        vabsq_f64(minimum_product_high),
                        vabsq_f64(maximum_product_high),
                    ),
                ),
            );
            dimension += 4;
        }
        let mut slab_maximum = vaddvq_f64(maximum_accumulator);
        let mut slab_absolute = vaddvq_f64(absolute_accumulator);
        for (&query_value, bounds) in query_slab
            .get(processed..)
            .unwrap_or_default()
            .iter()
            .zip(extrema_slab.get(processed..).unwrap_or_default())
        {
            let minimum_product =
                f64::from(query_value) * f64::from(f32::from_bits(bounds.minimum_bits));
            let maximum_product =
                f64::from(query_value) * f64::from(f32::from_bits(bounds.maximum_bits));
            slab_maximum += minimum_product.max(maximum_product);
            slab_absolute += minimum_product.abs().max(maximum_product.abs());
        }
        if let Some(output) = slab_bounds.get_mut(slab_index) {
            *output = slab_maximum;
        }
        maximum_contribution += slab_maximum;
        absolute_contribution += slab_absolute;
    }
    super::F32BoundTotals {
        maximum_contribution,
        absolute_contribution,
    }
}

fn max_f32(values: &[f32]) -> f32 {
    // SAFETY: runtime dispatch established NEON support; complete loads stay
    // in `values` and the scalar tail handles the remainder.
    unsafe { max_f32_inner(values) }
}

#[target_feature(enable = "neon")]
unsafe fn max_f32_inner(values: &[f32]) -> f32 {
    let processed = values.len() / 4 * 4;
    let mut maximum = vdupq_n_f32(f32::NEG_INFINITY);
    let mut base = 0_usize;
    while base < processed {
        // SAFETY: four values remain by `processed`.
        maximum = vmaxq_f32(maximum, unsafe { vld1q_f32(values.as_ptr().add(base)) });
        base += 4;
    }
    values
        .get(processed..)
        .unwrap_or_default()
        .iter()
        .copied()
        .fold(vmaxvq_f32(maximum), f32::max)
}

fn max_i32(values: &[i32]) -> i32 {
    // SAFETY: runtime dispatch established NEON support; complete loads stay
    // in `values` and the scalar tail handles the remainder.
    unsafe { max_i32_inner(values) }
}

#[target_feature(enable = "neon")]
unsafe fn max_i32_inner(values: &[i32]) -> i32 {
    let processed = values.len() / 4 * 4;
    let mut maximum = vdupq_n_s32(i32::MIN);
    let mut base = 0_usize;
    while base < processed {
        // SAFETY: four values remain by `processed`.
        maximum = vmaxq_s32(maximum, unsafe { vld1q_s32(values.as_ptr().add(base)) });
        base += 4;
    }
    values
        .get(processed..)
        .unwrap_or_default()
        .iter()
        .copied()
        .fold(vmaxvq_s32(maximum), i32::max)
}

pub(super) fn wide_stream_checksum(bytes: &[u8]) -> u64 {
    // SAFETY: Advanced SIMD is mandatory in AArch64 user space and this module
    // is compiled only for AArch64. Complete blocks bound every vector load.
    unsafe { wide_stream_checksum_inner(bytes) }
}

#[target_feature(enable = "neon")]
unsafe fn wide_stream_checksum_inner(bytes: &[u8]) -> u64 {
    // SAFETY: each complete WIDE_STREAM_BLOCK iteration keeps all four
    // unaligned 16-byte loads within the live input slice.
    unsafe {
        let mut acc0 = vdupq_n_u64(0);
        let mut acc1 = vdupq_n_u64(0);
        let mut acc2 = vdupq_n_u64(0);
        let mut acc3 = vdupq_n_u64(0);
        let processed = bytes.len() / WIDE_STREAM_BLOCK * WIDE_STREAM_BLOCK;
        let mut base = 0_usize;
        while base < processed {
            let ptr = bytes.as_ptr().wrapping_add(base);
            acc0 = vaddq_u64(acc0, vreinterpretq_u64_u8(vld1q_u8(ptr)));
            acc1 = vaddq_u64(
                acc1,
                vreinterpretq_u64_u8(vld1q_u8(ptr.wrapping_add(WIDE_STREAM_LANES))),
            );
            acc2 = vaddq_u64(
                acc2,
                vreinterpretq_u64_u8(vld1q_u8(ptr.wrapping_add(WIDE_STREAM_LANES * 2))),
            );
            acc3 = vaddq_u64(
                acc3,
                vreinterpretq_u64_u8(vld1q_u8(ptr.wrapping_add(WIDE_STREAM_LANES * 3))),
            );
            base += WIDE_STREAM_BLOCK;
        }
        let vectors = vaddq_u64(vaddq_u64(acc0, acc1), vaddq_u64(acc2, acc3));
        bytes.get(processed..).map_or(vaddvq_u64(vectors), |tail| {
            tail.iter().fold(vaddvq_u64(vectors), |checksum, &byte| {
                checksum.wrapping_add(u64::from(byte))
            })
        })
    }
}

#[inline]
fn hamming_u1(a: &[u8], b: &[u8]) -> u32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    // SAFETY: dispatch establishes NEON support and complete blocks bound every
    // vector load within both live slices.
    unsafe { hamming_u1_inner(a, b) }
}

#[target_feature(enable = "neon")]
#[inline]
unsafe fn hamming_u1_inner(a: &[u8], b: &[u8]) -> u32 {
    // SAFETY: complete HAMMING_BLOCK iterations keep all four unaligned vector
    // loads in both equal-length slices. Per-lane count sums are at most 32;
    // vaddlvq widens before the horizontal sum, whose maximum is 512.
    unsafe {
        let block_processed = a.len() / HAMMING_BLOCK * HAMMING_BLOCK;
        let vector_processed = a.len() / HAMMING_LANES * HAMMING_LANES;
        let mut sum = 0_u32;
        let mut base = 0_usize;
        while base < block_processed {
            let a_ptr = a.as_ptr().wrapping_add(base);
            let b_ptr = b.as_ptr().wrapping_add(base);
            let count0 = vcntq_u8(veorq_u8(vld1q_u8(a_ptr), vld1q_u8(b_ptr)));
            let count1 = vcntq_u8(veorq_u8(
                vld1q_u8(a_ptr.wrapping_add(HAMMING_LANES)),
                vld1q_u8(b_ptr.wrapping_add(HAMMING_LANES)),
            ));
            let count2 = vcntq_u8(veorq_u8(
                vld1q_u8(a_ptr.wrapping_add(HAMMING_LANES * 2)),
                vld1q_u8(b_ptr.wrapping_add(HAMMING_LANES * 2)),
            ));
            let count3 = vcntq_u8(veorq_u8(
                vld1q_u8(a_ptr.wrapping_add(HAMMING_LANES * 3)),
                vld1q_u8(b_ptr.wrapping_add(HAMMING_LANES * 3)),
            ));
            let counts = vaddq_u8(vaddq_u8(count0, count1), vaddq_u8(count2, count3));
            sum += u32::from(vaddlvq_u8(counts));
            base += HAMMING_BLOCK;
        }
        while base < vector_processed {
            let count = vcntq_u8(veorq_u8(
                vld1q_u8(a.as_ptr().wrapping_add(base)),
                vld1q_u8(b.as_ptr().wrapping_add(base)),
            ));
            // One count vector sums to at most 128, so the task-specified
            // byte horizontal add cannot overflow here.
            sum += u32::from(vaddvq_u8(count));
            base += HAMMING_LANES;
        }
        if let (Some(a_tail), Some(b_tail)) = (a.get(vector_processed..), b.get(vector_processed..))
        {
            sum += scalar::hamming_u1(a_tail, b_tail);
        }
        sum
    }
}

fn dot_i8_batch_dotprod(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    batch_i8(q, rows, d, out, dot_i8_dotprod);
}

fn dot_i8_batch_widen(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    batch_i8(q, rows, d, out, dot_i8_widen);
}

fn dot_i8_batch_dotprod_u2(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    batch_i8(q, rows, d, out, dot_i8_dotprod_u2);
}

fn dot_i8_batch_dotprod_u6(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    batch_i8(q, rows, d, out, dot_i8_dotprod_u6);
}

fn dot_i8_batch_dotprod_u8(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    batch_i8(q, rows, d, out, dot_i8_dotprod_u8);
}

fn dot_i8_batch_dotprod_prefetch(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
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
    // SAFETY: this table is installed only after DotProd detection; the
    // asserted row shape bounds both the prefetched address and kernel loads.
    unsafe { dot_i8_batch_dotprod_prefetch_inner(q, rows, d, out) };
}

#[target_feature(enable = "dotprod")]
unsafe fn dot_i8_batch_dotprod_prefetch_inner(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    let row_count = out.len();
    for (row_index, (row, result)) in rows.chunks_exact(d).zip(out.iter_mut()).enumerate() {
        if row_index + 1 < row_count {
            let next = rows.as_ptr().wrapping_add((row_index + 1) * d);
            // SAFETY: the next row exists by the branch and row-shape contract.
            unsafe {
                asm!(
                    "prfm pldl1keep, [{address}]",
                    address = in(reg) next,
                    options(readonly, nostack)
                );
            }
        }
        // SAFETY: this function established DotProd and each row matches q.
        *result = unsafe { dot_i8_dotprod_inner(q, row) };
    }
}

fn dot_i8_batch_i8mm(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
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
    // SAFETY: this table is installed only after runtime I8MM and DotProd
    // detection. The asserted row-major shape bounds every paired-row load.
    unsafe { dot_i8_batch_i8mm_inner(q, rows, d, out) };
}

#[target_feature(enable = "i8mm,dotprod")]
unsafe fn dot_i8_batch_i8mm_inner(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    let paired_rows = out.len() / 2 * 2;
    let processed = d / I8_LANES * I8_LANES;
    let mut row_index = 0_usize;
    while row_index < paired_rows {
        let row0_start = row_index * d;
        let row1_start = row0_start + d;
        // SAFETY: paired_rows and the asserted rows.len() == d * out.len()
        // keep both row slices in bounds.
        let row0 = unsafe { rows.get_unchecked(row0_start..row0_start + d) };
        let row1 = unsafe { rows.get_unchecked(row1_start..row1_start + d) };
        let mut acc = vdupq_n_s32(0);
        let mut base = 0_usize;
        while base < processed {
            // SAFETY: processed is a multiple of 16 no greater than d, so all
            // three complete vector loads remain within their live slices.
            let query = unsafe { vld1q_s8(q.as_ptr().add(base)) };
            let left = unsafe { vld1q_s8(row0.as_ptr().add(base)) };
            let right = unsafe { vld1q_s8(row1.as_ptr().add(base)) };
            let query_low = vget_low_s8(query);
            let query_high = vget_high_s8(query);
            let duplicated_low = vcombine_s8(query_low, query_low);
            let duplicated_high = vcombine_s8(query_high, query_high);
            let rows_low = vcombine_s8(vget_low_s8(left), vget_low_s8(right));
            let rows_high = vcombine_s8(vget_high_s8(left), vget_high_s8(right));
            acc = unsafe { i8mm_mac(acc, duplicated_low, rows_low) };
            acc = unsafe { i8mm_mac(acc, duplicated_high, rows_high) };
            base += I8_LANES;
        }
        let sum0 = vgetq_lane_s32::<0>(acc);
        let sum1 = vgetq_lane_s32::<1>(acc);
        let tail0 = add_i8_tail(sum0, q, row0, processed);
        let tail1 = add_i8_tail(sum1, q, row1, processed);
        // SAFETY: row_index and row_index + 1 are below paired_rows <= out.len().
        unsafe {
            *out.get_unchecked_mut(row_index) = tail0;
            *out.get_unchecked_mut(row_index + 1) = tail1;
        }
        row_index += 2;
    }
    if paired_rows < out.len() {
        let start = paired_rows * d;
        // SAFETY: the remaining row is exactly the final d-element row.
        let row = unsafe { rows.get_unchecked(start..start + d) };
        // SAFETY: this target-feature function established DotProd execution.
        *unsafe { out.get_unchecked_mut(paired_rows) } = unsafe { dot_i8_dotprod_inner(q, row) };
    }
}

#[target_feature(enable = "i8mm")]
unsafe fn i8mm_mac(mut acc: int32x4_t, left: int8x16_t, right: int8x16_t) -> int32x4_t {
    // SAFETY: runtime dispatch established FEAT_I8MM. SMMLA treats both
    // operands as two 8-byte rows and returns their four pairwise dot products.
    unsafe {
        asm!(
            "smmla {acc:v}.4s, {left:v}.16b, {right:v}.16b",
            acc = inout(vreg) acc,
            left = in(vreg) left,
            right = in(vreg) right,
            options(pure, nomem, nostack)
        );
    }
    acc
}

fn dot_bit4_batch_dotprod(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    debug_assert_eq!(q.len(), d, "query dimension must be pre-validated");
    debug_assert!(d <= MAX_DOT_I8_DIMENSION);
    let row_bytes = d.div_ceil(2);
    debug_assert_eq!(rows.len(), row_bytes.saturating_mul(out.len()));
    if row_bytes == 0 {
        out.fill(0);
        return;
    }
    let query_sum = q.iter().map(|&code| i32::from(code)).sum();
    // SAFETY: the DotProd table is installed only after runtime feature
    // detection. The asserted flat batch shape bounds every four-row load.
    unsafe { dot_bit4_batch_dotprod_r4_inner(q, query_sum, rows, row_bytes, out) };
}

#[target_feature(enable = "dotprod")]
unsafe fn dot_bit4_batch_dotprod_r4_inner(
    q: &[i8],
    query_sum: i32,
    rows: &[u8],
    row_bytes: usize,
    out: &mut [i32],
) {
    // SAFETY: each grouped iteration advances by four complete rows. UZP
    // converts each shared coordinate-order query block to high/low-nibble
    // order once, and all code loads stay within the asserted row shape.
    unsafe {
        const BLOCK: usize = BIT4_DIMENSIONS_PER_BLOCK * 2;
        let processed = q.len() / BLOCK * BLOCK;
        let processed_query_sum = if processed == q.len() {
            query_sum
        } else {
            q.get(..processed)
                .unwrap_or_default()
                .iter()
                .map(|&code| i32::from(code))
                .sum()
        };
        let grouped_rows = out.len() / 4 * 4;
        let mut row_index = 0_usize;
        while row_index < grouped_rows {
            let row0_ptr = rows.as_ptr().add(row_index * row_bytes);
            let row1_ptr = row0_ptr.add(row_bytes);
            let row2_ptr = row1_ptr.add(row_bytes);
            let row3_ptr = row2_ptr.add(row_bytes);
            let mut row0_acc0 = vdupq_n_s32(0);
            let mut row0_acc1 = vdupq_n_s32(0);
            let mut row1_acc0 = vdupq_n_s32(0);
            let mut row1_acc1 = vdupq_n_s32(0);
            let mut row2_acc0 = vdupq_n_s32(0);
            let mut row2_acc1 = vdupq_n_s32(0);
            let mut row3_acc0 = vdupq_n_s32(0);
            let mut row3_acc1 = vdupq_n_s32(0);
            let mut base = 0_usize;
            while base < processed {
                let query_ptr = q.as_ptr().add(base);
                let coordinate0 = vld1q_s8(query_ptr);
                let coordinate1 = vld1q_s8(query_ptr.add(I8_LANES));
                let coordinate2 = vld1q_s8(query_ptr.add(I8_LANES * 2));
                let coordinate3 = vld1q_s8(query_ptr.add(I8_LANES * 3));
                let query_even0 = vuzp1q_s8(coordinate0, coordinate1);
                let query_odd0 = vuzp2q_s8(coordinate0, coordinate1);
                let query_even1 = vuzp1q_s8(coordinate2, coordinate3);
                let query_odd1 = vuzp2q_s8(coordinate2, coordinate3);
                let code_offset = base / 2;
                row0_acc0 = bit4_split_mac(
                    row0_acc0,
                    query_even0,
                    query_odd0,
                    row0_ptr.add(code_offset),
                );
                row1_acc0 = bit4_split_mac(
                    row1_acc0,
                    query_even0,
                    query_odd0,
                    row1_ptr.add(code_offset),
                );
                row2_acc0 = bit4_split_mac(
                    row2_acc0,
                    query_even0,
                    query_odd0,
                    row2_ptr.add(code_offset),
                );
                row3_acc0 = bit4_split_mac(
                    row3_acc0,
                    query_even0,
                    query_odd0,
                    row3_ptr.add(code_offset),
                );
                row0_acc1 = bit4_split_mac(
                    row0_acc1,
                    query_even1,
                    query_odd1,
                    row0_ptr.add(code_offset + PACKED_LANES),
                );
                row1_acc1 = bit4_split_mac(
                    row1_acc1,
                    query_even1,
                    query_odd1,
                    row1_ptr.add(code_offset + PACKED_LANES),
                );
                row2_acc1 = bit4_split_mac(
                    row2_acc1,
                    query_even1,
                    query_odd1,
                    row2_ptr.add(code_offset + PACKED_LANES),
                );
                row3_acc1 = bit4_split_mac(
                    row3_acc1,
                    query_even1,
                    query_odd1,
                    row3_ptr.add(code_offset + PACKED_LANES),
                );
                base += BLOCK;
            }
            let pairs01 = vpaddq_s32(
                vaddq_s32(row0_acc0, row0_acc1),
                vaddq_s32(row1_acc0, row1_acc1),
            );
            let pairs23 = vpaddq_s32(
                vaddq_s32(row2_acc0, row2_acc1),
                vaddq_s32(row3_acc0, row3_acc1),
            );
            let unsigned_sums = vpaddq_s32(pairs01, pairs23);
            let integer_dots = if processed == q.len() {
                vsubq_s32(vshlq_n_s32(unsigned_sums, 1), vdupq_n_s32(15 * query_sum))
            } else {
                let mut sums = [0_i32; 4];
                vst1q_s32(sums.as_mut_ptr(), unsigned_sums);
                let row0 = std::slice::from_raw_parts(row0_ptr, row_bytes);
                let row1 = std::slice::from_raw_parts(row1_ptr, row_bytes);
                let row2 = std::slice::from_raw_parts(row2_ptr, row_bytes);
                let row3 = std::slice::from_raw_parts(row3_ptr, row_bytes);
                sums[0] =
                    packed_bit4_coordinate_tail(sums[0], q, row0, processed, processed_query_sum);
                sums[1] =
                    packed_bit4_coordinate_tail(sums[1], q, row1, processed, processed_query_sum);
                sums[2] =
                    packed_bit4_coordinate_tail(sums[2], q, row2, processed, processed_query_sum);
                sums[3] =
                    packed_bit4_coordinate_tail(sums[3], q, row3, processed, processed_query_sum);
                vld1q_s32(sums.as_ptr())
            };
            vst1q_s32(out.as_mut_ptr().add(row_index), integer_dots);
            row_index += 4;
        }
        while row_index < out.len() {
            let row_ptr = rows.as_ptr().add(row_index * row_bytes);
            let row = std::slice::from_raw_parts(row_ptr, row_bytes);
            *out.get_unchecked_mut(row_index) = dot_bit4_dotprod_inner(q, row);
            row_index += 1;
        }
    }
}

fn packed_bit4_coordinate_tail(
    unsigned_sum: i32,
    q: &[i8],
    codes: &[u8],
    processed: usize,
    processed_query_sum: i32,
) -> i32 {
    let full_blocks = 2 * unsigned_sum - 15 * processed_query_sum;
    let Some(q_tail) = q.get(processed..) else {
        return full_blocks;
    };
    let Some(code_tail) = codes.get(processed / 2..) else {
        return full_blocks;
    };
    full_blocks + scalar::dot_bit4(q_tail, code_tail)
}

fn score_bit4_prepared_batch_dotprod(
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
    if row_bytes == 0 {
        out.fill(0.0);
        return;
    }
    // SAFETY: the DotProd table is installed only after runtime feature
    // detection. The asserted batch shape bounds every row, factor, output,
    // and complete-block load used by the four-row microkernel.
    unsafe {
        score_bit4_prepared_batch_dotprod_r4_inner(
            q,
            query_sum,
            query_scale_half,
            rows,
            row_bytes,
            factors,
            out,
        );
    }
}
#[target_feature(enable = "dotprod")]
unsafe fn score_bit4_prepared_batch_dotprod_r4_inner(
    q: &[i8],
    query_sum: i32,
    query_scale_half: f64,
    rows: &[u8],
    row_bytes: usize,
    factors: &[crate::quant::Bit4Factors],
    out: &mut [f32],
) {
    // SAFETY: the wrapper proves the flat batch shape. Each grouped iteration
    // advances by four whole rows; each dimension iteration loads exactly one
    // complete 64-coordinate query block and two 16-byte blocks per row.
    unsafe {
        const BLOCK: usize = BIT4_DIMENSIONS_PER_BLOCK * 2;
        let processed = q.len() / BLOCK * BLOCK;
        let grouped_rows = out.len() / 4 * 4;
        let mut row_index = 0_usize;
        while row_index < grouped_rows {
            let row0_ptr = rows.as_ptr().add(row_index * row_bytes);
            let row1_ptr = row0_ptr.add(row_bytes);
            let row2_ptr = row1_ptr.add(row_bytes);
            let row3_ptr = row2_ptr.add(row_bytes);
            let mut row0_acc0 = vdupq_n_s32(0);
            let mut row0_acc1 = vdupq_n_s32(0);
            let mut row1_acc0 = vdupq_n_s32(0);
            let mut row1_acc1 = vdupq_n_s32(0);
            let mut row2_acc0 = vdupq_n_s32(0);
            let mut row2_acc1 = vdupq_n_s32(0);
            let mut row3_acc0 = vdupq_n_s32(0);
            let mut row3_acc1 = vdupq_n_s32(0);
            let mut base = 0_usize;
            while base < processed {
                let query_ptr = q.as_ptr().add(base);
                let query0 = vld1q_s8(query_ptr);
                let query1 = vld1q_s8(query_ptr.add(I8_LANES));
                let query2 = vld1q_s8(query_ptr.add(I8_LANES * 2));
                let query3 = vld1q_s8(query_ptr.add(I8_LANES * 3));
                let code_offset = base / 2;
                row0_acc0 = bit4_split_mac(row0_acc0, query0, query1, row0_ptr.add(code_offset));
                row1_acc0 = bit4_split_mac(row1_acc0, query0, query1, row1_ptr.add(code_offset));
                row2_acc0 = bit4_split_mac(row2_acc0, query0, query1, row2_ptr.add(code_offset));
                row3_acc0 = bit4_split_mac(row3_acc0, query0, query1, row3_ptr.add(code_offset));
                row0_acc1 = bit4_split_mac(
                    row0_acc1,
                    query2,
                    query3,
                    row0_ptr.add(code_offset + PACKED_LANES),
                );
                row1_acc1 = bit4_split_mac(
                    row1_acc1,
                    query2,
                    query3,
                    row1_ptr.add(code_offset + PACKED_LANES),
                );
                row2_acc1 = bit4_split_mac(
                    row2_acc1,
                    query2,
                    query3,
                    row2_ptr.add(code_offset + PACKED_LANES),
                );
                row3_acc1 = bit4_split_mac(
                    row3_acc1,
                    query2,
                    query3,
                    row3_ptr.add(code_offset + PACKED_LANES),
                );
                base += BLOCK;
            }
            let row0 = vaddq_s32(row0_acc0, row0_acc1);
            let row1 = vaddq_s32(row1_acc0, row1_acc1);
            let row2 = vaddq_s32(row2_acc0, row2_acc1);
            let row3 = vaddq_s32(row3_acc0, row3_acc1);
            let pairs01 = vpaddq_s32(row0, row1);
            let pairs23 = vpaddq_s32(row2, row3);
            let unsigned_sums = vpaddq_s32(pairs01, pairs23);
            let integer_dots = if processed == q.len() {
                vsubq_s32(vshlq_n_s32(unsigned_sums, 1), vdupq_n_s32(15 * query_sum))
            } else {
                let mut sums = [0_i32; 4];
                vst1q_s32(sums.as_mut_ptr(), unsigned_sums);
                let row0_slice = std::slice::from_raw_parts(row0_ptr, row_bytes);
                let row1_slice = std::slice::from_raw_parts(row1_ptr, row_bytes);
                let row2_slice = std::slice::from_raw_parts(row2_ptr, row_bytes);
                let row3_slice = std::slice::from_raw_parts(row3_ptr, row_bytes);
                sums[0] = packed_bit4_prepared_tail(sums[0], q, query_sum, row0_slice, processed);
                sums[1] = packed_bit4_prepared_tail(sums[1], q, query_sum, row1_slice, processed);
                sums[2] = packed_bit4_prepared_tail(sums[2], q, query_sum, row2_slice, processed);
                sums[3] = packed_bit4_prepared_tail(sums[3], q, query_sum, row3_slice, processed);
                vld1q_s32(sums.as_ptr())
            };
            store_bit4_score_pair(
                integer_dots,
                *factors.get_unchecked(row_index),
                *factors.get_unchecked(row_index + 1),
                query_scale_half,
                out.as_mut_ptr().add(row_index),
            );
            store_bit4_score_pair(
                vextq_s32(integer_dots, integer_dots, 2),
                *factors.get_unchecked(row_index + 2),
                *factors.get_unchecked(row_index + 3),
                query_scale_half,
                out.as_mut_ptr().add(row_index + 2),
            );
            row_index += 4;
        }
        while row_index < out.len() {
            let row_ptr = rows.as_ptr().add(row_index * row_bytes);
            let row = std::slice::from_raw_parts(row_ptr, row_bytes);
            let integer_dot = dot_bit4_prepared_dotprod_inner(q, query_sum, row);
            *out.get_unchecked_mut(row_index) = scalar::bit4_score(
                integer_dot,
                *factors.get_unchecked(row_index),
                query_scale_half,
            );
            row_index += 1;
        }
    }
}

#[target_feature(enable = "dotprod")]
unsafe fn bit4_split_mac(
    mut acc: int32x4_t,
    query_high: int8x16_t,
    query_low: int8x16_t,
    code_ptr: *const u8,
) -> int32x4_t {
    // SAFETY: callers pass a complete 16-byte code block. Both SDOT calls are
    // reached only from the runtime-detected DotProd table.
    unsafe {
        let packed = vld1q_u8(code_ptr);
        let high = vreinterpretq_s8_u8(vshrq_n_u8(packed, 4));
        let low = vreinterpretq_s8_u8(vandq_u8(packed, vdupq_n_u8(0x0f)));
        acc = dotprod_mac(acc, query_high, high);
        dotprod_mac(acc, query_low, low)
    }
}

#[inline]
#[target_feature(enable = "neon")]
unsafe fn store_bit4_score_pair(
    integer_dots: int32x4_t,
    factor0: crate::quant::Bit4Factors,
    factor1: crate::quant::Bit4Factors,
    query_scale_half: f64,
    output: *mut f32,
) {
    // SAFETY: the caller provides two writable output lanes. Factor values are
    // gathered into vectors before the f64 correction chain; the zero-scale
    // mask preserves the scalar oracle's positive-zero fast-path exactly.
    unsafe {
        let (scale0, correction0) = factor0.scoring_parts();
        let (scale1, correction1) = factor1.scoring_parts();
        let scales = [scale0, scale1];
        let corrections = [correction0, correction1];
        let scale_f32 = vld1_f32(scales.as_ptr());
        let correction_f32 = vld1_f32(corrections.as_ptr());
        let factor_correction = vmulq_f64(vcvt_f64_f32(scale_f32), vcvt_f64_f32(correction_f32));
        let dots_i64 = vmovl_s32(vget_low_s32(integer_dots));
        let dots_f64 = vcvtq_f64_s64(dots_i64);
        let scaled = vmulq_f64(vmulq_n_f64(factor_correction, query_scale_half), dots_f64);
        let scores = vcvt_f32_f64(scaled);
        let zero_mask = vceq_f32(scale_f32, vdup_n_f32(0.0));
        let exact_scores = vbsl_u32(
            zero_mask,
            vreinterpret_u32_f32(vdup_n_f32(0.0)),
            vreinterpret_u32_f32(scores),
        );
        vst1_f32(output, vreinterpret_f32_u32(exact_scores));
    }
}

fn dot_bit4_batch_widen(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    batch_packed(q, rows, d, out, 2, dot_bit4_widen);
}

fn batch_i8(q: &[i8], rows: &[i8], d: usize, out: &mut [i32], kernel: fn(&[i8], &[i8]) -> i32) {
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
        *result = kernel(q, row);
    }
}

fn batch_packed(
    q: &[i8],
    rows: &[u8],
    d: usize,
    out: &mut [i32],
    fields_per_byte: usize,
    kernel: fn(&[i8], &[u8]) -> i32,
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
        *result = kernel(q, row);
    }
}

fn hamming_u1_batch(q: &[u8], rows: &[u8], d_bytes: usize, out: &mut [u32]) {
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
    // SAFETY: dispatch installs this batch function only after runtime NEON
    // detection; the asserted row shape bounds every inner vector load.
    unsafe { hamming_u1_batch_inner(q, rows, d_bytes, out) };
}

#[target_feature(enable = "neon")]
unsafe fn hamming_u1_batch_inner(q: &[u8], rows: &[u8], d_bytes: usize, out: &mut [u32]) {
    for (row, result) in rows.chunks_exact(d_bytes).zip(out.iter_mut()) {
        // SAFETY: each chunk has exactly `d_bytes == q.len()` bytes, and this
        // target-feature function runs only after NEON runtime detection.
        *result = unsafe { hamming_u1_inner(q, row) };
    }
}

#[cfg(target_os = "macos")]
pub(super) fn darwin_optional_feature(name: &[u8]) -> bool {
    let Ok(name) = std::ffi::CStr::from_bytes_with_nul(name) else {
        return false;
    };
    let mut value = 0_i32;
    let mut value_len = size_of::<i32>();
    // SAFETY: `name` is NUL-terminated; `value` and `value_len` are live,
    // correctly sized output storage; no new value is supplied.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::addr_of_mut!(value).cast(),
            std::ptr::addr_of_mut!(value_len),
            std::ptr::null_mut(),
            0,
        )
    };
    status == 0 && value_len == size_of::<i32>() && value != 0
}
