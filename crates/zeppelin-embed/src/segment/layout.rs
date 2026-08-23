//! Permanent little-endian segment layout declarations and region codecs.

use crate::format::{FormatFamily, FormatRegistry};
use crate::meta::{
    AliveSet, Column, ColumnDefinition, ColumnId, ColumnInput, ColumnStore, ColumnStoreBuilder,
    ColumnType, ColumnValue, Schema, TIMESTAMP_COLUMN,
};
use crate::quant::Bit4Factors;

use super::SegmentError;

/// Darwin-friendly region alignment; no row is padded.
pub const REGION_ALIGNMENT: usize = 16 * 1024;
/// Per-chunk checksum granularity.
pub const CHECKSUM_CHUNK_BYTES: usize = 64 * 1024;
/// Segment-specific bytes following the common file header.
pub const SEGMENT_PREFIX_LEN: usize = 32;
/// Permanent region-directory entry width.
pub const REGION_ENTRY_LEN: usize = 32;
/// Permanent vector-region geometry header width.
pub const VECTOR_HEADER_LEN: usize = 32;

/// Known and reserved region identifiers.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum RegionKind {
    /// Typed metadata arrays.
    Columns = 1,
    /// Alive bitmap and tombstone mask.
    Alive = 2,
    /// Structure-of-arrays packed vector codes.
    VectorCodes = 3,
    /// Structure-of-arrays factor records.
    VectorFactors = 4,
    /// Structure-of-arrays exact-rescore rows.
    VectorRescore = 5,
    /// Reserved for Task 13 postings.
    Postings = 6,
    /// Fixed-stride graph node blocks; originally reserved under the obsolete CSR name.
    GraphNodeBlocks = 7,
    /// Reserved for graph-neighbor-colocated codes.
    GraphColocatedCodes = 8,
    /// Reserved for a future sign-plane payload.
    SignPlane = 9,
    /// Reserved for optional clustered PDX blocks.
    PdxClusteredBlocks = 10,
    /// Per-64-KB xxh3 checksum table.
    ChecksumTable = 11,
    /// Dense `(doc_id:u128, revision:u64)` records for sealed store rows.
    DocumentVersions = 12,
    /// First id reserved for additive vector-space-N region triples.
    VectorSpaceN = 4096,
}

impl RegionKind {
    /// Returns the permanent numeric identifier.
    #[must_use]
    pub const fn id(self) -> u16 {
        self as u16
    }

    /// Resolves a known or reserved identifier; unknown values remain skippable.
    #[must_use]
    pub const fn from_id(id: u16) -> Option<Self> {
        match id {
            1 => Some(Self::Columns),
            2 => Some(Self::Alive),
            3 => Some(Self::VectorCodes),
            4 => Some(Self::VectorFactors),
            5 => Some(Self::VectorRescore),
            6 => Some(Self::Postings),
            7 => Some(Self::GraphNodeBlocks),
            8 => Some(Self::GraphColocatedCodes),
            9 => Some(Self::SignPlane),
            10 => Some(Self::PdxClusteredBlocks),
            11 => Some(Self::ChecksumTable),
            12 => Some(Self::DocumentVersions),
            4096 => Some(Self::VectorSpaceN),
            _ => None,
        }
    }

    pub(crate) const fn family(self) -> Option<FormatFamily> {
        match self {
            Self::Columns => Some(FormatFamily::Columns),
            Self::Alive => Some(FormatFamily::Alive),
            Self::VectorCodes => Some(FormatFamily::VectorCodes),
            Self::VectorFactors => Some(FormatFamily::VectorFactors),
            Self::VectorRescore => Some(FormatFamily::VectorRescore),
            Self::Postings => Some(FormatFamily::Postings),
            Self::GraphNodeBlocks => Some(FormatFamily::GraphNodeBlocks),
            Self::DocumentVersions => Some(FormatFamily::DocumentVersions),
            Self::ChecksumTable => Some(FormatFamily::ChecksumTable),
            Self::GraphColocatedCodes
            | Self::SignPlane
            | Self::PdxClusteredBlocks
            | Self::VectorSpaceN => None,
        }
    }
}

