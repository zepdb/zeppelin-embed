//! Byte-exact WAL record framing.

use xxhash_rust::xxh3::xxh3_64;

use super::LogSeq;

/// Bytes before a WAL record payload: length, sequence, and operation.
pub const RECORD_HEADER_LEN: usize = 14;
/// Width of the trailing xxh3-64 checksum.
pub const RECORD_CHECKSUM_LEN: usize = 8;
/// Smallest encoded WAL record.
pub const MIN_RECORD_LEN: usize = RECORD_HEADER_LEN + RECORD_CHECKSUM_LEN;

/// One decoded WAL record borrowing its checksummed payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalRecord<'a> {
    /// Monotonic log sequence.
    pub seq: LogSeq,
    /// Caller-defined operation identifier.
    pub op: u16,
    /// Operation payload bytes.
    pub payload: &'a [u8],
}

/// A complete checked record plus its encoded width.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedRecord<'a> {
    /// Checked logical record.
    pub record: WalRecord<'a>,
    /// Bytes consumed by this record, including its checksum.
    pub encoded_len: usize,
}

/// Record encoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordEncodeError {
    /// The payload cannot fit the permanent u32 length field.
    PayloadTooLarge(usize),
}

impl std::fmt::Display for RecordEncodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PayloadTooLarge(length) => {
                write!(formatter, "WAL payload length {length} exceeds u32")
            }
        }
    }
}

impl std::error::Error for RecordEncodeError {}

/// Specific validation failure for one WAL record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordError {
    /// The fixed length/sequence/operation header is torn.
    HeaderTruncated {
        /// Required fixed header width.
        needed: usize,
        /// Bytes available at the record offset.
        available: usize,
    },
    /// The declared payload plus framing cannot fit `usize`.
    LengthOverflow {
        /// Persisted u32 payload length.
        payload_length: u32,
    },
    /// The record body or checksum is torn.
    BodyTruncated {
        /// Persisted u32 payload length.
        payload_length: u32,
        /// Complete framed width implied by the header.
        needed: usize,
        /// Bytes available at the record offset.
        available: usize,
    },
    /// The trailing xxh3-64 does not cover the observed record bytes.
    ChecksumMismatch {
        /// Persisted checksum.
        expected: u64,
        /// Checksum computed from the header and payload.
        actual: u64,
        /// Complete framed width used to find a possible successor.
        record_length: usize,
    },
}

impl RecordError {
    pub(crate) const fn framed_length(self) -> Option<usize> {
        match self {
            Self::ChecksumMismatch { record_length, .. } => Some(record_length),
            Self::HeaderTruncated { .. }
            | Self::LengthOverflow { .. }
            | Self::BodyTruncated { .. } => None,
        }
    }
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeaderTruncated { needed, available } => {
                write!(
                    formatter,
                    "WAL header needs {needed} bytes, got {available}"
                )
            }
            Self::LengthOverflow { payload_length } => {
                write!(
                    formatter,
                    "WAL payload length {payload_length} overflows framing"
                )
            }
            Self::BodyTruncated {
                payload_length,
                needed,
                available,
            } => write!(
                formatter,
                "WAL payload length {payload_length} needs {needed} framed bytes, got {available}"
            ),
            Self::ChecksumMismatch {
                expected, actual, ..
            } => write!(
                formatter,
                "WAL checksum expected {expected:#018x}, computed {actual:#018x}"
            ),
        }
    }
}

impl std::error::Error for RecordError {}

/// Encodes `[payload_len:u32, seq:u64, op:u16, payload, checksum:u64]`.
///
/// The trailing xxh3-64 checksum covers exactly the little-endian payload
/// length bytes `0..4`, sequence bytes `4..12`, operation bytes `12..14`, and
/// every payload byte `14..14+payload_len`. It excludes only the checksum
/// field itself.
pub fn encode_record(record: WalRecord<'_>) -> Result<Vec<u8>, RecordEncodeError> {
    let encoded_len = encoded_record_len(record.payload.len())?;
    let mut encoded = Vec::with_capacity(encoded_len);
    encode_record_into(record, &mut encoded)?;
    Ok(encoded)
}

/// Returns the exact framed width needed by one payload.
pub(crate) fn encoded_record_len(payload_len: usize) -> Result<usize, RecordEncodeError> {
    u32::try_from(payload_len).map_err(|_| RecordEncodeError::PayloadTooLarge(payload_len))?;
    Ok(MIN_RECORD_LEN.saturating_add(payload_len))
}

