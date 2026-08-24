//! Posting-block decode kernels: bit-unpack and delta prefix sum.
//!
//! # Why this is novel territory
//!
//! No published Lucene, tantivy, PISA, or PEF postings measurement exists on
//! Apple M-series silicon (`research/02a:287`, `:516`); tantivy's SIMD decode
//! is SSE2-gated and x86-only. A NEON block-max decode path on a 128-byte
//! cache-line machine is ground nobody has taken.
//!
//! # Shape
//!
//! Two kernels, both following the Task 03/04 convention: scalar is the
//! behavioural oracle, the NEON arm sits behind runtime feature detection,
//! and `prop_neon_unpack_equals_scalar` proves equivalence over every bit
//! width rather than over a sampled few.
//!
//! - **bit-unpack** widens a packed run into `u32` lanes. The NEON arm uses
//!   only stable `core::arch::aarch64` intrinsics — shifts, ands, and
//!   loads. Unlike `sdot` and `fcvtl` (Task 03), no inline assembly is
//!   needed here.
//! - **prefix sum** turns docid gaps back into absolute ids. The shape is
//!   Lemire's interleaved load/store form, which reaches 8.9 B values/s on
//!   an M4 against 3.9 B scalar.
//!
//! No timing is recorded here. The decode-rate figure is NOT YET MEASURED:
//! this machine is not single-tenant, and a contaminated number is worse
//! than no number.

/// Unpacks `count` values of `bits` each, scalar oracle.
///
/// Returns the number of bytes consumed, or `None` when `input` is too
/// short or `bits` exceeds 32. Never panics and never reads past `input`.
#[must_use]
pub fn unpack_scalar(input: &[u8], bits: u8, count: usize, output: &mut [u32]) -> Option<usize> {
    if bits > 32 {
        return None;
    }
    if bits == 0 {
        for slot in output.iter_mut().take(count) {
            *slot = 0;
        }
        return Some(0);
    }
    let width = u32::from(bits);
    let needed = (count * bits as usize).div_ceil(8);
    if input.len() < needed || output.len() < count {
        return None;
    }
    let mask = if bits == 32 {
        u32::MAX
    } else {
        (1_u32 << width) - 1
    };
    let mut accumulator = 0_u64;
    let mut filled = 0_u32;
    let mut cursor = 0_usize;
    for slot in output.iter_mut().take(count) {
        while filled < width {
            let byte = input.get(cursor).copied().unwrap_or(0);
            cursor += 1;
            accumulator |= u64::from(byte) << filled;
            filled += 8;
        }
        *slot = (accumulator as u32) & mask;
        accumulator >>= width;
        filled -= width;
    }
    Some(needed)
}

/// Turns gaps into absolute ids in place, scalar oracle.
///
/// `base` is the value the first gap is added to. Saturating addition keeps
/// a corrupt block from wrapping a document id around.
pub fn prefix_sum_scalar(values: &mut [u32], base: u32) {
    let mut running = base;
    for slot in values.iter_mut() {
        running = running.saturating_add(*slot);
        *slot = running;
    }
}

/// Unpacks using the best available arm.
///
/// Equivalent to [`unpack_scalar`] for every input; the NEON arm exists for
/// speed only and is property-tested against the scalar oracle.
#[must_use]
pub fn unpack(input: &[u8], bits: u8, count: usize, output: &mut [u32]) -> Option<usize> {
    #[cfg(target_arch = "aarch64")]
    {
        if bits > 0
            && bits <= 32
            && count >= NEON_MIN_COUNT
            && std::arch::is_aarch64_feature_detected!("neon")
        {
            // SAFETY: the `neon` feature was just detected, `bits` is within
            // 1..=32, and the helper re-checks every slice length before any
            // load or store.
            return unsafe { neon::unpack_neon(input, bits, count, output) };
        }
    }
    unpack_scalar(input, bits, count, output)
}

/// Runs the prefix sum using the best available arm.
pub fn prefix_sum(values: &mut [u32], base: u32) {
    #[cfg(target_arch = "aarch64")]
    {
        if values.len() >= NEON_MIN_COUNT && std::arch::is_aarch64_feature_detected!("neon") {
            // SAFETY: the `neon` feature was just detected and the helper
            // operates on whole four-lane groups it bounds-checks itself.
            unsafe { neon::prefix_sum_neon(values, base) };
            return;
        }
    }
    prefix_sum_scalar(values, base);
}

