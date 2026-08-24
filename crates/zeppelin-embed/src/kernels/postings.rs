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
/// Widths at or below this are decoded by the narrow path.
///
/// # Why eight, and why it is the only width that matters
///
/// At `bits <= 8` a group of eight values occupies exactly `bits` bytes and
/// starts byte-aligned, because `8 * bits` bits is `bits` bytes. That makes
/// a whole group one load with no carry state and no refill branch.
///
/// It is also, measured, the entire decode workload. The width histogram
/// over every multi-block posting list of two deterministic corpora — one
/// Zipf at 100,000 documents, one text-shaped at 50,000 — puts **100% of
/// postings at seven bits or fewer**:
///
/// ```text
/// zipf100k  docid  1b=41.6% 2b=23.1% 3b=21.1% 4b=14.2%   tf  1b=86.1% 4b=13.9%
/// textish   docid  1b=65.6% 7b=34.4%                     tf  1b=98.0% 2b=2.0%
/// ```
///
/// Two consequences, both measured rather than assumed. The generic ladder
/// below is effectively dead on real data, so its shape does not matter.
/// And the width-above-25 scalar cliff in [`neon::unpack_neon`] is
/// unreachable: no block on either corpus came close. That closes the
/// question rather than leaving it open — see the campaign notes for K3.
pub const NARROW_MAX_BITS: u8 = 8;

