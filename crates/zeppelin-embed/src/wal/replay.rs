//! Checked WAL prefix replay.

use super::LogSeq;
use super::header::{WAL_HEADER_LEN, WalHeaderError, decode_header};
use super::record::{RecordError, WalRecord, decode_record};

/// Whether a corrupt record is the final observable record or precedes a
/// checksum-valid successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorruptionLocation {
    /// No checksum-valid successor was found at the declared next boundary.
    Tail,
    /// A checksum-valid record follows the corrupt record.
    Middle,
}

/// Specific corruption signal that ended replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorruptionReason {
    /// Record framing or checksum validation failed.
    Record {
        /// Tail versus proven middle corruption.
        location: CorruptionLocation,
        /// Exact record validation failure.
        error: RecordError,
    },
    /// The first valid record disagreed with the sequence declared by the file header.
    FirstSequenceMismatch {
        /// Sequence declared by the WAL file header.
        expected: LogSeq,
        /// Sequence observed in the first checked record.
        actual: LogSeq,
        /// Tail versus proven middle corruption.
        location: CorruptionLocation,
    },
    /// A valid record skipped one or more required sequence numbers.
    SequenceGap {
        /// Required next sequence.
        expected: LogSeq,
        /// Sequence observed in the checked record.
        actual: LogSeq,
        /// Tail versus proven middle corruption.
        location: CorruptionLocation,
    },
    /// A valid record repeated or regressed below the required sequence.
    SequenceRegression {
        /// Required next sequence.
        expected: LogSeq,
        /// Sequence observed in the checked record.
        actual: LogSeq,
        /// Tail versus proven middle corruption.
        location: CorruptionLocation,
    },
}

/// Why replay stopped after its trusted prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayTerminator {
    /// WAL file-header validation failed before record replay began.
    InvalidHeader(WalHeaderError),
    /// All bytes ended exactly after a valid record, or the log was empty.
    CleanEnd,
    /// Replay stopped before consuming the named invalid record.
    CorruptAt {
        /// Byte offset of the first invalid record.
        offset: usize,
        /// Specific validation or sequence failure.
        reason: CorruptionReason,
    },
}

/// A checksum- and sequence-validated prefix plus its explicit terminator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayResult<'a> {
    /// Records preceding the first invalid record.
    pub records: Vec<WalRecord<'a>>,
    /// Clean end versus typed corruption boundary.
    pub terminator: ReplayTerminator,
}

/// Replays the largest trusted prefix of an append-only WAL.
///
/// Every record considered for return is checksum-checked. Replay never skips
/// an invalid record. If a checksum-valid record begins at the invalid
/// record's declared successor boundary, the reason is classified as
/// [`CorruptionLocation::Middle`], but that successor is not returned. A torn
/// tail or corruption without such a successor is classified as `Tail`.
/// The file header declares the first required sequence, and every subsequent
/// record must increment by exactly one; gaps, duplicates, and regressions
/// terminate the trusted prefix.
#[must_use]
pub fn replay(bytes: &[u8]) -> ReplayResult<'_> {
    let mut records = Vec::new();
    let header = match decode_header(bytes) {
        Ok(header) => header,
        Err(error) => {
            return ReplayResult {
                records,
                terminator: ReplayTerminator::InvalidHeader(error),
            };
        }
    };
    let mut offset = WAL_HEADER_LEN;
    let mut expected = header.first_seq;
    while let Some(remaining) = bytes
        .get(offset..)
        .filter(|remaining| !remaining.is_empty())
    {
        let decoded = match decode_record(remaining) {
            Ok(decoded) => decoded,
            Err(error) => {
                let location = error
                    .framed_length()
                    .and_then(|length| offset.checked_add(length))
                    .map_or(CorruptionLocation::Tail, |next| {
                        successor_location(bytes, next)
                    });
                return ReplayResult {
                    records,
                    terminator: ReplayTerminator::CorruptAt {
                        offset,
                        reason: CorruptionReason::Record { location, error },
                    },
                };
            }
        };
        let next = offset.saturating_add(decoded.encoded_len);
        if decoded.record.seq != expected {
            let location = successor_location(bytes, next);
            let reason = if records.is_empty() {
                CorruptionReason::FirstSequenceMismatch {
                    expected,
                    actual: decoded.record.seq,
                    location,
                }
            } else if decoded.record.seq > expected {
                CorruptionReason::SequenceGap {
                    expected,
                    actual: decoded.record.seq,
                    location,
                }
            } else {
                CorruptionReason::SequenceRegression {
                    expected,
                    actual: decoded.record.seq,
                    location,
                }
            };
            return ReplayResult {
                records,
                terminator: ReplayTerminator::CorruptAt { offset, reason },
            };
        }
        records.push(decoded.record);
        offset = next;
        expected = LogSeq::new(expected.get().saturating_add(1));
    }
    ReplayResult {
        records,
        terminator: ReplayTerminator::CleanEnd,
    }
}

fn successor_location(bytes: &[u8], offset: usize) -> CorruptionLocation {
    bytes
        .get(offset..)
        .filter(|successor| !successor.is_empty())
        .and_then(|successor| decode_record(successor).ok())
        .map_or(CorruptionLocation::Tail, |_| CorruptionLocation::Middle)
}
