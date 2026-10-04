//! Versioned payloads inside the frozen task-08 WAL record frame.

use crate::meta::{ColumnId, PredicateValue};

use super::{DocId, DocumentVersion, IngestDocument, Revision};

// Operation ids are an append-only persisted contract. Once assigned, an id
// is never reused, even if its record kind is retired:
//   1 = vector/document upsert v1
//   2 = document delete v1
//   3 = metadata edit v1
//   4 = vector/document upsert with canonical ts v1
//   5 = vector/document upsert with opaque stored metadata v1
//   6 = vector/document upsert with canonical ts and opaque stored metadata v1
//   7 = document upsert v2 with a u32 field-presence bitmap
//   8 = one member of a multi-record upsert batch: `index:u32`,
//       `count:u32`, then the unchanged upsert-v2 payload (ZE-216)
//
// Upsert-v2 bitmap bits are append-only persisted meanings:
//   bit 0 = vector (`dims:u32`, then `f32[dims]`)
//   bit 1 = UTF-8 text (`len:u32`, then bytes)
//   bit 2 = canonical timestamp (`i64`)
//   bit 3 = opaque stored metadata (`len:u32`, then bytes)
//   bit 4 = typed columns (count plus typed length-delimited entries)
//   bit 5 = reserved-unused for a future per-record epoch tag
//   bits 6..31 = reserved-unused
/// Upsert payload v1.
pub const UPSERT_V1: u16 = 1;
/// Delete payload v1.
pub const DELETE_V1: u16 = 2;
/// Metadata-edit payload v1.
pub const METADATA_EDIT_V1: u16 = 3;
/// Upsert payload with canonical timestamp v1.
pub const UPSERT_WITH_TIMESTAMP_V1: u16 = 4;
/// Upsert payload with opaque stored metadata v1.
pub const UPSERT_WITH_METADATA_V1: u16 = 5;
/// Upsert payload with canonical timestamp and opaque stored metadata v1.
pub const UPSERT_WITH_TIMESTAMP_AND_METADATA_V1: u16 = 6;
/// Bitmap-based upsert payload v2.
pub const UPSERT_V2: u16 = 7;
/// One member of a multi-record upsert batch: `[index:u32][count:u32]`, then
/// an upsert-v2 payload. Replay applies a batch only when members
/// `0..count` are all present.
pub const UPSERT_V2_BATCH_MEMBER: u16 = 8;

/// Upsert-v2 vector field bit.
pub const UPSERT_V2_VECTOR: u32 = 1 << 0;
/// Upsert-v2 text field bit.
pub const UPSERT_V2_TEXT: u32 = 1 << 1;
/// Upsert-v2 timestamp field bit.
pub const UPSERT_V2_TIMESTAMP: u32 = 1 << 2;
/// Upsert-v2 opaque-metadata field bit.
pub const UPSERT_V2_METADATA: u32 = 1 << 3;
/// Upsert-v2 typed-column field bit.
pub const UPSERT_V2_TYPED_COLUMNS: u32 = 1 << 4;
/// Reserved-unused upsert-v2 bit for a future per-record epoch tag.
pub const UPSERT_V2_EPOCH_TAG_RESERVED: u32 = 1 << 5;

const UPSERT_V2_KNOWN_FIELDS: u32 = UPSERT_V2_VECTOR
    | UPSERT_V2_TEXT
    | UPSERT_V2_TIMESTAMP
    | UPSERT_V2_METADATA
    | UPSERT_V2_TYPED_COLUMNS;

const PAYLOAD_VERSION: u16 = 1;
const COMMON_HEADER_LEN: usize = 4;
const DOCUMENT_VERSION_LEN: usize = 24;
const UPSERT_PREFIX_LEN: usize = COMMON_HEADER_LEN + DOCUMENT_VERSION_LEN + 4;
const TIMESTAMPED_UPSERT_PREFIX_LEN: usize = COMMON_HEADER_LEN + DOCUMENT_VERSION_LEN + 8 + 4;
const METADATA_UPSERT_PREFIX_LEN: usize = COMMON_HEADER_LEN + DOCUMENT_VERSION_LEN + 4 + 4;
const TIMESTAMPED_METADATA_UPSERT_PREFIX_LEN: usize =
    COMMON_HEADER_LEN + DOCUMENT_VERSION_LEN + 8 + 4 + 4;
const DELETE_PREFIX_LEN: usize = COMMON_HEADER_LEN + 4;
const METADATA_PREFIX_LEN: usize = COMMON_HEADER_LEN + DOCUMENT_VERSION_LEN + 4 + 1 + 3 + 4;

const VALUE_NULL: u8 = 0;
const VALUE_ID128: u8 = 6;
const VALUE_U64: u8 = 1;
const VALUE_I64: u8 = 2;
const VALUE_F64: u8 = 3;
const VALUE_BOOL: u8 = 4;
const VALUE_STRING: u8 = 5;