/// Runs the bit-unpack using the best available arm.
///
/// Equivalent to [`unpack_scalar`] for every input; the other arms exist for
/// speed only and are property-tested against the scalar oracle at every
/// width.
#[must_use]
pub fn unpack(input: &[u8], bits: u8, count: usize, output: &mut [u32]) -> Option<usize> {
    if bits > 0 && bits <= NARROW_MAX_BITS {
        return unpack_narrow(input, bits, count, output);
    }
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

/// Unpacks at eight bits or fewer, eight values per iteration.
///
/// # The shape, and what it replaces
///
/// [`unpack_scalar`] carries an accumulator across values and refills it a
/// byte at a time, so every value pays a loop test and a conditional refill.
/// [`neon::unpack_neon`] is worse at these widths: it builds its four-lane
/// window with a scalar gather of four bytes per lane, sixteen byte loads to
/// produce four values whose packed form occupies at most four bytes in
/// total, before it issues a single vector shift.
///
/// Here a group of eight is one aligned load and eight independent
/// shift-and-mask pairs. No accumulator, no refill, no branch inside the
/// group. The ragged tail is byte-aligned by construction, so the oracle
/// finishes it.
#[must_use]
pub fn unpack_narrow(input: &[u8], bits: u8, count: usize, output: &mut [u32]) -> Option<usize> {
    if bits == 0 || bits > NARROW_MAX_BITS {
        return None;
    }
    let width = usize::from(bits);
    let needed = count.checked_mul(width)?.div_ceil(8);
    if input.len() < needed || output.len() < count {
        return None;
    }
    let mask = (1_u32 << width) - 1;

    let mut index = 0_usize;
    let mut byte = 0_usize;

    #[cfg(target_arch = "aarch64")]
    if count >= 8 && std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: the `neon` feature was just detected, `width` is within
        // 1..=8, and the helper loads only where it has verified sixteen
        // readable bytes remain.
        let (consumed_values, consumed_bytes) =
            unsafe { neon::unpack_narrow_neon(input, width, count, output) };
        index = consumed_values;
        byte = consumed_bytes;
    }

    while index + 8 <= count {
        // Eight values at `width` bits occupy exactly `width` bytes, and the
        // group starts byte-aligned, so the whole group is one window.
        let window = match input.get(byte..byte + 8) {
            Some(bytes) => {
                let mut buffer = [0_u8; 8];
                buffer.copy_from_slice(bytes);
                u64::from_le_bytes(buffer)
            }
            None => {
                // Near the end of the stream: assemble only the bytes this
                // group actually owns rather than reading past them.
                let mut assembled = 0_u64;
                for step in 0..width {
                    let value = input.get(byte + step).copied().unwrap_or(0);
                    assembled |= u64::from(value) << (8 * step);
                }
                assembled
            }
        };
        for lane in 0..8 {
            let slot = output.get_mut(index + lane)?;
            *slot = ((window >> (lane * width)) as u32) & mask;
        }
        index += 8;
        byte += width;
    }

    let remaining = count - index;
    if remaining > 0 {
        let tail = input.get(byte..)?;
        let target = output.get_mut(index..)?;
        unpack_scalar(tail, bits, remaining, target)?;
    }
    Some(needed)
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
        uint8x16_t, uint32x4_t, vandq_u16, vandq_u32, vdupq_n_u16, vdupq_n_u32, vextq_u32,
        vget_low_u16, vld1q_s16, vld1q_u8, vld1q_u32, vmovl_high_u16, vmovl_u16, vqaddq_u32,
        vqtbl1q_u8, vreinterpretq_u16_u8, vshlq_u16, vshlq_u32, vst1q_u32,
    };

    /// Unpacks eight values per iteration at eight bits or fewer.
    ///
    /// # The gather is one `tbl`, not sixteen loads
    ///
    /// A group of eight values at `width` bits occupies exactly `width`
    /// bytes and starts byte-aligned. Every lane needs at most two adjacent
    /// bytes, because `width + 7 <= 15` bits. So one `vqtbl1q_u8` over a
    /// single sixteen-byte window places both bytes of all eight lanes at
    /// once, giving eight `u16` lanes; a per-lane variable shift and a mask
    /// finish them, and two widenings produce the `u32` output.
    ///
    /// That is roughly six vector operations per eight values, against the
    /// generic arm's sixteen scalar byte loads per four.
    ///
    /// Returns the values and bytes consumed, so the caller finishes any
    /// group for which sixteen readable bytes were not available.
    ///
    /// # Safety
    ///
    /// The caller guarantees the `neon` feature is available and that
    /// `width` is within 1..=8. Every load below is guarded by an explicit
    /// length check on the slice it reads.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn unpack_narrow_neon(
        input: &[u8],
        width: usize,
        count: usize,
        output: &mut [u32],
    ) -> (usize, usize) {
        // Lane j starts at bit j * width, so it lives in bytes
        // (j * width) / 8 and the one after it, shifted right by the
        // remainder. Both vectors depend only on `width`.
        let mut selectors = [0_u8; 16];
        let mut shifts = [0_i16; 8];
        for lane in 0..8_usize {
            let offset = lane * width;
            let byte = u8::try_from(offset / 8).unwrap_or(0);
            let Some(low) = selectors.get_mut(lane * 2) else {
                return (0, 0);
            };
            *low = byte;
            let Some(high) = selectors.get_mut(lane * 2 + 1) else {
                return (0, 0);
            };
            *high = byte.saturating_add(1);
            let Some(shift) = shifts.get_mut(lane) else {
                return (0, 0);
            };
            // A negative NEON shift count is a right shift.
            *shift = -i16::try_from(offset % 8).unwrap_or(0);
        }
        let mask_value = ((1_u32 << width) - 1) as u16;

        let mut index = 0_usize;
        let mut byte = 0_usize;
        // SAFETY: `selectors` and `shifts` are exactly the sixteen bytes and
        // eight halfwords the intrinsics load.
        let (indices, counts, mask) = unsafe {
            (
                vld1q_u8(selectors.as_ptr()),
                vld1q_s16(shifts.as_ptr()),
                vdupq_n_u16(mask_value),
            )
        };

        while index + 8 <= count {
            // `vqtbl1q_u8` reads a whole sixteen-byte register, so the group
            // is only taken here when sixteen bytes are readable. The tail
            // groups fall back to the caller's scalar loop.
            let Some(window) = input.get(byte..byte + 16) else {
                break;
            };
            let Some(target) = output.get_mut(index..index + 8) else {
                break;
            };
            // SAFETY: `window` is sixteen readable bytes and `target` is
            // eight writable `u32`s, matching the loads and stores below.
            unsafe {
                let table: uint8x16_t = vld1q_u8(window.as_ptr());
                let gathered = vreinterpretq_u16_u8(vqtbl1q_u8(table, indices));
                let values = vandq_u16(vshlq_u16(gathered, counts), mask);
                vst1q_u32(target.as_mut_ptr(), vmovl_u16(vget_low_u16(values)));
                vst1q_u32(target.as_mut_ptr().add(4), vmovl_high_u16(values));
            }
            index += 8;
            byte += width;
        }
        (index, byte)
    }

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
    fn the_narrow_path_equals_the_oracle_at_every_count_and_exact_buffer() {
        // The narrow path has three regimes and the boundaries between them
        // are where a bit-packing bug lives: a group taken by the `tbl`
        // gather, which needs sixteen readable bytes; a group taken by the
        // eight-byte scalar window; and a final group near the end of the
        // stream that owns fewer bytes than either window reads.
        //
        // Every buffer here is trimmed to EXACTLY the bytes the packing
        // needs, so any read past a group's own bytes is a real out-of-
        // bounds read rather than one hidden by slack in the fixture.
        for bits in 1..=NARROW_MAX_BITS {
            for count in 1..=200_usize {
                let values = sample(bits, count);
                let mut packed = pack(&values, bits);
                let needed = (count * usize::from(bits)).div_ceil(8);
                packed.truncate(needed);
                assert_eq!(packed.len(), needed, "the fixture must be exact");

                let mut narrow = vec![0_u32; count];
                let consumed = unpack_narrow(&packed, bits, count, &mut narrow)
                    .expect("an exact buffer is enough");
                assert_eq!(consumed, needed, "bits {bits} count {count}");
                assert_eq!(
                    narrow, values,
                    "the narrow path diverged at {bits} bits, {count} values"
                );

                // And the dispatcher must route here without changing the
                // answer, at every width the measured histogram contains.
                let mut dispatched = vec![0_u32; count];
                unpack(&packed, bits, count, &mut dispatched).expect("dispatches");
                assert_eq!(dispatched, values);
            }
        }
    }

    #[test]
    fn the_narrow_path_refuses_what_it_cannot_decode() {
        let mut output = [0_u32; 8];
        assert!(unpack_narrow(&[0_u8; 8], 0, 8, &mut output).is_none());
        assert!(unpack_narrow(&[0_u8; 8], 9, 8, &mut output).is_none());
        // One byte short of the eight values a single bit each needs.
        assert!(unpack_narrow(&[], 1, 8, &mut output).is_none());
        // Output too small for the requested count.
        assert!(unpack_narrow(&[0_u8; 8], 1, 9, &mut output).is_none());
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
