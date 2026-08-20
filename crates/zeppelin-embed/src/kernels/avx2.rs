//! x86-64 AVX2 kernels selected only after runtime AVX2+POPCNT detection.

use std::arch::x86_64::*;

use super::{InstructionTier, KernelArm, KernelTable, MAX_DOT_I8_DIMENSION, scalar};

const I8_LANES: usize = 32;
const I8_UNROLL: usize = 4; // baseline — pending frontier campaign B1
const F32_LANES: usize = 8;
const HAMMING_LANES: usize = 32;
const HAMMING_UNROLL: usize = 4; // baseline — pending frontier campaign B1

pub(super) fn table() -> KernelTable {
    KernelTable {
        arm: KernelArm::Avx2,
        tier: InstructionTier::Avx2,
        dot_i8,
        dot_f32,
        dot_f16,
        hamming_u1,
        dot_i8_batch,
        hamming_u1_batch,
        dot_bit4: scalar::dot_bit4,
        dot_bit4_prepared: scalar::dot_bit4_prepared,
        dot_bit4_batch: scalar::dot_bit4_batch,
        score_bit4_prepared_batch: scalar::score_bit4_prepared_batch,
    }
}

#[inline]
fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    debug_assert!(
        a.len() <= MAX_DOT_I8_DIMENSION,
        "i8 dimension must be pre-validated"
    );
    // SAFETY: dispatch installs this wrapper only when AVX2 is detected. The
    // complete-block loop bounds every load within the asserted slices.
    unsafe { dot_i8_inner(a, b) }
}

#[target_feature(enable = "avx2")]
unsafe fn dot_i8_inner(a: &[i8], b: &[i8]) -> i32 {
    // SAFETY: complete I8_LANES chunks bound unaligned loads. The even/odd
    // masks leave one unsigned*signed product per vpmaddubsw pair, so its i16
    // saturation cannot trigger even for i8::MIN. The 128*b correction turns
    // biased unsigned a bytes back into exact signed products.
    unsafe {
        let mut biased0 = _mm256_setzero_si256();
        let mut biased1 = _mm256_setzero_si256();
        let mut biased2 = _mm256_setzero_si256();
        let mut biased3 = _mm256_setzero_si256();
        let mut correction0 = _mm256_setzero_si256();
        let mut correction1 = _mm256_setzero_si256();
        let mut correction2 = _mm256_setzero_si256();
        let mut correction3 = _mm256_setzero_si256();
        let processed = a.len() / I8_LANES * I8_LANES;
        let mut base = 0_usize;
        let mut block = 0_usize;
        while base < processed {
            let a_vector = _mm256_loadu_si256(a.as_ptr().wrapping_add(base).cast());
            let b_vector = _mm256_loadu_si256(b.as_ptr().wrapping_add(base).cast());
            let (biased, correction) = signed_i8_chunk(a_vector, b_vector);
            match block % I8_UNROLL {
                0 => {
                    biased0 = _mm256_add_epi32(biased0, biased);
                    correction0 = _mm256_add_epi32(correction0, correction);
                }
                1 => {
                    biased1 = _mm256_add_epi32(biased1, biased);
                    correction1 = _mm256_add_epi32(correction1, correction);
                }
                2 => {
                    biased2 = _mm256_add_epi32(biased2, biased);
                    correction2 = _mm256_add_epi32(correction2, correction);
                }
                _ => {
                    biased3 = _mm256_add_epi32(biased3, biased);
                    correction3 = _mm256_add_epi32(correction3, correction);
                }
            }
            base += I8_LANES;
            block += 1;
        }
        let biased = _mm256_add_epi32(
            _mm256_add_epi32(biased0, biased1),
            _mm256_add_epi32(biased2, biased3),
        );
        let correction = _mm256_add_epi32(
            _mm256_add_epi32(correction0, correction1),
            _mm256_add_epi32(correction2, correction3),
        );
        let sum = hsum_i32(biased) - 128 * hsum_i32(correction);
        add_i8_tail(sum, a, b, processed)
    }
}

#[target_feature(enable = "avx2")]
unsafe fn signed_i8_chunk(a: __m256i, b: __m256i) -> (__m256i, __m256i) {
    let bias = _mm256_set1_epi8(i8::MIN);
    let even_bytes = _mm256_set1_epi16(0x00ff);
    let odd_bytes = _mm256_set1_epi16(-256);
    let ones = _mm256_set1_epi16(1);
    let biased_a = _mm256_xor_si256(a, bias);
    let even_products = _mm256_maddubs_epi16(_mm256_and_si256(biased_a, even_bytes), b);
    let odd_products = _mm256_maddubs_epi16(_mm256_and_si256(biased_a, odd_bytes), b);
    let biased = _mm256_add_epi32(
        _mm256_madd_epi16(even_products, ones),
        _mm256_madd_epi16(odd_products, ones),
    );

    let low = _mm256_cvtepi8_epi16(_mm256_castsi256_si128(b));
    let high = _mm256_cvtepi8_epi16(_mm256_extracti128_si256(b, 1));
    let correction = _mm256_add_epi32(_mm256_madd_epi16(low, ones), _mm256_madd_epi16(high, ones));
    (biased, correction)
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
    // SAFETY: dispatch establishes AVX2 support and complete chunks bound all
    // unaligned loads within both live slices.
    unsafe { dot_f32_inner(a, b) }
}