/// Typed operation-payload encoding or decoding failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PayloadError {
    /// A WAL operation id has no assigned payload contract.
    UnknownOperation(u16),
    /// Invalid transaction identity, generation or sequence range.
    TransactionBinding,
    /// A count or byte length does not fit its frozen field.
    LengthOverflow,
    /// The payload is shorter than the required fixed prefix or declared body.
    Truncated,
    /// The payload version is not v1.
    Version(u16),
    /// A must-be-zero flags field was non-zero.
    Flags(u16),
    /// A must-be-zero reserved metadata field was non-zero.
    Reserved(u32),
    /// An upsert contained no vector coordinates.
    EmptyVector,
    /// A delete contained no document ids.
    EmptyDelete,
    /// A vector coordinate was NaN or infinite.
    NonFiniteVector {
        /// Zero-based vector coordinate.
        index: usize,
    },
    /// A metadata value kind has no assigned v1 meaning.
    ValueKind(u8),
    /// A value body has the wrong width for its declared kind.
    ValueLength {
        /// Declared metadata value kind.
        kind: u8,
        /// Required body width.
        expected: usize,
        /// Declared body width.
        actual: usize,
    },
    /// A Boolean metadata byte was neither zero nor one.
    Boolean(u8),
    /// A string metadata body was not valid UTF-8.
    Utf8,
    /// Bytes remain after the declared payload.
    TrailingBytes(usize),
    /// An upsert-v2 bitmap named a reserved or unimplemented field.
    FieldBitmap(u32),
    /// Active lexical reconstruction failed during WAL replay.
    Lexical(String),
    /// Active typed-column reconstruction failed during WAL replay.
    Columns(String),
    /// A batch member position is not `index < count` with `count >= 2`.
    BatchPosition {
        /// Persisted zero-based member index.
        index: u32,
        /// Persisted member count.
        count: u32,
    },
    /// A batch member does not continue the batch the log was replaying.
    OrphanBatchMember {
        /// Persisted zero-based member index.
        index: u32,
        /// Persisted member count.
        count: u32,
    },
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TransactionBinding => write!(formatter, "invalid transaction binding"),
            Self::UnknownOperation(op) => {
                write!(formatter, "WAL mutation operation {op} is unknown")
            }
            Self::LengthOverflow => formatter.write_str("WAL mutation payload length overflow"),
            Self::Truncated => formatter.write_str("WAL mutation payload is truncated"),
            Self::Version(version) => write!(
                formatter,
                "WAL mutation payload version {version} is unsupported"
            ),
            Self::Flags(flags) => write!(
                formatter,
                "WAL mutation payload flags {flags:#06x} are non-zero"
            ),
            Self::Reserved(value) => write!(
                formatter,
                "WAL metadata payload reserved bytes are {value:#08x}, expected zero"
            ),
            Self::EmptyVector => formatter.write_str("WAL upsert vector is empty"),
            Self::EmptyDelete => formatter.write_str("WAL delete document list is empty"),
            Self::NonFiniteVector { index } => write!(
                formatter,
                "WAL upsert vector coordinate {index} is non-finite"
            ),
            Self::ValueKind(kind) => {
                write!(formatter, "WAL metadata value kind {kind} is unknown")
            }
            Self::ValueLength {
                kind,
                expected,
                actual,
            } => write!(
                formatter,
                "WAL metadata value kind {kind} needs {expected} bytes, got {actual}"
            ),
            Self::Boolean(value) => write!(
                formatter,
                "WAL metadata Boolean byte {value} is not canonical zero or one"
            ),
            Self::Utf8 => formatter.write_str("WAL metadata string is not valid UTF-8"),
            Self::TrailingBytes(bytes) => {
                write!(formatter, "WAL mutation payload has {bytes} trailing bytes")
            }
            Self::FieldBitmap(bitmap) => write!(
                formatter,
                "WAL upsert-v2 field bitmap {bitmap:#010x} names reserved fields"
            ),
            Self::Lexical(detail) => write!(formatter, "WAL lexical replay failed: {detail}"),
            Self::Columns(detail) => write!(formatter, "WAL column replay failed: {detail}"),
            Self::BatchPosition { index, count } => write!(
                formatter,
                "WAL batch member {index} of {count} is not a valid position"
            ),
            Self::OrphanBatchMember { index, count } => write!(
                formatter,
                "WAL batch member {index} of {count} continues no earlier member"
            ),
        }
    }
}

impl std::error::Error for PayloadError {}

/// Owned typed value carried by one metadata-edit record.
#[derive(Clone, Debug, PartialEq)]
pub enum MetadataValue {
    /// Clears a nullable metadata value.
    Null,
    /// Unsigned integer value.
    U64(u64),
    /// Full document identifier, encoded as sixteen little-endian bytes.
    Id128(DocId),
    /// Signed integer value.
    I64(i64),
    /// IEEE-754 value, preserving its exact bits.
    F64(f64),
    /// Boolean value.
    Bool(bool),
    /// UTF-8 string value.
    String(String),
}

/// One structured document metadata mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct MetadataEdit {
    version: DocumentVersion,
    column: ColumnId,
    value: MetadataValue,
}

impl MetadataEdit {
    /// Constructs one metadata edit for a specific document revision.
    #[must_use]
    pub const fn new(version: DocumentVersion, column: ColumnId, value: MetadataValue) -> Self {
        Self {
            version,
            column,
            value,
        }
    }

    /// Returns the document/revision key to edit.
    #[must_use]
    pub const fn version(&self) -> DocumentVersion {
        self.version
    }

    /// Returns the schema-local column id.
    #[must_use]
    pub const fn column(&self) -> ColumnId {
        self.column
    }

    /// Returns the owned typed replacement value.
    #[must_use]
    pub const fn value(&self) -> &MetadataValue {
        &self.value
    }
}

/// One decoded record from the append-only mutation operation space.
#[derive(Clone, Debug, PartialEq)]
pub enum MutationPayload {
    /// Vector/document upsert.
    Upsert(IngestDocument),
    /// Document tombstones.
    Delete(Vec<DocId>),
    /// Structured metadata edit.
    MetadataEdit(MetadataEdit),
    /// One upsert inside a multi-record batch.
    BatchMember {
        /// Zero-based position in the batch.
        index: u32,
        /// Number of members in the batch.
        count: u32,
        /// The member's upsert.
        document: IngestDocument,
    },
}

