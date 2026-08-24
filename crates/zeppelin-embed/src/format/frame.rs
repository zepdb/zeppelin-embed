//! Length-prefixed xxh3-64 block framing.

use xxhash_rust::xxh3::xxh3_64;

use super::{FormatFamily, FormatRegistry, RegistryError};

/// Fixed header length shared by every persisted file.
pub const FILE_HEADER_LEN: usize = 32;
/// Whole-file xxh3-64 trailer width.
pub const FILE_TRAILER_LEN: usize = 8;
/// Permanent eight-byte file magic.
pub const FILE_MAGIC: [u8; 8] = *b"ZEPEMBED";

/// Decoded fixed-width file header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct FileHeader {
    /// Permanent eight-byte file magic.
    pub magic: [u8; 8],
    /// Registered artifact family.
    pub family: u16,
    /// Additive family version.
    pub version: u16,
    /// Family-owned flags.
    pub flags: u32,
    /// Bytes occupied by the complete artifact header.
    pub header_length: u64,
    /// Exact file length including the whole-file trailer.
    pub file_length: u64,
}

const _: [(); FILE_HEADER_LEN] = [(); std::mem::size_of::<FileHeader>()];

/// The exact validation check that rejected an artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatCheck {
    /// A required byte range was truncated.
    Length,
    /// The permanent magic did not match.
    Magic,
    /// The artifact belonged to another registered family.
    Family,
    /// The format registry rejected the declared version.
    Version,
    /// The declared header length was invalid.
    HeaderLength,
    /// The declared file length did not match the bytes supplied.
    FileLength,
    /// A block length was invalid or did not consume the framed body exactly.
    BlockLength,
    /// A framed block's xxh3-64 did not match.
    BlockChecksum,
    /// The whole-file xxh3-64 trailer did not match.
    FileChecksum,
    /// The immutable object identity did not match the caller's expectation.
    ObjectIdentity,
}

/// Typed, artifact-naming persisted-format validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatError {
    artifact: String,
    check: FormatCheck,
    detail: String,
}

impl FormatError {
    /// Creates an artifact-scoped validation error.
    #[must_use]
    pub fn new(artifact: impl Into<String>, check: FormatCheck, detail: impl Into<String>) -> Self {
        Self {
            artifact: artifact.into(),
            check,
            detail: detail.into(),
        }
    }

    /// Returns the caller-supplied artifact name.
    #[must_use]
    pub fn artifact(&self) -> &str {
        &self.artifact
    }

    /// Returns the exact failed validation check.
    #[must_use]
    pub const fn check(&self) -> FormatCheck {
        self.check
    }

    /// Returns the value-bearing failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "artifact {} failed {:?}: {}",
            self.artifact, self.check, self.detail
        )
    }
}

impl std::error::Error for FormatError {}

/// One validated framed artifact borrowing its original payload bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedArtifact<'a> {
    /// Validated fixed-width file header.
    pub header: FileHeader,
    /// Validated single-block payload.
    pub payload: &'a [u8],
}

/// Encodes one complete single-block artifact with xxh3-64 block and file checksums.
#[must_use]
pub fn encode_artifact(family: FormatFamily, flags: u32, payload: &[u8]) -> Vec<u8> {
    let block_overhead = 16_usize;
    let file_length = FILE_HEADER_LEN
        .saturating_add(block_overhead)
        .saturating_add(payload.len())
        .saturating_add(FILE_TRAILER_LEN);
    let mut encoded = Vec::with_capacity(file_length);
    encoded.extend_from_slice(&encode_header(FileHeader {
        magic: FILE_MAGIC,
        family: family.id(),
        version: 1,
        flags,
        header_length: FILE_HEADER_LEN as u64,
        file_length: file_length as u64,
    }));
    encoded.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    encoded.extend_from_slice(payload);
    encoded.extend_from_slice(&xxh3_64(payload).to_le_bytes());
    let file_checksum = xxh3_64(&encoded);
    encoded.extend_from_slice(&file_checksum.to_le_bytes());
    encoded
}

/// Encodes the shared fixed-width persisted-file header without a body or trailer.
#[must_use]
pub fn encode_header(header: FileHeader) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(FILE_HEADER_LEN);
    encoded.extend_from_slice(&header.magic);
    encoded.extend_from_slice(&header.family.to_le_bytes());
    encoded.extend_from_slice(&header.version.to_le_bytes());
    encoded.extend_from_slice(&header.flags.to_le_bytes());
    encoded.extend_from_slice(&header.header_length.to_le_bytes());
    encoded.extend_from_slice(&header.file_length.to_le_bytes());
    encoded
}

/// Parses only the fixed 32-byte header and validates its registry declaration.
pub fn decode_header(
    artifact: &str,
    expected_family: FormatFamily,
    bytes: &[u8],
) -> Result<FileHeader, FormatError> {
    let fixed = bytes.get(..FILE_HEADER_LEN).ok_or_else(|| {
        FormatError::new(
            artifact,
            FormatCheck::Length,
            format!("need {FILE_HEADER_LEN} header bytes, got {}", bytes.len()),
        )
    })?;
    let magic = fixed
        .get(..8)
        .ok_or_else(|| FormatError::new(artifact, FormatCheck::Length, "missing magic bytes"))?;
    if magic != FILE_MAGIC {
        return Err(FormatError::new(
            artifact,
            FormatCheck::Magic,
            format!("expected {FILE_MAGIC:?}, got {magic:?}"),
        ));
    }
    let family = read_u16(artifact, fixed, 8)?;
    if family != expected_family.id() {
        return Err(FormatError::new(
            artifact,
            FormatCheck::Family,
            format!("expected {}, got {family}", expected_family.id()),
        ));
    }
    let version = read_u16(artifact, fixed, 10)?;
    FormatRegistry::require(family, version).map_err(|error| registry_error(artifact, error))?;
    let header = FileHeader {
        magic: FILE_MAGIC,
        family,
        version,
        flags: read_u32(artifact, fixed, 12)?,
        header_length: read_u64(artifact, fixed, 16)?,
        file_length: read_u64(artifact, fixed, 24)?,
    };
    if header.header_length < FILE_HEADER_LEN as u64 {
        return Err(FormatError::new(
            artifact,
            FormatCheck::HeaderLength,
            format!(
                "header length {} is below {FILE_HEADER_LEN}",
                header.header_length
            ),
        ));
    }
    Ok(header)
}

