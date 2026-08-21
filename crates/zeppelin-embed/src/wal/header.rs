//! Shared-file-header encoding for append-only WAL files.

use crate::format::frame::{
    FILE_HEADER_LEN, FILE_MAGIC, FileHeader, FormatCheck, FormatError,
    decode_header as decode_frame_header, encode_header as encode_frame_header, read_u16, read_u64,
};
use crate::format::{FormatFamily, FormatRegistry, RegistryError};

use super::LogSeq;

/// Complete v1 WAL header: shared header plus the family-owned sequence field.
pub const WAL_HEADER_LEN: usize = FILE_HEADER_LEN + 8;

/// Validated WAL-specific interpretation of the shared file header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalHeader {
    /// Sequence required from the first record in this WAL file.
    pub first_seq: LogSeq,
}

/// Specific failure found before WAL record replay begins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalHeaderError {
    /// No file header bytes were present.
    Missing,
    /// Some, but not all, complete WAL header bytes were present.
    Truncated {
        /// Required complete WAL header width.
        needed: usize,
        /// Bytes available in the file.
        available: usize,
    },
    /// The permanent eight-byte magic did not match.
    WrongMagic {
        /// Required repository file magic.
        expected: [u8; 8],
        /// Eight bytes found at the magic position.
        actual: [u8; 8],
    },
    /// The file declares a persisted family other than WAL.
    WrongFamily {
        /// Permanent WAL family identifier.
        expected: u16,
        /// Family identifier found in the file.
        actual: u16,
    },
    /// The WAL version is outside the registry's accepted range.
    UnsupportedVersion {
        /// WAL family identifier.
        family: u16,
        /// Version found in the file.
        version: u16,
        /// Lowest accepted WAL version.
        minimum: u16,
        /// Highest accepted WAL version.
        maximum: u16,
    },
    /// The shared header width is not exactly the v1 WAL header width.
    InvalidHeaderLength {
        /// Required v1 WAL header width.
        expected: u64,
        /// Header width found in the file.
        actual: u64,
    },
    /// The append-only WAL declared a non-zero immutable file length.
    NonZeroFileLength {
        /// Reserved value found in the shared file-length slot.
        actual: u64,
    },
    /// The shared decoder rejected a check that WAL-specific mapping did not expect.
    InvalidSharedHeader {
        /// Exact shared-format check that failed.
        check: FormatCheck,
    },
}

impl std::fmt::Display for WalHeaderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("WAL file header is missing"),
            Self::Truncated { needed, available } => {
                write!(
                    formatter,
                    "WAL file header needs {needed} bytes, got {available}"
                )
            }
            Self::WrongMagic { expected, actual } => {
                write!(formatter, "WAL magic expected {expected:?}, got {actual:?}")
            }
            Self::WrongFamily { expected, actual } => {
                write!(formatter, "WAL family expected {expected}, got {actual}")
            }
            Self::UnsupportedVersion {
                family,
                version,
                minimum,
                maximum,
            } => write!(
                formatter,
                "WAL family {family} version {version} is outside accepted range {minimum}..={maximum}"
            ),
            Self::InvalidHeaderLength { expected, actual } => {
                write!(
                    formatter,
                    "WAL header length expected {expected}, got {actual}"
                )
            }
            Self::NonZeroFileLength { actual } => {
                write!(formatter, "WAL file length must be zero, got {actual}")
            }
            Self::InvalidSharedHeader { check } => {
                write!(formatter, "WAL shared header failed {check:?}")
            }
        }
    }
}

impl std::error::Error for WalHeaderError {}

/// Encodes the complete WAL file header for a log beginning at `first_seq`.
pub fn encode_header(first_seq: LogSeq) -> Result<Vec<u8>, RegistryError> {
    let family = FormatFamily::Wal;
    let spec = FormatRegistry::families()
        .iter()
        .find(|candidate| candidate.family == family)
        .ok_or(RegistryError::UnknownFamily(family.id()))?;
    let mut encoded = encode_frame_header(FileHeader {
        magic: FILE_MAGIC,
        family: family.id(),
        version: spec.current_version,
        flags: 0,
        header_length: WAL_HEADER_LEN as u64,
        file_length: 0,
    });
    encoded.extend_from_slice(&first_seq.get().to_le_bytes());
    Ok(encoded)
}

/// Decodes and validates a WAL file header before any record is consumed.
pub fn decode_header(bytes: &[u8]) -> Result<WalHeader, WalHeaderError> {
    if bytes.is_empty() {
        return Err(WalHeaderError::Missing);
    }
    if bytes.len() < WAL_HEADER_LEN {
        return Err(WalHeaderError::Truncated {
            needed: WAL_HEADER_LEN,
            available: bytes.len(),
        });
    }
    let actual_magic = bytes
        .get(..8)
        .and_then(|value| <[u8; 8]>::try_from(value).ok())
        .ok_or(WalHeaderError::Truncated {
            needed: WAL_HEADER_LEN,
            available: bytes.len(),
        })?;
    let family = read_u16("WAL", bytes, 8).map_err(invalid_shared_header)?;
    let version = read_u16("WAL", bytes, 10).map_err(invalid_shared_header)?;
    let header_length = read_u64("WAL", bytes, 16).map_err(invalid_shared_header)?;
    let common =
        decode_frame_header("WAL", FormatFamily::Wal, bytes).map_err(|error| {
            match error.check() {
                FormatCheck::Magic => WalHeaderError::WrongMagic {
                    expected: FILE_MAGIC,
                    actual: actual_magic,
                },
                FormatCheck::Family => WalHeaderError::WrongFamily {
                    expected: FormatFamily::Wal.id(),
                    actual: family,
                },
                FormatCheck::Version => match FormatRegistry::require(family, version) {
                    Err(RegistryError::UnsupportedVersion {
                        family,
                        version,
                        minimum,
                        maximum,
                    }) => WalHeaderError::UnsupportedVersion {
                        family,
                        version,
                        minimum,
                        maximum,
                    },
                    Ok(_) | Err(_) => WalHeaderError::InvalidSharedHeader {
                        check: FormatCheck::Version,
                    },
                },
                FormatCheck::HeaderLength => WalHeaderError::InvalidHeaderLength {
                    expected: WAL_HEADER_LEN as u64,
                    actual: header_length,
                },
                check => WalHeaderError::InvalidSharedHeader { check },
            }
        })?;
    if common.header_length != WAL_HEADER_LEN as u64 {
        return Err(WalHeaderError::InvalidHeaderLength {
            expected: WAL_HEADER_LEN as u64,
            actual: common.header_length,
        });
    }
    if common.file_length != 0 {
        return Err(WalHeaderError::NonZeroFileLength {
            actual: common.file_length,
        });
    }
    let first_seq = read_u64("WAL", bytes, FILE_HEADER_LEN).map_err(invalid_shared_header)?;
    Ok(WalHeader {
        first_seq: LogSeq::new(first_seq),
    })
}

fn invalid_shared_header(error: FormatError) -> WalHeaderError {
    WalHeaderError::InvalidSharedHeader {
        check: error.check(),
    }
}
