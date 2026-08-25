//! Byte-quantized document length norms.
//!
//! # Why a byte, and why that is the FAST choice
//!
//! A `u8` norm is not a size saving; it is a bandwidth saving on the hottest
//! read in the scorer. Every scored posting touches its document's length,
//! so the norm array is streamed in full on any non-trivial query. One byte
//! per document instead of four is four times less norm traffic per query
//! and four times more of the array per 128-byte cache line, on a machine
//! where one core owns 80.7 GB/s of the 127.9 GB/s available
//! (`tasks/evidence/02-platform-truth.md:261`). Under the cardinal rule this
//! is the faster structure, and it would still be chosen with an unlimited
//! byte budget.
//!
//! The accuracy cost is carried from the research, not measured here: Lucene
//! has shipped byte norms for two decades and the task-13 spec records the
//! measured trade as "free 4x, .0002 AP cost". That figure belongs to the
//! source it came from; this module does not restate it as its own result.
//!
//! # The encoding
//!
//! Lengths are mapped through a monotone table so that short documents —
//! where BM25's length normalization actually bites — keep fine resolution,
//! and long documents share coarse buckets where a few tokens change
//! nothing. Decoding is a 256-entry lookup, so scoring never divides or
//! calls a transcendental on the hot path.

/// Distinct norm buckets.
pub const NORM_BUCKETS: usize = 256;

/// Encodes a document length in analyzed tokens into its norm byte.
///
/// The mapping is monotone non-decreasing: a longer document never encodes
/// to a smaller byte. Task 14's bounds rely on that, because a block's
/// shortest document must also carry the smallest norm byte in the block.
#[must_use]
pub fn encode_length(length: u32) -> u8 {
    // Lengths 0..=39 are exact; beyond that the bucket width grows
    // geometrically, which keeps 8 bits meaningful out to ~2^31 tokens.
    if length < 40 {
        return u8::try_from(length).unwrap_or(u8::MAX);
    }
    let mut bucket = 40_u32;
    let mut value = 40_u32;
    let mut step = 1_u32;
    let mut since = 0_u32;
    while bucket < 255 {
        value = value.saturating_add(step);
        if length < value {
            return u8::try_from(bucket).unwrap_or(u8::MAX);
        }
        bucket += 1;
        since += 1;
        // Widen the step every eight buckets: a smooth geometric ramp.
        if since == 8 {
            since = 0;
            step = step.saturating_mul(2);
        }
    }
    u8::MAX
}

/// Decodes a norm byte back into a representative document length.
///
/// The result is the *largest* length in the bucket. Rounding up here keeps
/// the decoded length at or above the true length, so the length
/// normalization term is at or above its true value and the resulting score
/// is at or below the true score — the safe direction for an upper bound.
#[must_use]
pub fn decode_length(norm: u8) -> u32 {
    let target = u32::from(norm);
    if target < 40 {
        return target;
    }
    let mut bucket = 40_u32;
    let mut value = 40_u32;
    let mut step = 1_u32;
    let mut since = 0_u32;
    while bucket < 255 {
        value = value.saturating_add(step);
        if bucket == target {
            return value.saturating_sub(1);
        }
        bucket += 1;
        since += 1;
        if since == 8 {
            since = 0;
            step = step.saturating_mul(2);
        }
    }
    u32::MAX
}

/// Per-document norm bytes for one segment, indexed by dense row id.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Norms {
    bytes: Vec<u8>,
}

impl Norms {
    /// Creates an empty norm array.
    #[must_use]
    pub const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Builds norms from exact lengths, in row-id order.
    #[must_use]
    pub fn from_lengths(lengths: &[u32]) -> Self {
        Self {
            bytes: lengths.iter().copied().map(encode_length).collect(),
        }
    }

    /// Returns the raw norm bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Wraps existing norm bytes.
    #[must_use]
    pub const fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Returns the number of documents covered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns true when no document is covered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the decoded length for one row, or `None` when out of range.
    #[must_use]
    pub fn length(&self, row: u32) -> Option<u32> {
        let index = usize::try_from(row).ok()?;
        self.bytes.get(index).copied().map(decode_length)
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

    #[test]
    fn encoding_is_monotone_non_decreasing_in_length() {
        let mut previous = 0_u8;
        let mut length = 0_u32;
        while length < 1_000_000 {
            let encoded = encode_length(length);
            assert!(
                encoded >= previous,
                "encoding fell from {previous} to {encoded} at length {length}"
            );
            previous = encoded;
            length += if length < 128 { 1 } else { length / 16 };
        }
    }

    #[test]
    fn short_documents_are_encoded_exactly() {
        for length in 0..40_u32 {
            assert_eq!(encode_length(length), u8::try_from(length).expect("small"));
            assert_eq!(decode_length(encode_length(length)), length);
        }
    }

    #[test]
    fn decoding_never_understates_the_true_length() {
        let mut length = 0_u32;
        while length < 5_000_000 {
            let restored = decode_length(encode_length(length));
            assert!(
                restored >= length,
                "decoded {restored} is below the true length {length}"
            );
            length += if length < 256 { 1 } else { length / 8 };
        }
    }

    #[test]
    fn the_full_byte_range_is_reachable_and_saturating() {
        assert_eq!(encode_length(u32::MAX), u8::MAX);
        assert_eq!(decode_length(u8::MAX), u32::MAX);
        assert_eq!(encode_length(0), 0);
    }

    #[test]
    fn norms_index_by_row_id() {
        let norms = Norms::from_lengths(&[0, 5, 39, 4096]);
        assert_eq!(norms.len(), 4);
        assert!(!norms.is_empty());
        assert_eq!(norms.length(0), Some(0));
        assert_eq!(norms.length(1), Some(5));
        assert_eq!(norms.length(2), Some(39));
        assert!(norms.length(3).expect("row 3") >= 4096);
        assert_eq!(norms.length(4), None);
        assert!(Norms::new().is_empty());
    }

    #[test]
    fn norm_bytes_round_trip_through_the_byte_array() {
        let norms = Norms::from_lengths(&[1, 2, 3]);
        let restored = Norms::from_bytes(norms.as_bytes().to_vec());
        assert_eq!(norms, restored);
    }
}
