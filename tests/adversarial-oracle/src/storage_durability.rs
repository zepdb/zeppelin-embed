//! Independent primitive checkers for the `storage-durability` campaign.
//!
//! This module intentionally uses only `std` types. Production adapters must
//! translate public results and raw-artifact observations into these DTOs;
//! expected values must come from the seed-derived primitive fixture.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;

const FILE_MAGIC: [u8; 8] = *b"ZEPEMBED";
const FILE_HEADER_LEN: usize = 32;
const FILE_TRAILER_LEN: usize = 8;
const WAL_HEADER_LEN: usize = 40;
const WAL_FAMILY: u16 = 11;
/// Frozen manifest family id used by literal persisted-byte expectations.
pub const MANIFEST_FAMILY_ID: u16 = 10;
/// Frozen segment family id used by literal persisted-byte expectations.
pub const SEGMENT_FAMILY_ID: u16 = 2;
const MANIFEST_FAMILY: u16 = MANIFEST_FAMILY_ID;
const SEGMENT_FAMILY: u16 = SEGMENT_FAMILY_ID;
const REGION_ALIGNMENT: u64 = 16 * 1024;
const REGION_ENTRY_LEN: usize = 32;
const DEFAULT_SECRET: [u8; 192] = [
    0xb8, 0xfe, 0x6c, 0x39, 0x23, 0xa4, 0x4b, 0xbe, 0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c,
    0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb, 0x72, 0x40, 0xa4, 0xa4, 0xb7, 0xb3, 0x67, 0x1f,
    0xcb, 0x79, 0xe6, 0x4e, 0xcc, 0xc0, 0xe5, 0x78, 0x82, 0x5a, 0xd0, 0x7d, 0xcc, 0xff, 0x72, 0x21,
    0xb8, 0x08, 0x46, 0x74, 0xf7, 0x43, 0x24, 0x8e, 0xe0, 0x35, 0x90, 0xe6, 0x81, 0x3a, 0x26, 0x4c,
    0x3c, 0x28, 0x52, 0xbb, 0x91, 0xc3, 0x00, 0xcb, 0x88, 0xd0, 0x65, 0x8b, 0x1b, 0x53, 0x2e, 0xa3,
    0x71, 0x64, 0x48, 0x97, 0xa2, 0x0d, 0xf9, 0x4e, 0x38, 0x19, 0xef, 0x46, 0xa9, 0xde, 0xac, 0xd8,
    0xa8, 0xfa, 0x76, 0x3f, 0xe3, 0x9c, 0x34, 0x3f, 0xf9, 0xdc, 0xbb, 0xc7, 0xc7, 0x0b, 0x4f, 0x1d,
    0x8a, 0x51, 0xe0, 0x4b, 0xcd, 0xb4, 0x59, 0x31, 0xc8, 0x9f, 0x7e, 0xc9, 0xd9, 0x78, 0x73, 0x64,
    0xea, 0xc5, 0xac, 0x83, 0x34, 0xd3, 0xeb, 0xc3, 0xc5, 0x81, 0xa0, 0xff, 0xfa, 0x13, 0x63, 0xeb,
    0x17, 0x0d, 0xdd, 0x51, 0xb7, 0xf0, 0xda, 0x49, 0xd3, 0x16, 0x55, 0x26, 0x29, 0xd4, 0x68, 0x9e,
    0x2b, 0x16, 0xbe, 0x58, 0x7d, 0x47, 0xa1, 0xfc, 0x8f, 0xf8, 0xb8, 0xd1, 0x7a, 0xd0, 0x31, 0xce,
    0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e,
];

const PRIME32_1: u64 = 0x9e37_79b1;
const PRIME32_2: u64 = 0x85eb_ca77;
const PRIME32_3: u64 = 0xc2b2_ae3d;
const PRIME64_1: u64 = 0x9e37_79b1_85eb_ca87;
const PRIME64_2: u64 = 0xc2b2_ae3d_27d4_eb4f;
const PRIME64_3: u64 = 0x1656_67b1_9e37_79f9;
const PRIME64_4: u64 = 0x85eb_ca77_c2b2_ae63;
const PRIME64_5: u64 = 0x27d4_eb2f_1656_67c5;
const PRIME_MX1: u64 = 0x1656_6791_9e37_79f9;
const PRIME_MX2: u64 = 0x9fb2_1c65_1e98_df25;

fn hash_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut word = [0_u8; 4];
    word.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(word)
}

fn hash_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(word)
}

fn fold_product(left: u64, right: u64) -> u64 {
    let product = u128::from(left).wrapping_mul(u128::from(right));
    (product as u64) ^ ((product >> 64) as u64)
}

fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 37;
    value = value.wrapping_mul(PRIME_MX1);
    value ^ (value >> 32)
}

fn xxh64_avalanche(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(PRIME64_2);
    value ^= value >> 29;
    value = value.wrapping_mul(PRIME64_3);
    value ^ (value >> 32)
}

fn mix_sixteen(input: &[u8], input_offset: usize, secret_offset: usize) -> u64 {
    let first = hash_u64(input, input_offset) ^ hash_u64(&DEFAULT_SECRET, secret_offset);
    let second = hash_u64(input, input_offset + 8) ^ hash_u64(&DEFAULT_SECRET, secret_offset + 8);
    fold_product(first, second)
}

fn accumulate_stripe(accumulators: &mut [u64; 8], input: &[u8], secret_offset: usize) {
    for lane in 0..8 {
        let offset = lane * 8;
        let data = hash_u64(input, offset);
        let key = hash_u64(&DEFAULT_SECRET, secret_offset + offset);
        let mixed = data ^ key;
        accumulators[lane ^ 1] = accumulators[lane ^ 1].wrapping_add(data);
        accumulators[lane] =
            accumulators[lane].wrapping_add((mixed & 0xffff_ffff).wrapping_mul(mixed >> 32));
    }
}

fn scramble(accumulators: &mut [u64; 8]) {
    for (lane, accumulator) in accumulators.iter_mut().enumerate() {
        *accumulator ^= *accumulator >> 47;
        *accumulator ^= hash_u64(&DEFAULT_SECRET, 128 + lane * 8);
        *accumulator = accumulator.wrapping_mul(PRIME32_1);
    }
}

fn xxh3_long(input: &[u8]) -> u64 {
    let mut accumulators = [
        PRIME32_3, PRIME64_1, PRIME64_2, PRIME64_3, PRIME64_4, PRIME32_2, PRIME64_5, PRIME32_1,
    ];
    let full_blocks = (input.len() - 1) / 1024;
    for block in 0..full_blocks {
        let block_start = block * 1024;
        for stripe in 0..16 {
            let start = block_start + stripe * 64;
            accumulate_stripe(&mut accumulators, &input[start..start + 64], stripe * 8);
        }
        scramble(&mut accumulators);
    }
    let last_block_start = full_blocks * 1024;
    let ordinary_stripes = (input.len() - 1 - last_block_start) / 64;
    for stripe in 0..ordinary_stripes {
        let start = last_block_start + stripe * 64;
        accumulate_stripe(&mut accumulators, &input[start..start + 64], stripe * 8);
    }
    accumulate_stripe(&mut accumulators, &input[input.len() - 64..], 121);
    let mut result = (input.len() as u64).wrapping_mul(PRIME64_1);
    for pair in 0..4 {
        let left = accumulators[pair * 2] ^ hash_u64(&DEFAULT_SECRET, 11 + pair * 16);
        let right = accumulators[pair * 2 + 1] ^ hash_u64(&DEFAULT_SECRET, 19 + pair * 16);
        result = result.wrapping_add(fold_product(left, right));
    }
    avalanche(result)
}

/// Independent, scalar, unseeded XXH3-64 used only by the std-only oracle.
#[must_use]
pub fn xxh3_64(input: &[u8]) -> u64 {
    match input.len() {
        0 => xxh64_avalanche(hash_u64(&DEFAULT_SECRET, 56) ^ hash_u64(&DEFAULT_SECRET, 64)),
        1..=3 => {
            let combined = u32::from(input[input.len() - 1])
                | ((input.len() as u32) << 8)
                | (u32::from(input[0]) << 16)
                | (u32::from(input[input.len() / 2]) << 24);
            let key = u64::from(hash_u32(&DEFAULT_SECRET, 0) ^ hash_u32(&DEFAULT_SECRET, 4));
            xxh64_avalanche(key ^ u64::from(combined))
        }
        4..=8 => {
            let first = u64::from(hash_u32(input, 0));
            let last = u64::from(hash_u32(input, input.len() - 4));
            let combined = last | (first << 32);
            let mut value =
                combined ^ (hash_u64(&DEFAULT_SECRET, 8) ^ hash_u64(&DEFAULT_SECRET, 16));
            value ^= value.rotate_left(49) ^ value.rotate_left(24);
            value = value.wrapping_mul(PRIME_MX2);
            value ^= (value >> 35).wrapping_add(input.len() as u64);
            value = value.wrapping_mul(PRIME_MX2);
            value ^ (value >> 28)
        }
        9..=16 => {
            let first = hash_u64(input, 0);
            let last = hash_u64(input, input.len() - 8);
            let low = first ^ (hash_u64(&DEFAULT_SECRET, 24) ^ hash_u64(&DEFAULT_SECRET, 32));
            let high = last ^ (hash_u64(&DEFAULT_SECRET, 40) ^ hash_u64(&DEFAULT_SECRET, 48));
            avalanche(
                (input.len() as u64)
                    .wrapping_add(low.swap_bytes())
                    .wrapping_add(high)
                    .wrapping_add(fold_product(low, high)),
            )
        }
        17..=128 => {
            let mut result = (input.len() as u64).wrapping_mul(PRIME64_1);
            if input.len() > 32 {
                if input.len() > 64 {
                    if input.len() > 96 {
                        result = result.wrapping_add(mix_sixteen(input, 48, 96));
                        result = result.wrapping_add(mix_sixteen(input, input.len() - 64, 112));
                    }
                    result = result.wrapping_add(mix_sixteen(input, 32, 64));
                    result = result.wrapping_add(mix_sixteen(input, input.len() - 48, 80));
                }
                result = result.wrapping_add(mix_sixteen(input, 16, 32));
                result = result.wrapping_add(mix_sixteen(input, input.len() - 32, 48));
            }
            result = result.wrapping_add(mix_sixteen(input, 0, 0));
            result = result.wrapping_add(mix_sixteen(input, input.len() - 16, 16));
            avalanche(result)
        }
        129..=240 => {
            let mut result = (input.len() as u64).wrapping_mul(PRIME64_1);
            for chunk in 0..8 {
                result = result.wrapping_add(mix_sixteen(input, chunk * 16, chunk * 16));
            }
            result = avalanche(result);
            let complete_chunks = input.len() / 16;
            for chunk in 8..complete_chunks {
                result = result.wrapping_add(mix_sixteen(input, chunk * 16, 3 + (chunk - 8) * 16));
            }
            result = result.wrapping_add(mix_sixteen(input, input.len() - 16, 119));
            avalanche(result)
        }
        _ => xxh3_long(input),
    }
}

/// Version attested by storage campaign evidence.
pub const ORACLE_CONTRACT_VERSION: &str = "storage-durability-v1";
/// Exact I15 checker identity.
pub const I15_CHECKER_ID: &str = "I15.storage-publication-v1";
/// Exact I16 checker identity.
pub const I16_CHECKER_ID: &str = "I16.storage-wal-prefix-v1";
/// Exact I17 checker identity.
pub const I17_CHECKER_ID: &str = "I17.storage-retry-idempotence-v1";
/// Exact I18 checker identity.
pub const I18_CHECKER_ID: &str = "I18.storage-typed-artifact-refusal-v1";
/// Exact I19 checker identity.
pub const I19_CHECKER_ID: &str = "I19.storage-reachability-v1";

/// One exact checker refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckFailure {
    /// Stable checker ID.
    pub checker_id: &'static str,
    /// Stable contract-specific reason.
    pub detail: &'static str,
}

impl CheckFailure {
    const fn new(checker_id: &'static str, detail: &'static str) -> Self {
        Self { checker_id, detail }
    }
}

impl fmt::Display for CheckFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.checker_id, self.detail)
    }
}

impl std::error::Error for CheckFailure {}

/// Primitive immutable-segment descriptor parsed from persisted bytes.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SegmentFact {
    pub id: [u8; 16],
    pub rows: u32,
    pub scheme: u16,
    pub dims: u32,
    pub file_length: u64,
    pub header_checksum: u64,
    pub whole_file_checksum: u64,
}

/// Stable parser failure independent from product error types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseCheck {
    Length,
    Magic,
    Family,
    Version,
    Flags,
    HeaderLength,
    FileLength,
    Reserved,
    Bounds,
    Ordering,
    BlockChecksum,
    FileChecksum,
    TrailingBytes,
}

/// Exact artifact and offset rejected by one raw parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    pub artifact: String,
    pub check: ParseCheck,
    pub offset: u64,
    pub detail: String,
}