/// One fixed-width region-directory entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct RegionEntry {
    /// Known or forward-compatible region kind id.
    pub kind: u16,
    /// Additive region-family version.
    pub version: u16,
    /// Explicit zero padding reserved for future flags.
    pub reserved: u32,
    /// Absolute aligned file offset.
    pub offset: u64,
    /// Exact byte length.
    pub length: u64,
    /// Whole-region xxh3-64 checksum.
    pub checksum: u64,
}

/// Permanent vector geometry and transform descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct VectorHeader {
    /// Per-segment quantization or rescore scheme id.
    pub scheme: u16,
    /// Vector-space identifier; v1 requires zero.
    pub vector_space_id: u16,
    /// Logical dimension.
    pub dims: u32,
    /// Exact bytes between adjacent code/rescore rows.
    pub row_stride_bytes: u32,
    /// Exact bytes between factor records.
    pub factor_stride_bytes: u16,
    /// Transform kind; v1 requires identity zero.
    pub transform_kind: u16,
    /// Transform seed; v1 requires zero.
    pub transform_seed: u64,
    /// Dense segment-local row count.
    pub row_count: u32,
    /// Explicit must-be-zero padding.
    pub reserved: u32,
}

impl VectorHeader {
    /// Validates all v1 invariants before any payload cast.
    pub fn validate(self) -> Result<(), SegmentError> {
        FormatRegistry::require_scheme(self.scheme)
            .map_err(|error| SegmentError::Geometry(error.to_string()))?;
        if self.vector_space_id != 0 {
            return Err(SegmentError::Geometry(format!(
                "v1 vector_space_id must be 0, got {}",
                self.vector_space_id
            )));
        }
        if self.transform_kind != 0 || self.transform_seed != 0 {
            return Err(SegmentError::Geometry(format!(
                "v1 transform must be identity/0, got kind {} seed {}",
                self.transform_kind, self.transform_seed
            )));
        }
        if self.reserved != 0 {
            return Err(SegmentError::Geometry(format!(
                "vector header reserved field must be 0, got {}",
                self.reserved
            )));
        }
        Ok(())
    }
}

/// Permanent eight-byte affine Int8 factor record.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Int8Factors {
    /// Reconstruction step.
    pub scale: f32,
    /// Reconstructed value at code zero.
    pub offset: f32,
}

const _: [(); 12] = [(); std::mem::size_of::<Bit4Factors>()];
const _: [(); 8] = [(); std::mem::size_of::<Int8Factors>()];
const _: [(); REGION_ENTRY_LEN] = [(); std::mem::size_of::<RegionEntry>()];
const _: [(); VECTOR_HEADER_LEN] = [(); std::mem::size_of::<VectorHeader>()];

pub(crate) fn align_up(value: usize, alignment: usize) -> Result<usize, SegmentError> {
    let adjustment = alignment.saturating_sub(1);
    value
        .checked_add(adjustment)
        .map(|rounded| rounded / alignment * alignment)
        .ok_or_else(|| SegmentError::Geometry("aligned offset overflow".to_owned()))
}

pub(crate) fn encode_vector_header(header: VectorHeader, output: &mut Vec<u8>) {
    output.extend_from_slice(&header.scheme.to_le_bytes());
    output.extend_from_slice(&header.vector_space_id.to_le_bytes());
    output.extend_from_slice(&header.dims.to_le_bytes());
    output.extend_from_slice(&header.row_stride_bytes.to_le_bytes());
    output.extend_from_slice(&header.factor_stride_bytes.to_le_bytes());
    output.extend_from_slice(&header.transform_kind.to_le_bytes());
    output.extend_from_slice(&header.transform_seed.to_le_bytes());
    output.extend_from_slice(&header.row_count.to_le_bytes());
    output.extend_from_slice(&header.reserved.to_le_bytes());
}

