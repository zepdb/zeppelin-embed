//! PDX dimension-major block layout.

use crate::quant::QuantScheme;

/// Provisional number of rows in one PDX block.
///
/// Part B's required 64-versus-128 micro-experiment may replace this single
/// value. All block construction and traversal derives its geometry from this
/// constant; no other production path hardcodes a row count.
pub const PDX_ROWS_PER_BLOCK: usize = 64;

/// Stable, pointer-free descriptor for one dimension-major block.
///
/// The descriptor and its corresponding payload slice are independently
/// checksummable. This is layout metadata only: Task 07 owns file framing,
/// magic values, checksum choice, and durable-format versioning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PdxBlock {
    first_row: u64,
    row_count: u32,
    column_count: u32,
    element_width: u32,
    reserved: u32,
    payload_offset: u64,
    payload_length: u64,
}

impl PdxBlock {
    /// Returns the zero-based first row in this block.
    #[must_use]
    pub const fn first_row(self) -> u64 {
        self.first_row
    }

    /// Returns the number of rows in this block.
    #[must_use]
    pub const fn row_count(self) -> u32 {
        self.row_count
    }

    /// Returns the number of stored dimension columns.
    #[must_use]
    pub const fn column_count(self) -> u32 {
        self.column_count
    }

    /// Returns the byte width of one stored column element.
    #[must_use]
    pub const fn element_width(self) -> u32 {
        self.element_width
    }

    /// Returns this block's offset in [`PdxMatrix::encoded_bytes`].
    #[must_use]
    pub const fn payload_offset(self) -> u64 {
        self.payload_offset
    }

    /// Returns this block's payload length.
    #[must_use]
    pub const fn payload_length(self) -> u64 {
        self.payload_length
    }

    /// Returns a canonical little-endian byte representation for checksumming.
    ///
    /// The returned metadata deliberately contains no file header or magic.
    #[must_use]
    pub fn checksum_metadata(self) -> [u8; 40] {
        let bytes = self
            .first_row
            .to_le_bytes()
            .into_iter()
            .chain(self.row_count.to_le_bytes())
            .chain(self.column_count.to_le_bytes())
            .chain(self.element_width.to_le_bytes())
            .chain(self.reserved.to_le_bytes())
            .chain(self.payload_offset.to_le_bytes())
            .chain(self.payload_length.to_le_bytes());
        let mut metadata = [0_u8; 40];
        for (output, byte) in metadata.iter_mut().zip(bytes) {
            *output = byte;
        }
        metadata
    }
}

/// Owned PDX matrix with dimension-major bytes inside each row block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PdxMatrix {
    scheme: QuantScheme,
    dimension: usize,
    row_count: usize,
    row_width: usize,
    element_width: usize,
    rows_per_block: usize,
    blocks: Vec<PdxBlock>,
    encoded: Vec<u8>,
    f32_metadata: Vec<F32BlockMetadata>,
    f32_first_non_finite: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct F32BlockMetadata {
    extrema_bits: Vec<(u32, u32)>,
}

/// Typed failure from PDX geometry validation or decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PdxError {
    /// Logical dimensions must contain at least one coordinate.
    ZeroDimension,
    /// Row-major input did not contain a whole number of rows.
    RowDataLength {
        /// Required byte or scalar width of one row.
        row_width: usize,
        /// Supplied byte or scalar count.
        actual: usize,
    },
    /// Encoded input was truncated or had trailing torn bytes.
    PayloadLength {
        /// Exact payload length implied by the out-of-band geometry.
        expected: usize,
        /// Supplied payload length.
        actual: usize,
    },
    /// The typed decoder did not match the encoded scheme.
    SchemeMismatch {
        /// Scheme required by the decoder.
        expected: QuantScheme,
        /// Scheme stored by the matrix.
        actual: QuantScheme,
    },
    /// A Bit4 row had non-zero low-nibble padding.
    NonZeroBit4Padding {
        /// Zero-based row id.
        row_id: usize,
        /// Final packed byte.
        byte: u8,
    },
    /// Row count exceeded the `u32` row-id domain used by filters.
    RowCountTooLarge {
        /// Supplied row count.
        actual: usize,
    },
    /// Geometry arithmetic overflowed `usize` or stable descriptor fields.
    ArithmeticOverflow,
    /// A caller-selected PDX block size was zero.
    ZeroRowsPerBlock,
    /// An owned descriptor did not address exactly its validated payload.
    CorruptBlock,
}

impl std::fmt::Display for PdxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDimension => formatter.write_str("PDX dimension must not be zero"),
            Self::RowDataLength { row_width, actual } => write!(
                formatter,
                "PDX row data length {actual} is not divisible by row width {row_width}"
            ),
            Self::PayloadLength { expected, actual } => write!(
                formatter,
                "PDX payload length mismatch: expected {expected}, got {actual}"
            ),
            Self::SchemeMismatch { expected, actual } => write!(
                formatter,
                "PDX scheme mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::NonZeroBit4Padding { row_id, byte } => write!(
                formatter,
                "PDX Bit4 row {row_id} has non-zero padding in byte {byte:#04x}"
            ),
            Self::RowCountTooLarge { actual } => {
                write!(formatter, "PDX row count {actual} exceeds the u32 domain")
            }
            Self::ArithmeticOverflow => formatter.write_str("PDX geometry arithmetic overflowed"),
            Self::ZeroRowsPerBlock => formatter.write_str("PDX rows per block must not be zero"),
            Self::CorruptBlock => formatter.write_str("PDX block descriptor is inconsistent"),
        }
    }
}