impl ParseError {
    fn new(artifact: &str, check: ParseCheck, offset: usize, detail: impl Into<String>) -> Self {
        Self {
            artifact: artifact.to_owned(),
            check,
            offset: offset as u64,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {} failed {:?}: {}",
            self.artifact, self.offset, self.check, self.detail
        )
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawHeader {
    header_length: u64,
    file_length: u64,
}

fn exact_bytes<const N: usize>(
    artifact: &str,
    bytes: &[u8],
    offset: usize,
) -> Result<[u8; N], ParseError> {
    bytes
        .get(
            offset..offset.checked_add(N).ok_or_else(|| {
                ParseError::new(artifact, ParseCheck::Bounds, offset, "offset overflow")
            })?,
        )
        .ok_or_else(|| {
            ParseError::new(
                artifact,
                ParseCheck::Length,
                offset,
                format!("need {N} bytes, total {}", bytes.len()),
            )
        })?
        .try_into()
        .map_err(|_| ParseError::new(artifact, ParseCheck::Length, offset, "invalid byte width"))
}

fn raw_u16(artifact: &str, bytes: &[u8], offset: usize) -> Result<u16, ParseError> {
    exact_bytes(artifact, bytes, offset).map(u16::from_le_bytes)
}

fn raw_u32(artifact: &str, bytes: &[u8], offset: usize) -> Result<u32, ParseError> {
    exact_bytes(artifact, bytes, offset).map(u32::from_le_bytes)
}

fn raw_u64(artifact: &str, bytes: &[u8], offset: usize) -> Result<u64, ParseError> {
    exact_bytes(artifact, bytes, offset).map(u64::from_le_bytes)
}

fn parse_header(
    artifact: &str,
    bytes: &[u8],
    family: u16,
    version: u16,
) -> Result<RawHeader, ParseError> {
    if bytes.len() < FILE_HEADER_LEN {
        return Err(ParseError::new(
            artifact,
            ParseCheck::Length,
            0,
            format!("need {FILE_HEADER_LEN} bytes, got {}", bytes.len()),
        ));
    }
    let magic = exact_bytes::<8>(artifact, bytes, 0)?;
    if magic != FILE_MAGIC {
        return Err(ParseError::new(
            artifact,
            ParseCheck::Magic,
            0,
            format!("expected {FILE_MAGIC:?}, got {magic:?}"),
        ));
    }
    let actual_family = raw_u16(artifact, bytes, 8)?;
    if actual_family != family {
        return Err(ParseError::new(
            artifact,
            ParseCheck::Family,
            8,
            format!("expected {family}, got {actual_family}"),
        ));
    }
    let actual_version = raw_u16(artifact, bytes, 10)?;
    if actual_version != version {
        return Err(ParseError::new(
            artifact,
            ParseCheck::Version,
            10,
            format!("expected {version}, got {actual_version}"),
        ));
    }
    let flags = raw_u32(artifact, bytes, 12)?;
    if flags != 0 {
        return Err(ParseError::new(
            artifact,
            ParseCheck::Flags,
            12,
            format!("expected zero, got {flags}"),
        ));
    }
    Ok(RawHeader {
        header_length: raw_u64(artifact, bytes, 16)?,
        file_length: raw_u64(artifact, bytes, 24)?,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestSegmentFact {
    pub id: [u8; 16],
    pub rows: u32,
    pub scheme: u16,
    pub dims: u32,
    pub file_length: u64,
}

/// Facts decoded literally from a complete manifest v2 artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestArtifact {
    pub generation: u64,
    pub log_seq: u64,
    pub segments: Vec<ManifestSegmentFact>,
    pub block_checksum: u64,
    pub file_checksum: u64,
    pub non_segment_payload_checksum: u64,
}

#[derive(Clone, Debug)]
struct Cursor<'a> {
    artifact: &'a str,
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(artifact: &'a str, bytes: &'a [u8]) -> Self {
        Self {
            artifact,
            bytes,
            offset: 0,
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ParseError> {
        let start = self.offset;
        let end = start.checked_add(length).ok_or_else(|| {
            ParseError::new(self.artifact, ParseCheck::Bounds, start, "cursor overflow")
        })?;
        let value = self.bytes.get(start..end).ok_or_else(|| {
            ParseError::new(
                self.artifact,
                ParseCheck::Length,
                start,
                format!(
                    "need {length} bytes, {} remain",
                    self.bytes.len().saturating_sub(start)
                ),
            )
        })?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, ParseError> {
        self.take(1)?.first().copied().ok_or_else(|| {
            ParseError::new(
                self.artifact,
                ParseCheck::Length,
                self.offset,
                "missing byte",
            )
        })
    }

    fn u16(&mut self) -> Result<u16, ParseError> {
        self.take(2)?
            .try_into()
            .map(u16::from_le_bytes)
            .map_err(|_| ParseError::new(self.artifact, ParseCheck::Length, self.offset, "u16"))
    }

    fn u32(&mut self) -> Result<u32, ParseError> {
        self.take(4)?
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(|_| ParseError::new(self.artifact, ParseCheck::Length, self.offset, "u32"))
    }

    fn u64(&mut self) -> Result<u64, ParseError> {
        self.take(8)?
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|_| ParseError::new(self.artifact, ParseCheck::Length, self.offset, "u64"))
    }

    fn length_prefixed(&mut self) -> Result<&'a [u8], ParseError> {
        let length = usize::try_from(self.u32()?).map_err(|_| {
            ParseError::new(
                self.artifact,
                ParseCheck::Bounds,
                self.offset,
                "length exceeds usize",
            )
        })?;
        self.take(length)
    }

    fn zeroes(&mut self, length: usize) -> Result<(), ParseError> {
        let start = self.offset;
        if self.take(length)?.iter().any(|byte| *byte != 0) {
            return Err(ParseError::new(
                self.artifact,
                ParseCheck::Reserved,
                start,
                "reserved bytes are nonzero",
            ));
        }
        Ok(())
    }
}

fn parse_tower(cursor: &mut Cursor<'_>) -> Result<(), ParseError> {
    let _model_id = cursor.length_prefixed()?;
    let _model_version = cursor.length_prefixed()?;
    let _weights_digest = cursor.length_prefixed()?;
    let _dims = cursor.u32()?;
    let _normalization = cursor.u16()?;
    let _prompt = cursor.length_prefixed()?;
    let _max_tokens = cursor.u32()?;
    let _runtime = cursor.u16()?;
    let _compute_units = cursor.u16()?;
    let tag_offset = cursor.offset;
    match cursor.u8()? {
        0 => cursor.zeroes(3),
        1 => {
            cursor.zeroes(3)?;
            let _os_build = cursor.length_prefixed()?;
            Ok(())
        }
        tag => Err(ParseError::new(
            cursor.artifact,
            ParseCheck::Reserved,
            tag_offset,
            format!("invalid optional OS tag {tag}"),
        )),
    }
}

fn parse_optional_identity(cursor: &mut Cursor<'_>, width: usize) -> Result<(), ParseError> {
    let tag_offset = cursor.offset;
    let tag = cursor.u8()?;
    cursor.zeroes(7)?;
    let values = cursor.take(width)?;
    match tag {
        0 if values.iter().all(|byte| *byte == 0) => Ok(()),
        1 => Ok(()),
        0 => Err(ParseError::new(
            cursor.artifact,
            ParseCheck::Reserved,
            tag_offset,
            "absent identity contains nonzero value",
        )),
        other => Err(ParseError::new(
            cursor.artifact,
            ParseCheck::Reserved,
            tag_offset,
            format!("invalid identity tag {other}"),
        )),
    }
}

fn parse_single_block<'a>(
    artifact: &str,
    bytes: &'a [u8],
    family: u16,
    version: u16,
) -> Result<(&'a [u8], u64, u64), ParseError> {
    let header = parse_header(artifact, bytes, family, version)?;
    if header.header_length != FILE_HEADER_LEN as u64 {
        return Err(ParseError::new(
            artifact,
            ParseCheck::HeaderLength,
            16,
            format!("expected {FILE_HEADER_LEN}, got {}", header.header_length),
        ));
    }
    if header.file_length != bytes.len() as u64 {
        return Err(ParseError::new(
            artifact,
            ParseCheck::FileLength,
            24,
            format!("declared {}, actual {}", header.file_length, bytes.len()),
        ));
    }
    let trailer_offset = bytes.len().checked_sub(FILE_TRAILER_LEN).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Length,
            bytes.len(),
            "missing file trailer",
        )
    })?;
    let file_checksum = raw_u64(artifact, bytes, trailer_offset)?;
    let computed_file = xxh3_64(bytes.get(..trailer_offset).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            0,
            "invalid file checksum range",
        )
    })?);
    if file_checksum != computed_file {
        return Err(ParseError::new(
            artifact,
            ParseCheck::FileChecksum,
            trailer_offset,
            format!("expected {file_checksum:#018x}, computed {computed_file:#018x}"),
        ));
    }
    let payload_length =
        usize::try_from(raw_u64(artifact, bytes, FILE_HEADER_LEN)?).map_err(|_| {
            ParseError::new(
                artifact,
                ParseCheck::Bounds,
                FILE_HEADER_LEN,
                "payload length exceeds usize",
            )
        })?;
    let payload_start = FILE_HEADER_LEN + 8;
    let payload_end = payload_start.checked_add(payload_length).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            payload_start,
            "payload end overflow",
        )
    })?;
    let block_checksum_offset = payload_end;
    let framed_end = block_checksum_offset.checked_add(8).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            payload_end,
            "block end overflow",
        )
    })?;
    if framed_end != trailer_offset {
        return Err(ParseError::new(
            artifact,
            ParseCheck::TrailingBytes,
            framed_end,
            format!("framed body ends at {framed_end}, trailer starts at {trailer_offset}"),
        ));
    }
    let payload = bytes.get(payload_start..payload_end).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Length,
            payload_start,
            "payload truncated",
        )
    })?;
    let block_checksum = raw_u64(artifact, bytes, block_checksum_offset)?;
    let computed_block = xxh3_64(payload);
    if block_checksum != computed_block {
        return Err(ParseError::new(
            artifact,
            ParseCheck::BlockChecksum,
            block_checksum_offset,
            format!("expected {block_checksum:#018x}, computed {computed_block:#018x}"),
        ));
    }
    Ok((payload, block_checksum, file_checksum))
}

/// Parses the complete current manifest framing and all variable-width bounds.
pub fn parse_manifest(artifact: &str, bytes: &[u8]) -> Result<ManifestArtifact, ParseError> {
    let (payload, block_checksum, file_checksum) =
        parse_single_block(artifact, bytes, MANIFEST_FAMILY, 2)?;
    let mut cursor = Cursor::new(artifact, payload);
    let generation = cursor.u64()?;
    let log_seq = cursor.u64()?;
    let segment_count = usize::try_from(cursor.u32()?).map_err(|_| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            cursor.offset,
            "segment count exceeds usize",
        )
    })?;
    let epoch_count = usize::try_from(cursor.u32()?).map_err(|_| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            cursor.offset,
            "epoch count exceeds usize",
        )
    })?;
    let schema_count = usize::try_from(cursor.u32()?).map_err(|_| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            cursor.offset,
            "schema count exceeds usize",
        )
    })?;
    if cursor.u32()? != 0 {
        return Err(ParseError::new(
            artifact,
            ParseCheck::Reserved,
            cursor.offset.saturating_sub(4),
            "manifest reserved word is nonzero",
        ));
    }
    parse_optional_identity(&mut cursor, 16)?;
    let mut segments = Vec::with_capacity(segment_count);
    for _ in 0..segment_count {
        let id = cursor.take(16)?.try_into().map_err(|_| {
            ParseError::new(artifact, ParseCheck::Length, cursor.offset, "segment id")
        })?;
        let rows = cursor.u32()?;
        let scheme = cursor.u16()?;
        if !matches!(scheme, 0 | 1 | 2 | 4) {
            return Err(ParseError::new(
                artifact,
                ParseCheck::Version,
                cursor.offset.saturating_sub(2),
                format!("unknown or retired quantization scheme {scheme}"),
            ));
        }
        if cursor.u16()? != 0 {
            return Err(ParseError::new(
                artifact,
                ParseCheck::Reserved,
                cursor.offset.saturating_sub(2),
                "segment reserved word is nonzero",
            ));
        }
        let dims = cursor.u32()?;
        let file_length = cursor.u64()?;
        parse_optional_identity(&mut cursor, 8)?;
        segments.push(ManifestSegmentFact {
            id,
            rows,
            scheme,
            dims,
            file_length,
        });
    }
    let non_segment_start = cursor.offset;
    for _ in 0..epoch_count {
        let _embedding_epoch = cursor.u64()?;
        let _tokenizer_epoch = cursor.u64()?;
        parse_tower(&mut cursor)?;
        parse_tower(&mut cursor)?;
        let _alignment_digest = cursor.length_prefixed()?;
    }
    for _ in 0..schema_count {
        let _id = cursor.u32()?;
        let _column_type = cursor.u16()?;
        let nullable_offset = cursor.offset;
        if !matches!(cursor.u16()?, 0 | 1) {
            return Err(ParseError::new(
                artifact,
                ParseCheck::Reserved,
                nullable_offset,
                "invalid nullable flag",
            ));
        }
        let name = cursor.length_prefixed()?;
        std::str::from_utf8(name).map_err(|error| {
            ParseError::new(
                artifact,
                ParseCheck::Bounds,
                cursor.offset.saturating_sub(name.len()),
                format!("schema name is not UTF-8: {error}"),
            )
        })?;
    }
    if cursor.offset != payload.len() {
        let magic_offset = cursor.offset;
        if cursor.take(4)? != b"TSR1" {
            return Err(ParseError::new(
                artifact,
                ParseCheck::TrailingBytes,
                magic_offset,
                "unknown manifest extension",
            ));
        }
        let count = usize::try_from(cursor.u32()?).map_err(|_| {
            ParseError::new(
                artifact,
                ParseCheck::Bounds,
                cursor.offset,
                "range count exceeds usize",
            )
        })?;
        if count != segment_count {
            return Err(ParseError::new(
                artifact,
                ParseCheck::Ordering,
                cursor.offset.saturating_sub(4),
                format!("range count {count}, segment count {segment_count}"),
            ));
        }
        for _ in 0..count {
            let tag_offset = cursor.offset;
            let tag = cursor.u8()?;
            cursor.zeroes(7)?;
            let min = cursor.u64()? as i64;
            let max = cursor.u64()? as i64;
            match tag {
                0 | 1 if min == 0 && max == 0 => {}
                2 if min <= max => {}
                _ => {
                    return Err(ParseError::new(
                        artifact,
                        ParseCheck::Bounds,
                        tag_offset,
                        format!("invalid clustering range tag={tag} min={min} max={max}"),
                    ));
                }
            }
        }
    }
    if cursor.offset != payload.len() {
        return Err(ParseError::new(
            artifact,
            ParseCheck::TrailingBytes,
            cursor.offset,
            format!(
                "{} bytes remain",
                payload.len().saturating_sub(cursor.offset)
            ),
        ));
    }
    let non_segment = payload.get(non_segment_start..).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            non_segment_start,
            "invalid opaque section",
        )
    })?;
    Ok(ManifestArtifact {
        generation,
        log_seq,
        segments,
        block_checksum,
        file_checksum,
        non_segment_payload_checksum: xxh3_64(non_segment),
    })
}

/// One literally decoded segment directory entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentRegionFact {
    pub kind: u16,
    pub version: u16,
    pub offset: u64,
    pub length: u64,
    pub checksum: u64,
}

/// Complete segment identity and directory facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentArtifact {
    pub fact: SegmentFact,
    pub regions: Vec<SegmentRegionFact>,
}

/// Parses and checksum-validates the complete current segment v1 artifact.
pub fn parse_segment(artifact: &str, bytes: &[u8]) -> Result<SegmentArtifact, ParseError> {
    let header = parse_header(artifact, bytes, SEGMENT_FAMILY, 1)?;
    if header.file_length != bytes.len() as u64 {
        return Err(ParseError::new(
            artifact,
            ParseCheck::FileLength,
            24,
            format!("declared {}, actual {}", header.file_length, bytes.len()),
        ));
    }
    let id = exact_bytes(artifact, bytes, 32)?;
    let rows = raw_u32(artifact, bytes, 48)?;
    let region_count = usize::from(raw_u16(artifact, bytes, 52)?);
    if raw_u16(artifact, bytes, 54)? != 0 || raw_u16(artifact, bytes, 58)? != 0 {
        return Err(ParseError::new(
            artifact,
            ParseCheck::Reserved,
            54,
            "segment prefix reserved word is nonzero",
        ));
    }
    let scheme = raw_u16(artifact, bytes, 56)?;
    if !matches!(scheme, 0 | 1 | 2 | 4) {
        return Err(ParseError::new(
            artifact,
            ParseCheck::Version,
            56,
            format!("unknown or retired quantization scheme {scheme}"),
        ));
    }
    let dims = raw_u32(artifact, bytes, 60)?;
    let directory_bytes = region_count.checked_mul(REGION_ENTRY_LEN).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            52,
            "region directory size overflow",
        )
    })?;
    let header_checksum_offset = 64_usize.checked_add(directory_bytes).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            64,
            "header checksum offset overflow",
        )
    })?;
    let expected_header_length = header_checksum_offset.checked_add(8).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            header_checksum_offset,
            "header end overflow",
        )
    })?;
    if header.header_length != expected_header_length as u64 {
        return Err(ParseError::new(
            artifact,
            ParseCheck::HeaderLength,
            16,
            format!(
                "declared {}, expected {expected_header_length}",
                header.header_length
            ),
        ));
    }
    let header_checksum = raw_u64(artifact, bytes, header_checksum_offset)?;
    let actual_header = xxh3_64(bytes.get(..header_checksum_offset).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            0,
            "invalid header checksum range",
        )
    })?);
    if header_checksum != actual_header {
        return Err(ParseError::new(
            artifact,
            ParseCheck::BlockChecksum,
            header_checksum_offset,
            format!("expected {header_checksum:#018x}, computed {actual_header:#018x}"),
        ));
    }
    let trailer_offset = bytes.len().checked_sub(FILE_TRAILER_LEN).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Length,
            bytes.len(),
            "missing file trailer",
        )
    })?;
    let whole_file_checksum = raw_u64(artifact, bytes, trailer_offset)?;
    let actual_file = xxh3_64(bytes.get(..trailer_offset).ok_or_else(|| {
        ParseError::new(
            artifact,
            ParseCheck::Bounds,
            0,
            "invalid file checksum range",
        )
    })?);
    if whole_file_checksum != actual_file {
        return Err(ParseError::new(
            artifact,
            ParseCheck::FileChecksum,
            trailer_offset,
            format!("expected {whole_file_checksum:#018x}, computed {actual_file:#018x}"),
        ));
    }
    let mut regions = Vec::with_capacity(region_count);
    let mut previous_end = header.header_length;
    for index in 0..region_count {
        let entry = 64 + index * REGION_ENTRY_LEN;
        let kind = raw_u16(artifact, bytes, entry)?;
        let version = raw_u16(artifact, bytes, entry + 2)?;
        if raw_u32(artifact, bytes, entry + 4)? != 0 {
            return Err(ParseError::new(
                artifact,
                ParseCheck::Reserved,
                entry + 4,
                "region reserved word is nonzero",
            ));
        }
        let offset = raw_u64(artifact, bytes, entry + 8)?;
        let length = raw_u64(artifact, bytes, entry + 16)?;
        let checksum = raw_u64(artifact, bytes, entry + 24)?;
        let end = offset.checked_add(length).ok_or_else(|| {
            ParseError::new(
                artifact,
                ParseCheck::Bounds,
                entry + 8,
                "region end overflow",
            )
        })?;
        if offset % REGION_ALIGNMENT != 0 || offset < previous_end || end > trailer_offset as u64 {
            return Err(ParseError::new(
                artifact,
                ParseCheck::Ordering,
                entry + 8,
                format!("region {kind} range {offset}..{end} after {previous_end}"),
            ));
        }
        let start = usize::try_from(offset).map_err(|_| {
            ParseError::new(
                artifact,
                ParseCheck::Bounds,
                entry + 8,
                "region offset exceeds usize",
            )
        })?;
        let finish = usize::try_from(end).map_err(|_| {
            ParseError::new(
                artifact,
                ParseCheck::Bounds,
                entry + 16,
                "region end exceeds usize",
            )
        })?;
        let region = bytes.get(start..finish).ok_or_else(|| {
            ParseError::new(
                artifact,
                ParseCheck::Length,
                start,
                "region bytes truncated",
            )
        })?;
        let actual = xxh3_64(region);
        if checksum != actual {
            return Err(ParseError::new(
                artifact,
                ParseCheck::BlockChecksum,
                entry + 24,
                format!("region {kind} expected {checksum:#018x}, computed {actual:#018x}"),
            ));
        }
        regions.push(SegmentRegionFact {
            kind,
            version,
            offset,
            length,
            checksum,
        });
        previous_end = end;
    }
    Ok(SegmentArtifact {
        fact: SegmentFact {
            id,
            rows,
            scheme,
            dims,
            file_length: header.file_length,
            header_checksum,
            whole_file_checksum,
        },
        regions,
    })
}