#[target_feature(enable = "avx2")]
unsafe fn dot_f32_inner(a: &[f32], b: &[f32]) -> f32 {
    let mut sum = 0.0_f32;
    let mut a_chunks = a.chunks_exact(F32_LANES);
    let mut b_chunks = b.chunks_exact(F32_LANES);
    for (a_chunk, b_chunk) in a_chunks.by_ref().zip(b_chunks.by_ref()) {
        let mut products = [0.0_f32; F32_LANES];
        // SAFETY: each load/store covers exactly eight initialized f32 values
        // in live slices/stack storage; dispatch established AVX2 support.
        unsafe {
            let lanes = _mm256_mul_ps(
                _mm256_loadu_ps(a_chunk.as_ptr()),
                _mm256_loadu_ps(b_chunk.as_ptr()),
            );
            _mm256_storeu_ps(products.as_mut_ptr(), lanes);
        }
        // Preserve oracle addition order so cancellation cannot exceed the
        // task's fixed relative epsilon while multiplication remains SIMD.
        for product in products {
            sum += product;
        }
    }
    for (&left, &right) in a_chunks.remainder().iter().zip(b_chunks.remainder()) {
        sum += left * right;
    }
    sum
}

#[inline]
fn dot_f16(a: &[u16], b: &[u16]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    // SAFETY: dispatch establishes AVX2 support; vector loads and stores use
    // fixed-size initialized stack arrays.
    unsafe { dot_f16_inner(a, b) }
}

#[target_feature(enable = "avx2")]
unsafe fn dot_f16_inner(a: &[u16], b: &[u16]) -> f32 {
    let mut sum = 0.0_f32;
    let mut a_chunks = a.chunks_exact(F32_LANES);
    let mut b_chunks = b.chunks_exact(F32_LANES);
    for (a_chunk, b_chunk) in a_chunks.by_ref().zip(b_chunks.by_ref()) {
        let mut converted_a = [0.0_f32; F32_LANES];
        let mut converted_b = [0.0_f32; F32_LANES];
        for ((left_out, right_out), (&left, &right)) in converted_a
            .iter_mut()
            .zip(converted_b.iter_mut())
            .zip(a_chunk.iter().zip(b_chunk))
        {
            *left_out = scalar::f16_to_f32(left);
            *right_out = scalar::f16_to_f32(right);
        }
        let mut products = [0.0_f32; F32_LANES];
        // SAFETY: all loads/stores cover exactly eight initialized f32 values
        // in live stack arrays. AVX2 support was established by dispatch.
        unsafe {
            let lanes = _mm256_mul_ps(
                _mm256_loadu_ps(converted_a.as_ptr()),
                _mm256_loadu_ps(converted_b.as_ptr()),
            );
            _mm256_storeu_ps(products.as_mut_ptr(), lanes);
        }
        for product in products {
            sum += product;
        }
    }
    for (&left, &right) in a_chunks.remainder().iter().zip(b_chunks.remainder()) {
        sum += scalar::f16_to_f32(left) * scalar::f16_to_f32(right);
    }
    sum
}

#[inline]
fn hamming_u1(a: &[u8], b: &[u8]) -> u32 {
    debug_assert_eq!(a.len(), b.len(), "kernel lengths must be pre-validated");
    // SAFETY: dispatch establishes AVX2+POPCNT support and complete vector
    // chunks bound every unaligned load.
    unsafe { hamming_u1_inner(a, b) }
}

#[target_feature(enable = "avx2,popcnt")]
unsafe fn hamming_u1_inner(a: &[u8], b: &[u8]) -> u32 {
    // SAFETY: complete HAMMING_LANES chunks bound both unaligned loads. The XOR
    // result is stored to exactly four initialized u64 lanes before POPCNT.
    unsafe {
        let processed = a.len() / HAMMING_LANES * HAMMING_LANES;
        let mut sums = [0_u32; HAMMING_UNROLL];
        let mut base = 0_usize;
        let mut block = 0_usize;
        while base < processed {
            let left = _mm256_loadu_si256(a.as_ptr().wrapping_add(base).cast());
            let right = _mm256_loadu_si256(b.as_ptr().wrapping_add(base).cast());
            let xor = _mm256_xor_si256(left, right);
            let mut words = [0_u64; 4];
            _mm256_storeu_si256(words.as_mut_ptr().cast(), xor);
            let count: u32 = words.into_iter().map(u64::count_ones).sum();
            if let Some(slot) = sums.get_mut(block % HAMMING_UNROLL) {
                *slot += count;
            }
            base += HAMMING_LANES;
            block += 1;
        }
        let mut sum: u32 = sums.into_iter().sum();
        if let (Some(a_tail), Some(b_tail)) = (a.get(processed..), b.get(processed..)) {
            sum += scalar::hamming_u1(a_tail, b_tail);
        }
        sum
    }
}

fn dot_i8_batch(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
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
    for (row, result) in rows.chunks_exact(d_bytes).zip(out.iter_mut()) {
        *result = hamming_u1(q, row);
    }
}

#[target_feature(enable = "avx2")]
unsafe fn hsum_i32(vector: __m256i) -> i32 {
    let mut lanes = [0_i32; 8];
    // SAFETY: `lanes` provides exactly 32 writable bytes for the AVX2 store;
    // the wrapper established AVX2 support.
    unsafe { _mm256_storeu_si256(lanes.as_mut_ptr().cast(), vector) };
    lanes.into_iter().sum()
}
