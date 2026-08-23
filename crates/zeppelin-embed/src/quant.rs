//! Training-free vector quantization.

mod bits4;
mod int8;
mod rescore;

pub use bits4::{
    Bit4Factors, Bit4Query, dequantize_bit4, est_dot_bit4, est_dot_bit4_batch, prepare_bit4_query,
    quantize_bit4,
};
pub use int8::{
    Int8Query, Int8Vec, dequantize_int8, dot_int8_query, prepare_int8_query, quantize_int8,
};
pub use rescore::{
    RescoreError, RescoreHit, RescoreMetric, RescorePool, RescoreResult, SearchByteCounts,
    rescore_top_k,
};

/// Typed failure from a training-free quantizer or estimator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuantError {
    /// The input dimension is zero, for which a code direction is undefined.
    EmptyVector,
    /// The vector exceeds the dimension supported by the reused i8 kernel.
    DimensionTooLarge {
        /// Supplied coordinate count.
        actual: usize,
        /// Largest accepted coordinate count.
        maximum: usize,
    },
    /// A coordinate was NaN or infinite.
    NonFinite {
        /// Zero-based coordinate of the first rejected value.
        index: usize,
    },
    /// A caller-owned output buffer had the wrong byte length.
    OutputLength {
        /// Required byte count.
        expected: usize,
        /// Supplied byte count.
        actual: usize,
    },
    /// A packed row had the wrong byte length for the query dimension.
    CodeLength {
        /// Required byte count.
        expected: usize,
        /// Supplied byte count.
        actual: usize,
    },
    /// Unused low-order fields in a final partial byte were not zero.
    NonZeroPadding {
        /// Final byte containing non-canonical padding.
        byte: u8,
        /// Mask selecting the unused low-order bits.
        mask: u8,
    },
}

impl std::fmt::Display for QuantError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyVector => formatter.write_str("quantization vector must not be empty"),
            Self::DimensionTooLarge { actual, maximum } => write!(
                formatter,
                "quantization dimension {actual} exceeds supported maximum {maximum}"
            ),
            Self::NonFinite { index } => {
                write!(
                    formatter,
                    "quantization input is non-finite at coordinate {index}"
                )
            }
            Self::OutputLength { expected, actual } => write!(
                formatter,
                "quantization output length mismatch: expected {expected}, got {actual}"
            ),
            Self::CodeLength { expected, actual } => write!(
                formatter,
                "packed quantization code length mismatch: expected {expected}, got {actual}"
            ),
            Self::NonZeroPadding { byte, mask } => write!(
                formatter,
                "packed quantization code has non-zero padding: byte={byte:#04x}, mask={mask:#04x}"
            ),
        }
    }
}

impl std::error::Error for QuantError {}

/// Persisted quantization scheme identifier.
///
/// Discriminants are an append-only storage contract. They must never be
/// reordered, renumbered, or reused. Ids 3 and 5 are retired and permanently
/// reserved by `docs/adr/ADR-002-retire-bit1-bit2.md`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum QuantScheme {
    /// Uncompressed IEEE binary32 coordinates.
    F32 = 0,
    /// IEEE binary16 coordinates.
    F16 = 1,
    /// Signed eight-bit scalar quantization.
    Int8 = 2,
    /// Four-bit Extended-RaBitQ codes.
    #[default]
    Bit4 = 4,
}

impl QuantScheme {
    /// Returns the permanent persisted identifier.
    #[must_use]
    pub const fn id(self) -> u8 {
        self as u8
    }

    /// Decodes a permanent persisted identifier.
    #[must_use]
    pub const fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::F32),
            1 => Some(Self::F16),
            2 => Some(Self::Int8),
            4 => Some(Self::Bit4),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