/// One primitive typed-column value in a seed-derived storage fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FixtureColumnValueV1 {
    I64(i64),
    F64Bits(u64),
    Bool(bool),
    Bytes(Vec<u8>),
}

/// One document description generated without production RNG or encoders.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageDocumentV1 {
    pub doc_id: u128,
    pub revision: u64,
    pub vector_bits: Vec<u32>,
    pub timestamp: Option<i64>,
    pub metadata: Option<Vec<u8>>,
    pub text: Option<String>,
    pub columns: Vec<(u32, FixtureColumnValueV1)>,
}

/// One canonical mutation group and its planned durable acknowledgement range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageMutationV1 {
    pub operation_id: [u8; 16],
    pub document_index: u32,
    pub canonical_payload_digest: [u8; 32],
    pub first_seq: u64,
    pub last_seq: u64,
    pub acknowledged: bool,
}

/// Primitive storage campaign fixture derived only from namespace and seed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageFixtureV1 {
    pub namespace: &'static str,
    pub seed: u64,
    pub durability: &'static str,
    pub commit_tier: &'static str,
    pub dimensions: u32,
    pub scheme: u16,
    pub documents: Vec<StorageDocumentV1>,
    pub mutations: Vec<StorageMutationV1>,
    pub old_generation: u64,
    pub planned_new_generation: u64,
    pub absorbed_through: u64,
    pub wal_mutation_offset: u64,
    pub segment_region_kind: u16,
    pub segment_chunk: u32,
    pub segment_byte: u32,
    pub omission_orphan: OrphanKind,
    pub omission_is_delete: bool,
    pub eligible_orphans: Vec<String>,
    pub preserved_files: Vec<String>,
    pub clean_fault_pair_id: [u8; 16],
    pub operation_fixture_id: [u8; 16],
}

#[derive(Clone, Copy)]
struct FixtureGenerator {
    state: u64,
}

impl FixtureGenerator {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x5354_4f52_4147_4531,
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut output = [0_u8; N];
        for chunk in output.chunks_mut(8) {
            let next = self.next().to_le_bytes();
            let length = chunk.len();
            chunk.copy_from_slice(&next[..length]);
        }
        output
    }
}

fn fixture_digest(domain: u64, bytes: &[u8]) -> [u8; 32] {
    let mut output = [0_u8; 32];
    for index in 0..4 {
        let mut framed = Vec::with_capacity(bytes.len().saturating_add(16));
        framed.extend_from_slice(&domain.to_le_bytes());
        framed.extend_from_slice(&(index as u64).to_le_bytes());
        framed.extend_from_slice(bytes);
        let start = index * 8;
        output[start..start + 8].copy_from_slice(&xxh3_64(&framed).to_le_bytes());
    }
    output
}

fn append_optional_bytes(output: &mut Vec<u8>, value: Option<&[u8]>) {
    match value {
        Some(bytes) => {
            output.push(1);
            output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            output.extend_from_slice(bytes);
        }
        None => {
            output.push(0);
            output.extend_from_slice(&0_u32.to_le_bytes());
        }
    }
}

fn canonical_document_bytes(document: &StorageDocumentV1) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&document.doc_id.to_le_bytes());
    output.extend_from_slice(&document.revision.to_le_bytes());
    output.extend_from_slice(&(document.vector_bits.len() as u32).to_le_bytes());
    for bits in &document.vector_bits {
        output.extend_from_slice(&bits.to_le_bytes());
    }
    match document.timestamp {
        Some(timestamp) => {
            output.push(1);
            output.extend_from_slice(&timestamp.to_le_bytes());
        }
        None => {
            output.push(0);
            output.extend_from_slice(&0_i64.to_le_bytes());
        }
    }
    append_optional_bytes(&mut output, document.metadata.as_deref());
    append_optional_bytes(&mut output, document.text.as_deref().map(str::as_bytes));
    output.extend_from_slice(&(document.columns.len() as u32).to_le_bytes());
    for (column, value) in &document.columns {
        output.extend_from_slice(&column.to_le_bytes());
        match value {
            FixtureColumnValueV1::I64(value) => {
                output.push(1);
                output.extend_from_slice(&value.to_le_bytes());
            }
            FixtureColumnValueV1::F64Bits(value) => {
                output.push(2);
                output.extend_from_slice(&value.to_le_bytes());
            }
            FixtureColumnValueV1::Bool(value) => {
                output.push(3);
                output.push(u8::from(*value));
            }
            FixtureColumnValueV1::Bytes(value) => {
                output.push(4);
                output.extend_from_slice(&(value.len() as u32).to_le_bytes());
                output.extend_from_slice(value);
            }
        }
    }
    output
}

fn append_length_prefixed_fixture(bytes: &[u8], output: &mut Vec<u8>) -> Result<(), String> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| "fixture length-prefixed value exceeds u32".to_owned())?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

/// Literally encodes the frozen upsert-v2 payload for one primitive mutation.
/// This intentionally duplicates the persisted contract without importing a
/// production encoder or operation-id constant.
pub fn expected_wal_record(
    fixture: &StorageFixtureV1,
    mutation: &StorageMutationV1,
) -> Result<WalRecordFact, String> {
    let document = fixture
        .documents
        .get(mutation.document_index as usize)
        .ok_or_else(|| "fixture WAL mutation names an absent document".to_owned())?;
    let mut presence = 1_u32;
    if document.text.is_some() {
        presence |= 1 << 1;
    }
    if document.timestamp.is_some() {
        presence |= 1 << 2;
    }
    if document.metadata.is_some() {
        presence |= 1 << 3;
    }
    if !document.columns.is_empty() {
        presence |= 1 << 4;
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&presence.to_le_bytes());
    payload.extend_from_slice(&document.doc_id.to_le_bytes());
    payload.extend_from_slice(&document.revision.to_le_bytes());
    let dimensions = u32::try_from(document.vector_bits.len())
        .map_err(|_| "fixture vector dimensions exceed u32".to_owned())?;
    payload.extend_from_slice(&dimensions.to_le_bytes());
    for bits in &document.vector_bits {
        payload.extend_from_slice(&bits.to_le_bytes());
    }
    if let Some(text) = document.text.as_ref() {
        append_length_prefixed_fixture(text.as_bytes(), &mut payload)?;
    }
    if let Some(timestamp) = document.timestamp {
        payload.extend_from_slice(&timestamp.to_le_bytes());
    }
    if let Some(metadata) = document.metadata.as_ref() {
        append_length_prefixed_fixture(metadata, &mut payload)?;
    }
    if !document.columns.is_empty() {
        let count = u32::try_from(document.columns.len())
            .map_err(|_| "fixture typed-column count exceeds u32".to_owned())?;
        payload.extend_from_slice(&count.to_le_bytes());
        for (column, value) in &document.columns {
            payload.extend_from_slice(&column.to_le_bytes());
            let (kind, body) = match value {
                FixtureColumnValueV1::I64(value) => (2_u8, value.to_le_bytes().to_vec()),
                FixtureColumnValueV1::F64Bits(value) => (3_u8, value.to_le_bytes().to_vec()),
                FixtureColumnValueV1::Bool(value) => (4_u8, vec![u8::from(*value)]),
                FixtureColumnValueV1::Bytes(_) => {
                    return Err("byte fixture value has no frozen typed-column kind".to_owned());
                }
            };
            payload.push(kind);
            payload.extend_from_slice(&[0_u8; 3]);
            append_length_prefixed_fixture(&body, &mut payload)?;
        }
    }
    Ok(WalRecordFact {
        seq: mutation.first_seq,
        op: 7,
        payload,
    })
}

/// One fixture-authored mutation coordinate in the frozen WAL layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalMutationKind {
    HeaderTruncation,
    BodyTruncation,
    ChecksumFlip,
}

/// Resolves the primitive fixture selector to an exact absolute WAL offset.
///
/// The calculation uses only fixture-owned payload bytes and literal frozen
/// frame widths. No observed WAL length or production encoder participates.
pub fn planned_wal_mutation_offset(
    fixture: &StorageFixtureV1,
    kind: WalMutationKind,
) -> Result<u64, String> {
    if kind == WalMutationKind::HeaderTruncation {
        if fixture.wal_mutation_offset >= WAL_HEADER_LEN as u64 {
            return Err("fixture WAL header truncation is outside the header".to_owned());
        }
        return Ok(fixture.wal_mutation_offset);
    }
    let mut record_start = WAL_HEADER_LEN as u64;
    let mut final_record = None;
    for mutation in &fixture.mutations {
        let record = expected_wal_record(fixture, mutation)?;
        let payload_length = u64::try_from(record.payload.len())
            .map_err(|_| "fixture WAL payload exceeds u64".to_owned())?;
        final_record = Some((record_start, payload_length));
        record_start = record_start
            .checked_add(14)
            .and_then(|offset| offset.checked_add(payload_length))
            .and_then(|offset| offset.checked_add(8))
            .ok_or_else(|| "fixture WAL record end overflowed".to_owned())?;
    }
    let (record_start, payload_length) =
        final_record.ok_or_else(|| "fixture has no WAL record to mutate".to_owned())?;
    match kind {
        WalMutationKind::HeaderTruncation => Err("unreachable header mutation".to_owned()),
        WalMutationKind::BodyTruncation => {
            if payload_length == 0 {
                return Err("fixture final WAL payload is empty".to_owned());
            }
            record_start
                .checked_add(14)
                .and_then(|offset| offset.checked_add(fixture.wal_mutation_offset % payload_length))
                .ok_or_else(|| "fixture WAL body mutation offset overflowed".to_owned())
        }
        WalMutationKind::ChecksumFlip => record_start
            .checked_add(14)
            .and_then(|offset| offset.checked_add(payload_length))
            .and_then(|offset| offset.checked_add(fixture.wal_mutation_offset % 8))
            .ok_or_else(|| "fixture WAL checksum mutation offset overflowed".to_owned()),
    }
}

impl StorageFixtureV1 {
    /// Derives the complete primitive fixture deterministically from one seed.
    #[must_use]
    pub fn derive(seed: u64) -> Self {
        let mut generator = FixtureGenerator::new(seed);
        let dimensions = 2 + (generator.next() % 7) as u32;
        let scheme = 4;
        let document_count = 3 + (generator.next() % 3) as usize;
        let mut documents = Vec::with_capacity(document_count);
        let mut mutations = Vec::with_capacity(document_count);
        for index in 0..document_count {
            let high = u128::from(generator.next()) << 64;
            let low = u128::from(generator.next());
            let doc_id = high | low;
            let revision = 1 + generator.next() % 4;
            let vector_bits = (0..dimensions)
                .map(|_| {
                    let signed = (generator.next() % 2001) as i32 - 1000;
                    ((signed as f32) / 257.0).to_bits()
                })
                .collect::<Vec<_>>();
            let timestamp =
                (generator.next() & 1 != 0).then(|| (generator.next() % 20_001) as i64 - 10_000);
            let metadata = (generator.next() & 1 != 0).then(|| generator.bytes::<12>().to_vec());
            let text = (generator.next() & 1 != 0)
                .then(|| format!("storage-{seed}-{index}-{:016x}", generator.next()));
            let columns = vec![
                (1, FixtureColumnValueV1::I64(generator.next() as i64)),
                (2, FixtureColumnValueV1::Bool(generator.next() & 1 != 0)),
            ];
            let document = StorageDocumentV1 {
                doc_id,
                revision,
                vector_bits,
                timestamp,
                metadata,
                text,
                columns,
            };
            let canonical = canonical_document_bytes(&document);
            mutations.push(StorageMutationV1 {
                operation_id: generator.bytes(),
                document_index: index as u32,
                canonical_payload_digest: fixture_digest(0x4d55_5441_5449_4f4e, &canonical),
                first_seq: index as u64 + 1,
                last_seq: index as u64 + 1,
                acknowledged: index + 1 != document_count,
            });
            documents.push(document);
        }
        let absorbed_through = document_count.saturating_sub(1) as u64;
        let old_generation = absorbed_through.saturating_add(2);
        Self {
            namespace: "adversarial::storage-durability::v1",
            seed,
            durability: "Durable",
            commit_tier: "Ordered",
            dimensions,
            scheme,
            documents,
            mutations,
            old_generation,
            planned_new_generation: old_generation.saturating_add(1),
            absorbed_through,
            wal_mutation_offset: 20 + generator.next() % 20,
            // Exact public search is contractually required to validate the
            // rescore chunk it reads; this fixture therefore authors that
            // precise lazy-validation region rather than letting the adapter
            // replace a seed-selected, possibly unread region.
            segment_region_kind: 5,
            segment_chunk: 0,
            segment_byte: (generator.next() % 2) as u32,
            omission_orphan: match generator.next() % 3 {
                0 => OrphanKind::FinalSegment,
                1 => OrphanKind::SegmentTemporary,
                _ => OrphanKind::ManifestTemporary,
            },
            omission_is_delete: generator.next() & 1 != 0,
            eligible_orphans: vec![
                format!("segment-{:032x}.zseg", generator.next() as u128),
                format!(".segment-{:032x}.zseg.tmp", generator.next() as u128),
                ".manifest.ze.tmp".to_owned(),
            ],
            preserved_files: vec![
                "manifest.ze".to_owned(),
                "wal.ze".to_owned(),
                "writer.lock".to_owned(),
                "purge.ze".to_owned(),
                ".purge.ze.tmp".to_owned(),
                ".wal.ze.purge.tmp".to_owned(),
                "owner-sentinel.bin".to_owned(),
            ],
            clean_fault_pair_id: generator.bytes(),
            operation_fixture_id: generator.bytes(),
        }
    }
}