impl std::error::Error for PdxError {}

impl PdxMatrix {
    /// Encodes contiguous row-major f32 coordinates into PDX blocks.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for a zero dimension, a partial row, excessive row
    /// count, or arithmetic overflow.
    pub fn encode_f32(rows: &[f32], dimension: usize) -> Result<Self, PdxError> {
        Self::encode_f32_with_rows_per_block(rows, dimension, PDX_ROWS_PER_BLOCK)
    }

    /// Encodes f32 rows with a caller-selected runtime block geometry.
    ///
    /// This exists so the provisional 64-versus-128 block experiment can run
    /// without recompiling the crate. The durable default remains
    /// [`PDX_ROWS_PER_BLOCK`].
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid row geometry or a zero block size.
    pub fn encode_f32_with_rows_per_block(
        rows: &[f32],
        dimension: usize,
        rows_per_block: usize,
    ) -> Result<Self, PdxError> {
        let row_count = validate_scalar_rows(rows.len(), dimension)?;
        let row_width = dimension
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or(PdxError::ArithmeticOverflow)?;
        let mut bytes = Vec::with_capacity(
            rows.len()
                .checked_mul(std::mem::size_of::<f32>())
                .ok_or(PdxError::ArithmeticOverflow)?,
        );
        for &value in rows {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        Self::encode_row_bytes(
            QuantScheme::F32,
            dimension,
            row_count,
            row_width,
            std::mem::size_of::<f32>(),
            rows_per_block,
            &bytes,
        )
    }

    /// Encodes contiguous row-major IEEE-f16 bit patterns into PDX blocks.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid geometry or arithmetic overflow.
    pub fn encode_f16(rows: &[u16], dimension: usize) -> Result<Self, PdxError> {
        Self::encode_f16_with_rows_per_block(rows, dimension, PDX_ROWS_PER_BLOCK)
    }

    /// Encodes f16 rows with a caller-selected runtime block geometry.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid row geometry or a zero block size.
    pub fn encode_f16_with_rows_per_block(
        rows: &[u16],
        dimension: usize,
        rows_per_block: usize,
    ) -> Result<Self, PdxError> {
        let row_count = validate_scalar_rows(rows.len(), dimension)?;
        let row_width = dimension
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or(PdxError::ArithmeticOverflow)?;
        let mut bytes = Vec::with_capacity(
            rows.len()
                .checked_mul(std::mem::size_of::<u16>())
                .ok_or(PdxError::ArithmeticOverflow)?,
        );
        for &value in rows {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Self::encode_row_bytes(
            QuantScheme::F16,
            dimension,
            row_count,
            row_width,
            std::mem::size_of::<u16>(),
            rows_per_block,
            &bytes,
        )
    }

    /// Encodes contiguous row-major signed-byte codes into PDX blocks.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid geometry or arithmetic overflow.
    pub fn encode_int8(rows: &[i8], dimension: usize) -> Result<Self, PdxError> {
        Self::encode_int8_with_rows_per_block(rows, dimension, PDX_ROWS_PER_BLOCK)
    }

    /// Encodes Int8 rows with a caller-selected runtime block geometry.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid row geometry or a zero block size.
    pub fn encode_int8_with_rows_per_block(
        rows: &[i8],
        dimension: usize,
        rows_per_block: usize,
    ) -> Result<Self, PdxError> {
        let row_count = validate_scalar_rows(rows.len(), dimension)?;
        let bytes = rows.iter().map(|&value| value as u8).collect::<Vec<_>>();
        Self::encode_row_bytes(
            QuantScheme::Int8,
            dimension,
            row_count,
            dimension,
            1,
            rows_per_block,
            &bytes,
        )
    }

    /// Encodes contiguous row-major MSB-first Bit4 codes into PDX blocks.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid geometry, partial rows, non-canonical
    /// odd-dimension padding, excessive row count, or arithmetic overflow.
    pub fn encode_bit4(rows: &[u8], dimension: usize) -> Result<Self, PdxError> {
        Self::encode_bit4_with_rows_per_block(rows, dimension, PDX_ROWS_PER_BLOCK)
    }

    /// Encodes Bit4 rows with a caller-selected runtime block geometry.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid row geometry or a zero block size.
    pub fn encode_bit4_with_rows_per_block(
        rows: &[u8],
        dimension: usize,
        rows_per_block: usize,
    ) -> Result<Self, PdxError> {
        if dimension == 0 {
            return Err(PdxError::ZeroDimension);
        }
        let row_width = dimension.div_ceil(2);
        if !rows.len().is_multiple_of(row_width) {
            return Err(PdxError::RowDataLength {
                row_width,
                actual: rows.len(),
            });
        }
        let row_count = rows.len() / row_width;
        validate_row_count(row_count)?;
        validate_bit4_padding(rows, dimension, row_width)?;
        Self::encode_row_bytes(
            QuantScheme::Bit4,
            dimension,
            row_count,
            row_width,
            1,
            rows_per_block,
            rows,
        )
    }

    /// Validates dimension-major bytes using caller-supplied layout geometry.
    ///
    /// There is deliberately no embedded header or magic number. Task 07 must
    /// supply and authenticate `scheme`, `dimension`, and `row_count` before
    /// calling this decoder.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid geometry, truncated or torn payloads,
    /// non-canonical Bit4 padding, or arithmetic overflow.
    pub fn from_encoded_bytes(
        scheme: QuantScheme,
        dimension: usize,
        row_count: usize,
        encoded: &[u8],
    ) -> Result<Self, PdxError> {
        Self::from_encoded_bytes_with_rows_per_block(
            scheme,
            dimension,
            row_count,
            PDX_ROWS_PER_BLOCK,
            encoded,
        )
    }

    /// Validates dimension-major bytes with caller-selected block geometry.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] for invalid geometry, payload, or block size.
    pub fn from_encoded_bytes_with_rows_per_block(
        scheme: QuantScheme,
        dimension: usize,
        row_count: usize,
        rows_per_block: usize,
        encoded: &[u8],
    ) -> Result<Self, PdxError> {
        if dimension == 0 {
            return Err(PdxError::ZeroDimension);
        }
        validate_row_count(row_count)?;
        let (row_width, element_width) = scheme_geometry(scheme, dimension)?;
        let expected = row_count
            .checked_mul(row_width)
            .ok_or(PdxError::ArithmeticOverflow)?;
        if encoded.len() != expected {
            return Err(PdxError::PayloadLength {
                expected,
                actual: encoded.len(),
            });
        }
        let blocks = build_blocks(row_count, row_width, element_width, rows_per_block)?;
        let mut matrix = Self {
            scheme,
            dimension,
            row_count,
            row_width,
            element_width,
            rows_per_block,
            blocks,
            encoded: encoded.to_vec(),
            f32_metadata: Vec::new(),
            f32_first_non_finite: None,
        };
        let rows = matrix.decode_row_bytes()?;
        if scheme == QuantScheme::Bit4 {
            validate_bit4_padding(&rows, dimension, row_width)?;
        }
        let (metadata, first_non_finite) =
            build_f32_metadata(scheme, dimension, &matrix.blocks, &rows)?;
        matrix.f32_metadata = metadata;
        matrix.f32_first_non_finite = first_non_finite;
        Ok(matrix)
    }

    /// Returns the encoded scheme.
    #[must_use]
    pub const fn scheme(&self) -> QuantScheme {
        self.scheme
    }

    /// Returns the logical coordinate dimension.
    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dimension
    }

    /// Returns the number of encoded rows.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns the runtime-selected maximum rows in each PDX block.
    #[must_use]
    pub const fn rows_per_block(&self) -> usize {
        self.rows_per_block
    }

    /// Returns stable block descriptors in row order.
    #[must_use]
    pub fn blocks(&self) -> &[PdxBlock] {
        &self.blocks
    }

    /// Returns the concatenated dimension-major block payloads.
    #[must_use]
    pub fn encoded_bytes(&self) -> &[u8] {
        &self.encoded
    }

    /// Decodes PDX bytes to contiguous row-major f32 coordinates.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] if the scheme or owned block descriptors disagree.
    pub fn decode_f32(&self) -> Result<Vec<f32>, PdxError> {
        self.require_scheme(QuantScheme::F32)?;
        let bytes = self.decode_row_bytes()?;
        bytes
            .chunks_exact(std::mem::size_of::<f32>())
            .map(|chunk| {
                let array = <[u8; 4]>::try_from(chunk).map_err(|_| PdxError::CorruptBlock)?;
                Ok(f32::from_bits(u32::from_le_bytes(array)))
            })
            .collect()
    }

    /// Decodes PDX bytes to contiguous row-major IEEE-f16 bit patterns.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] if the scheme or owned block descriptors disagree.
    pub fn decode_f16(&self) -> Result<Vec<u16>, PdxError> {
        self.require_scheme(QuantScheme::F16)?;
        let bytes = self.decode_row_bytes()?;
        bytes
            .chunks_exact(std::mem::size_of::<u16>())
            .map(|chunk| {
                let array = <[u8; 2]>::try_from(chunk).map_err(|_| PdxError::CorruptBlock)?;
                Ok(u16::from_le_bytes(array))
            })
            .collect()
    }

    /// Decodes PDX bytes to contiguous row-major signed-byte codes.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] if the scheme or owned block descriptors disagree.
    pub fn decode_int8(&self) -> Result<Vec<i8>, PdxError> {
        self.require_scheme(QuantScheme::Int8)?;
        Ok(self
            .decode_row_bytes()?
            .into_iter()
            .map(|value| value as i8)
            .collect())
    }

    /// Decodes PDX bytes to contiguous row-major MSB-first Bit4 codes.
    ///
    /// # Errors
    ///
    /// Returns [`PdxError`] if the scheme, padding, or block descriptors
    /// disagree.
    pub fn decode_bit4(&self) -> Result<Vec<u8>, PdxError> {
        self.require_scheme(QuantScheme::Bit4)?;
        let rows = self.decode_row_bytes()?;
        validate_bit4_padding(&rows, self.dimension, self.row_width)?;
        Ok(rows)
    }

    pub(crate) fn decode_f32_range(
        &self,
        rows: std::ops::Range<usize>,
    ) -> Result<Vec<f32>, PdxError> {
        self.require_scheme(QuantScheme::F32)?;
        let bytes = self.decode_row_byte_range(rows)?;
        bytes
            .chunks_exact(std::mem::size_of::<f32>())
            .map(|chunk| {
                let array = <[u8; 4]>::try_from(chunk).map_err(|_| PdxError::CorruptBlock)?;
                Ok(f32::from_bits(u32::from_le_bytes(array)))
            })
            .collect()
    }

    pub(crate) fn decode_f16_range(
        &self,
        rows: std::ops::Range<usize>,
    ) -> Result<Vec<u16>, PdxError> {
        self.require_scheme(QuantScheme::F16)?;
        let bytes = self.decode_row_byte_range(rows)?;
        bytes
            .chunks_exact(std::mem::size_of::<u16>())
            .map(|chunk| {
                let array = <[u8; 2]>::try_from(chunk).map_err(|_| PdxError::CorruptBlock)?;
                Ok(u16::from_le_bytes(array))
            })
            .collect()
    }

    pub(crate) fn decode_int8_range(
        &self,
        rows: std::ops::Range<usize>,
    ) -> Result<Vec<i8>, PdxError> {
        self.require_scheme(QuantScheme::Int8)?;
        Ok(self
            .decode_row_byte_range(rows)?
            .into_iter()
            .map(|value| value as i8)
            .collect())
    }

    pub(crate) fn decode_bit4_range(
        &self,
        rows: std::ops::Range<usize>,
    ) -> Result<Vec<u8>, PdxError> {
        self.require_scheme(QuantScheme::Bit4)?;
        let decoded = self.decode_row_byte_range(rows)?;
        validate_bit4_padding(&decoded, self.dimension, self.row_width)?;
        Ok(decoded)
    }

    pub(crate) fn f32_first_non_finite(&self) -> Option<usize> {
        self.f32_first_non_finite
    }

    pub(crate) fn f32_extrema(&self, block_index: usize) -> Result<&[(u32, u32)], PdxError> {
        self.require_scheme(QuantScheme::F32)?;
        self.f32_metadata
            .get(block_index)
            .map(|metadata| metadata.extrema_bits.as_slice())
            .ok_or(PdxError::CorruptBlock)
    }

    pub(crate) fn f32_column(&self, block_index: usize, column: usize) -> Result<&[u8], PdxError> {
        self.require_scheme(QuantScheme::F32)?;
        let block = self.blocks.get(block_index).ok_or(PdxError::CorruptBlock)?;
        if column >= self.dimension {
            return Err(PdxError::CorruptBlock);
        }
        let payload_offset =
            usize::try_from(block.payload_offset).map_err(|_| PdxError::CorruptBlock)?;
        let column_width = (block.row_count as usize)
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or(PdxError::CorruptBlock)?;
        let start = column
            .checked_mul(column_width)
            .and_then(|offset| payload_offset.checked_add(offset))
            .ok_or(PdxError::CorruptBlock)?;
        let end = start
            .checked_add(column_width)
            .ok_or(PdxError::CorruptBlock)?;
        self.encoded.get(start..end).ok_or(PdxError::CorruptBlock)
    }

    fn encode_row_bytes(
        scheme: QuantScheme,
        dimension: usize,
        row_count: usize,
        row_width: usize,
        element_width: usize,
        rows_per_block: usize,
        rows: &[u8],
    ) -> Result<Self, PdxError> {
        let expected = row_count
            .checked_mul(row_width)
            .ok_or(PdxError::ArithmeticOverflow)?;
        if rows.len() != expected {
            return Err(PdxError::RowDataLength {
                row_width,
                actual: rows.len(),
            });
        }
        let blocks = build_blocks(row_count, row_width, element_width, rows_per_block)?;
        let (f32_metadata, f32_first_non_finite) =
            build_f32_metadata(scheme, dimension, &blocks, rows)?;
        let mut encoded = Vec::with_capacity(expected);
        for block in &blocks {
            let first_row =
                usize::try_from(block.first_row).map_err(|_| PdxError::ArithmeticOverflow)?;
            let block_rows = block.row_count as usize;
            let columns = block.column_count as usize;
            for column in 0..columns {
                for local_row in 0..block_rows {
                    let row = first_row
                        .checked_add(local_row)
                        .ok_or(PdxError::ArithmeticOverflow)?;
                    let start = row
                        .checked_mul(row_width)
                        .and_then(|offset| {
                            column
                                .checked_mul(element_width)
                                .and_then(|column_offset| offset.checked_add(column_offset))
                        })
                        .ok_or(PdxError::ArithmeticOverflow)?;
                    let end = start
                        .checked_add(element_width)
                        .ok_or(PdxError::ArithmeticOverflow)?;
                    let element = rows.get(start..end).ok_or(PdxError::CorruptBlock)?;
                    encoded.extend_from_slice(element);
                }
            }
        }
        Ok(Self {
            scheme,
            dimension,
            row_count,
            row_width,
            element_width,
            rows_per_block,
            blocks,
            encoded,
            f32_metadata,
            f32_first_non_finite,
        })
    }

    fn require_scheme(&self, expected: QuantScheme) -> Result<(), PdxError> {
        if self.scheme != expected {
            return Err(PdxError::SchemeMismatch {
                expected,
                actual: self.scheme,
            });
        }
        Ok(())
    }

    fn decode_row_bytes(&self) -> Result<Vec<u8>, PdxError> {
        let expected = self
            .row_count
            .checked_mul(self.row_width)
            .ok_or(PdxError::ArithmeticOverflow)?;
        if self.encoded.len() != expected {
            return Err(PdxError::PayloadLength {
                expected,
                actual: self.encoded.len(),
            });
        }
        let mut rows = vec![0_u8; expected];
        for block in &self.blocks {
            let first_row = usize::try_from(block.first_row).map_err(|_| PdxError::CorruptBlock)?;
            let block_rows = block.row_count as usize;
            let columns = block.column_count as usize;
            let payload_offset =
                usize::try_from(block.payload_offset).map_err(|_| PdxError::CorruptBlock)?;
            let payload_length =
                usize::try_from(block.payload_length).map_err(|_| PdxError::CorruptBlock)?;
            let payload_end = payload_offset
                .checked_add(payload_length)
                .ok_or(PdxError::CorruptBlock)?;
            let payload = self
                .encoded
                .get(payload_offset..payload_end)
                .ok_or(PdxError::CorruptBlock)?;
            for column in 0..columns {
                for local_row in 0..block_rows {
                    let source = column
                        .checked_mul(block_rows)
                        .and_then(|offset| offset.checked_add(local_row))
                        .and_then(|element| element.checked_mul(self.element_width))
                        .ok_or(PdxError::CorruptBlock)?;
                    let source_end = source
                        .checked_add(self.element_width)
                        .ok_or(PdxError::CorruptBlock)?;
                    let row = first_row
                        .checked_add(local_row)
                        .ok_or(PdxError::CorruptBlock)?;
                    let target = row
                        .checked_mul(self.row_width)
                        .and_then(|offset| {
                            column
                                .checked_mul(self.element_width)
                                .and_then(|column_offset| offset.checked_add(column_offset))
                        })
                        .ok_or(PdxError::CorruptBlock)?;
                    let target_end = target
                        .checked_add(self.element_width)
                        .ok_or(PdxError::CorruptBlock)?;
                    let source = payload
                        .get(source..source_end)
                        .ok_or(PdxError::CorruptBlock)?;
                    let target = rows
                        .get_mut(target..target_end)
                        .ok_or(PdxError::CorruptBlock)?;
                    target.copy_from_slice(source);
                }
            }
        }
        Ok(rows)
    }

    fn decode_row_byte_range(&self, rows: std::ops::Range<usize>) -> Result<Vec<u8>, PdxError> {
        if rows.start > rows.end || rows.end > self.row_count {
            return Err(PdxError::CorruptBlock);
        }
        let output_rows = rows
            .end
            .checked_sub(rows.start)
            .ok_or(PdxError::CorruptBlock)?;
        let output_length = output_rows
            .checked_mul(self.row_width)
            .ok_or(PdxError::ArithmeticOverflow)?;
        let mut decoded = vec![0_u8; output_length];
        for block in &self.blocks {
            let first_row = usize::try_from(block.first_row).map_err(|_| PdxError::CorruptBlock)?;
            let block_rows = block.row_count as usize;
            let block_end = first_row
                .checked_add(block_rows)
                .ok_or(PdxError::CorruptBlock)?;
            let intersection_start = first_row.max(rows.start);
            let intersection_end = block_end.min(rows.end);
            if intersection_start >= intersection_end {
                continue;
            }
            let payload_offset =
                usize::try_from(block.payload_offset).map_err(|_| PdxError::CorruptBlock)?;
            let payload_length =
                usize::try_from(block.payload_length).map_err(|_| PdxError::CorruptBlock)?;
            let payload_end = payload_offset
                .checked_add(payload_length)
                .ok_or(PdxError::CorruptBlock)?;
            let payload = self
                .encoded
                .get(payload_offset..payload_end)
                .ok_or(PdxError::CorruptBlock)?;
            let columns = block.column_count as usize;
            for column in 0..columns {
                for row_id in intersection_start..intersection_end {
                    let local_row = row_id
                        .checked_sub(first_row)
                        .ok_or(PdxError::CorruptBlock)?;
                    let source = column
                        .checked_mul(block_rows)
                        .and_then(|offset| offset.checked_add(local_row))
                        .and_then(|element| element.checked_mul(self.element_width))
                        .ok_or(PdxError::CorruptBlock)?;
                    let source_end = source
                        .checked_add(self.element_width)
                        .ok_or(PdxError::CorruptBlock)?;
                    let output_row = row_id
                        .checked_sub(rows.start)
                        .ok_or(PdxError::CorruptBlock)?;
                    let target = output_row
                        .checked_mul(self.row_width)
                        .and_then(|offset| {
                            column
                                .checked_mul(self.element_width)
                                .and_then(|column_offset| offset.checked_add(column_offset))
                        })
                        .ok_or(PdxError::CorruptBlock)?;
                    let target_end = target
                        .checked_add(self.element_width)
                        .ok_or(PdxError::CorruptBlock)?;
                    let source = payload
                        .get(source..source_end)
                        .ok_or(PdxError::CorruptBlock)?;
                    let target = decoded
                        .get_mut(target..target_end)
                        .ok_or(PdxError::CorruptBlock)?;
                    target.copy_from_slice(source);
                }
            }
        }
        Ok(decoded)
    }
}

fn validate_scalar_rows(scalar_count: usize, dimension: usize) -> Result<usize, PdxError> {
    if dimension == 0 {
        return Err(PdxError::ZeroDimension);
    }
    if !scalar_count.is_multiple_of(dimension) {
        return Err(PdxError::RowDataLength {
            row_width: dimension,
            actual: scalar_count,
        });
    }
    let row_count = scalar_count / dimension;
    validate_row_count(row_count)?;
    Ok(row_count)
}

fn validate_row_count(row_count: usize) -> Result<(), PdxError> {
    if u32::try_from(row_count).is_err() {
        return Err(PdxError::RowCountTooLarge { actual: row_count });
    }
    Ok(())
}

fn scheme_geometry(scheme: QuantScheme, dimension: usize) -> Result<(usize, usize), PdxError> {
    let element_width = match scheme {
        QuantScheme::F32 => std::mem::size_of::<f32>(),
        QuantScheme::F16 => std::mem::size_of::<u16>(),
        QuantScheme::Int8 | QuantScheme::Bit4 => 1,
    };
    let column_count = if scheme == QuantScheme::Bit4 {
        dimension.div_ceil(2)
    } else {
        dimension
    };
    let row_width = column_count
        .checked_mul(element_width)
        .ok_or(PdxError::ArithmeticOverflow)?;
    Ok((row_width, element_width))
}

fn build_blocks(
    row_count: usize,
    row_width: usize,
    element_width: usize,
    rows_per_block: usize,
) -> Result<Vec<PdxBlock>, PdxError> {
    if rows_per_block == 0 {
        return Err(PdxError::ZeroRowsPerBlock);
    }
    let column_count = row_width
        .checked_div(element_width)
        .ok_or(PdxError::ArithmeticOverflow)?;
    let column_count = u32::try_from(column_count).map_err(|_| PdxError::ArithmeticOverflow)?;
    let element_width = u32::try_from(element_width).map_err(|_| PdxError::ArithmeticOverflow)?;
    let mut blocks = Vec::with_capacity(row_count.div_ceil(rows_per_block));
    let mut first_row = 0_usize;
    let mut payload_offset = 0_usize;
    while first_row < row_count {
        let block_rows = (row_count - first_row).min(rows_per_block);
        let payload_length = block_rows
            .checked_mul(row_width)
            .ok_or(PdxError::ArithmeticOverflow)?;
        blocks.push(PdxBlock {
            first_row: u64::try_from(first_row).map_err(|_| PdxError::ArithmeticOverflow)?,
            row_count: u32::try_from(block_rows).map_err(|_| PdxError::ArithmeticOverflow)?,
            column_count,
            element_width,
            reserved: 0,
            payload_offset: u64::try_from(payload_offset)
                .map_err(|_| PdxError::ArithmeticOverflow)?,
            payload_length: u64::try_from(payload_length)
                .map_err(|_| PdxError::ArithmeticOverflow)?,
        });
        first_row = first_row
            .checked_add(block_rows)
            .ok_or(PdxError::ArithmeticOverflow)?;
        payload_offset = payload_offset
            .checked_add(payload_length)
            .ok_or(PdxError::ArithmeticOverflow)?;
    }
    Ok(blocks)
}

fn build_f32_metadata(
    scheme: QuantScheme,
    dimension: usize,
    blocks: &[PdxBlock],
    rows: &[u8],
) -> Result<(Vec<F32BlockMetadata>, Option<usize>), PdxError> {
    if scheme != QuantScheme::F32 {
        return Ok((Vec::new(), None));
    }
    let row_width = dimension
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or(PdxError::ArithmeticOverflow)?;
    let mut metadata = Vec::with_capacity(blocks.len());
    let mut first_non_finite: Option<usize> = None;
    for block in blocks {
        let first_row =
            usize::try_from(block.first_row).map_err(|_| PdxError::ArithmeticOverflow)?;
        let block_rows = block.row_count as usize;
        let mut extrema_bits = Vec::with_capacity(dimension);
        for column in 0..dimension {
            let mut minimum = f32::INFINITY;
            let mut maximum = f32::NEG_INFINITY;
            for local_row in 0..block_rows {
                let row_id = first_row
                    .checked_add(local_row)
                    .ok_or(PdxError::ArithmeticOverflow)?;
                let scalar_index = row_id
                    .checked_mul(dimension)
                    .and_then(|offset| offset.checked_add(column))
                    .ok_or(PdxError::ArithmeticOverflow)?;
                let byte_start = row_id
                    .checked_mul(row_width)
                    .and_then(|offset| {
                        column
                            .checked_mul(std::mem::size_of::<f32>())
                            .and_then(|column_offset| offset.checked_add(column_offset))
                    })
                    .ok_or(PdxError::ArithmeticOverflow)?;
                let byte_end = byte_start
                    .checked_add(std::mem::size_of::<f32>())
                    .ok_or(PdxError::ArithmeticOverflow)?;
                let bytes = rows
                    .get(byte_start..byte_end)
                    .ok_or(PdxError::CorruptBlock)?;
                let array = <[u8; 4]>::try_from(bytes).map_err(|_| PdxError::CorruptBlock)?;
                let value = f32::from_bits(u32::from_le_bytes(array));
                if value.is_finite() {
                    minimum = minimum.min(value);
                    maximum = maximum.max(value);
                } else {
                    first_non_finite = Some(
                        first_non_finite.map_or(scalar_index, |current| current.min(scalar_index)),
                    );
                }
            }
            extrema_bits.push((minimum.to_bits(), maximum.to_bits()));
        }
        metadata.push(F32BlockMetadata { extrema_bits });
    }
    Ok((metadata, first_non_finite))
}

fn validate_bit4_padding(rows: &[u8], dimension: usize, row_width: usize) -> Result<(), PdxError> {
    if dimension.is_multiple_of(2) {
        return Ok(());
    }
    for (row_id, row) in rows.chunks_exact(row_width).enumerate() {
        if let Some(&byte) = row.last()
            && byte & 0x0f != 0
        {
            return Err(PdxError::NonZeroBit4Padding { row_id, byte });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use rand::{Rng, RngCore};

    use crate::quant::QuantScheme;

    use super::{PDX_ROWS_PER_BLOCK, PdxError, PdxMatrix};

    #[test]
    fn prop_pdx_roundtrip_preserves_rows() {
        let mut random =
            crate::test_support::seeded_rng("scan::pdx::prop_pdx_roundtrip_preserves_rows");
        let cases = std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(256);

        for case in 0..cases {
            let dimension = match case % 8 {
                0 => 1,
                1 => 7,
                2 => 8,
                3 => 15,
                4 => 17,
                5 => 31,
                6 => 33,
                _ => random.random_range(1..=1_024),
            };
            let row_count = match case % 5 {
                0 => 0,
                1 => PDX_ROWS_PER_BLOCK - 1,
                2 => PDX_ROWS_PER_BLOCK,
                3 => PDX_ROWS_PER_BLOCK + 1,
                _ => random.random_range(0..=256),
            };

            match case % 4 {
                0 => {
                    let rows = (0..row_count * dimension)
                        .map(|_| random.random_range(-128_i16..=127_i16) as f32 / 8.0)
                        .collect::<Vec<_>>();
                    let pdx = PdxMatrix::encode_f32(&rows, dimension).expect("valid f32 rows");
                    assert_eq!(pdx.scheme(), QuantScheme::F32);
                    assert_eq!(pdx.decode_f32().expect("valid PDX"), rows);
                }
                1 => {
                    let rows = (0..row_count * dimension)
                        .map(|_| random.next_u32() as u16)
                        .collect::<Vec<_>>();
                    let pdx = PdxMatrix::encode_f16(&rows, dimension).expect("valid f16 rows");
                    assert_eq!(pdx.scheme(), QuantScheme::F16);
                    assert_eq!(pdx.decode_f16().expect("valid PDX"), rows);
                }
                2 => {
                    let rows = (0..row_count * dimension)
                        .map(|_| random.random::<i8>())
                        .collect::<Vec<_>>();
                    let pdx = PdxMatrix::encode_int8(&rows, dimension).expect("valid int8 rows");
                    assert_eq!(pdx.scheme(), QuantScheme::Int8);
                    assert_eq!(pdx.decode_int8().expect("valid PDX"), rows);
                }
                _ => {
                    let row_bytes = dimension.div_ceil(2);
                    let mut rows = (0..row_count * row_bytes)
                        .map(|_| random.random::<u8>())
                        .collect::<Vec<_>>();
                    if !dimension.is_multiple_of(2) {
                        for row in rows.chunks_exact_mut(row_bytes) {
                            if let Some(last) = row.last_mut() {
                                *last &= 0xf0;
                            }
                        }
                    }
                    let pdx = PdxMatrix::encode_bit4(&rows, dimension).expect("valid bit4 rows");
                    assert_eq!(pdx.scheme(), QuantScheme::Bit4);
                    assert_eq!(pdx.decode_bit4().expect("valid PDX"), rows);
                }
            }
        }
    }

    #[test]
    fn single_block_has_stable_checksummable_geometry() {
        let dimension = 17;
        let row_count = PDX_ROWS_PER_BLOCK - 1;
        let rows = (0..row_count * dimension)
            .map(|index| index.wrapping_mul(19) as i8)
            .collect::<Vec<_>>();

        let pdx = PdxMatrix::encode_int8(&rows, dimension).expect("single block");

        assert_eq!(pdx.blocks().len(), 1);
        let block = pdx.blocks()[0];
        assert_eq!(block.first_row(), 0);
        assert_eq!(block.row_count() as usize, row_count);
        assert_eq!(block.column_count() as usize, dimension);
        assert_eq!(block.element_width(), 1);
        assert_eq!(block.payload_offset(), 0);
        assert_eq!(block.payload_length() as usize, rows.len());
        assert_ne!(block.checksum_metadata(), [0_u8; 40]);
        assert_eq!(pdx.decode_int8().expect("valid block"), rows);
    }

    #[test]
    fn one_row_over_block_boundary_uses_two_blocks() {
        let dimension = 9;
        let row_count = PDX_ROWS_PER_BLOCK + 1;
        let rows = (0..row_count * dimension)
            .map(|index| index.wrapping_mul(7) as i8)
            .collect::<Vec<_>>();

        let pdx = PdxMatrix::encode_int8(&rows, dimension).expect("boundary PDX");

        assert_eq!(pdx.blocks().len(), 2);
        assert_eq!(pdx.blocks()[0].row_count() as usize, PDX_ROWS_PER_BLOCK);
        assert_eq!(pdx.blocks()[1].first_row() as usize, PDX_ROWS_PER_BLOCK);
        assert_eq!(pdx.blocks()[1].row_count(), 1);
        assert_eq!(pdx.decode_int8().expect("valid boundary PDX"), rows);
    }

    #[test]
    fn truncated_pdx_input_returns_typed_error() {
        let pdx = PdxMatrix::encode_int8(&[1_i8, 2, 3, 4, 5, 6], 3).expect("fixture PDX");
        let truncated = &pdx.encoded_bytes()[..pdx.encoded_bytes().len() - 1];

        assert_eq!(
            PdxMatrix::from_encoded_bytes(QuantScheme::Int8, 3, 2, truncated),
            Err(PdxError::PayloadLength {
                expected: 6,
                actual: 5,
            })
        );
    }

    #[test]
    fn torn_pdx_input_returns_typed_error() {
        let pdx = PdxMatrix::encode_int8(&[1_i8, 2, 3, 4, 5, 6], 3).expect("fixture PDX");
        let mut torn = pdx.encoded_bytes().to_vec();
        torn.push(0xff);

        assert_eq!(
            PdxMatrix::from_encoded_bytes(QuantScheme::Int8, 3, 2, &torn),
            Err(PdxError::PayloadLength {
                expected: 6,
                actual: 7,
            })
        );
    }

    #[test]
    fn pdx_validation_errors_are_typed_and_actionable() {
        assert_eq!(PdxMatrix::encode_int8(&[], 0), Err(PdxError::ZeroDimension));
        assert_eq!(
            PdxMatrix::encode_f32(&[1.0], 2),
            Err(PdxError::RowDataLength {
                row_width: 2,
                actual: 1,
            })
        );
        assert_eq!(
            PdxMatrix::encode_bit4(&[0xff], 0),
            Err(PdxError::ZeroDimension)
        );
        assert_eq!(
            PdxMatrix::encode_bit4(&[0xff], 4),
            Err(PdxError::RowDataLength {
                row_width: 2,
                actual: 1,
            })
        );
        assert_eq!(
            PdxMatrix::encode_bit4(&[0xff], 1),
            Err(PdxError::NonZeroBit4Padding {
                row_id: 0,
                byte: 0xff,
            })
        );
        assert_eq!(
            PdxMatrix::from_encoded_bytes(QuantScheme::F32, 0, 0, &[]),
            Err(PdxError::ZeroDimension)
        );
        assert_eq!(
            PdxMatrix::from_encoded_bytes(QuantScheme::F32, usize::MAX, 0, &[]),
            Err(PdxError::ArithmeticOverflow)
        );
        if usize::BITS > u32::BITS {
            assert_eq!(
                PdxMatrix::from_encoded_bytes(QuantScheme::Int8, 1, u32::MAX as usize + 1, &[],),
                Err(PdxError::RowCountTooLarge {
                    actual: u32::MAX as usize + 1,
                })
            );
        }

        let f32_pdx = PdxMatrix::encode_f32(&[1.0, 2.0], 1).expect("valid f32 PDX");
        assert_eq!(f32_pdx.row_count(), 2);
        assert_eq!(
            f32_pdx.decode_int8(),
            Err(PdxError::SchemeMismatch {
                expected: QuantScheme::Int8,
                actual: QuantScheme::F32,
            })
        );

        for error in [
            PdxError::ZeroDimension,
            PdxError::RowDataLength {
                row_width: 2,
                actual: 1,
            },
            PdxError::PayloadLength {
                expected: 2,
                actual: 1,
            },
            PdxError::SchemeMismatch {
                expected: QuantScheme::F32,
                actual: QuantScheme::F16,
            },
            PdxError::NonZeroBit4Padding {
                row_id: 3,
                byte: 0x0f,
            },
            PdxError::RowCountTooLarge { actual: usize::MAX },
            PdxError::ArithmeticOverflow,
            PdxError::ZeroRowsPerBlock,
            PdxError::CorruptBlock,
        ] {
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn decoder_accepts_valid_payloads_and_rejects_corrupt_owned_blocks() {
        let fixtures = [
            PdxMatrix::encode_f32(&[1.0_f32, 2.0], 1).expect("f32 PDX"),
            PdxMatrix::encode_f16(&[0x3c00_u16, 0x4000], 1).expect("f16 PDX"),
            PdxMatrix::encode_int8(&[1_i8, 2], 1).expect("Int8 PDX"),
            PdxMatrix::encode_bit4(&[0xf0_u8, 0xe0], 1).expect("Bit4 PDX"),
        ];
        for fixture in fixtures {
            let decoded = PdxMatrix::from_encoded_bytes(
                fixture.scheme(),
                fixture.dimension(),
                fixture.row_count(),
                fixture.encoded_bytes(),
            )
            .expect("valid payload");
            assert_eq!(decoded, fixture);
        }

        let mut corrupt = PdxMatrix::encode_int8(&[1_i8, 2], 1).expect("valid PDX");
        corrupt.encoded.pop();
        assert_eq!(
            corrupt.decode_int8(),
            Err(PdxError::PayloadLength {
                expected: 2,
                actual: 1,
            })
        );

        let mut corrupt = PdxMatrix::encode_int8(&[1_i8, 2], 1).expect("valid PDX");
        corrupt.blocks[0].payload_offset = u64::MAX;
        assert_eq!(corrupt.decode_int8(), Err(PdxError::CorruptBlock));
    }

    #[test]
    fn runtime_block_geometry_supports_64_and_128_without_recompilation() {
        let dimension = 5_usize;
        let row_count = 257_usize;
        let rows = (0..row_count * dimension)
            .map(|index| index.wrapping_mul(13) as i8)
            .collect::<Vec<_>>();
        for rows_per_block in [64, 128] {
            let pdx = PdxMatrix::encode_int8_with_rows_per_block(&rows, dimension, rows_per_block)
                .expect("runtime block geometry");
            assert_eq!(pdx.rows_per_block(), rows_per_block);
            assert_eq!(pdx.blocks().len(), row_count.div_ceil(rows_per_block));
            assert_eq!(pdx.decode_int8().expect("round trip"), rows);
            assert_eq!(
                PdxMatrix::from_encoded_bytes_with_rows_per_block(
                    QuantScheme::Int8,
                    dimension,
                    row_count,
                    rows_per_block,
                    pdx.encoded_bytes(),
                )
                .expect("runtime decode"),
                pdx
            );
        }
        assert_eq!(
            PdxMatrix::encode_int8_with_rows_per_block(&rows, dimension, 0),
            Err(PdxError::ZeroRowsPerBlock)
        );
    }
}
