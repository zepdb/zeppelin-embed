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
const BIT2_DIMENSIONS_PER_BLOCK: usize = PACKED_LANES * 4;
const BIT4_DIMENSIONS_PER_BLOCK: usize = PACKED_LANES * 2;
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
        dot_bit2: dot_bit2_dotprod,
        dot_bit4: dot_bit4_dotprod,
        dot_bit2_batch: dot_bit2_batch_dotprod,
        dot_bit4_batch: dot_bit4_batch_dotprod,
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
        dot_bit2: dot_bit2_dotprod,
        dot_bit4: dot_bit4_dotprod,
        dot_bit2_batch: dot_bit2_batch_dotprod,
        dot_bit4_batch: dot_bit4_batch_dotprod,
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
        dot_bit2: dot_bit2_dotprod,
        dot_bit4: dot_bit4_dotprod,
        dot_bit2_batch: dot_bit2_batch_dotprod,
        dot_bit4_batch: dot_bit4_batch_dotprod,
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
        dot_bit2: dot_bit2_widen,
        dot_bit4: dot_bit4_widen,
        dot_bit2_batch: dot_bit2_batch_widen,
        dot_bit4_batch: dot_bit4_batch_widen,
    }
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
fn dot_bit2_dotprod(q: &[i8], codes: &[u8]) -> i32 {
    packed_shape_debug_assertions(q, codes, 4);
    // SAFETY: the DotProd table is installed only after runtime feature
    // detection; complete packed blocks bound every query and code load.
    unsafe { dot_bit2_dotprod_inner(q, codes) }
}

#[target_feature(enable = "dotprod")]
unsafe fn dot_bit2_dotprod_inner(q: &[i8], codes: &[u8]) -> i32 {
    // SAFETY: each iteration consumes exactly 16 code bytes and 64 query
    // bytes. Shift/mask extraction stays in registers, and the wrapper
    // established FEAT_DotProd before any SDOT instruction is reached.
    unsafe {
        let processed = q.len() / BIT2_DIMENSIONS_PER_BLOCK * BIT2_DIMENSIONS_PER_BLOCK;
        let mut acc = vdupq_n_s32(0);
        let mut base = 0_usize;
        while base < processed {
            let code_ptr = codes.as_ptr().wrapping_add(base / 4);
            let (values0, values1, values2, values3) = unpack_bit2_block(code_ptr);
            let query_ptr = q.as_ptr().wrapping_add(base);
            acc = dotprod_mac(acc, vld1q_s8(query_ptr), values0);
            acc = dotprod_mac(acc, vld1q_s8(query_ptr.wrapping_add(I8_LANES)), values1);
            acc = dotprod_mac(acc, vld1q_s8(query_ptr.wrapping_add(I8_LANES * 2)), values2);
            acc = dotprod_mac(acc, vld1q_s8(query_ptr.wrapping_add(I8_LANES * 3)), values3);
            base += BIT2_DIMENSIONS_PER_BLOCK;
        }
        packed_bit2_tail(vaddvq_s32(acc), q, codes, processed)
    }
}

#[inline]
fn dot_bit2_widen(q: &[i8], codes: &[u8]) -> i32 {
    packed_shape_debug_assertions(q, codes, 4);
    // SAFETY: the NEON table is installed only after runtime feature
    // detection; complete packed blocks bound every query and code load.
    unsafe { dot_bit2_widen_inner(q, codes) }
}