/// Validates and decodes one complete single-block artifact.
pub fn decode_artifact<'a>(
    artifact: &str,
    expected_family: FormatFamily,
    bytes: &'a [u8],
) -> Result<DecodedArtifact<'a>, FormatError> {
    let header = decode_header(artifact, expected_family, bytes)?;
    let actual_file_length = u64::try_from(bytes.len()).map_err(|_| {
        FormatError::new(artifact, FormatCheck::FileLength, "file length exceeds u64")
    })?;
    if header.file_length != actual_file_length {
        return Err(FormatError::new(
            artifact,
            FormatCheck::FileLength,
            format!(
                "declared {}, actual {actual_file_length}",
                header.file_length
            ),
        ));
    }
    if header.header_length != FILE_HEADER_LEN as u64 {
        return Err(FormatError::new(
            artifact,
            FormatCheck::HeaderLength,
            format!(
                "single-block header must be {FILE_HEADER_LEN}, got {}",
                header.header_length
            ),
        ));
    }
    let trailer_start = bytes.len().checked_sub(FILE_TRAILER_LEN).ok_or_else(|| {
        FormatError::new(artifact, FormatCheck::Length, "missing whole-file trailer")
    })?;
    let checksummed = bytes.get(..trailer_start).ok_or_else(|| {
        FormatError::new(artifact, FormatCheck::Length, "invalid trailer position")
    })?;
    let expected_file_checksum = read_u64(artifact, bytes, trailer_start)?;
    let actual_file_checksum = xxh3_64(checksummed);
    if actual_file_checksum != expected_file_checksum {
        return Err(FormatError::new(
            artifact,
            FormatCheck::FileChecksum,
            format!(
                "expected {expected_file_checksum:#018x}, computed {actual_file_checksum:#018x}"
            ),
        ));
    }

    let payload_length = read_u64(artifact, bytes, FILE_HEADER_LEN)?;
    let payload_length = usize::try_from(payload_length).map_err(|_| {
        FormatError::new(
            artifact,
            FormatCheck::BlockLength,
            "block length exceeds usize",
        )
    })?;
    let payload_start = FILE_HEADER_LEN.saturating_add(8);
    let payload_end = payload_start.checked_add(payload_length).ok_or_else(|| {
        FormatError::new(artifact, FormatCheck::BlockLength, "block end overflow")
    })?;
    let checksum_end = payload_end.checked_add(8).ok_or_else(|| {
        FormatError::new(artifact, FormatCheck::BlockLength, "checksum end overflow")
    })?;
    if checksum_end != trailer_start {
        return Err(FormatError::new(
            artifact,
            FormatCheck::BlockLength,
            format!("framed block ends at {checksum_end}, trailer starts at {trailer_start}"),
        ));
    }
    let payload = bytes.get(payload_start..payload_end).ok_or_else(|| {
        FormatError::new(artifact, FormatCheck::Length, "block payload is truncated")
    })?;
    let expected_block_checksum = read_u64(artifact, bytes, payload_end)?;
    let actual_block_checksum = xxh3_64(payload);
    if actual_block_checksum != expected_block_checksum {
        return Err(FormatError::new(
            artifact,
            FormatCheck::BlockChecksum,
            format!(
                "expected {expected_block_checksum:#018x}, computed {actual_block_checksum:#018x}"
            ),
        ));
    }
    Ok(DecodedArtifact { header, payload })
}

fn registry_error(artifact: &str, error: RegistryError) -> FormatError {
    FormatError::new(artifact, FormatCheck::Version, error.to_string())
}

pub(crate) fn read_u16(artifact: &str, bytes: &[u8], offset: usize) -> Result<u16, FormatError> {
    let raw = bytes
        .get(offset..)
        .and_then(|tail| tail.first_chunk::<2>())
        .ok_or_else(|| {
            FormatError::new(
                artifact,
                FormatCheck::Length,
                format!("missing u16 at {offset}"),
            )
        })?;
    Ok(u16::from_le_bytes(*raw))
}

pub(crate) fn read_u32(artifact: &str, bytes: &[u8], offset: usize) -> Result<u32, FormatError> {
    let raw = bytes
        .get(offset..)
        .and_then(|tail| tail.first_chunk::<4>())
        .ok_or_else(|| {
            FormatError::new(
                artifact,
                FormatCheck::Length,
                format!("missing u32 at {offset}"),
            )
        })?;
    Ok(u32::from_le_bytes(*raw))
}

pub(crate) fn read_u64(artifact: &str, bytes: &[u8], offset: usize) -> Result<u64, FormatError> {
    let raw = bytes
        .get(offset..)
        .and_then(|tail| tail.first_chunk::<8>())
        .ok_or_else(|| {
            FormatError::new(
                artifact,
                FormatCheck::Length,
                format!("missing u64 at {offset}"),
            )
        })?;
    Ok(u64::from_le_bytes(*raw))
}