pub(crate) fn decode_vector_header(bytes: &[u8]) -> Result<VectorHeader, SegmentError> {
    let mut cursor = Cursor::new("vector-header", bytes);
    let header = VectorHeader {
        scheme: cursor.u16()?,
        vector_space_id: cursor.u16()?,
        dims: cursor.u32()?,
        row_stride_bytes: cursor.u32()?,
        factor_stride_bytes: cursor.u16()?,
        transform_kind: cursor.u16()?,
        transform_seed: cursor.u64()?,
        row_count: cursor.u32()?,
        reserved: cursor.u32()?,
    };
    header.validate()?;
    Ok(header)
}

pub(crate) fn encode_alive(alive: &AliveSet) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&alive.row_count().to_le_bytes());
    let byte_len = usize::try_from(alive.row_count())
        .unwrap_or(usize::MAX)
        .div_ceil(8);
    let mut bits = vec![0_u8; byte_len];
    for row in alive.iter_alive() {
        let position = row as usize;
        let byte = position / 8;
        let bit = position % 8;
        if let Some(value) = bits.get_mut(byte) {
            *value |= 1_u8 << bit;
        }
    }
    output.extend_from_slice(&(bits.len() as u32).to_le_bytes());
    output.extend_from_slice(&bits);
    output
}

pub(crate) fn decode_alive(bytes: &[u8]) -> Result<AliveSet, SegmentError> {
    let mut cursor = Cursor::new("alive", bytes);
    let row_count = cursor.u32()?;
    let byte_len = cursor.usize_from_u32()?;
    let expected = (row_count as usize).div_ceil(8);
    if byte_len != expected {
        return Err(SegmentError::Alive(format!(
            "bitmap length {byte_len}, expected {expected}"
        )));
    }
    let bits = cursor.take(byte_len)?;
    cursor.finish().map_err(SegmentError::Alive)?;
    if !row_count.is_multiple_of(8) {
        let used = row_count % 8;
        if let Some(last) = bits.last() {
            let mask = !((1_u8 << used) - 1);
            if last & mask != 0 {
                return Err(SegmentError::Alive(
                    "non-zero bitmap tail padding".to_owned(),
                ));
            }
        }
    }
    let mut alive = AliveSet::new(row_count);
    for row in 0..row_count {
        let position = row as usize;
        let is_alive = bits
            .get(position / 8)
            .is_some_and(|value| value & (1_u8 << (position % 8)) != 0);
        if !is_alive {
            alive
                .tombstone(row)
                .map_err(|error| SegmentError::Alive(error.to_string()))?;
        }
    }
    Ok(alive)
}

fn column_type_id(column_type: ColumnType) -> u16 {
    match column_type {
        ColumnType::U64 => 1,
        ColumnType::I64 => 2,
        ColumnType::F64 => 3,
        ColumnType::Bool => 4,
        ColumnType::DictionaryString => 5,
        ColumnType::RawString => 6,
    }
}

fn column_type_from_id(id: u16) -> Result<ColumnType, SegmentError> {
    match id {
        1 => Ok(ColumnType::U64),
        2 => Ok(ColumnType::I64),
        3 => Ok(ColumnType::F64),
        4 => Ok(ColumnType::Bool),
        5 => Ok(ColumnType::DictionaryString),
        6 => Ok(ColumnType::RawString),
        _ => Err(SegmentError::Columns(format!("unknown column type {id}"))),
    }
}

pub(crate) fn encode_columns(store: &ColumnStore) -> Result<Vec<u8>, SegmentError> {
    let mut output = Vec::new();
    output.extend_from_slice(&store.row_count().to_le_bytes());
    output.extend_from_slice(&(store.schema().columns().len() as u32).to_le_bytes());
    for definition in store.schema().columns() {
        output.extend_from_slice(&definition.id().get().to_le_bytes());
        output.extend_from_slice(&column_type_id(definition.column_type()).to_le_bytes());
        output.extend_from_slice(&u16::from(definition.is_nullable()).to_le_bytes());
        let name = definition.name().as_bytes();
        let name_length = u32::try_from(name.len())
            .map_err(|_| SegmentError::Columns("column name exceeds u32".to_owned()))?;
        output.extend_from_slice(&name_length.to_le_bytes());
        output.extend_from_slice(name);
    }
    for definition in store.schema().columns() {
        let column = store.column(definition.id()).ok_or_else(|| {
            SegmentError::Columns(format!("missing column {}", definition.id().get()))
        })?;
        let presence = encode_presence(column, store.row_count());
        output.extend_from_slice(&(presence.len() as u32).to_le_bytes());
        output.extend_from_slice(&presence);
        encode_column_values(column, store.row_count(), &mut output)?;
    }
    Ok(output)
}