/// Encodes one upsert payload as little-endian
/// `[version:u16, flags:u16, doc_id:u128, revision:u64, dims:u32, f32[dims]]`.
pub fn encode_upsert(document: &IngestDocument) -> Result<Vec<u8>, PayloadError> {
    encode_upsert_body(document, None, false)
}

/// Encodes one upsert with its canonical `ts` value before vector geometry.
pub fn encode_upsert_with_timestamp(document: &IngestDocument) -> Result<Vec<u8>, PayloadError> {
    encode_upsert_body(document, Some(document.timestamp()), false)
}

/// Encodes one upsert with opaque stored metadata.
pub fn encode_upsert_with_metadata(document: &IngestDocument) -> Result<Vec<u8>, PayloadError> {
    encode_upsert_body(document, None, true)
}

/// Encodes one timestamped upsert with opaque stored metadata.
pub fn encode_upsert_with_timestamp_and_metadata(
    document: &IngestDocument,
) -> Result<Vec<u8>, PayloadError> {
    encode_upsert_body(document, Some(document.timestamp()), true)
}

/// Encodes one bitmap-based upsert v2. The operation id carries the payload
/// version, so the first four payload bytes are the field-presence bitmap.
pub fn encode_upsert_v2(document: &IngestDocument) -> Result<Vec<u8>, PayloadError> {
    let mut bitmap = UPSERT_V2_VECTOR;
    if document.text().is_some() {
        bitmap |= UPSERT_V2_TEXT;
    }
    if document.has_timestamp() {
        bitmap |= UPSERT_V2_TIMESTAMP;
    }
    if document.has_metadata() {
        bitmap |= UPSERT_V2_METADATA;
    }
    if document.has_columns() {
        bitmap |= UPSERT_V2_TYPED_COLUMNS;
    }
    validate_vector(document.vector())?;
    let dims = u32::try_from(document.vector().len()).map_err(|_| PayloadError::LengthOverflow)?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&bitmap.to_le_bytes());
    append_version(&mut payload, document.version());
    payload.extend_from_slice(&dims.to_le_bytes());
    for value in document.vector() {
        payload.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    if let Some(text) = document.text() {
        append_length_prefixed(text.as_bytes(), &mut payload)?;
    }
    if document.has_timestamp() {
        payload.extend_from_slice(&document.timestamp().to_le_bytes());
    }
    if document.has_metadata() {
        append_length_prefixed(document.metadata(), &mut payload)?;
    }
    if document.has_columns() {
        encode_typed_columns(document.columns(), &mut payload)?;
    }
    Ok(payload)
}

fn validate_vector(vector: &[f32]) -> Result<(), PayloadError> {
    if vector.is_empty() {
        return Err(PayloadError::EmptyVector);
    }
    if let Some(index) = vector.iter().position(|value| !value.is_finite()) {
        return Err(PayloadError::NonFiniteVector { index });
    }
    Ok(())
}