/// Complete logical and persisted snapshot identity used by I15.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotState {
    pub generation: u64,
    pub manifest_log_seq: u64,
    pub segments: Vec<SegmentFact>,
    pub live_versions: Vec<(u128, u64)>,
    pub absorbed_through: u64,
}

/// Public Store projection of a segment, excluding parser-only checksums.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PublishedSegmentFact {
    pub id: [u8; 16],
    pub rows: u32,
    pub scheme: u16,
    pub dims: u32,
    pub file_length: u64,
}

/// Public Store projection of one published snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedSnapshotState {
    pub generation: u64,
    pub segments: Vec<PublishedSegmentFact>,
    pub live_versions: Vec<(u128, u64)>,
    pub absorbed_through: u64,
}

/// Fixture-derived segment identity and independently parsed integrity facts
/// used by the I15 expected model.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PublicationSegmentModel {
    pub id: [u8; 16],
    pub rows: u32,
    pub scheme: u16,
    pub dims: u32,
    pub file_length: u64,
    pub header_checksum: u64,
    pub whole_file_checksum: u64,
}

/// Complete fixture-derived state at one legal publication side.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationModelState {
    pub generation: u64,
    pub manifest_log_seq: u64,
    pub segments: Vec<PublicationSegmentModel>,
    pub live_versions: Vec<(u128, u64)>,
    pub absorbed_through: u64,
}

fn published_projection(value: &SnapshotState) -> PublishedSnapshotState {
    PublishedSnapshotState {
        generation: value.generation,
        segments: value
            .segments
            .iter()
            .map(|segment| PublishedSegmentFact {
                id: segment.id,
                rows: segment.rows,
                scheme: segment.scheme,
                dims: segment.dims,
                file_length: segment.file_length,
            })
            .collect(),
        live_versions: value.live_versions.clone(),
        absorbed_through: value.absorbed_through,
    }
}

fn publication_model_projection(value: &SnapshotState) -> PublicationModelState {
    PublicationModelState {
        generation: value.generation,
        manifest_log_seq: value.manifest_log_seq,
        segments: value
            .segments
            .iter()
            .map(|segment| PublicationSegmentModel {
                id: segment.id,
                rows: segment.rows,
                scheme: segment.scheme,
                dims: segment.dims,
                file_length: segment.file_length,
                header_checksum: segment.header_checksum,
                whole_file_checksum: segment.whole_file_checksum,
            })
            .collect(),
        live_versions: value.live_versions.clone(),
        absorbed_through: value.absorbed_through,
    }
}

fn planned_segment_id(generation: u64, absorbed_through: u64) -> [u8; 16] {
    let mut id = [0_u8; 16];
    id[..8].copy_from_slice(&generation.to_be_bytes());
    id[8..].copy_from_slice(&absorbed_through.to_be_bytes());
    id
}

/// Derives the publication segment identity from the primitive generation and
/// absorption plan, without consulting a product snapshot or manifest.
pub fn planned_publication_segment_id(fixture: &StorageFixtureV1) -> Result<[u8; 16], String> {
    let absorbed_through = u64::try_from(fixture.documents.len())
        .map_err(|_| "publication document count exceeds u64".to_owned())?;
    Ok(planned_segment_id(
        fixture.planned_new_generation,
        absorbed_through,
    ))
}

fn checked_publication_segment(
    controls: &[SegmentFact],
    id: [u8; 16],
    rows: u32,
    scheme: u16,
    dims: u32,
) -> Result<PublicationSegmentModel, String> {
    let segment = controls
        .iter()
        .find(|segment| segment.id == id)
        .ok_or_else(|| "independent publication control lacks planned segment".to_owned())?;
    if segment.rows != rows || segment.scheme != scheme || segment.dims != dims {
        return Err("independent publication control differs from primitive shape".to_owned());
    }
    Ok(PublicationSegmentModel {
        id,
        rows,
        scheme,
        dims,
        file_length: segment.file_length,
        header_checksum: segment.header_checksum,
        whole_file_checksum: segment.whole_file_checksum,
    })
}

/// State selected at the manifest publication checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationClass {
    Old,
    New,
    Hybrid,
    Invalid,
}

/// Independent I15 expected facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationExpected {
    pub old: PublicationModelState,
    pub new: PublicationModelState,
    pub legal: Vec<PublicationClass>,
}

impl PublicationExpected {
    /// Builds both legal publication states from the primitive fixture and
    /// checksum-valid segment facts produced by this oracle's literal parser.
    pub fn from_fixture(
        fixture: &StorageFixtureV1,
        old_segment_controls: &[SegmentFact],
        new_segment_controls: &[SegmentFact],
        legal: Vec<PublicationClass>,
    ) -> Result<Self, String> {
        let document_count = fixture.documents.len();
        let old_rows = document_count
            .checked_sub(1)
            .ok_or_else(|| "publication fixture has no documents".to_owned())?;
        let old_rows_u32 = u32::try_from(old_rows)
            .map_err(|_| "publication old row count exceeds u32".to_owned())?;
        let new_rows = document_count
            .checked_sub(old_rows)
            .ok_or_else(|| "publication new row count underflowed".to_owned())?;
        let new_rows_u32 = u32::try_from(new_rows)
            .map_err(|_| "publication new row count exceeds u32".to_owned())?;
        let old_log_seq = u64::try_from(old_rows)
            .map_err(|_| "publication old sequence exceeds u64".to_owned())?;
        let new_log_seq = u64::try_from(document_count)
            .map_err(|_| "publication new sequence exceeds u64".to_owned())?;
        if fixture.absorbed_through != old_log_seq {
            return Err("fixture publication absorption boundary differs".to_owned());
        }
        let expected_old_generation = old_log_seq
            .checked_add(2)
            .ok_or_else(|| "publication old generation overflowed".to_owned())?;
        let expected_new_generation = expected_old_generation
            .checked_add(1)
            .ok_or_else(|| "publication new generation overflowed".to_owned())?;
        if fixture.old_generation != expected_old_generation
            || fixture.planned_new_generation != expected_new_generation
        {
            return Err(
                "fixture publication generations differ from its primitive plan".to_owned(),
            );
        }
        let live_versions = fixture
            .documents
            .iter()
            .map(|document| (document.doc_id, document.revision))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect::<Vec<_>>();
        if live_versions.len() != document_count {
            return Err("publication fixture contains duplicate document ids".to_owned());
        }
        let old_segment_id = planned_segment_id(old_log_seq + 1, old_log_seq);
        let new_segment_id = planned_publication_segment_id(fixture)?;
        let old_segment = checked_publication_segment(
            old_segment_controls,
            old_segment_id,
            old_rows_u32,
            fixture.scheme,
            fixture.dimensions,
        )?;
        let old_segment_after = checked_publication_segment(
            new_segment_controls,
            old_segment_id,
            old_rows_u32,
            fixture.scheme,
            fixture.dimensions,
        )?;
        if old_segment_after != old_segment {
            return Err("clean publication changed immutable old segment bytes".to_owned());
        }
        let new_segment = checked_publication_segment(
            new_segment_controls,
            new_segment_id,
            new_rows_u32,
            fixture.scheme,
            fixture.dimensions,
        )?;
        Ok(Self {
            old: PublicationModelState {
                generation: expected_old_generation,
                manifest_log_seq: old_log_seq,
                segments: vec![old_segment.clone()],
                live_versions: live_versions.clone(),
                absorbed_through: old_log_seq,
            },
            new: PublicationModelState {
                generation: expected_new_generation,
                manifest_log_seq: new_log_seq,
                segments: vec![old_segment, new_segment],
                live_versions,
                absorbed_through: new_log_seq,
            },
            legal,
        })
    }

    /// Classifies one independently parsed observation against the fixture model.
    #[must_use]
    pub fn classify(&self, observed: &SnapshotState) -> PublicationClass {
        let projection = publication_model_projection(observed);
        let old_matches = projection == self.old;
        let new_matches = projection == self.new;
        match (old_matches, new_matches) {
            (true, false) => PublicationClass::Old,
            (false, true) => PublicationClass::New,
            (false, false) => PublicationClass::Hybrid,
            (true, true) => PublicationClass::Invalid,
        }
    }
}

/// Production/raw I15 observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationObserved {
    pub class: PublicationClass,
    pub raw: SnapshotState,
    pub public: PublishedSnapshotState,
    pub referenced_segments_complete: bool,
    pub trusted_wal_end: u64,
}

/// Checks that publication is exactly one complete legal state.
pub fn check_i15(
    expected: &PublicationExpected,
    observed: &PublicationObserved,
) -> Result<(), CheckFailure> {
    if !observed.referenced_segments_complete {
        return Err(CheckFailure::new(
            I15_CHECKER_ID,
            "referenced segment missing",
        ));
    }
    if observed.raw.manifest_log_seq > observed.trusted_wal_end {
        return Err(CheckFailure::new(
            I15_CHECKER_ID,
            "manifest ahead of trusted WAL",
        ));
    }
    if published_projection(&observed.raw) != observed.public {
        return Err(CheckFailure::new(
            I15_CHECKER_ID,
            "raw and public states disagree",
        ));
    }
    if observed.class == PublicationClass::Hybrid {
        return Err(CheckFailure::new(
            I15_CHECKER_ID,
            "observed hybrid publication",
        ));
    }
    if !expected.legal.contains(&observed.class) {
        return Err(CheckFailure::new(
            I15_CHECKER_ID,
            "observed publication class is illegal",
        ));
    }
    let projected = publication_model_projection(&observed.raw);
    let old_matches = projected == expected.old;
    let new_matches = projected == expected.new;
    match u8::from(old_matches).checked_add(u8::from(new_matches)) {
        Some(2) => {
            return Err(CheckFailure::new(
                I15_CHECKER_ID,
                "publication matched multiple canonical states",
            ));
        }
        Some(0) | None => {
            return Err(CheckFailure::new(
                I15_CHECKER_ID,
                "classified state bytes do not match model",
            ));
        }
        Some(1) => {}
        Some(_) => {
            return Err(CheckFailure::new(
                I15_CHECKER_ID,
                "publication match cardinality is invalid",
            ));
        }
    }
    let matching = match observed.class {
        PublicationClass::Old if old_matches => &expected.old,
        PublicationClass::New if new_matches => &expected.new,
        PublicationClass::Old | PublicationClass::New => {
            return Err(CheckFailure::new(
                I15_CHECKER_ID,
                "observed publication classification differs",
            ));
        }
        PublicationClass::Hybrid | PublicationClass::Invalid => {
            return Err(CheckFailure::new(
                I15_CHECKER_ID,
                "observed publication is invalid",
            ));
        }
    };
    if &projected != matching {
        return Err(CheckFailure::new(
            I15_CHECKER_ID,
            "classified state bytes do not match model",
        ));
    }
    Ok(())
}

/// One independently parsed WAL record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalRecordFact {
    pub seq: u64,
    pub op: u16,
    pub payload: Vec<u8>,
}

/// Exact WAL-header failure derived from raw bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalHeaderFailure {
    Missing,
    Truncated {
        needed: u64,
        available: u64,
    },
    WrongMagic {
        expected: [u8; 8],
        actual: [u8; 8],
    },
    WrongFamily {
        expected: u16,
        actual: u16,
    },
    UnsupportedVersion {
        family: u16,
        version: u16,
        minimum: u16,
        maximum: u16,
    },
    NonZeroFlags {
        actual: u32,
    },
    InvalidHeaderLength {
        expected: u64,
        actual: u64,
    },
    NonZeroFileLength {
        actual: u64,
    },
}

/// Exact WAL-record failure derived from raw bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalRecordFailure {
    HeaderTruncated {
        needed: u64,
        available: u64,
    },
    BodyTruncated {
        payload_length: u32,
        needed: u64,
        available: u64,
    },
    ChecksumMismatch {
        expected: u64,
        actual: u64,
        record_length: u64,
    },
    Sequence {
        expected: u64,
        actual: u64,
    },
    SequenceOverflow {
        previous: u64,
    },
}

/// Exact replay boundary classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalTerminator {
    CleanEnd,
    InvalidHeader {
        artifact: String,
        reason: WalHeaderFailure,
    },
    CorruptAt {
        artifact: String,
        offset: u64,
        location: CorruptionLocation,
        reason: WalRecordFailure,
    },
}

/// Complete independent WAL parse, including a typed failure boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalArtifact {
    pub first_seq: u64,
    pub records: Vec<WalRecordFact>,
    pub terminator: WalTerminator,
}

fn wal_header_failure(bytes: &[u8]) -> Option<WalHeaderFailure> {
    if bytes.is_empty() {
        return Some(WalHeaderFailure::Missing);
    }
    if bytes.len() < WAL_HEADER_LEN {
        return Some(WalHeaderFailure::Truncated {
            needed: WAL_HEADER_LEN as u64,
            available: bytes.len() as u64,
        });
    }
    let magic = bytes.get(..8)?.try_into().ok()?;
    if magic != FILE_MAGIC {
        return Some(WalHeaderFailure::WrongMagic {
            expected: FILE_MAGIC,
            actual: magic,
        });
    }
    let family = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?);
    if family != WAL_FAMILY {
        return Some(WalHeaderFailure::WrongFamily {
            expected: WAL_FAMILY,
            actual: family,
        });
    }
    let version = u16::from_le_bytes(bytes.get(10..12)?.try_into().ok()?);
    if version != 1 {
        return Some(WalHeaderFailure::UnsupportedVersion {
            family,
            version,
            minimum: 1,
            maximum: 1,
        });
    }
    let flags = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?);
    if flags != 0 {
        return Some(WalHeaderFailure::NonZeroFlags { actual: flags });
    }
    let header_length = u64::from_le_bytes(bytes.get(16..24)?.try_into().ok()?);
    if header_length != WAL_HEADER_LEN as u64 {
        return Some(WalHeaderFailure::InvalidHeaderLength {
            expected: WAL_HEADER_LEN as u64,
            actual: header_length,
        });
    }
    let file_length = u64::from_le_bytes(bytes.get(24..32)?.try_into().ok()?);
    (file_length != 0).then_some(WalHeaderFailure::NonZeroFileLength {
        actual: file_length,
    })
}

fn record_failure_at(bytes: &[u8], offset: usize) -> Option<WalRecordFailure> {
    let remaining = bytes.get(offset..)?;
    if remaining.len() < 14 {
        return Some(WalRecordFailure::HeaderTruncated {
            needed: 14,
            available: remaining.len() as u64,
        });
    }
    let payload_length = u32::from_le_bytes(remaining.get(..4)?.try_into().ok()?);
    let needed = 22_usize.checked_add(payload_length as usize)?;
    if remaining.len() < needed {
        return Some(WalRecordFailure::BodyTruncated {
            payload_length,
            needed: needed as u64,
            available: remaining.len() as u64,
        });
    }
    let checksum_offset = 14_usize.checked_add(payload_length as usize)?;
    let expected = u64::from_le_bytes(
        remaining
            .get(checksum_offset..checksum_offset.checked_add(8)?)?
            .try_into()
            .ok()?,
    );
    let actual = xxh3_64(remaining.get(..checksum_offset)?);
    (expected != actual).then_some(WalRecordFailure::ChecksumMismatch {
        expected,
        actual,
        record_length: needed as u64,
    })
}