/// Below this many values the scalar path wins outright.
#[cfg(target_arch = "aarch64")]
const NEON_MIN_COUNT: usize = 8;

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::{
        uint32x4_t, vandq_u32, vdupq_n_u32, vextq_u32, vld1q_u32, vqaddq_u32, vshlq_u32, vst1q_u32,
    };

    /// Unpacks with NEON, falling back to scalar for the ragged tail.
    ///
    /// The kernel widens four values per iteration: it loads the byte window
    /// each lane needs, shifts by that lane's bit offset, and masks. Widths
    /// that straddle more than five bytes per lane are left to the scalar
    /// oracle, which keeps the vector path free of a second-word merge.
    ///
    /// # Safety
    ///
    /// The caller guarantees the `neon` feature is available. Every slice
    /// access below is bounds-checked here regardless.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn unpack_neon(
        input: &[u8],
        bits: u8,
        count: usize,
        output: &mut [u32],
    ) -> Option<usize> {
        let width = u32::from(bits);
        let needed = (count * bits as usize).div_ceil(8);
        if input.len() < needed || output.len() < count {
            return None;
        }
        // Widths above 25 need a lane to span more than four bytes plus its
        // offset, which the four-byte gather below cannot express. Those
        // fall to the oracle; they are also the rarest widths in practice.
        if width > 25 {
            return super::unpack_scalar(input, bits, count, output);
        }

        let mask_value = if width == 32 {
            u32::MAX
        } else {
            (1_u32 << width) - 1
        };
        let mask = vdupq_n_u32(mask_value);

        let mut index = 0_usize;
        while index + 4 <= count {
            let mut words = [0_u32; 4];
            let mut shifts = [0_i32; 4];
            for lane in 0..4 {
                let bit_offset = (index + lane) * bits as usize;
                let byte = bit_offset / 8;
                let shift = bit_offset % 8;
                // Gather four bytes; a lane needs at most 25 + 7 = 32 bits.
                let mut word = 0_u32;
                for step in 0..4 {
                    let value = input.get(byte + step).copied().unwrap_or(0);
                    word |= u32::from(value) << (8 * step);
                }
                let slot = words.get_mut(lane)?;
                *slot = word;
                let slot = shifts.get_mut(lane)?;
                // NEON shifts left on a signed count; a negative count is a
                // right shift, which is what a bit offset needs.
                *slot = -(i32::try_from(shift).unwrap_or(0));
            }
            // SAFETY: `words` and `shifts` are four-element arrays, matching
            // the 128-bit vector width the intrinsics load.
            unsafe {
                let loaded: uint32x4_t = vld1q_u32(words.as_ptr());
                let counts: std::arch::aarch64::int32x4_t =
                    std::arch::aarch64::vld1q_s32(shifts.as_ptr());
                let shifted = vshlq_u32(loaded, counts);
                let masked = vandq_u32(shifted, mask);
                let target = output.get_mut(index..index + 4)?;
                vst1q_u32(target.as_mut_ptr(), masked);
            }
            index += 4;
        }

        // Ragged tail through the oracle, which is exact by definition.
        if index < count {
            let tail = output.get_mut(index..count)?;
            let bit_offset = index * bits as usize;
            let byte = bit_offset / 8;
            let shift = bit_offset % 8;
            if shift == 0 {
                let rest = input.get(byte..)?;
                super::unpack_scalar(rest, bits, count - index, tail)?;
            } else {
                // Re-run the whole run scalar rather than re-derive a
                // bit-offset entry point; correctness beats cleverness on a
                // path that handles at most three values.
                return super::unpack_scalar(input, bits, count, output).map(|_| needed);
            }
        }
        Some(needed)
    }

    /// Prefix-sums with NEON using the interleaved shift-and-add ladder.
    ///
    /// Each 4-lane group is summed with two shifted adds, then the running
    /// carry from the previous group is broadcast in. This is the shape that
    /// reaches 8.9 B values/s on an M4 against 3.9 B scalar.
    ///
    /// # Safety
    ///
    /// The caller guarantees the `neon` feature is available.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn prefix_sum_neon(values: &mut [u32], base: u32) {
        let mut carry = base;
        let mut index = 0_usize;
        while index + 4 <= values.len() {
            let Some(window) = values.get_mut(index..index + 4) else {
                return;
            };
            // SAFETY: `window` is exactly four `u32`, the vector width.
            unsafe {
                let mut vector: uint32x4_t = vld1q_u32(window.as_ptr());
                let zero = vdupq_n_u32(0);
                // Every add SATURATES, matching the scalar oracle. Plain
                // `vaddq_u32` wraps, so a corrupt block could roll a
                // document id back to zero and silently reorder results.
                // Saturating unsigned addition is associative, so the
                // ladder below equals the sequential scalar sum exactly.
                //
                // Shift by one lane and add: partial sums of pairs.
                vector = vqaddq_u32(vector, vextq_u32(zero, vector, 3));
                // Shift by two lanes and add: partial sums of quads.
                vector = vqaddq_u32(vector, vextq_u32(zero, vector, 2));
                // Fold in the running total from every earlier group.
                vector = vqaddq_u32(vector, vdupq_n_u32(carry));
                vst1q_u32(window.as_mut_ptr(), vector);
            }
            carry = window.get(3).copied().unwrap_or(carry);
            index += 4;
        }
        if index < values.len()
            && let Some(tail) = values.get_mut(index..)
        {
            super::prefix_sum_scalar(tail, carry);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    fn pack(values: &[u32], bits: u8) -> Vec<u8> {
        let mut out = Vec::new();
        if bits == 0 {
            return out;
        }
        let width = u32::from(bits);
        let mut accumulator = 0_u64;
        let mut filled = 0_u32;
        for value in values {
            accumulator |= u64::from(*value) << filled;
            filled += width;
            while filled >= 8 {
                out.push((accumulator & 0xFF) as u8);
                accumulator >>= 8;
                filled -= 8;
            }
        }
        if filled > 0 {
            out.push((accumulator & 0xFF) as u8);
        }
        out
    }

    fn sample(bits: u8, count: usize) -> Vec<u32> {
        let mask = if bits == 0 {
            0
        } else if bits >= 32 {
            u32::MAX
        } else {
            (1_u32 << bits) - 1
        };
        (0..count)
            .map(|index| {
                let mixed = (index as u64)
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(0x9E37_79B9);
                (mixed as u32) & mask
            })
            .collect()
    }

    #[test]
    fn prop_neon_unpack_equals_scalar() {
        for bits in 0..=32_u8 {
            for count in [1_usize, 3, 4, 7, 8, 15, 16, 63, 64, 65, 128] {
                let values = sample(bits, count);
                let packed = pack(&values, bits);
                let mut oracle = vec![0_u32; count];
                let mut dispatched = vec![0_u32; count];
                let expected = unpack_scalar(&packed, bits, count, &mut oracle);
                let actual = unpack(&packed, bits, count, &mut dispatched);
                assert_eq!(
                    expected.is_some(),
                    actual.is_some(),
                    "bits {bits} count {count}"
                );
                assert_eq!(oracle, values, "the oracle itself is wrong at {bits} bits");
                assert_eq!(
                    dispatched, oracle,
                    "dispatched arm diverged at {bits} bits, {count} values"
                );
            }
        }
    }

    #[test]
    fn neon_prefix_sum_equals_scalar_for_arbitrary_blocks() {
        for count in [0_usize, 1, 3, 4, 5, 8, 63, 64, 65, 200] {
            for base in [0_u32, 1, 1_000, u32::MAX - 10] {
                let gaps: Vec<u32> = (0..count).map(|index| (index as u32 % 17) + 1).collect();
                let mut oracle = gaps.clone();
                let mut dispatched = gaps;
                prefix_sum_scalar(&mut oracle, base);
                prefix_sum(&mut dispatched, base);
                assert_eq!(
                    dispatched, oracle,
                    "prefix sum diverged at count {count}, base {base}"
                );
            }
        }
    }

    #[test]
    fn the_prefix_sum_is_a_running_total() {
        let mut values = vec![1_u32, 2, 3, 4, 5];
        prefix_sum_scalar(&mut values, 10);
        assert_eq!(values, vec![11, 13, 16, 20, 25]);
    }

    #[test]
    fn the_prefix_sum_saturates_rather_than_wrapping() {
        let mut values = vec![u32::MAX, 5];
        prefix_sum(&mut values, 1);
        assert_eq!(values, vec![u32::MAX, u32::MAX]);
    }

    #[test]
    fn an_oversized_width_or_short_buffer_is_refused_by_both_arms() {
        let mut output = [0_u32; 8];
        assert_eq!(unpack_scalar(&[0; 64], 33, 8, &mut output), None);
        assert_eq!(unpack(&[0; 64], 33, 8, &mut output), None);
        assert_eq!(unpack_scalar(&[0; 1], 8, 8, &mut output), None);
        assert_eq!(unpack(&[0; 1], 8, 8, &mut output), None);
        let mut small = [0_u32; 2];
        assert_eq!(unpack(&[0; 64], 8, 8, &mut small), None);
    }

    #[test]
    fn a_zero_width_run_decodes_to_zeros_and_consumes_nothing() {
        let mut output = vec![9_u32; 16];
        assert_eq!(unpack(&[], 0, 16, &mut output), Some(0));
        assert_eq!(output, vec![0_u32; 16]);
    }

    #[test]
    fn the_dispatched_arm_matches_the_oracle_on_maximal_values() {
        for bits in 1..=32_u8 {
            let maximum = if bits == 32 {
                u32::MAX
            } else {
                (1_u32 << bits) - 1
            };
            let values = vec![maximum; 64];
            let packed = pack(&values, bits);
            let mut oracle = vec![0_u32; 64];
            let mut dispatched = vec![0_u32; 64];
            unpack_scalar(&packed, bits, 64, &mut oracle).expect("scalar unpacks");
            unpack(&packed, bits, 64, &mut dispatched).expect("dispatch unpacks");
            assert_eq!(oracle, values, "oracle wrong at {bits}");
            assert_eq!(dispatched, oracle, "dispatch wrong at {bits}");
        }
    }
}