fn append_length_prefixed(bytes: &[u8], output: &mut Vec<u8>) -> Result<(), PayloadError> {
    let length = u32::try_from(bytes.len()).map_err(|_| PayloadError::LengthOverflow)?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

fn encode_typed_columns(
    columns: &[(ColumnId, PredicateValue)],
    output: &mut Vec<u8>,
) -> Result<(), PayloadError> {
    let count = u32::try_from(columns.len()).map_err(|_| PayloadError::LengthOverflow)?;
    output.extend_from_slice(&count.to_le_bytes());
    for (column, value) in columns {
        let metadata = predicate_to_metadata(value);
        let (kind, body) = encode_metadata_value(&metadata)?;
        output.extend_from_slice(&column.get().to_le_bytes());
        output.push(kind);
        output.extend_from_slice(&[0_u8; 3]);
        append_length_prefixed(&body, output)?;
    }
    Ok(())
}

pub(crate) fn encode_column_values(
    columns: &[(ColumnId, PredicateValue)],
) -> Result<Vec<u8>, PayloadError> {
    let mut encoded = Vec::new();
    encode_typed_columns(columns, &mut encoded)?;
    Ok(encoded)
}

fn predicate_to_metadata(value: &PredicateValue) -> MetadataValue {
    match value {
        PredicateValue::Id128(value) => MetadataValue::Id128(*value),
        PredicateValue::U64(value) => MetadataValue::U64(*value),
        PredicateValue::I64(value) => MetadataValue::I64(*value),
        PredicateValue::F64(value) => MetadataValue::F64(*value),
        PredicateValue::Bool(value) => MetadataValue::Bool(*value),
        PredicateValue::String(value) => MetadataValue::String(value.clone()),
    }
}

fn encode_upsert_body(
    document: &IngestDocument,
    timestamp: Option<i64>,
    include_metadata: bool,
) -> Result<Vec<u8>, PayloadError> {
    if document.vector().is_empty() {
        return Err(PayloadError::EmptyVector);
    }
    let dims = u32::try_from(document.vector().len()).map_err(|_| PayloadError::LengthOverflow)?;
    if let Some(index) = document
        .vector()
        .iter()
        .position(|value| !value.is_finite())
    {
        return Err(PayloadError::NonFiniteVector { index });
    }
    let vector_bytes = document
        .vector()
        .len()
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or(PayloadError::LengthOverflow)?;
    let prefix = match (timestamp.is_some(), include_metadata) {
        (false, false) => UPSERT_PREFIX_LEN,
        (true, false) => TIMESTAMPED_UPSERT_PREFIX_LEN,
        (false, true) => METADATA_UPSERT_PREFIX_LEN,
        (true, true) => TIMESTAMPED_METADATA_UPSERT_PREFIX_LEN,
    };
    let capacity = prefix
        .checked_add(if include_metadata {
            document.metadata().len()
        } else {
            0
        })
        .and_then(|value| value.checked_add(vector_bytes))
        .ok_or(PayloadError::LengthOverflow)?;
    let mut payload = Vec::with_capacity(capacity);
    append_header(&mut payload);
    append_version(&mut payload, document.version());
    if let Some(timestamp) = timestamp {
        payload.extend_from_slice(&timestamp.to_le_bytes());
    }
    if include_metadata {
        let metadata_len =
            u32::try_from(document.metadata().len()).map_err(|_| PayloadError::LengthOverflow)?;
        payload.extend_from_slice(&metadata_len.to_le_bytes());
    }
    payload.extend_from_slice(&dims.to_le_bytes());
    if include_metadata {
        payload.extend_from_slice(document.metadata());
    }
    for value in document.vector() {
        payload.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    Ok(payload)
}

/// Encodes one delete payload as little-endian
/// `[version:u16, flags:u16, count:u32, doc_id:u128[count]]`.
pub fn encode_delete(doc_ids: &[DocId]) -> Result<Vec<u8>, PayloadError> {
    if doc_ids.is_empty() {
        return Err(PayloadError::EmptyDelete);
    }
    let count = u32::try_from(doc_ids.len()).map_err(|_| PayloadError::LengthOverflow)?;
    let body_bytes = doc_ids
        .len()
        .checked_mul(std::mem::size_of::<u128>())
        .ok_or(PayloadError::LengthOverflow)?;
    let capacity = DELETE_PREFIX_LEN
        .checked_add(body_bytes)
        .ok_or(PayloadError::LengthOverflow)?;
    let mut payload = Vec::with_capacity(capacity);
    append_header(&mut payload);
    payload.extend_from_slice(&count.to_le_bytes());
    for doc_id in doc_ids {
        payload.extend_from_slice(&doc_id.get().to_le_bytes());
    }
    Ok(payload)
}

/// Encodes one metadata edit as little-endian fixed identity/type fields plus
/// a length-delimited value body.
pub fn encode_metadata_edit(edit: &MetadataEdit) -> Result<Vec<u8>, PayloadError> {
    let (kind, body) = encode_metadata_value(edit.value())?;
    let value_len = u32::try_from(body.len()).map_err(|_| PayloadError::LengthOverflow)?;
    let capacity = METADATA_PREFIX_LEN
        .checked_add(body.len())
        .ok_or(PayloadError::LengthOverflow)?;
    let mut payload = Vec::with_capacity(capacity);
    append_header(&mut payload);
    append_version(&mut payload, edit.version());
    payload.extend_from_slice(&edit.column().get().to_le_bytes());
    payload.push(kind);
    payload.extend_from_slice(&[0_u8; 3]);
    payload.extend_from_slice(&value_len.to_le_bytes());
    payload.extend_from_slice(&body);
    Ok(payload)
}

/// Decodes a payload according to its append-only operation id.
pub fn decode_mutation(op: u16, payload: &[u8]) -> Result<MutationPayload, PayloadError> {
    match op {
        UPSERT_V1 => decode_upsert(payload).map(MutationPayload::Upsert),
        DELETE_V1 => decode_delete(payload).map(MutationPayload::Delete),
        METADATA_EDIT_V1 => decode_metadata_edit(payload).map(MutationPayload::MetadataEdit),
        UPSERT_WITH_TIMESTAMP_V1 => {
            decode_upsert_with_timestamp(payload).map(MutationPayload::Upsert)
        }
        UPSERT_WITH_METADATA_V1 => {
            decode_upsert_with_metadata(payload).map(MutationPayload::Upsert)
        }
        UPSERT_WITH_TIMESTAMP_AND_METADATA_V1 => {
            decode_upsert_with_timestamp_and_metadata(payload).map(MutationPayload::Upsert)
        }
        UPSERT_V2 => decode_upsert_v2(payload).map(MutationPayload::Upsert),
        UPSERT_V2_BATCH_MEMBER => {
            decode_upsert_v2_batch_member(payload).map(|(index, count, document)| {
                MutationPayload::BatchMember {
                    index,
                    count,
                    document,
                }
            })
        }
        unknown => Err(PayloadError::UnknownOperation(unknown)),
    }
}

/// Frames an upsert-v2 payload as member `index` of a `count`-record batch.
pub fn encode_upsert_v2_batch_member(
    index: u32,
    count: u32,
    upsert_v2: &[u8],
) -> Result<Vec<u8>, PayloadError> {
    validate_batch_position(index, count)?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(upsert_v2.len().saturating_add(8))
        .map_err(|_| PayloadError::LengthOverflow)?;
    payload.extend_from_slice(&index.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    payload.extend_from_slice(upsert_v2);
    Ok(payload)
}

/// Decodes one batch member into `(index, count, document)`.
pub fn decode_upsert_v2_batch_member(
    payload: &[u8],
) -> Result<(u32, u32, IngestDocument), PayloadError> {
    let mut cursor = Cursor::new(payload);
    let index = cursor.read_u32()?;
    let count = cursor.read_u32()?;
    validate_batch_position(index, count)?;
    let body = payload.get(8..).ok_or(PayloadError::Truncated)?;
    decode_upsert_v2(body).map(|document| (index, count, document))
}

fn validate_batch_position(index: u32, count: u32) -> Result<(), PayloadError> {
    if count < 2 || index >= count {
        return Err(PayloadError::BatchPosition { index, count });
    }
    Ok(())
}

/// Decodes one bitmap-based upsert-v2 payload.
pub fn decode_upsert_v2(payload: &[u8]) -> Result<IngestDocument, PayloadError> {
    let mut cursor = Cursor::new(payload);
    let bitmap = cursor.read_u32()?;
    let reserved = bitmap & !UPSERT_V2_KNOWN_FIELDS;
    if reserved != 0 {
        return Err(PayloadError::FieldBitmap(reserved));
    }
    let version = cursor.read_version()?;
    let vector = if bitmap & UPSERT_V2_VECTOR != 0 {
        let dims = usize::try_from(cursor.read_u32()?).map_err(|_| PayloadError::LengthOverflow)?;
        if dims == 0 {
            return Err(PayloadError::EmptyVector);
        }
        let mut vector = Vec::with_capacity(dims);
        for index in 0..dims {
            let value = f32::from_bits(cursor.read_u32()?);
            if !value.is_finite() {
                return Err(PayloadError::NonFiniteVector { index });
            }
            vector.push(value);
        }
        vector
    } else {
        Vec::new()
    };
    let text = if bitmap & UPSERT_V2_TEXT != 0 {
        Some(cursor.read_string()?)
    } else {
        None
    };
    let timestamp = if bitmap & UPSERT_V2_TIMESTAMP != 0 {
        Some(i64::from_le_bytes(cursor.take()?))
    } else {
        None
    };
    let metadata = if bitmap & UPSERT_V2_METADATA != 0 {
        Some(cursor.read_length_prefixed()?.to_vec())
    } else {
        None
    };
    let columns = if bitmap & UPSERT_V2_TYPED_COLUMNS != 0 {
        Some(decode_typed_columns(&mut cursor)?)
    } else {
        None
    };
    cursor.finish()?;
    let mut document = IngestDocument::new(version, vector);
    if let Some(text) = text {
        document = document.with_text(text);
    }
    if let Some(timestamp) = timestamp {
        document = document.with_timestamp(timestamp);
    }
    if let Some(metadata) = metadata {
        document = document.with_metadata(metadata);
    }
    if let Some(columns) = columns {
        document = document.with_columns(columns);
    }
    Ok(document)
}

fn decode_typed_columns(
    cursor: &mut Cursor<'_>,
) -> Result<Vec<(ColumnId, PredicateValue)>, PayloadError> {
    let count = usize::try_from(cursor.read_u32()?).map_err(|_| PayloadError::LengthOverflow)?;
    let mut columns = Vec::with_capacity(count);
    for _ in 0..count {
        let column = ColumnId::new(cursor.read_u32()?);
        let kind = cursor.read_u8()?;
        let reserved = cursor.read_u24()?;
        if reserved != 0 {
            return Err(PayloadError::Reserved(reserved));
        }
        let body = cursor.read_length_prefixed()?;
        let value = metadata_to_predicate(decode_metadata_value(kind, body)?)?;
        columns.push((column, value));
    }
    Ok(columns)
}

pub(crate) fn decode_column_values(
    encoded: &[u8],
) -> Result<Vec<(ColumnId, PredicateValue)>, PayloadError> {
    let mut cursor = Cursor::new(encoded);
    let columns = decode_typed_columns(&mut cursor)?;
    cursor.finish()?;
    Ok(columns)
}

fn metadata_to_predicate(value: MetadataValue) -> Result<PredicateValue, PayloadError> {
    match value {
        MetadataValue::Null => Err(PayloadError::ValueKind(VALUE_NULL)),
        MetadataValue::Id128(value) => Ok(PredicateValue::Id128(value)),
        MetadataValue::U64(value) => Ok(PredicateValue::U64(value)),
        MetadataValue::I64(value) => Ok(PredicateValue::I64(value)),
        MetadataValue::F64(value) => Ok(PredicateValue::F64(value)),
        MetadataValue::Bool(value) => Ok(PredicateValue::Bool(value)),
        MetadataValue::String(value) => Ok(PredicateValue::String(value)),
    }
}

/// Decodes and validates one complete upsert payload.
pub fn decode_upsert(payload: &[u8]) -> Result<IngestDocument, PayloadError> {
    decode_upsert_body(payload, false, false)
}

/// Decodes one complete upsert carrying the canonical `ts` value.
pub fn decode_upsert_with_timestamp(payload: &[u8]) -> Result<IngestDocument, PayloadError> {
    decode_upsert_body(payload, true, false)
}

/// Decodes one complete upsert carrying opaque stored metadata.
pub fn decode_upsert_with_metadata(payload: &[u8]) -> Result<IngestDocument, PayloadError> {
    decode_upsert_body(payload, false, true)
}

/// Decodes one complete timestamped upsert carrying opaque stored metadata.
pub fn decode_upsert_with_timestamp_and_metadata(
    payload: &[u8],
) -> Result<IngestDocument, PayloadError> {
    decode_upsert_body(payload, true, true)
}

fn decode_upsert_body(
    payload: &[u8],
    has_timestamp: bool,
    has_metadata: bool,
) -> Result<IngestDocument, PayloadError> {
    let mut cursor = Cursor::new(payload);
    cursor.require_header()?;
    let version = cursor.read_version()?;
    let timestamp = if has_timestamp {
        i64::from_le_bytes(cursor.take()?)
    } else {
        0
    };
    let metadata_len = if has_metadata {
        usize::try_from(cursor.read_u32()?).map_err(|_| PayloadError::LengthOverflow)?
    } else {
        0
    };
    let dims = usize::try_from(cursor.read_u32()?).map_err(|_| PayloadError::LengthOverflow)?;
    if dims == 0 {
        return Err(PayloadError::EmptyVector);
    }
    let body_bytes = dims
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or(PayloadError::LengthOverflow)?;
    let remaining = metadata_len
        .checked_add(body_bytes)
        .ok_or(PayloadError::LengthOverflow)?;
    cursor.require_remaining(remaining)?;
    let metadata = cursor.read_bytes(metadata_len)?.to_vec();
    let mut vector = Vec::with_capacity(dims);
    for index in 0..dims {
        let value = f32::from_bits(cursor.read_u32()?);
        if !value.is_finite() {
            return Err(PayloadError::NonFiniteVector { index });
        }
        vector.push(value);
    }
    cursor.finish()?;
    let mut document = IngestDocument::new(version, vector);
    if has_timestamp {
        document = document.with_timestamp(timestamp);
    }
    if has_metadata {
        document = document.with_metadata(metadata);
    }
    Ok(document)
}

/// Decodes and validates one complete delete payload.
pub fn decode_delete(payload: &[u8]) -> Result<Vec<DocId>, PayloadError> {
    let mut cursor = Cursor::new(payload);
    cursor.require_header()?;
    let count = usize::try_from(cursor.read_u32()?).map_err(|_| PayloadError::LengthOverflow)?;
    if count == 0 {
        return Err(PayloadError::EmptyDelete);
    }
    let body_bytes = count
        .checked_mul(std::mem::size_of::<u128>())
        .ok_or(PayloadError::LengthOverflow)?;
    cursor.require_remaining(body_bytes)?;
    let mut doc_ids = Vec::with_capacity(count);
    for _ in 0..count {
        doc_ids.push(DocId::new(cursor.read_u128()?));
    }
    cursor.finish()?;
    Ok(doc_ids)
}

/// Decodes and validates one complete metadata-edit payload.
pub fn decode_metadata_edit(payload: &[u8]) -> Result<MetadataEdit, PayloadError> {
    let mut cursor = Cursor::new(payload);
    cursor.require_header()?;
    let version = cursor.read_version()?;
    let column = ColumnId::new(cursor.read_u32()?);
    let kind = cursor.read_u8()?;
    let reserved = cursor.read_u24()?;
    if reserved != 0 {
        return Err(PayloadError::Reserved(reserved));
    }
    let value_len =
        usize::try_from(cursor.read_u32()?).map_err(|_| PayloadError::LengthOverflow)?;
    let body = cursor.read_bytes(value_len)?;
    let value = decode_metadata_value(kind, body)?;
    cursor.finish()?;
    Ok(MetadataEdit::new(version, column, value))
}

fn append_header(payload: &mut Vec<u8>) {
    payload.extend_from_slice(&PAYLOAD_VERSION.to_le_bytes());
    payload.extend_from_slice(&0_u16.to_le_bytes());
}

fn append_version(payload: &mut Vec<u8>, version: DocumentVersion) {
    payload.extend_from_slice(&version.doc_id().get().to_le_bytes());
    payload.extend_from_slice(&version.revision().get().to_le_bytes());
}

fn encode_metadata_value(value: &MetadataValue) -> Result<(u8, Vec<u8>), PayloadError> {
    let encoded = match value {
        MetadataValue::Null => (VALUE_NULL, Vec::new()),
        MetadataValue::Id128(value) => (VALUE_ID128, value.get().to_le_bytes().to_vec()),
        MetadataValue::U64(value) => (VALUE_U64, value.to_le_bytes().to_vec()),
        MetadataValue::I64(value) => (VALUE_I64, value.to_le_bytes().to_vec()),
        MetadataValue::F64(value) => (VALUE_F64, value.to_bits().to_le_bytes().to_vec()),
        MetadataValue::Bool(value) => (VALUE_BOOL, vec![u8::from(*value)]),
        MetadataValue::String(value) => {
            u32::try_from(value.len()).map_err(|_| PayloadError::LengthOverflow)?;
            (VALUE_STRING, value.as_bytes().to_vec())
        }
    };
    Ok(encoded)
}

fn decode_metadata_value(kind: u8, body: &[u8]) -> Result<MetadataValue, PayloadError> {
    match kind {
        VALUE_NULL => {
            require_value_len(kind, body, 0)?;
            Ok(MetadataValue::Null)
        }
        VALUE_ID128 => {
            require_value_len(kind, body, 16)?;
            read_array(body)
                .map(u128::from_le_bytes)
                .map(DocId::new)
                .map(MetadataValue::Id128)
        }
        VALUE_U64 => {
            require_value_len(kind, body, 8)?;
            read_array(body)
                .map(u64::from_le_bytes)
                .map(MetadataValue::U64)
        }
        VALUE_I64 => {
            require_value_len(kind, body, 8)?;
            read_array(body)
                .map(i64::from_le_bytes)
                .map(MetadataValue::I64)
        }
        VALUE_F64 => {
            require_value_len(kind, body, 8)?;
            read_array(body)
                .map(u64::from_le_bytes)
                .map(f64::from_bits)
                .map(MetadataValue::F64)
        }
        VALUE_BOOL => {
            require_value_len(kind, body, 1)?;
            match body.first().copied().ok_or(PayloadError::Truncated)? {
                0 => Ok(MetadataValue::Bool(false)),
                1 => Ok(MetadataValue::Bool(true)),
                value => Err(PayloadError::Boolean(value)),
            }
        }
        VALUE_STRING => std::str::from_utf8(body)
            .map(|value| MetadataValue::String(value.to_owned()))
            .map_err(|_| PayloadError::Utf8),
        unknown => Err(PayloadError::ValueKind(unknown)),
    }
}

fn require_value_len(kind: u8, body: &[u8], expected: usize) -> Result<(), PayloadError> {
    if body.len() == expected {
        Ok(())
    } else {
        Err(PayloadError::ValueLength {
            kind,
            expected,
            actual: body.len(),
        })
    }
}

fn read_array<const N: usize>(bytes: &[u8]) -> Result<[u8; N], PayloadError> {
    <[u8; N]>::try_from(bytes).map_err(|_| PayloadError::Truncated)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn require_header(&mut self) -> Result<(), PayloadError> {
        let version = self.read_u16()?;
        if version != PAYLOAD_VERSION {
            return Err(PayloadError::Version(version));
        }
        let flags = self.read_u16()?;
        if flags != 0 {
            return Err(PayloadError::Flags(flags));
        }
        Ok(())
    }

    fn read_version(&mut self) -> Result<DocumentVersion, PayloadError> {
        Ok(DocumentVersion::new(
            DocId::new(self.read_u128()?),
            Revision::new(self.read_u64()?),
        ))
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], PayloadError> {
        read_array(self.read_bytes(N)?)
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], PayloadError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(PayloadError::LengthOverflow)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(PayloadError::Truncated)?;
        self.offset = end;
        Ok(bytes)
    }

    fn require_remaining(&self, length: usize) -> Result<(), PayloadError> {
        if self.remaining() < length {
            Err(PayloadError::Truncated)
        } else {
            Ok(())
        }
    }

    fn read_u8(&mut self) -> Result<u8, PayloadError> {
        self.take::<1>().map(u8::from_le_bytes)
    }

    fn read_u16(&mut self) -> Result<u16, PayloadError> {
        self.take().map(u16::from_le_bytes)
    }

    fn read_u24(&mut self) -> Result<u32, PayloadError> {
        let [low, middle, high] = self.take::<3>()?;
        Ok(u32::from(low) | (u32::from(middle) << 8) | (u32::from(high) << 16))
    }

    fn read_u32(&mut self) -> Result<u32, PayloadError> {
        self.take().map(u32::from_le_bytes)
    }

    fn read_u64(&mut self) -> Result<u64, PayloadError> {
        self.take().map(u64::from_le_bytes)
    }

    fn read_u128(&mut self) -> Result<u128, PayloadError> {
        self.take().map(u128::from_le_bytes)
    }

    fn read_length_prefixed(&mut self) -> Result<&'a [u8], PayloadError> {
        let length = usize::try_from(self.read_u32()?).map_err(|_| PayloadError::LengthOverflow)?;
        self.read_bytes(length)
    }

    fn read_string(&mut self) -> Result<String, PayloadError> {
        std::str::from_utf8(self.read_length_prefixed()?)
            .map(str::to_owned)
            .map_err(|_| PayloadError::Utf8)
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn finish(self) -> Result<(), PayloadError> {
        let remaining = self.remaining();
        if remaining == 0 {
            Ok(())
        } else {
            Err(PayloadError::TrailingBytes(remaining))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod id128_tests {
    use super::*;

    #[test]
    fn id128_wal_layout_is_sixteen_little_endian_bytes() {
        let value = MetadataValue::Id128(DocId::new(0xffeeddccbbaa99887766554433221100));
        let bytes = vec![
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(encode_metadata_value(&value).unwrap(), (6, bytes.clone()));
        assert_eq!(decode_metadata_value(6, &bytes).unwrap(), value);
        for length in [0, 8, 15, 17] {
            assert!(decode_metadata_value(6, &vec![0; length]).is_err());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod transaction_format_tests {
    use super::*;
    fn binding() -> TransactionBinding {
        TransactionBinding {
            transaction: 1,
            participant: 2,
            first_seq: 3,
            last_seq: 3,
            manifest_digest: 4,
            final_generation: 5,
        }
    }
    #[test]
    fn prepared_encoding_golden_and_rejections() {
        let body = encode_delete(&[DocId::new(6)]).unwrap();
        let bytes = encode_prepared(binding(), 0, 1, DELETE_V1, &body).unwrap();
        let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex,
            "010000000100000000000000000000000000000002000000000000000000000000000000030000000000000003000000000000000400000000000000050000000000000000000000010000000200010000000100000006000000000000000000000000000000"
        );
        let decoded = decode_prepared(&bytes).unwrap();
        assert_eq!(decoded.binding, binding());
        assert_eq!(
            decoded.mutation,
            MutationPayload::Delete(vec![DocId::new(6)])
        );
        for end in 0..bytes.len() {
            assert!(decode_prepared(&bytes[..end]).is_err(), "{end}");
        }
        for offset in [0, 2, 4, 20, 36, 60, 68, 72, 76] {
            let mut bad = bytes.clone();
            bad[offset] = 255;
            if matches!(offset, 4 | 20 | 36 | 60) {
                // These are valid different bindings, but cannot satisfy the original decision.
                if let Ok(member) = decode_prepared(&bad) {
                    assert_eq!(
                        transaction_decision(member.binding, Some(binding())),
                        TransactionDecision::Mismatch
                    );
                }
            } else {
                assert!(decode_prepared(&bad).is_err(), "{offset}");
            }
        }
        assert!(encode_prepared(binding(), 0, 1, PREPARED_MUTATION_V1, &bytes).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_prepared(&trailing).is_err());
    }
    #[test]
    fn transaction_decision_table() {
        assert_eq!(
            transaction_decision(binding(), None),
            TransactionDecision::Undecided
        );
        assert_eq!(
            transaction_decision(binding(), Some(binding())),
            TransactionDecision::Committed
        );
        for field in 0..6 {
            let mut changed = binding();
            match field {
                0 => changed.transaction += 1,
                1 => changed.participant += 1,
                2 => changed.first_seq += 1,
                3 => changed.last_seq += 1,
                4 => changed.manifest_digest += 1,
                _ => changed.final_generation += 1,
            }
            assert_eq!(
                transaction_decision(binding(), Some(changed)),
                TransactionDecision::Mismatch
            );
        }
        let mut invalid = binding();
        invalid.transaction = 0;
        assert_eq!(
            transaction_decision(invalid, None),
            TransactionDecision::Mismatch
        );
    }
    #[test]
    fn prepared_operation_is_recognized() {
        // Valid v1 envelope: tx=1, participant=2, range=3..3,
        // digest=4, generation=5, index=0, count=1, delete doc=6.
        let mut bytes = vec![1, 0, 0, 0];
        bytes.extend_from_slice(&1_u128.to_le_bytes());
        bytes.extend_from_slice(&2_u128.to_le_bytes());
        for value in [3_u64, 3, 4, 5] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&super::encode_delete(&[super::DocId::new(6)]).unwrap());
        assert!(super::decode_prepared(&bytes).is_ok());
    }
}

/// Cross-namespace prepared mutation (append-only operation 9).
pub const PREPARED_MUTATION_V1: u16 = 9;

/// Exact participant evidence bound by a root decision. All integers are LE;
/// identities are 128 bits, sequence/digest/generation fields are 64 bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionBinding {
    /// Coordinator transaction identity.
    pub transaction: u128,
    /// Stable participant identity, independent of its route.
    pub participant: u128,
    /// Inclusive first prepared WAL sequence.
    pub first_seq: u64,
    /// Inclusive last prepared WAL sequence.
    pub last_seq: u64,
    /// Xxh3-64 of the selected complete manifest bytes.
    pub manifest_digest: u64,
    /// Generation installed by this transaction.
    pub final_generation: u64,
}
impl TransactionBinding {
    /// Frozen 64-byte binding, shared by root and prepared frames.
    pub fn encode(self) -> Result<Vec<u8>, PayloadError> {
        self.member_count()?;
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&self.transaction.to_le_bytes());
        bytes.extend_from_slice(&self.participant.to_le_bytes());
        for value in [
            self.first_seq,
            self.last_seq,
            self.manifest_digest,
            self.final_generation,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Ok(bytes)
    }
    /// Decodes exactly one frozen binding.
    pub fn decode(bytes: &[u8]) -> Result<Self, PayloadError> {
        let mut cursor = Cursor::new(bytes);
        let binding = Self {
            transaction: cursor.read_u128()?,
            participant: cursor.read_u128()?,
            first_seq: cursor.read_u64()?,
            last_seq: cursor.read_u64()?,
            manifest_digest: cursor.read_u64()?,
            final_generation: cursor.read_u64()?,
        };
        cursor.finish()?;
        binding.member_count()?;
        Ok(binding)
    }
    fn member_count(self) -> Result<u32, PayloadError> {
        if self.transaction == 0
            || self.participant == 0
            || self.first_seq == 0
            || self.final_generation == 0
        {
            return Err(PayloadError::TransactionBinding);
        }
        self.last_seq
            .checked_sub(self.first_seq)
            .and_then(|n| n.checked_add(1))
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(PayloadError::TransactionBinding)
    }
}

/// Decoded prepared member; it is not itself a committed mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedMutation {
    /// Exact evidence required from the root decision.
    pub binding: TransactionBinding,
    /// Zero-based member position.
    pub index: u32,
    /// Number of members in the participant's prepared range.
    pub count: u32,
    /// Existing standalone operation (never another envelope).
    pub op: u16,
    /// Validated standalone mutation.
    pub mutation: MutationPayload,
}

/// `[version:u16,flags:u16,binding:64,index:u32,count:u32,op:u16,body]`.
pub fn encode_prepared(
    binding: TransactionBinding,
    index: u32,
    count: u32,
    op: u16,
    body: &[u8],
) -> Result<Vec<u8>, PayloadError> {
    if count != binding.member_count()? || index >= count {
        return Err(PayloadError::BatchPosition { index, count });
    }
    standalone_mutation(op, body)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(
            body.len()
                .checked_add(78)
                .ok_or(PayloadError::LengthOverflow)?,
        )
        .map_err(|_| PayloadError::LengthOverflow)?;
    append_header(&mut bytes);
    bytes.extend_from_slice(&binding.encode()?);
    bytes.extend_from_slice(&index.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&op.to_le_bytes());
    bytes.extend_from_slice(body);
    Ok(bytes)
}

/// Validates an envelope without deciding whether it committed.
pub fn decode_prepared(bytes: &[u8]) -> Result<PreparedMutation, PayloadError> {
    let mut cursor = Cursor::new(bytes);
    cursor.require_header()?;
    let binding = TransactionBinding::decode(cursor.read_bytes(64)?)?;
    let index = cursor.read_u32()?;
    let count = cursor.read_u32()?;
    if count != binding.member_count()? || index >= count {
        return Err(PayloadError::BatchPosition { index, count });
    }
    let op = cursor.read_u16()?;
    let mutation = standalone_mutation(op, cursor.read_bytes(cursor.remaining())?)?;
    Ok(PreparedMutation {
        binding,
        index,
        count,
        op,
        mutation,
    })
}
fn standalone_mutation(op: u16, bytes: &[u8]) -> Result<MutationPayload, PayloadError> {
    if !(UPSERT_V1..=UPSERT_V2).contains(&op) {
        return Err(PayloadError::UnknownOperation(op));
    }
    decode_mutation(op, bytes)
}

/// Pure root-decision result; absence is explicitly uncommitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionDecision {
    /// All participant evidence agrees.
    Committed,
    /// No decision exists for these frames.
    Undecided,
    /// A decision exists but contradicts the participant evidence.
    Mismatch,
}
/// Compares identities, inclusive WAL range, manifest digest and generation.
pub fn transaction_decision(
    prepared: TransactionBinding,
    selected: Option<TransactionBinding>,
) -> TransactionDecision {
    match selected {
        None if prepared.member_count().is_ok() => TransactionDecision::Undecided,
        Some(binding) if prepared.member_count().is_ok() && binding == prepared => {
            TransactionDecision::Committed
        }
        _ => TransactionDecision::Mismatch,
    }
}