fn corruption_location(
    bytes: &[u8],
    offset: usize,
    failure: &WalRecordFailure,
) -> CorruptionLocation {
    let WalRecordFailure::ChecksumMismatch { record_length, .. } = failure else {
        return CorruptionLocation::Tail;
    };
    let Ok(length) = usize::try_from(*record_length) else {
        return CorruptionLocation::Tail;
    };
    let Some(successor) = offset.checked_add(length) else {
        return CorruptionLocation::Tail;
    };
    if successor < bytes.len() && record_failure_at(bytes, successor).is_none() {
        CorruptionLocation::Middle
    } else {
        CorruptionLocation::Tail
    }
}

/// Parses the frozen WAL header and every checksum-valid, strictly consecutive record.
pub fn parse_wal(artifact: &str, bytes: &[u8]) -> Result<WalArtifact, ParseError> {
    if let Some(reason) = wal_header_failure(bytes) {
        return Ok(WalArtifact {
            first_seq: 0,
            records: Vec::new(),
            terminator: WalTerminator::InvalidHeader {
                artifact: artifact.to_owned(),
                reason,
            },
        });
    }
    let first_seq = raw_u64(artifact, bytes, 32)?;
    let mut expected_seq = first_seq;
    let mut offset = WAL_HEADER_LEN;
    let mut records = Vec::new();
    while offset < bytes.len() {
        if let Some(reason) = record_failure_at(bytes, offset) {
            return Ok(WalArtifact {
                first_seq,
                records,
                terminator: WalTerminator::CorruptAt {
                    artifact: artifact.to_owned(),
                    offset: offset as u64,
                    location: corruption_location(bytes, offset, &reason),
                    reason,
                },
            });
        }
        let payload_length = raw_u32(artifact, bytes, offset)? as usize;
        let sequence = raw_u64(artifact, bytes, offset + 4)?;
        if sequence != expected_seq {
            return Ok(WalArtifact {
                first_seq,
                records,
                terminator: WalTerminator::CorruptAt {
                    artifact: artifact.to_owned(),
                    offset: offset as u64,
                    location: CorruptionLocation::Tail,
                    reason: WalRecordFailure::Sequence {
                        expected: expected_seq,
                        actual: sequence,
                    },
                },
            });
        }
        let op = raw_u16(artifact, bytes, offset + 12)?;
        let payload_start = offset + 14;
        let payload_end = payload_start.checked_add(payload_length).ok_or_else(|| {
            ParseError::new(
                artifact,
                ParseCheck::Bounds,
                offset,
                "WAL payload end overflow",
            )
        })?;
        let payload = bytes.get(payload_start..payload_end).ok_or_else(|| {
            ParseError::new(
                artifact,
                ParseCheck::Length,
                payload_start,
                "WAL payload truncated",
            )
        })?;
        records.push(WalRecordFact {
            seq: sequence,
            op,
            payload: payload.to_vec(),
        });
        offset = payload_end.checked_add(8).ok_or_else(|| {
            ParseError::new(
                artifact,
                ParseCheck::Bounds,
                payload_end,
                "WAL record end overflow",
            )
        })?;
        expected_seq = match expected_seq.checked_add(1) {
            Some(next) => next,
            None if offset == bytes.len() => expected_seq,
            None => {
                return Ok(WalArtifact {
                    first_seq,
                    records,
                    terminator: WalTerminator::CorruptAt {
                        artifact: artifact.to_owned(),
                        offset: offset as u64,
                        location: CorruptionLocation::Tail,
                        reason: WalRecordFailure::SequenceOverflow {
                            previous: expected_seq,
                        },
                    },
                });
            }
        };
    }
    Ok(WalArtifact {
        first_seq,
        records,
        terminator: WalTerminator::CleanEnd,
    })
}

/// Position of a WAL record failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorruptionLocation {
    Tail,
    Middle,
}

/// Public-open result paired with raw WAL observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalPublicOutcome {
    Opened { live_versions: Vec<(u128, u64)> },
    Refused { terminator: WalTerminator },
}

/// Exact raw/public facts captured after one durable acknowledgement returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalAckBoundary {
    pub records: Vec<WalRecordFact>,
    pub live_versions: Vec<(u128, u64)>,
}

/// Independent I16 expected facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalPrefixExpected {
    pub first_seq: u64,
    pub acknowledged: Vec<WalRecordFact>,
    pub optional_unacknowledged_tail: Vec<WalRecordFact>,
    pub terminator: WalTerminator,
    pub live_versions: Vec<(u128, u64)>,
    pub ack_boundaries: Vec<WalAckBoundary>,
}

/// Production/raw I16 observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalPrefixObserved {
    pub first_seq: u64,
    pub records: Vec<WalRecordFact>,
    pub terminator: WalTerminator,
    pub clean_public: WalPublicOutcome,
    pub public: WalPublicOutcome,
    pub reopened_ack_boundaries: Vec<WalAckBoundary>,
}

/// Checks acknowledgement coverage, strict sequence order, and refusal truth.
pub fn check_i16(
    expected: &WalPrefixExpected,
    observed: &WalPrefixObserved,
) -> Result<(), CheckFailure> {
    match &observed.clean_public {
        WalPublicOutcome::Opened { live_versions } if live_versions == &expected.live_versions => {}
        WalPublicOutcome::Opened { .. } => {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "same-seed clean logical versions differ",
            ));
        }
        WalPublicOutcome::Refused { .. } => {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "same-seed clean WAL was refused",
            ));
        }
    }
    if observed.reopened_ack_boundaries.len() != expected.ack_boundaries.len() {
        return Err(CheckFailure::new(
            I16_CHECKER_ID,
            "acknowledgement boundary reopen count differs",
        ));
    }
    if observed.reopened_ack_boundaries != expected.ack_boundaries {
        return Err(CheckFailure::new(
            I16_CHECKER_ID,
            "acknowledgement boundary reopen differs",
        ));
    }
    if observed.first_seq != expected.first_seq {
        return Err(CheckFailure::new(I16_CHECKER_ID, "first sequence differs"));
    }
    for (index, record) in observed.records.iter().enumerate() {
        let Ok(index) = u64::try_from(index) else {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "record sequence index overflowed",
            ));
        };
        let Some(expected_seq) = observed.first_seq.checked_add(index) else {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "record sequence overflowed",
            ));
        };
        if record.seq != expected_seq {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "record sequence is not a prefix",
            ));
        }
    }
    match &expected.terminator {
        WalTerminator::CleanEnd => {
            if observed.records.len() < expected.acknowledged.len()
                || observed.records.get(..expected.acknowledged.len())
                    != Some(expected.acknowledged.as_slice())
            {
                return Err(CheckFailure::new(
                    I16_CHECKER_ID,
                    "acknowledged record missing or changed",
                ));
            }
            let observed_tail = observed
                .records
                .get(expected.acknowledged.len()..)
                .unwrap_or_default();
            if !expected
                .optional_unacknowledged_tail
                .starts_with(observed_tail)
            {
                return Err(CheckFailure::new(
                    I16_CHECKER_ID,
                    "unacknowledged records are not a legal tail",
                ));
            }
        }
        WalTerminator::InvalidHeader { .. } => {
            if !observed.records.is_empty() {
                return Err(CheckFailure::new(
                    I16_CHECKER_ID,
                    "invalid WAL header exposed trusted records",
                ));
            }
        }
        WalTerminator::CorruptAt { .. } => {
            if observed.records != expected.acknowledged {
                return Err(CheckFailure::new(
                    I16_CHECKER_ID,
                    "trusted records before corrupt tail differ from acknowledged model",
                ));
            }
        }
    }
    if observed.terminator != expected.terminator {
        return Err(CheckFailure::new(I16_CHECKER_ID, "WAL terminator differs"));
    }
    match (&expected.terminator, &observed.public) {
        (WalTerminator::CleanEnd, WalPublicOutcome::Opened { live_versions })
            if live_versions == &expected.live_versions => {}
        (WalTerminator::CleanEnd, WalPublicOutcome::Opened { .. }) => {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "reopened logical versions differ",
            ));
        }
        (WalTerminator::CleanEnd, WalPublicOutcome::Refused { .. }) => {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "clean WAL prefix was refused",
            ));
        }
        (terminator, WalPublicOutcome::Refused { terminator: public }) if terminator == public => {}
        (_, WalPublicOutcome::Refused { .. }) => {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "public WAL refusal differs",
            ));
        }
        (_, WalPublicOutcome::Opened { .. }) => {
            return Err(CheckFailure::new(
                I16_CHECKER_ID,
                "damaged WAL was accepted",
            ));
        }
    }
    Ok(())
}

/// Independent I17 expected facts for one retry leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryExpected {
    pub canonical_request_digest: [u8; 32],
    pub version: (u128, u64),
    pub original_seq: u64,
    pub generation_after_first: u64,
    pub canonical_record_occurrences: u32,
    pub ambiguous_first: RetryPublicResult,
}

impl RetryExpected {
    /// Derives the retry identity, first sequence, generation, and ambiguous
    /// result from the primitive empty-store operation grammar.
    pub fn from_fixture(
        fixture: &StorageFixtureV1,
        post_commit_error: bool,
    ) -> Result<Self, String> {
        let target = fixture
            .documents
            .last()
            .ok_or_else(|| "retry fixture has no target document".to_owned())?;
        let mutation = fixture
            .mutations
            .last()
            .ok_or_else(|| "retry fixture has no target mutation".to_owned())?;
        let original_seq = 1;
        let generation_after_first = 1;
        Ok(Self {
            canonical_request_digest: mutation.canonical_payload_digest,
            version: (target.doc_id, target.revision),
            original_seq,
            generation_after_first,
            canonical_record_occurrences: 1,
            ambiguous_first: if post_commit_error {
                RetryPublicResult::ScheduledPostCommitError
            } else {
                RetryPublicResult::Committed {
                    seq: original_seq,
                    generation: generation_after_first,
                }
            },
        })
    }

    /// Derives retry identity after a closed episode base has durably committed
    /// `committed_prefix` fixture mutations at `base_generation`.
    pub fn from_fixture_after_closed_base(
        fixture: &StorageFixtureV1,
        committed_prefix: usize,
        base_generation: u64,
        post_commit_error: bool,
    ) -> Result<Self, String> {
        let target = fixture
            .documents
            .get(committed_prefix)
            .ok_or_else(|| "retry fixture has no target document".to_owned())?;
        let mutation = fixture
            .mutations
            .get(committed_prefix)
            .ok_or_else(|| "retry fixture has no target mutation".to_owned())?;
        if committed_prefix.checked_add(1) != Some(fixture.documents.len())
            || fixture.mutations.len() != fixture.documents.len()
        {
            return Err("retry fixture must retain exactly one pending mutation".to_owned());
        }
        let original_seq = mutation.first_seq;
        if mutation.last_seq != original_seq {
            return Err("retry target mutation spans multiple sequence values".to_owned());
        }
        let generation_after_first = base_generation
            .checked_add(1)
            .ok_or_else(|| "retry generation overflow".to_owned())?;
        Ok(Self {
            canonical_request_digest: mutation.canonical_payload_digest,
            version: (target.doc_id, target.revision),
            original_seq,
            generation_after_first,
            canonical_record_occurrences: 1,
            ambiguous_first: if post_commit_error {
                RetryPublicResult::ScheduledPostCommitError
            } else {
                RetryPublicResult::Committed {
                    seq: original_seq,
                    generation: generation_after_first,
                }
            },
        })
    }
}

/// Typed public result of one retry operation call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryPublicResult {
    Committed { seq: u64, generation: u64 },
    ScheduledPostCommitError,
}

/// Same-handle active-state equal-revision retry facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRetryObserved {
    pub first: RetryPublicResult,
    pub retry: RetryPublicResult,
    pub canonical_record_occurrences: u32,
    pub live_version_occurrences: u32,
}

/// Production/raw I17 observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedRetryObserved {
    pub retry_seq: u64,
    pub generation_before_retry: u64,
    pub generation_after_retry: u64,
    pub wal_length_before_retry: u64,
    pub wal_length_after_retry: u64,
    pub wal_digest_before_retry: [u8; 32],
    pub wal_digest_after_retry: [u8; 32],
    pub live_version_occurrences: u32,
}

/// Production/raw I17 observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryObserved {
    pub canonical_request_digest: [u8; 32],
    pub version: (u128, u64),
    pub first_seq: u64,
    pub retry_seq: u64,
    pub generation_before_retry: u64,
    pub generation_after_retry: u64,
    pub wal_length_before_retry: u64,
    pub wal_length_after_retry: u64,
    pub wal_digest_before_retry: [u8; 32],
    pub wal_digest_after_retry: [u8; 32],
    pub canonical_record_occurrences: u32,
    pub live_version_occurrences: u32,
    pub ambiguous_first: RetryPublicResult,
    pub active_same_handle: ActiveRetryObserved,
    pub sealed_reopened: Option<SealedRetryObserved>,
}

/// Checks exact retry byte and logical idempotence.
pub fn check_i17(expected: &RetryExpected, observed: &RetryObserved) -> Result<(), CheckFailure> {
    if observed.canonical_request_digest != expected.canonical_request_digest
        || observed.version != expected.version
    {
        return Err(CheckFailure::new(I17_CHECKER_ID, "retry identity differs"));
    }
    if observed.ambiguous_first != expected.ambiguous_first {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "ambiguous first public result differs",
        ));
    }
    let committed = RetryPublicResult::Committed {
        seq: expected.original_seq,
        generation: expected.generation_after_first,
    };
    if observed.active_same_handle.first != committed
        || observed.active_same_handle.retry != committed
    {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "active same-handle public results differ",
        ));
    }
    if observed.active_same_handle.canonical_record_occurrences != 1
        || observed.active_same_handle.live_version_occurrences != 1
    {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "active same-handle retry duplicated mutation",
        ));
    }
    if observed.first_seq != expected.original_seq || observed.retry_seq != expected.original_seq {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "retry sequence differs from original",
        ));
    }
    if observed.generation_before_retry != expected.generation_after_first
        || observed.generation_after_retry != expected.generation_after_first
    {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "retry changed generation",
        ));
    }
    if observed.wal_length_before_retry != observed.wal_length_after_retry
        || observed.wal_digest_before_retry != observed.wal_digest_after_retry
    {
        return Err(CheckFailure::new(I17_CHECKER_ID, "retry changed WAL bytes"));
    }
    if expected.canonical_record_occurrences != 1
        || observed.canonical_record_occurrences != 1
        || observed.live_version_occurrences != 1
    {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "retry duplicated mutation",
        ));
    }
    let sealed = observed.sealed_reopened.as_ref().ok_or_else(|| {
        CheckFailure::new(I17_CHECKER_ID, "sealed/reopened retry evidence missing")
    })?;
    if sealed.retry_seq != expected.original_seq {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "sealed/reopened retry sequence differs from original",
        ));
    }
    if sealed.generation_before_retry != sealed.generation_after_retry {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "sealed/reopened retry changed generation",
        ));
    }
    if sealed.wal_length_before_retry != sealed.wal_length_after_retry
        || sealed.wal_digest_before_retry != sealed.wal_digest_after_retry
    {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "sealed/reopened retry changed WAL bytes",
        ));
    }
    if sealed.live_version_occurrences != 1 {
        return Err(CheckFailure::new(
            I17_CHECKER_ID,
            "sealed/reopened retry duplicated mutation",
        ));
    }
    Ok(())
}

/// Exact public call expected to reject damaged bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatPublicCall {
    Open,
    ExactSearch,
}

/// Closed, exhaustive I18 damaged-artifact case catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatCase {
    WalHeader,
    WalRecordBody,
    WalRecordChecksum,
    SegmentRegion,
    ManifestWrongFamily,
    SegmentWrongFamily,
    SegmentWrongIdentity,
}