fn encode_presence(column: &Column, row_count: u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; (row_count as usize).div_ceil(8)];
    for row in column.present().iter() {
        let position = row as usize;
        if let Some(value) = bytes.get_mut(position / 8) {
            *value |= 1_u8 << (position % 8);
        }
    }
    bytes
}

fn encode_column_values(
    column: &Column,
    row_count: u32,
    output: &mut Vec<u8>,
) -> Result<(), SegmentError> {
    match column {
        Column::U64(values) => {
            for row in 0..row_count {
                output.extend_from_slice(&values.get(row).unwrap_or(0).to_le_bytes());
            }
        }
        Column::I64(values) => {
            for row in 0..row_count {
                output.extend_from_slice(&values.get(row).unwrap_or(0).to_le_bytes());
            }
        }
        Column::F64(values) => {
            for row in 0..row_count {
                output.extend_from_slice(&values.get(row).unwrap_or(0.0).to_bits().to_le_bytes());
            }
        }
        Column::Bool(values) => {
            for row in 0..row_count {
                output.push(u8::from(values.get(row).unwrap_or(false)));
            }
        }
        Column::DictionaryString(values) => {
            let dictionary_length = u32::try_from(values.dictionary().len())
                .map_err(|_| SegmentError::Columns("dictionary exceeds u32".to_owned()))?;
            output.extend_from_slice(&dictionary_length.to_le_bytes());
            for code in 0..dictionary_length {
                let value = values.dictionary().get(code).ok_or_else(|| {
                    SegmentError::Columns(format!("missing dictionary code {code}"))
                })?;
                encode_string(value, output)?;
            }
            output.extend_from_slice(
                &match values.codes().width() {
                    crate::meta::CodeWidth::U16 => 2_u16,
                    crate::meta::CodeWidth::U32 => 4_u16,
                }
                .to_le_bytes(),
            );
            output.extend_from_slice(&0_u16.to_le_bytes());
            for row in 0..row_count {
                let code = values.codes().get(row as usize).unwrap_or(0);
                match values.codes().width() {
                    crate::meta::CodeWidth::U16 => {
                        output.extend_from_slice(&(code as u16).to_le_bytes())
                    }
                    crate::meta::CodeWidth::U32 => output.extend_from_slice(&code.to_le_bytes()),
                }
            }
        }
        Column::RawString(values) => {
            for row in 0..row_count {
                encode_string(values.get(row).unwrap_or(""), output)?;
            }
        }
    }
    Ok(())
}

