//! AArch64 NEON kernels selected only after runtime feature detection.

use std::arch::aarch64::*;
use std::arch::asm;

use super::{
    InstructionTier, KernelArm, KernelFeatures, KernelTable, MAX_DOT_I8_DIMENSION, scalar,
};

const I8_LANES: usize = 16;
const I8_UNROLL: usize = 4; // baseline — pending frontier campaign B1
const I8_BLOCK: usize = I8_LANES * I8_UNROLL; // baseline — pending frontier campaign B1
const F32_LANES: usize = 4;
const FLOAT_UNROLL: usize = 4; // baseline — pending frontier campaign B1
const FLOAT_ACCUMULATORS: usize = 4; // baseline — pending frontier campaign B1
const FLOAT_BLOCK: usize = F32_LANES * FLOAT_UNROLL; // baseline — pending frontier campaign B1
const _: [(); FLOAT_UNROLL] = [(); FLOAT_ACCUMULATORS];
const HAMMING_LANES: usize = 16;
const HAMMING_UNROLL: usize = 4; // baseline — pending frontier campaign B1
const HAMMING_BLOCK: usize = HAMMING_LANES * HAMMING_UNROLL; // baseline — pending frontier campaign B1
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