/// Typed success result from the byte-identical same-seed clean public call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatCleanOutcome {
    Opened,
    ExactSearch { candidates: u32 },
}

/// Every independently required I18 case in stable evidence order.
pub const ALL_FORMAT_CASES: [FormatCase; 7] = [
    FormatCase::WalHeader,
    FormatCase::WalRecordBody,
    FormatCase::WalRecordChecksum,
    FormatCase::SegmentRegion,
    FormatCase::ManifestWrongFamily,
    FormatCase::SegmentWrongFamily,
    FormatCase::SegmentWrongIdentity,
];

/// Typed persisted artifact named by an I18 refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactFact {
    Wal {
        path: String,
    },
    Manifest {
        path: String,
    },
    Segment {
        path: String,
        id: Option<[u8; 16]>,
    },
    SegmentRegion {
        path: String,
        id: [u8; 16],
        kind: u16,
        chunk: u32,
    },
}

/// Closed storage format-check vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatCheckFact {
    Length,
    Magic,
    Family,
    Version,
    HeaderLength,
    FileLength,
    BlockLength,
    BlockChecksum,
    FileChecksum,
    ObjectIdentity,
}

/// Primitive typed format-error path, including all value-bearing fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormatRefusal {
    WalInvalidHeader {
        artifact: ArtifactFact,
        reason: WalHeaderFailure,
    },
    WalCorruptAt {
        artifact: ArtifactFact,
        offset: u64,
        location: CorruptionLocation,
        reason: WalRecordFailure,
    },
    ManifestFormat {
        artifact: ArtifactFact,
        check: FormatCheckFact,
        offset: u64,
        expected: u64,
        actual: u64,
    },
    SegmentFormat {
        artifact: ArtifactFact,
        check: FormatCheckFact,
        offset: u64,
        expected: u64,
        actual: u64,
    },
    SegmentWrongObject {
        artifact: ArtifactFact,
        expected: [u8; 16],
        actual: [u8; 16],
    },
}

/// Independent I18 expected facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatExpected {
    pub case: FormatCase,
    pub call: FormatPublicCall,
    pub clean: FormatCleanOutcome,
    pub refusal: FormatRefusal,
}

/// Production I18 observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatObserved {
    pub case: FormatCase,
    pub call: FormatPublicCall,
    pub clean: FormatCleanOutcome,
    pub refusal: Option<FormatRefusal>,
    pub partial_candidates: u32,
}

/// Checks exact public error propagation and no partial query output.
pub fn check_i18(expected: &FormatExpected, observed: &FormatObserved) -> Result<(), CheckFailure> {
    if observed.case != expected.case {
        return Err(CheckFailure::new(I18_CHECKER_ID, "format case differs"));
    }
    if observed.call != expected.call {
        return Err(CheckFailure::new(I18_CHECKER_ID, "public call differs"));
    }
    if observed.clean != expected.clean {
        return Err(CheckFailure::new(
            I18_CHECKER_ID,
            "same-seed clean public result differs",
        ));
    }
    if observed.partial_candidates != 0 {
        return Err(CheckFailure::new(
            I18_CHECKER_ID,
            "partial candidates escaped refusal",
        ));
    }
    match observed.refusal.as_ref() {
        None => Err(CheckFailure::new(
            I18_CHECKER_ID,
            "damaged artifact succeeded",
        )),
        Some(actual) if actual == &expected.refusal => Ok(()),
        Some(_) => Err(CheckFailure::new(
            I18_CHECKER_ID,
            "typed artifact refusal differs",
        )),
    }
}

/// One exact inventory row used by I19.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FileFact {
    pub path: String,
    pub length: u64,
    pub digest: [u8; 32],
}

/// Eligible orphan artifact family selected by one omission case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrphanKind {
    FinalSegment,
    SegmentTemporary,
    ManifestTemporary,
}

/// Store-owned VFS site that omits an eligible orphan once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OmissionSubsite {
    List,
    Delete,
}

/// One exact I19 omission case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OmissionCase {
    pub orphan: OrphanKind,
    pub subsite: OmissionSubsite,
}

/// Cross-product of all eligible artifact families and omission sites.
pub const ALL_OMISSION_CASES: [OmissionCase; 6] = [
    OmissionCase {
        orphan: OrphanKind::FinalSegment,
        subsite: OmissionSubsite::List,
    },
    OmissionCase {
        orphan: OrphanKind::FinalSegment,
        subsite: OmissionSubsite::Delete,
    },
    OmissionCase {
        orphan: OrphanKind::SegmentTemporary,
        subsite: OmissionSubsite::List,
    },
    OmissionCase {
        orphan: OrphanKind::SegmentTemporary,
        subsite: OmissionSubsite::Delete,
    },
    OmissionCase {
        orphan: OrphanKind::ManifestTemporary,
        subsite: OmissionSubsite::List,
    },
    OmissionCase {
        orphan: OrphanKind::ManifestTemporary,
        subsite: OmissionSubsite::Delete,
    },
];

/// Literal control and purge-recovery names that read-write open preserves.
pub const PRESERVED_CONTROL_FILES: [&str; 6] = [
    "manifest.ze",
    "wal.ze",
    "writer.lock",
    "purge.ze",
    ".purge.ze.tmp",
    ".wal.ze.purge.tmp",
];

fn segment_file_name(id: &[u8; 16]) -> String {
    let mut output = String::from("segment-");
    for byte in id {
        use fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output.push_str(".zseg");
    output
}

fn eligible_orphan_name(name: &str, referenced_segments: &[String]) -> bool {
    name == ".manifest.ze.tmp"
        || (name.starts_with(".segment-") && name.ends_with(".zseg.tmp"))
        || (name.starts_with("segment-")
            && name.ends_with(".zseg")
            && !referenced_segments.iter().any(|path| path == name))
}

/// Independent I19 expected facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReachabilityExpected {
    pub baseline: Vec<FileFact>,
    pub manifest_referenced: Vec<FileFact>,
    pub control_and_unknown: Vec<FileFact>,
    pub preserved: Vec<FileFact>,
    pub eligible_orphans: Vec<FileFact>,
    pub reclaimed_bytes: u64,
    pub directory_sync_required: bool,
    pub expected_directory_syncs: u32,
    pub committed_purge_read_only_required: bool,
}

impl ReachabilityExpected {
    /// Derives reachability from literal manifest bytes and exact inventory.
    pub fn from_manifest(
        baseline: Vec<FileFact>,
        manifest_bytes: &[u8],
        directory_sync_required: bool,
    ) -> Result<Self, String> {
        let manifest =
            parse_manifest("manifest.ze", manifest_bytes).map_err(|error| error.to_string())?;
        let referenced_paths = manifest
            .segments
            .iter()
            .map(|segment| segment_file_name(&segment.id))
            .collect::<Vec<_>>();
        let baseline_map = inventory(&baseline);
        if baseline_map.len() != baseline.len() {
            return Err("reachability baseline contains duplicate paths".to_owned());
        }
        let mut manifest_referenced = Vec::with_capacity(referenced_paths.len());
        for path in &referenced_paths {
            let file = baseline_map
                .get(path.as_str())
                .ok_or_else(|| format!("manifest-referenced segment {path} is absent"))?;
            manifest_referenced.push((*file).clone());
        }
        let mut control_and_unknown = Vec::new();
        let mut eligible_orphans = Vec::new();
        for file in &baseline {
            if referenced_paths.iter().any(|path| path == &file.path) {
                continue;
            }
            if eligible_orphan_name(&file.path, &referenced_paths) {
                eligible_orphans.push(file.clone());
            } else {
                control_and_unknown.push(file.clone());
            }
        }
        let mut preserved = manifest_referenced.clone();
        preserved.extend(control_and_unknown.iter().cloned());
        preserved.sort();
        eligible_orphans.sort();
        let reclaimed_bytes = eligible_orphans
            .iter()
            .try_fold(0_u64, |total, file| total.checked_add(file.length))
            .ok_or_else(|| "reachability reclaimed bytes overflowed".to_owned())?;
        let directory_sync_required = directory_sync_required && reclaimed_bytes != 0;
        Ok(Self {
            baseline,
            manifest_referenced,
            control_and_unknown,
            preserved,
            eligible_orphans,
            reclaimed_bytes,
            directory_sync_required,
            expected_directory_syncs: u32::from(directory_sync_required),
            committed_purge_read_only_required: false,
        })
    }

    /// Requires a separate public read-only leg with a real committed purge
    /// intent; that leg must refuse recovery without changing any bytes.
    #[must_use]
    pub fn with_committed_purge_read_only(mut self) -> Self {
        self.committed_purge_read_only_required = true;
        self
    }

    /// Models the fault leg plus required disarmed recovery open. Each open
    /// that deletes at least one orphan syncs the directory exactly once.
    #[must_use]
    pub fn with_omission_recovery_leg(mut self) -> Self {
        if self.directory_sync_required {
            self.expected_directory_syncs = 2;
        }
        self
    }
}

/// Typed result of opening a store read-only while `purge.ze` is committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommittedPurgeReadOnlyOutcome {
    RefusedPurgeRecoveryReadOnly,
}

/// Independent byte inventories around the committed-purge read-only leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedPurgeReadOnlyObserved {
    pub before: Vec<FileFact>,
    pub after: Vec<FileFact>,
    pub outcome: CommittedPurgeReadOnlyOutcome,
}

/// Production/raw I19 observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReachabilityObserved {
    pub after_read_only: Vec<FileFact>,
    pub final_inventory: Vec<FileFact>,
    pub reclaimed_bytes: u64,
    pub directory_syncs: u32,
    pub committed_purge_read_only: Option<CommittedPurgeReadOnlyObserved>,
}

fn inventory(files: &[FileFact]) -> BTreeMap<&str, &FileFact> {
    files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect()
}

/// Checks read-only nonmutation and reachable-safe exact orphan cleanup.
pub fn check_i19(
    expected: &ReachabilityExpected,
    observed: &ReachabilityObserved,
) -> Result<(), CheckFailure> {
    match (
        expected.committed_purge_read_only_required,
        observed.committed_purge_read_only.as_ref(),
    ) {
        (true, Some(purge)) => {
            if !purge.before.iter().any(|file| file.path == "purge.ze") {
                return Err(CheckFailure::new(
                    I19_CHECKER_ID,
                    "committed purge intent evidence missing",
                ));
            }
            if purge.before != purge.after {
                return Err(CheckFailure::new(
                    I19_CHECKER_ID,
                    "read-only committed-purge open mutated files",
                ));
            }
            if purge.outcome != CommittedPurgeReadOnlyOutcome::RefusedPurgeRecoveryReadOnly {
                return Err(CheckFailure::new(
                    I19_CHECKER_ID,
                    "read-only committed-purge result differs",
                ));
            }
        }
        (true, None) => {
            return Err(CheckFailure::new(
                I19_CHECKER_ID,
                "committed purge read-only evidence missing",
            ));
        }
        (false, None) => {}
        (false, Some(_)) => {
            return Err(CheckFailure::new(
                I19_CHECKER_ID,
                "unexpected committed purge read-only evidence",
            ));
        }
    }
    let baseline = inventory(&expected.baseline);
    let read_only = inventory(&observed.after_read_only);
    if baseline.len() != expected.baseline.len()
        || read_only.len() != observed.after_read_only.len()
        || read_only != baseline
    {
        return Err(CheckFailure::new(
            I19_CHECKER_ID,
            "read-only open mutated files",
        ));
    }
    let mut classified = BTreeMap::new();
    let manifest_referenced = inventory(&expected.manifest_referenced);
    let control_and_unknown = inventory(&expected.control_and_unknown);
    if manifest_referenced.len() != expected.manifest_referenced.len()
        || control_and_unknown.len() != expected.control_and_unknown.len()
        || expected
            .manifest_referenced
            .iter()
            .any(|file| control_and_unknown.contains_key(file.path.as_str()))
    {
        return Err(CheckFailure::new(
            I19_CHECKER_ID,
            "manifest reachability classification overlaps",
        ));
    }
    for file in expected
        .preserved
        .iter()
        .chain(expected.eligible_orphans.iter())
    {
        if classified.insert(file.path.as_str(), file).is_some() {
            return Err(CheckFailure::new(
                I19_CHECKER_ID,
                "expected inventory classification overlaps",
            ));
        }
    }
    if classified != baseline {
        return Err(CheckFailure::new(
            I19_CHECKER_ID,
            "expected inventory classification is incomplete",
        ));
    }
    let final_files = inventory(&observed.final_inventory);
    for preserved in &expected.preserved {
        match final_files.get(preserved.path.as_str()) {
            None => {
                return Err(CheckFailure::new(I19_CHECKER_ID, "reachable file removed"));
            }
            Some(actual) if *actual == preserved => {}
            Some(_) => {
                return Err(CheckFailure::new(
                    I19_CHECKER_ID,
                    "preserved file bytes changed",
                ));
            }
        }
    }
    if expected
        .eligible_orphans
        .iter()
        .any(|orphan| final_files.contains_key(orphan.path.as_str()))
    {
        return Err(CheckFailure::new(
            I19_CHECKER_ID,
            "eligible orphan retained",
        ));
    }
    let preserved = inventory(&expected.preserved);
    if preserved.len() != expected.preserved.len()
        || final_files.len() != observed.final_inventory.len()
        || final_files != preserved
    {
        return Err(CheckFailure::new(I19_CHECKER_ID, "final inventory differs"));
    }
    let exact_reclaimed = expected
        .eligible_orphans
        .iter()
        .try_fold(0_u64, |total, orphan| total.checked_add(orphan.length))
        .ok_or_else(|| CheckFailure::new(I19_CHECKER_ID, "reclaimed byte count overflowed"))?;
    if exact_reclaimed != expected.reclaimed_bytes {
        return Err(CheckFailure::new(
            I19_CHECKER_ID,
            "expected reclaimed byte count differs from inventory",
        ));
    }
    if observed.reclaimed_bytes != expected.reclaimed_bytes {
        return Err(CheckFailure::new(
            I19_CHECKER_ID,
            "reclaimed byte count differs",
        ));
    }
    if observed.directory_syncs != expected.expected_directory_syncs {
        return Err(CheckFailure::new(
            I19_CHECKER_ID,
            "directory sync count differs",
        ));
    }
    Ok(())
}