/// Encodes one record into caller-reserved storage.
///
/// Callers that reserve [`encoded_record_len`] bytes before taking a shared
/// lock can perform the only payload copy without allocating while locked.
pub(crate) fn encode_record_into(
    record: WalRecord<'_>,
    encoded: &mut Vec<u8>,
) -> Result<(), RecordEncodeError> {
    let payload_length = u32::try_from(record.payload.len())
        .map_err(|_| RecordEncodeError::PayloadTooLarge(record.payload.len()))?;
    encoded.clear();
    encoded.extend_from_slice(&payload_length.to_le_bytes());
    encoded.extend_from_slice(&record.seq.get().to_le_bytes());
    encoded.extend_from_slice(&record.op.to_le_bytes());
    encoded.extend_from_slice(record.payload);
    let checksum = xxh3_64(encoded);
    encoded.extend_from_slice(&checksum.to_le_bytes());
    Ok(())
}

/// Decodes one record and verifies xxh3-64 before returning any fields.
pub fn decode_record(bytes: &[u8]) -> Result<DecodedRecord<'_>, RecordError> {
    if bytes.len() < RECORD_HEADER_LEN {
        return Err(RecordError::HeaderTruncated {
            needed: RECORD_HEADER_LEN,
            available: bytes.len(),
        });
    }
    let payload_length = read_u32(bytes, 0)?;
    let payload_length_usize = usize::try_from(payload_length)
        .map_err(|_| RecordError::LengthOverflow { payload_length })?;
    let encoded_len = MIN_RECORD_LEN
        .checked_add(payload_length_usize)
        .ok_or(RecordError::LengthOverflow { payload_length })?;
    if bytes.len() < encoded_len {
        return Err(RecordError::BodyTruncated {
            payload_length,
            needed: encoded_len,
            available: bytes.len(),
        });
    }
    let payload_end = RECORD_HEADER_LEN.saturating_add(payload_length_usize);
    let payload = bytes
        .get(RECORD_HEADER_LEN..payload_end)
        .ok_or(RecordError::BodyTruncated {
            payload_length,
            needed: encoded_len,
            available: bytes.len(),
        })?;
    let expected = read_u64(bytes, payload_end)?;
    let checksummed = bytes.get(..payload_end).ok_or(RecordError::BodyTruncated {
        payload_length,
        needed: encoded_len,
        available: bytes.len(),
    })?;
    let actual = xxh3_64(checksummed);
    if actual != expected {
        return Err(RecordError::ChecksumMismatch {
            expected,
            actual,
            record_length: encoded_len,
        });
    }
    Ok(DecodedRecord {
        record: WalRecord {
            seq: LogSeq::new(read_u64(bytes, 4)?),
            op: read_u16(bytes, 12)?,
            payload,
        },
        encoded_len,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, RecordError> {
    let raw = bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|value| <[u8; 2]>::try_from(value).ok())
        .ok_or(RecordError::HeaderTruncated {
            needed: RECORD_HEADER_LEN,
            available: bytes.len(),
        })?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, RecordError> {
    let raw = bytes
        .get(offset..offset.saturating_add(4))
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
        .ok_or(RecordError::HeaderTruncated {
            needed: RECORD_HEADER_LEN,
            available: bytes.len(),
        })?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, RecordError> {
    let raw = bytes
        .get(offset..offset.saturating_add(8))
        .and_then(|value| <[u8; 8]>::try_from(value).ok())
        .ok_or(RecordError::BodyTruncated {
            payload_length: 0,
            needed: offset.saturating_add(8),
            available: bytes.len(),
        })?;
    Ok(u64::from_le_bytes(raw))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{WalRecord, encode_record_into, encoded_record_len};
    use crate::wal::LogSeq;

    #[test]
    fn caller_reserved_encoding_keeps_its_allocation() {
        let payload = [0x5a; 1_024];
        let encoded_len = encoded_record_len(payload.len()).expect("valid payload length");
        let mut encoded = Vec::with_capacity(encoded_len);
        let allocation = encoded.as_ptr();
        let capacity = encoded.capacity();

        encode_record_into(
            WalRecord {
                seq: LogSeq::new(7),
                op: 3,
                payload: &payload,
            },
            &mut encoded,
        )
        .expect("encode into reserved allocation");

        assert_eq!(encoded.as_ptr(), allocation);
        assert_eq!(encoded.capacity(), capacity);
        assert_eq!(encoded.len(), encoded_len);
    }
}