#[target_feature(enable = "neon")]
unsafe fn dot_bit2_widen_inner(q: &[i8], codes: &[u8]) -> i32 {
    // SAFETY: each iteration consumes exactly 16 code bytes and 64 query
    // bytes. Shift/mask extraction and widening MACs stay in registers.
    unsafe {
        let processed = q.len() / BIT2_DIMENSIONS_PER_BLOCK * BIT2_DIMENSIONS_PER_BLOCK;
        let mut acc = vdupq_n_s32(0);
        let mut base = 0_usize;
        while base < processed {
            let code_ptr = codes.as_ptr().wrapping_add(base / 4);
            let (values0, values1, values2, values3) = unpack_bit2_block(code_ptr);
            let query_ptr = q.as_ptr().wrapping_add(base);
            acc = widen_mac(acc, vld1q_s8(query_ptr), values0);
            acc = widen_mac(acc, vld1q_s8(query_ptr.wrapping_add(I8_LANES)), values1);
            acc = widen_mac(acc, vld1q_s8(query_ptr.wrapping_add(I8_LANES * 2)), values2);
            acc = widen_mac(acc, vld1q_s8(query_ptr.wrapping_add(I8_LANES * 3)), values3);
            base += BIT2_DIMENSIONS_PER_BLOCK;
        }
        packed_bit2_tail(vaddvq_s32(acc), q, codes, processed)
    }
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
    // SAFETY: each iteration consumes exactly 16 code bytes and 32 query
    // bytes. Shift/mask extraction stays in registers, and the wrapper
    // established FEAT_DotProd before any SDOT instruction is reached.
    unsafe {
        let processed = q.len() / BIT4_DIMENSIONS_PER_BLOCK * BIT4_DIMENSIONS_PER_BLOCK;
        let mut acc = vdupq_n_s32(0);
        let mut base = 0_usize;
        while base < processed {
            let code_ptr = codes.as_ptr().wrapping_add(base / 2);
            let (values0, values1) = unpack_bit4_block(code_ptr);
            let query_ptr = q.as_ptr().wrapping_add(base);
            acc = dotprod_mac(acc, vld1q_s8(query_ptr), values0);
            acc = dotprod_mac(acc, vld1q_s8(query_ptr.wrapping_add(I8_LANES)), values1);
            base += BIT4_DIMENSIONS_PER_BLOCK;
        }
        packed_bit4_tail(vaddvq_s32(acc), q, codes, processed)
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

#[target_feature(enable = "neon")]
unsafe fn unpack_bit2_block(code_ptr: *const u8) -> (int8x16_t, int8x16_t, int8x16_t, int8x16_t) {
    // SAFETY: callers provide a pointer to a complete 16-byte packed block.
    // ZIP on byte pairs and then halfwords restores field order without a
    // lookup table or memory-resident scratch space.
    unsafe {
        let packed = vld1q_u8(code_ptr);
        let mask = vdupq_n_u8(0x03);
        let field0 = vandq_u8(vshrq_n_u8(packed, 6), mask);
        let field1 = vandq_u8(vshrq_n_u8(packed, 4), mask);
        let field2 = vandq_u8(vshrq_n_u8(packed, 2), mask);
        let field3 = vandq_u8(packed, mask);
        let fields01_low = vzip1q_u8(field0, field1);
        let fields01_high = vzip2q_u8(field0, field1);
        let fields23_low = vzip1q_u8(field2, field3);
        let fields23_high = vzip2q_u8(field2, field3);
        let values0 = vzip1q_u16(
            vreinterpretq_u16_u8(fields01_low),
            vreinterpretq_u16_u8(fields23_low),
        );
        let values1 = vzip2q_u16(
            vreinterpretq_u16_u8(fields01_low),
            vreinterpretq_u16_u8(fields23_low),
        );
        let values2 = vzip1q_u16(
            vreinterpretq_u16_u8(fields01_high),
            vreinterpretq_u16_u8(fields23_high),
        );
        let values3 = vzip2q_u16(
            vreinterpretq_u16_u8(fields01_high),
            vreinterpretq_u16_u8(fields23_high),
        );
        (
            bit2_grid(vreinterpretq_u8_u16(values0)),
            bit2_grid(vreinterpretq_u8_u16(values1)),
            bit2_grid(vreinterpretq_u8_u16(values2)),
            bit2_grid(vreinterpretq_u8_u16(values3)),
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
unsafe fn bit2_grid(fields: uint8x16_t) -> int8x16_t {
    vreinterpretq_s8_u8(vsubq_u8(vshlq_n_u8(fields, 1), vdupq_n_u8(3)))
}

#[target_feature(enable = "neon")]
unsafe fn bit4_grid(fields: uint8x16_t) -> int8x16_t {
    vreinterpretq_s8_u8(vsubq_u8(vshlq_n_u8(fields, 1), vdupq_n_u8(15)))
}

fn packed_shape_debug_assertions(q: &[i8], codes: &[u8], fields_per_byte: usize) {
    debug_assert_eq!(codes.len(), q.len().div_ceil(fields_per_byte));
    debug_assert!(q.len() <= MAX_DOT_I8_DIMENSION);
}

fn packed_bit2_tail(sum: i32, q: &[i8], codes: &[u8], processed: usize) -> i32 {
    let Some(q_tail) = q.get(processed..) else {
        return sum;
    };
    let Some(code_tail) = codes.get(processed / 4..) else {
        return sum;
    };
    sum + scalar::dot_bit2(q_tail, code_tail)
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

fn dot_bit2_batch_dotprod(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    batch_packed(q, rows, d, out, 4, dot_bit2_dotprod);
}

fn dot_bit2_batch_widen(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    batch_packed(q, rows, d, out, 4, dot_bit2_widen);
}

fn dot_bit4_batch_dotprod(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    batch_packed(q, rows, d, out, 2, dot_bit4_dotprod);
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