/// Primitive production-operation receipt expected for one scheduled fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptOperation {
    WalPrefix,
    Publication,
    Retry,
    FormatCheck,
    OrphanCleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptFault {
    TornWalHeader,
    TornWalBody,
    TornWalChecksum,
    PostCommitError,
    ManifestPreRenameCrash,
    ManifestPostRenameCrash,
    CorruptSegmentRegion,
    WrongManifestObject,
    WrongSegmentObject,
    ListDeleteOmission,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptSite {
    WalOpenHeaderValidation,
    WalOpenRecordValidation,
    WalOpenRecordChecksum,
    WalCommitAppendAfterInnerSuccess,
    ManifestCommitBeforeRename,
    ManifestCommitAfterRename,
    SegmentReadRegionChecksum,
    ManifestOpenFamilyValidation,
    SegmentOpenFamilyValidation,
    SegmentOpenObjectIdentity,
    OrphanCleanupList,
    OrphanCleanupDelete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiptEffectFact {
    WalHeader {
        planned_offset: u64,
        retained_len: u64,
        observed: WalHeaderFailure,
    },
    WalRecord {
        planned_offset: u64,
        observed_offset: u64,
        location: CorruptionLocation,
        observed: WalRecordFailure,
    },
    WalAppend {
        encoded_len: u64,
        first_seq: u64,
        last_seq: u64,
        inner_append_completed: bool,
        caller_saw_error: bool,
    },
    ManifestRename {
        temporary: String,
        committed: String,
        rename_performed: bool,
        new_segment_final: bool,
        directory_sync_returned: bool,
    },
    SegmentChecksum {
        segment: [u8; 16],
        region_kind: u16,
        chunk: u32,
        expected_checksum: u64,
        actual_checksum: u64,
    },
    Format {
        artifact: ArtifactFact,
        check: FormatCheckFact,
        expected_family: Option<u16>,
        actual_family: Option<u16>,
        expected_id: Option<[u8; 16]>,
        actual_id: Option<[u8; 16]>,
    },
    Omission {
        omitted_path: String,
        deletion_observed: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptExpected {
    pub campaign: &'static str,
    pub operation: ReceiptOperation,
    pub fault: ReceiptFault,
    pub site: ReceiptSite,
    pub op_index: u32,
    pub artifact: ArtifactFact,
    pub effect: ReceiptEffectFact,
}

/// Observed production-operation receipt plus cardinality.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptObserved {
    pub value: ReceiptExpected,
    pub cardinality: u32,
}

/// Checks that one scheduled fault fired exactly once at its intended site.
pub fn check_receipt(
    checker_id: &'static str,
    expected: &ReceiptExpected,
    observed: Option<&ReceiptObserved>,
) -> Result<(), CheckFailure> {
    let Some(observed) = observed else {
        return Err(CheckFailure::new(checker_id, "production receipt missing"));
    };
    if observed.cardinality != 1 {
        return Err(CheckFailure::new(
            checker_id,
            "production receipt cardinality differs",
        ));
    }
    if &observed.value != expected {
        return Err(CheckFailure::new(checker_id, "production receipt differs"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hex(source: &str) -> Vec<u8> {
        let digits = source
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect::<Vec<_>>();
        assert_eq!(digits.len() % 2, 0, "hex fixture has an odd digit count");
        digits
            .chunks_exact(2)
            .map(|pair| {
                let high = (pair[0] as char).to_digit(16).expect("hex high nibble");
                let low = (pair[1] as char).to_digit(16).expect("hex low nibble");
                ((high << 4) | low) as u8
            })
            .collect()
    }

    fn snapshot(generation: u64) -> SnapshotState {
        SnapshotState {
            generation,
            manifest_log_seq: generation,
            segments: Vec::new(),
            live_versions: vec![(1, generation)],
            absorbed_through: generation,
        }
    }

    fn publication_model(generation: u64) -> PublicationModelState {
        publication_model_projection(&snapshot(generation))
    }

    fn file(path: &str, byte: u8) -> FileFact {
        FileFact {
            path: path.to_owned(),
            length: 1,
            digest: [byte; 32],
        }
    }

    #[test]
    fn storage_oracle_i15_plant_is_rejected() {
        let expected = PublicationExpected {
            old: publication_model(1),
            new: publication_model(2),
            legal: vec![PublicationClass::Old, PublicationClass::New],
        };
        let observed = PublicationObserved {
            class: PublicationClass::Hybrid,
            raw: snapshot(1),
            public: published_projection(&snapshot(1)),
            referenced_segments_complete: true,
            trusted_wal_end: 2,
        };
        assert_eq!(
            check_i15(&expected, &observed),
            Err(CheckFailure::new(
                I15_CHECKER_ID,
                "observed hybrid publication"
            ))
        );
    }

    #[test]
    fn i15_refuses_an_observation_matching_both_canonical_states() {
        let state = snapshot(1);
        let expected = PublicationExpected {
            old: publication_model_projection(&state),
            new: publication_model_projection(&state),
            legal: vec![PublicationClass::Old, PublicationClass::New],
        };
        let observed = PublicationObserved {
            class: PublicationClass::Old,
            raw: state.clone(),
            public: published_projection(&state),
            referenced_segments_complete: true,
            trusted_wal_end: 1,
        };
        assert_eq!(
            check_i15(&expected, &observed),
            Err(CheckFailure::new(
                I15_CHECKER_ID,
                "publication matched multiple canonical states"
            ))
        );
    }

    #[test]
    fn i15_rejects_missing_references_and_each_integrity_field_mutation() {
        let raw = SnapshotState {
            generation: 1,
            manifest_log_seq: 1,
            segments: vec![SegmentFact {
                id: [1; 16],
                rows: 1,
                scheme: 4,
                dims: 2,
                file_length: 100,
                header_checksum: 101,
                whole_file_checksum: 102,
            }],
            live_versions: vec![(1, 1)],
            absorbed_through: 1,
        };
        let expected = PublicationExpected {
            old: publication_model_projection(&raw),
            new: publication_model(2),
            legal: vec![PublicationClass::Old],
        };
        let mut observed = PublicationObserved {
            class: PublicationClass::Old,
            raw: raw.clone(),
            public: published_projection(&raw),
            referenced_segments_complete: false,
            trusted_wal_end: 1,
        };
        assert_eq!(
            check_i15(&expected, &observed),
            Err(CheckFailure::new(
                I15_CHECKER_ID,
                "referenced segment missing"
            ))
        );
        observed.referenced_segments_complete = true;
        for field in 0..3 {
            let mut planted = observed.clone();
            match field {
                0 => planted.raw.segments[0].file_length += 1,
                1 => planted.raw.segments[0].header_checksum ^= 1,
                _ => planted.raw.segments[0].whole_file_checksum ^= 1,
            }
            planted.public = published_projection(&planted.raw);
            planted.class = expected.classify(&planted.raw);
            assert_eq!(
                check_i15(&expected, &planted),
                Err(CheckFailure::new(
                    I15_CHECKER_ID,
                    "observed hybrid publication"
                ))
            );
        }
    }

    #[test]
    fn storage_oracle_i16_plant_is_rejected() {
        let record = WalRecordFact {
            seq: 1,
            op: 7,
            payload: vec![1],
        };
        let expected = WalPrefixExpected {
            first_seq: 1,
            acknowledged: vec![record],
            optional_unacknowledged_tail: Vec::new(),
            terminator: WalTerminator::CleanEnd,
            live_versions: vec![(1, 1)],
            ack_boundaries: Vec::new(),
        };
        let observed = WalPrefixObserved {
            first_seq: 1,
            records: Vec::new(),
            terminator: WalTerminator::CleanEnd,
            clean_public: WalPublicOutcome::Opened {
                live_versions: vec![(1, 1)],
            },
            public: WalPublicOutcome::Opened {
                live_versions: vec![(1, 1)],
            },
            reopened_ack_boundaries: Vec::new(),
        };
        assert_eq!(
            check_i16(&expected, &observed),
            Err(CheckFailure::new(
                I16_CHECKER_ID,
                "acknowledged record missing or changed"
            ))
        );
    }

    #[test]
    fn i16_sequence_check_does_not_saturate_at_u64_max() {
        let first = WalRecordFact {
            seq: u64::MAX,
            op: 7,
            payload: vec![1],
        };
        let duplicate = WalRecordFact {
            seq: u64::MAX,
            op: 7,
            payload: vec![2],
        };
        let expected = WalPrefixExpected {
            first_seq: u64::MAX,
            acknowledged: vec![first.clone(), duplicate.clone()],
            optional_unacknowledged_tail: Vec::new(),
            terminator: WalTerminator::CleanEnd,
            live_versions: Vec::new(),
            ack_boundaries: Vec::new(),
        };
        let observed = WalPrefixObserved {
            first_seq: u64::MAX,
            records: vec![first, duplicate],
            terminator: WalTerminator::CleanEnd,
            clean_public: WalPublicOutcome::Opened {
                live_versions: Vec::new(),
            },
            public: WalPublicOutcome::Opened {
                live_versions: Vec::new(),
            },
            reopened_ack_boundaries: Vec::new(),
        };
        assert_eq!(
            check_i16(&expected, &observed),
            Err(CheckFailure::new(
                I16_CHECKER_ID,
                "record sequence overflowed"
            ))
        );
    }

    #[test]
    fn i16_requires_a_public_reopen_at_every_acknowledged_boundary() {
        let record = WalRecordFact {
            seq: 1,
            op: 7,
            payload: vec![1],
        };
        let expected = WalPrefixExpected {
            first_seq: 1,
            acknowledged: vec![record.clone()],
            optional_unacknowledged_tail: Vec::new(),
            terminator: WalTerminator::CleanEnd,
            live_versions: vec![(1, 1)],
            ack_boundaries: vec![WalAckBoundary {
                records: vec![record.clone()],
                live_versions: vec![(1, 1)],
            }],
        };
        let observed = WalPrefixObserved {
            first_seq: 1,
            records: vec![record],
            terminator: WalTerminator::CleanEnd,
            clean_public: WalPublicOutcome::Opened {
                live_versions: vec![(1, 1)],
            },
            public: WalPublicOutcome::Opened {
                live_versions: vec![(1, 1)],
            },
            reopened_ack_boundaries: Vec::new(),
        };
        assert_eq!(
            check_i16(&expected, &observed),
            Err(CheckFailure::new(
                I16_CHECKER_ID,
                "acknowledgement boundary reopen count differs"
            ))
        );
    }

    #[test]
    fn i16_rejects_reordered_records_and_a_torn_record_claimed_as_acked() {
        let first = WalRecordFact {
            seq: 1,
            op: 7,
            payload: vec![1],
        };
        let second = WalRecordFact {
            seq: 2,
            op: 7,
            payload: vec![2],
        };
        let expected = WalPrefixExpected {
            first_seq: 1,
            acknowledged: vec![first.clone(), second.clone()],
            optional_unacknowledged_tail: Vec::new(),
            terminator: WalTerminator::CleanEnd,
            live_versions: vec![(1, 1), (2, 1)],
            ack_boundaries: Vec::new(),
        };
        let reordered = WalPrefixObserved {
            first_seq: 1,
            records: vec![second.clone(), first.clone()],
            terminator: WalTerminator::CleanEnd,
            clean_public: WalPublicOutcome::Opened {
                live_versions: expected.live_versions.clone(),
            },
            public: WalPublicOutcome::Opened {
                live_versions: expected.live_versions.clone(),
            },
            reopened_ack_boundaries: Vec::new(),
        };
        assert_eq!(
            check_i16(&expected, &reordered),
            Err(CheckFailure::new(
                I16_CHECKER_ID,
                "record sequence is not a prefix"
            ))
        );

        let corrupt = WalTerminator::CorruptAt {
            artifact: "wal.ze".to_owned(),
            offset: 64,
            location: CorruptionLocation::Tail,
            reason: WalRecordFailure::BodyTruncated {
                payload_length: 8,
                needed: 30,
                available: 7,
            },
        };
        let torn_expected = WalPrefixExpected {
            first_seq: 1,
            acknowledged: vec![first.clone(), second],
            optional_unacknowledged_tail: Vec::new(),
            terminator: corrupt.clone(),
            live_versions: vec![(1, 1)],
            ack_boundaries: Vec::new(),
        };
        let torn_observed = WalPrefixObserved {
            first_seq: 1,
            records: vec![first],
            terminator: corrupt.clone(),
            clean_public: WalPublicOutcome::Opened {
                live_versions: vec![(1, 1)],
            },
            public: WalPublicOutcome::Refused {
                terminator: corrupt,
            },
            reopened_ack_boundaries: Vec::new(),
        };
        assert_eq!(
            check_i16(&torn_expected, &torn_observed),
            Err(CheckFailure::new(
                I16_CHECKER_ID,
                "trusted records before corrupt tail differ from acknowledged model"
            ))
        );
    }

    #[test]
    fn i16_requires_the_same_seed_clean_public_operation_to_succeed() {
        let expected = WalPrefixExpected {
            first_seq: 1,
            acknowledged: Vec::new(),
            optional_unacknowledged_tail: Vec::new(),
            terminator: WalTerminator::CleanEnd,
            live_versions: Vec::new(),
            ack_boundaries: Vec::new(),
        };
        let observed = WalPrefixObserved {
            first_seq: 1,
            records: Vec::new(),
            terminator: WalTerminator::CleanEnd,
            clean_public: WalPublicOutcome::Refused {
                terminator: WalTerminator::InvalidHeader {
                    artifact: "wal.ze".to_owned(),
                    reason: WalHeaderFailure::Missing,
                },
            },
            public: WalPublicOutcome::Opened {
                live_versions: Vec::new(),
            },
            reopened_ack_boundaries: Vec::new(),
        };
        assert_eq!(
            check_i16(&expected, &observed),
            Err(CheckFailure::new(
                I16_CHECKER_ID,
                "same-seed clean WAL was refused"
            ))
        );
    }

    #[test]
    fn storage_oracle_i17_plant_is_rejected() {
        let expected = RetryExpected {
            canonical_request_digest: [1; 32],
            version: (1, 1),
            original_seq: 1,
            generation_after_first: 1,
            canonical_record_occurrences: 1,
            ambiguous_first: RetryPublicResult::Committed {
                seq: 1,
                generation: 1,
            },
        };
        let observed = RetryObserved {
            canonical_request_digest: [1; 32],
            version: (1, 1),
            first_seq: 1,
            retry_seq: 1,
            generation_before_retry: 1,
            generation_after_retry: 1,
            wal_length_before_retry: 40,
            wal_length_after_retry: 40,
            wal_digest_before_retry: [2; 32],
            wal_digest_after_retry: [2; 32],
            canonical_record_occurrences: 2,
            live_version_occurrences: 2,
            ambiguous_first: RetryPublicResult::Committed {
                seq: 1,
                generation: 1,
            },
            active_same_handle: ActiveRetryObserved {
                first: RetryPublicResult::Committed {
                    seq: 1,
                    generation: 1,
                },
                retry: RetryPublicResult::Committed {
                    seq: 1,
                    generation: 1,
                },
                canonical_record_occurrences: 1,
                live_version_occurrences: 1,
            },
            sealed_reopened: None,
        };
        assert_eq!(
            check_i17(&expected, &observed),
            Err(CheckFailure::new(
                I17_CHECKER_ID,
                "retry duplicated mutation"
            ))
        );
    }

    #[test]
    fn i17_requires_one_canonical_occurrence_even_if_expected_is_wrong() {
        let expected = RetryExpected {
            canonical_request_digest: [1; 32],
            version: (1, 1),
            original_seq: 1,
            generation_after_first: 1,
            canonical_record_occurrences: 2,
            ambiguous_first: RetryPublicResult::Committed {
                seq: 1,
                generation: 1,
            },
        };
        let observed = RetryObserved {
            canonical_request_digest: [1; 32],
            version: (1, 1),
            first_seq: 1,
            retry_seq: 1,
            generation_before_retry: 1,
            generation_after_retry: 1,
            wal_length_before_retry: 64,
            wal_length_after_retry: 64,
            wal_digest_before_retry: [2; 32],
            wal_digest_after_retry: [2; 32],
            canonical_record_occurrences: 2,
            live_version_occurrences: 1,
            ambiguous_first: RetryPublicResult::Committed {
                seq: 1,
                generation: 1,
            },
            active_same_handle: ActiveRetryObserved {
                first: RetryPublicResult::Committed {
                    seq: 1,
                    generation: 1,
                },
                retry: RetryPublicResult::Committed {
                    seq: 1,
                    generation: 1,
                },
                canonical_record_occurrences: 1,
                live_version_occurrences: 1,
            },
            sealed_reopened: None,
        };
        assert_eq!(
            check_i17(&expected, &observed),
            Err(CheckFailure::new(
                I17_CHECKER_ID,
                "retry duplicated mutation"
            ))
        );
    }

    #[test]
    fn i17_rejects_a_retry_generation_increment() {
        let fixture = StorageFixtureV1::derive(12);
        let expected = RetryExpected::from_fixture(&fixture, false).expect("retry model");
        let committed = RetryPublicResult::Committed {
            seq: expected.original_seq,
            generation: expected.generation_after_first,
        };
        let observed = RetryObserved {
            canonical_request_digest: expected.canonical_request_digest,
            version: expected.version,
            first_seq: expected.original_seq,
            retry_seq: expected.original_seq,
            generation_before_retry: expected.generation_after_first,
            generation_after_retry: expected.generation_after_first + 1,
            wal_length_before_retry: 64,
            wal_length_after_retry: 64,
            wal_digest_before_retry: [3; 32],
            wal_digest_after_retry: [3; 32],
            canonical_record_occurrences: 1,
            live_version_occurrences: 1,
            ambiguous_first: committed,
            active_same_handle: ActiveRetryObserved {
                first: committed,
                retry: committed,
                canonical_record_occurrences: 1,
                live_version_occurrences: 1,
            },
            sealed_reopened: None,
        };
        assert_eq!(
            check_i17(&expected, &observed),
            Err(CheckFailure::new(
                I17_CHECKER_ID,
                "retry changed generation"
            ))
        );
    }

    #[test]
    fn storage_oracle_i18_plant_is_rejected() {
        let expected = FormatExpected {
            case: FormatCase::SegmentRegion,
            call: FormatPublicCall::ExactSearch,
            clean: FormatCleanOutcome::ExactSearch { candidates: 1 },
            refusal: FormatRefusal::SegmentFormat {
                artifact: ArtifactFact::SegmentRegion {
                    path: "segment-a.zseg".to_owned(),
                    id: [1; 16],
                    kind: 3,
                    chunk: 0,
                },
                check: FormatCheckFact::BlockChecksum,
                offset: 16_384,
                expected: 1,
                actual: 2,
            },
        };
        let observed = FormatObserved {
            case: FormatCase::SegmentRegion,
            call: FormatPublicCall::ExactSearch,
            clean: FormatCleanOutcome::ExactSearch { candidates: 1 },
            refusal: None,
            partial_candidates: 0,
        };
        assert_eq!(
            check_i18(&expected, &observed),
            Err(CheckFailure::new(
                I18_CHECKER_ID,
                "damaged artifact succeeded"
            ))
        );
    }

    #[test]
    fn i18_rejects_file_checksum_and_sibling_artifact_plants() {
        let refusal = FormatRefusal::SegmentFormat {
            artifact: ArtifactFact::SegmentRegion {
                path: "segment-a.zseg".to_owned(),
                id: [1; 16],
                kind: 5,
                chunk: 0,
            },
            check: FormatCheckFact::BlockChecksum,
            offset: 16_384,
            expected: 1,
            actual: 2,
        };
        let expected = FormatExpected {
            case: FormatCase::SegmentRegion,
            call: FormatPublicCall::ExactSearch,
            clean: FormatCleanOutcome::ExactSearch { candidates: 1 },
            refusal: refusal.clone(),
        };
        let mut observed = FormatObserved {
            case: expected.case,
            call: expected.call,
            clean: expected.clean,
            refusal: Some(refusal),
            partial_candidates: 0,
        };
        let Some(FormatRefusal::SegmentFormat { check, .. }) = observed.refusal.as_mut() else {
            panic!("segment-format plant fixture");
        };
        *check = FormatCheckFact::FileChecksum;
        assert_eq!(
            check_i18(&expected, &observed),
            Err(CheckFailure::new(
                I18_CHECKER_ID,
                "typed artifact refusal differs"
            ))
        );
        observed.refusal = Some(expected.refusal.clone());
        let Some(FormatRefusal::SegmentFormat { artifact, .. }) = observed.refusal.as_mut() else {
            panic!("segment-format artifact plant fixture");
        };
        *artifact = ArtifactFact::SegmentRegion {
            path: "segment-sibling.zseg".to_owned(),
            id: [2; 16],
            kind: 5,
            chunk: 0,
        };
        assert_eq!(
            check_i18(&expected, &observed),
            Err(CheckFailure::new(
                I18_CHECKER_ID,
                "typed artifact refusal differs"
            ))
        );
    }

    #[test]
    fn storage_oracle_i19_plant_is_rejected() {
        let reachable = file("segment-live.zseg", 1);
        let orphan = file("segment-orphan.zseg", 2);
        let expected = ReachabilityExpected {
            baseline: vec![reachable.clone(), orphan.clone()],
            manifest_referenced: vec![reachable.clone()],
            control_and_unknown: Vec::new(),
            preserved: vec![reachable],
            eligible_orphans: vec![orphan.clone()],
            reclaimed_bytes: 1,
            directory_sync_required: true,
            expected_directory_syncs: 1,
            committed_purge_read_only_required: false,
        };
        let observed = ReachabilityObserved {
            after_read_only: expected.baseline.clone(),
            final_inventory: vec![orphan],
            reclaimed_bytes: 0,
            directory_syncs: 0,
            committed_purge_read_only: None,
        };
        assert_eq!(
            check_i19(&expected, &observed),
            Err(CheckFailure::new(I19_CHECKER_ID, "reachable file removed"))
        );
    }

    #[test]
    fn i19_eligible_orphan_plant_is_rejected_separately() {
        let reachable = file("segment-live.zseg", 1);
        let orphan = file("segment-orphan.zseg", 2);
        let expected = ReachabilityExpected {
            baseline: vec![reachable.clone(), orphan.clone()],
            manifest_referenced: vec![reachable.clone()],
            control_and_unknown: Vec::new(),
            preserved: vec![reachable.clone()],
            eligible_orphans: vec![orphan.clone()],
            reclaimed_bytes: 1,
            directory_sync_required: true,
            expected_directory_syncs: 1,
            committed_purge_read_only_required: false,
        };
        let observed = ReachabilityObserved {
            after_read_only: expected.baseline.clone(),
            final_inventory: vec![reachable, orphan],
            reclaimed_bytes: 0,
            directory_syncs: 0,
            committed_purge_read_only: None,
        };
        assert_eq!(
            check_i19(&expected, &observed),
            Err(CheckFailure::new(
                I19_CHECKER_ID,
                "eligible orphan retained"
            ))
        );
    }

    #[test]
    fn i19_requires_the_exact_final_inventory() {
        let reachable = file("segment-live.zseg", 1);
        let orphan = file("segment-orphan.zseg", 2);
        let unexpected = file("unexpected.bin", 3);
        let expected = ReachabilityExpected {
            baseline: vec![reachable.clone(), orphan],
            manifest_referenced: vec![reachable.clone()],
            control_and_unknown: Vec::new(),
            preserved: vec![reachable.clone()],
            eligible_orphans: vec![file("segment-orphan.zseg", 2)],
            reclaimed_bytes: 1,
            directory_sync_required: true,
            expected_directory_syncs: 1,
            committed_purge_read_only_required: false,
        };
        let observed = ReachabilityObserved {
            after_read_only: expected.baseline.clone(),
            final_inventory: vec![reachable, unexpected],
            reclaimed_bytes: 1,
            directory_syncs: 1,
            committed_purge_read_only: None,
        };
        assert_eq!(
            check_i19(&expected, &observed),
            Err(CheckFailure::new(I19_CHECKER_ID, "final inventory differs"))
        );
    }

    #[test]
    fn independent_xxh3_matches_published_boundary_vectors() {
        let lengths = [0_usize, 1, 3, 4, 8, 9, 16, 17, 128, 129, 240, 241, 1024];
        let expected = [
            0x2d06_8005_38d3_94c2,
            0xc44b_dff4_074e_ecdb,
            0x5f42_99fc_161c_9cbb,
            0x60da_b036_a582_11f2,
            0x3a1c_2d7c_85af_88f8,
            0xe961_2598_145b_b9dc,
            0x8355_e3a6_f617_70db,
            0x9ef3_41a9_9de3_7328,
            0x85c6_174c_7ff4_c46b,
            0xec76_42b4_31ba_3e5a,
            0x375a_384d_957f_e865,
            0x02e8_cd95_421c_6d02,
            0xe5d7_8baf_a45b_2aa5,
        ];
        for (length, expected) in lengths.into_iter().zip(expected) {
            let input = (0..length)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>();
            assert_eq!(xxh3_64(&input), expected, "length {length}");
        }
    }

    #[test]
    fn raw_parsers_accept_frozen_wal_manifest_and_segment_bytes() {
        let wal = decode_hex(include_str!(concat!(
            "../../../crates/zeppelin",
            "-embed/tests/fixtures/format/wal_single_v1.hex"
        )));
        let parsed_wal = parse_wal("wal.ze", &wal).expect("parse frozen WAL");
        assert_eq!(parsed_wal.first_seq, 1);
        assert_eq!(parsed_wal.records.len(), 1);
        assert_eq!(parsed_wal.records[0].payload, b"abc");
        assert_eq!(parsed_wal.terminator, WalTerminator::CleanEnd);

        let manifest = decode_hex(include_str!(concat!(
            "../../../crates/zeppelin",
            "-embed/tests/fixtures/format/manifest_v2.hex"
        )));
        let parsed_manifest = parse_manifest("manifest.ze", &manifest).expect("parse manifest v2");
        assert_eq!(parsed_manifest.generation, 21);
        assert_eq!(parsed_manifest.log_seq, 13);
        assert_eq!(parsed_manifest.segments.len(), 1);

        let segment = decode_hex(include_str!(concat!(
            "../../../crates/zeppelin",
            "-embed/tests/fixtures/format/segment_prechange_full_v1.hex"
        )));
        let parsed_segment = parse_segment("segment.zseg", &segment).expect("parse segment v1");
        assert_eq!(parsed_segment.fact.rows, 1);
        assert_eq!(parsed_segment.fact.scheme, 4);
        assert_eq!(parsed_segment.fact.dims, 3);
        assert!(!parsed_segment.regions.is_empty());
    }

    #[test]
    fn fixture_derivation_is_namespaced_seeded_and_literal() {
        let fixture = StorageFixtureV1::derive(7);
        assert_eq!(fixture, StorageFixtureV1::derive(7));
        assert_ne!(fixture, StorageFixtureV1::derive(8));
        assert_eq!(fixture.namespace, "adversarial::storage-durability::v1");
        assert_eq!(fixture.durability, "Durable");
        assert_eq!(fixture.commit_tier, "Ordered");
        assert_eq!(fixture.seed, 7);
        assert!(fixture.documents.len() >= 3);
        assert_eq!(fixture.mutations.len(), fixture.documents.len());
        assert_eq!(fixture.clean_fault_pair_id.len(), 16);
    }

    #[test]
    fn publication_expectation_binds_primitive_shape_to_independent_integrity_facts() {
        let fixture = StorageFixtureV1::derive(7);
        let old_id = planned_segment_id(fixture.old_generation - 1, fixture.absorbed_through);
        let new_id = planned_publication_segment_id(&fixture).expect("planned segment id");
        let old = SegmentFact {
            id: old_id,
            rows: fixture.documents.len() as u32 - 1,
            scheme: fixture.scheme,
            dims: fixture.dimensions,
            file_length: 101,
            header_checksum: 102,
            whole_file_checksum: 103,
        };
        let new = SegmentFact {
            id: new_id,
            rows: 1,
            scheme: fixture.scheme,
            dims: fixture.dimensions,
            file_length: 201,
            header_checksum: 202,
            whole_file_checksum: 203,
        };
        let expected = PublicationExpected::from_fixture(
            &fixture,
            std::slice::from_ref(&old),
            &[old.clone(), new.clone()],
            vec![PublicationClass::Old, PublicationClass::New],
        )
        .expect("bind publication model to independent controls");

        assert_eq!(expected.old.generation, fixture.old_generation);
        assert_eq!(expected.new.generation, fixture.planned_new_generation);
        assert_eq!(expected.old.segments.len(), 1);
        assert_eq!(expected.new.segments.len(), 2);
        assert_eq!(expected.old.live_versions, expected.new.live_versions);
        assert_eq!(expected.old.segments[0].file_length, old.file_length);
        assert_eq!(
            expected.new.segments[1].header_checksum,
            new.header_checksum
        );
        assert_eq!(
            expected.new.segments[1].whole_file_checksum,
            new.whole_file_checksum
        );
    }

    #[test]
    fn publication_expected_segment_model_pins_all_integrity_fields() {
        let segment = PublicationSegmentModel {
            id: [7; 16],
            rows: 3,
            scheme: 4,
            dims: 8,
            file_length: 4_096,
            header_checksum: 0x1122_3344_5566_7788,
            whole_file_checksum: 0x8877_6655_4433_2211,
        };
        assert_eq!(segment.file_length, 4_096);
        assert_eq!(segment.header_checksum, 0x1122_3344_5566_7788);
        assert_eq!(segment.whole_file_checksum, 0x8877_6655_4433_2211);
    }

    #[test]
    fn wal_mutation_offsets_are_owned_by_the_primitive_fixture() {
        let fixture = StorageFixtureV1::derive(7);
        assert_eq!(
            planned_wal_mutation_offset(&fixture, WalMutationKind::HeaderTruncation)
                .expect("header mutation plan"),
            fixture.wal_mutation_offset
        );
        for kind in [
            WalMutationKind::BodyTruncation,
            WalMutationKind::ChecksumFlip,
        ] {
            assert!(
                planned_wal_mutation_offset(&fixture, kind).expect("record mutation plan")
                    > fixture.wal_mutation_offset
            );
        }
    }

    #[test]
    fn i18_case_catalog_is_complete_and_typed() {
        assert_eq!(
            ALL_FORMAT_CASES,
            [
                FormatCase::WalHeader,
                FormatCase::WalRecordBody,
                FormatCase::WalRecordChecksum,
                FormatCase::SegmentRegion,
                FormatCase::ManifestWrongFamily,
                FormatCase::SegmentWrongFamily,
                FormatCase::SegmentWrongIdentity,
            ]
        );
    }

    #[test]
    fn i19_omission_catalog_covers_every_eligible_family_and_subsite() {
        assert_eq!(ALL_OMISSION_CASES.len(), 6);
        for orphan in [
            OrphanKind::FinalSegment,
            OrphanKind::SegmentTemporary,
            OrphanKind::ManifestTemporary,
        ] {
            assert!(ALL_OMISSION_CASES.contains(&OmissionCase {
                orphan,
                subsite: OmissionSubsite::List,
            }));
            assert!(ALL_OMISSION_CASES.contains(&OmissionCase {
                orphan,
                subsite: OmissionSubsite::Delete,
            }));
        }
    }

    #[test]
    fn i19_reachability_is_manifest_derived_with_exact_control_names() {
        let manifest_bytes = decode_hex(include_str!(concat!(
            "../../../crates/zeppelin",
            "-embed/tests/fixtures/format/manifest_v2.hex"
        )));
        let parsed = parse_manifest("manifest.ze", &manifest_bytes).expect("parse manifest");
        let referenced_name = segment_file_name(&parsed.segments[0].id);
        let baseline = vec![
            file("manifest.ze", 1),
            file("wal.ze", 2),
            file("writer.lock", 3),
            file(".purge.ze.tmp", 4),
            file(".wal.ze.purge.tmp", 5),
            file("owner-sentinel.bin", 6),
            file(&referenced_name, 7),
            file("segment-ffffffffffffffffffffffffffffffff.zseg", 8),
            file(".segment-ffffffffffffffffffffffffffffffff.zseg.tmp", 9),
            file(".manifest.ze.tmp", 10),
        ];
        let expected = ReachabilityExpected::from_manifest(baseline, &manifest_bytes, true)
            .expect("derive manifest reachability");

        assert_eq!(
            expected
                .manifest_referenced
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec![referenced_name.as_str()]
        );
        assert!(
            expected
                .preserved
                .iter()
                .any(|file| file.path == "writer.lock")
        );
        assert!(
            !expected
                .preserved
                .iter()
                .any(|file| file.path == ".writer.lock")
        );
        assert!(
            expected
                .control_and_unknown
                .iter()
                .any(|file| file.path == ".wal.ze.purge.tmp")
        );
        assert_eq!(expected.eligible_orphans.len(), 3);
    }
}