fn encode_string(value: &str, output: &mut Vec<u8>) -> Result<(), SegmentError> {
    let length = u32::try_from(value.len())
        .map_err(|_| SegmentError::Columns("string exceeds u32".to_owned()))?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

enum DecodedValues {
    U64(Vec<u64>),
    I64(Vec<i64>),
    F64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

struct DecodedColumn {
    definition: ColumnDefinition,
    present: Vec<u8>,
    values: DecodedValues,
}

pub(crate) fn decode_columns(bytes: &[u8]) -> Result<ColumnStore, SegmentError> {
    let mut cursor = Cursor::new("columns", bytes);
    let row_count = cursor.u32()?;
    let column_count = cursor.usize_from_u32()?;
    if column_count == 0 {
        return Err(SegmentError::Columns(
            "required timestamp column is absent".to_owned(),
        ));
    }
    let mut definitions = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        let id = ColumnId::new(cursor.u32()?);
        let column_type = column_type_from_id(cursor.u16()?)?;
        let nullable = match cursor.u16()? {
            0 => false,
            1 => true,
            value => {
                return Err(SegmentError::Columns(format!(
                    "invalid nullable flag {value}"
                )));
            }
        };
        let name_length = cursor.usize_from_u32()?;
        let name = std::str::from_utf8(cursor.take(name_length)?)
            .map_err(|error| SegmentError::Columns(format!("column name UTF-8: {error}")))?
            .to_owned();
        definitions.push(ColumnDefinition::new(id, name, column_type, nullable));
    }
    let timestamp = definitions.first().ok_or_else(|| {
        SegmentError::Columns("required timestamp definition is absent".to_owned())
    })?;
    if timestamp.id() != TIMESTAMP_COLUMN
        || timestamp.name() != "ts"
        || timestamp.column_type() != ColumnType::I64
        || timestamp.is_nullable()
    {
        return Err(SegmentError::Columns(
            "timestamp definition is not canonical".to_owned(),
        ));
    }

    let mut decoded = Vec::with_capacity(column_count);
    for definition in definitions {
        let presence_length = cursor.usize_from_u32()?;
        let expected_presence = (row_count as usize).div_ceil(8);
        if presence_length != expected_presence {
            return Err(SegmentError::Columns(format!(
                "column {} presence length {presence_length}, expected {expected_presence}",
                definition.id().get()
            )));
        }
        let present = cursor.take(presence_length)?.to_vec();
        validate_presence_tail(row_count, &present)?;
        let values = decode_column_values(definition.column_type(), row_count, &mut cursor)?;
        decoded.push(DecodedColumn {
            definition,
            present,
            values,
        });
    }
    cursor.finish().map_err(SegmentError::Columns)?;

    let user_definitions = decoded
        .iter()
        .skip(1)
        .map(|column| column.definition.clone())
        .collect();
    let schema =
        Schema::new(user_definitions).map_err(|error| SegmentError::Columns(error.to_string()))?;
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0..row_count {
        let timestamp = decoded
            .first()
            .and_then(|column| match &column.values {
                DecodedValues::I64(values) => values.get(row as usize).copied(),
                _ => None,
            })
            .ok_or_else(|| SegmentError::Columns(format!("missing timestamp row {row}")))?;
        let mut inputs = Vec::new();
        for column in decoded.iter().skip(1) {
            if !presence_contains(&column.present, row) {
                continue;
            }
            let value = decoded_value(&column.values, row)?;
            inputs.push(ColumnInput {
                column: column.definition.id(),
                value,
            });
        }
        builder
            .push_row(timestamp, &inputs)
            .map_err(|error| SegmentError::Columns(error.to_string()))?;
    }
    builder
        .finish()
        .map_err(|error| SegmentError::Columns(error.to_string()))
}

fn decode_column_values(
    column_type: ColumnType,
    row_count: u32,
    cursor: &mut Cursor<'_>,
) -> Result<DecodedValues, SegmentError> {
    match column_type {
        ColumnType::U64 => (0..row_count)
            .map(|_| cursor.u64())
            .collect::<Result<Vec<_>, _>>()
            .map(DecodedValues::U64),
        ColumnType::I64 => (0..row_count)
            .map(|_| cursor.i64())
            .collect::<Result<Vec<_>, _>>()
            .map(DecodedValues::I64),
        ColumnType::F64 => (0..row_count)
            .map(|_| cursor.u64().map(f64::from_bits))
            .collect::<Result<Vec<_>, _>>()
            .map(DecodedValues::F64),
        ColumnType::Bool => (0..row_count)
            .map(|_| match cursor.u8()? {
                0 => Ok(false),
                1 => Ok(true),
                value => Err(SegmentError::Columns(format!(
                    "invalid Boolean byte {value}"
                ))),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(DecodedValues::Bool),
        ColumnType::DictionaryString => {
            let dictionary_length = cursor.usize_from_u32()?;
            let dictionary = (0..dictionary_length)
                .map(|_| cursor.string())
                .collect::<Result<Vec<_>, _>>()?;
            let width = cursor.u16()?;
            if cursor.u16()? != 0 {
                return Err(SegmentError::Columns(
                    "dictionary reserved field is non-zero".to_owned(),
                ));
            }
            let mut values = Vec::with_capacity(row_count as usize);
            for _ in 0..row_count {
                let code = match width {
                    2 => u32::from(cursor.u16()?),
                    4 => cursor.u32()?,
                    _ => {
                        return Err(SegmentError::Columns(format!(
                            "invalid dictionary width {width}"
                        )));
                    }
                };
                let value = dictionary.get(code as usize).ok_or_else(|| {
                    SegmentError::Columns(format!("dictionary code {code} out of range"))
                })?;
                values.push(value.clone());
            }
            Ok(DecodedValues::String(values))
        }
        ColumnType::RawString => (0..row_count)
            .map(|_| cursor.string())
            .collect::<Result<Vec<_>, _>>()
            .map(DecodedValues::String),
    }
}

fn decoded_value(values: &DecodedValues, row: u32) -> Result<ColumnValue<'_>, SegmentError> {
    let position = row as usize;
    match values {
        DecodedValues::U64(values) => values.get(position).copied().map(ColumnValue::U64),
        DecodedValues::I64(values) => values.get(position).copied().map(ColumnValue::I64),
        DecodedValues::F64(values) => values.get(position).copied().map(ColumnValue::F64),
        DecodedValues::Bool(values) => values.get(position).copied().map(ColumnValue::Bool),
        DecodedValues::String(values) => {
            values.get(position).map(|value| ColumnValue::String(value))
        }
    }
    .ok_or_else(|| SegmentError::Columns(format!("missing row {row}")))
}

fn validate_presence_tail(row_count: u32, bytes: &[u8]) -> Result<(), SegmentError> {
    if !row_count.is_multiple_of(8) {
        let used = row_count % 8;
        if bytes
            .last()
            .is_some_and(|last| last & !((1_u8 << used) - 1) != 0)
        {
            return Err(SegmentError::Columns(
                "non-zero presence tail padding".to_owned(),
            ));
        }
    }
    Ok(())
}

fn presence_contains(bytes: &[u8], row: u32) -> bool {
    let position = row as usize;
    bytes
        .get(position / 8)
        .is_some_and(|byte| byte & (1_u8 << (position % 8)) != 0)
}

pub(crate) struct Cursor<'a> {
    artifact: &'static str,
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) const fn new(artifact: &'static str, bytes: &'a [u8]) -> Self {
        Self {
            artifact,
            bytes,
            position: 0,
        }
    }

    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8], SegmentError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| SegmentError::Columns(format!("{} offset overflow", self.artifact)))?;
        let value = self.bytes.get(self.position..end).ok_or_else(|| {
            SegmentError::Columns(format!(
                "{} truncated at {}, need {length}, total {}",
                self.artifact,
                self.position,
                self.bytes.len()
            ))
        })?;
        self.position = end;
        Ok(value)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, SegmentError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| SegmentError::Columns(format!("{} missing u8", self.artifact)))
    }

    pub(crate) fn u16(&mut self) -> Result<u16, SegmentError> {
        let raw: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| SegmentError::Columns(format!("{} invalid u16", self.artifact)))?;
        Ok(u16::from_le_bytes(raw))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, SegmentError> {
        let raw: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| SegmentError::Columns(format!("{} invalid u32", self.artifact)))?;
        Ok(u32::from_le_bytes(raw))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, SegmentError> {
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| SegmentError::Columns(format!("{} invalid u64", self.artifact)))?;
        Ok(u64::from_le_bytes(raw))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, SegmentError> {
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| SegmentError::Columns(format!("{} invalid i64", self.artifact)))?;
        Ok(i64::from_le_bytes(raw))
    }

    pub(crate) fn usize_from_u32(&mut self) -> Result<usize, SegmentError> {
        usize::try_from(self.u32()?)
            .map_err(|_| SegmentError::Columns(format!("{} u32 exceeds usize", self.artifact)))
    }

    pub(crate) fn string(&mut self) -> Result<String, SegmentError> {
        let length = self.usize_from_u32()?;
        std::str::from_utf8(self.take(length)?)
            .map(str::to_owned)
            .map_err(|error| SegmentError::Columns(format!("{} UTF-8: {error}", self.artifact)))
    }

    pub(crate) fn finish(&self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "{} has {} trailing bytes",
                self.artifact,
                self.bytes.len().saturating_sub(self.position)
            ))
        }
    }
}
