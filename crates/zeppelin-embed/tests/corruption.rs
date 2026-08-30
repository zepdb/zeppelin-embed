#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::ops::Range;
use std::path::Path;

use tempfile::tempdir;
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::format::frame::{FILE_HEADER_LEN, FILE_TRAILER_LEN, FormatCheck};
use zeppelin_embed::manifest::io::{DurableLog, MANIFEST_FILE, load_manifest, open_manifest};
use zeppelin_embed::manifest::{Manifest, ManifestError, decode_manifest, encode_manifest};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed::segment::layout::{
    REGION_ALIGNMENT, REGION_ENTRY_LEN, RegionKind, SEGMENT_PREFIX_LEN, VECTOR_HEADER_LEN,
};
use zeppelin_embed::segment::reader::{SegmentReader, validate_segment_bytes};
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, encode_segment};
use zeppelin_embed::segment::{SegmentError, SegmentId, SegmentMeta};
use zeppelin_embed::vfs::StdVfs;

const DIRECTORY_START: usize = FILE_HEADER_LEN + SEGMENT_PREFIX_LEN;
const COLUMNS_ENTRY: usize = 0;
const ALIVE_ENTRY: usize = 1;
const FACTORS_ENTRY: usize = 2;
const CODES_ENTRY: usize = 3;
const RESCORE_ENTRY: usize = 4;
const CHECKSUM_TABLE_ENTRY: usize = 5;
const UNKNOWN_REGION_KIND: u16 = 65_000;

fn one_row_segment(id: SegmentId) -> Vec<u8> {
    let mut builder = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("schema"));
    builder.push_row(7, &[]).expect("row");
    let columns = builder.finish().expect("columns");
    let alive = AliveSet::new(1);
    let factors = [Bit4Factors::from_persisted(1.0, 1.0, 1.0)];
    encode_segment(SegmentBuild {
        id,
        scheme: 4,
        dims: 3,
        codes: &[0x10, 0x20],
        factors: SegmentFactors::Bit4(&factors),
        rescore: &[1.0, 2.0, 3.0],
        columns: &columns,
        alive: &alive,
    })
    .expect("segment")
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn entry_offset(index: usize) -> usize {
    DIRECTORY_START + index * REGION_ENTRY_LEN
}

fn region_offset(bytes: &[u8], index: usize) -> usize {
    read_u64(bytes, entry_offset(index) + 8) as usize
}

fn region_length(bytes: &[u8], index: usize) -> usize {
    read_u64(bytes, entry_offset(index) + 16) as usize
}

fn rewrite_file_checksum(bytes: &mut [u8]) {
    let trailer = bytes.len() - FILE_TRAILER_LEN;
    let checksum = xxh3_64(&bytes[..trailer]).to_le_bytes();
    bytes[trailer..].copy_from_slice(&checksum);
}

fn rewrite_segment_header_checksum(bytes: &mut [u8]) {
    let header_length = read_u64(bytes, 16) as usize;
    let checksum_offset = header_length - 8;
    let checksum = xxh3_64(&bytes[..checksum_offset]).to_le_bytes();
    bytes[checksum_offset..header_length].copy_from_slice(&checksum);
}

fn rewrite_segment_envelope(bytes: &mut [u8]) {
    rewrite_segment_header_checksum(bytes);
    rewrite_file_checksum(bytes);
}

fn rewrite_region_checksum(bytes: &mut [u8], index: usize) {
    let offset = region_offset(bytes, index);
    let length = region_length(bytes, index);
    let checksum = xxh3_64(&bytes[offset..offset + length]).to_le_bytes();
    let checksum_offset = entry_offset(index) + 24;
    bytes[checksum_offset..checksum_offset + 8].copy_from_slice(&checksum);
    rewrite_segment_envelope(bytes);
}

fn rewrite_framed_checksums(bytes: &mut [u8]) {
    let payload_length = read_u64(bytes, FILE_HEADER_LEN) as usize;
    let payload_start = FILE_HEADER_LEN + 8;
    let payload_end = payload_start + payload_length;
    let checksum = xxh3_64(&bytes[payload_start..payload_end]).to_le_bytes();
    bytes[payload_end..payload_end + 8].copy_from_slice(&checksum);
    rewrite_file_checksum(bytes);
}

#[derive(Clone, Copy, Debug)]
enum SegmentMutation {
    BadMagic,
    UnknownFamily,
    Version(u16),
    RegionVersion(u16),
    HeaderChecksum,
    RegionDirectoryChecksum,
    FileChecksum,
    ChunkChecksum,
    RegionOffsetPastEof,
    RegionEndOverflow,
    RegionLengthPastEof,
    OverlappingRegions,
    RegionLengthMismatch,
    RequiredRegionAbsent,
    DuplicateRegionKind,
    VectorRowCountMismatch,
    RowStrideMismatch,
    FactorStrideMismatch,
    RetiredScheme(u16),
    TransformKind,
    TransformSeed,
    WrongObject,
    TruncateZero,
    TruncateMidHeader,
    TruncateMidDirectory,
    TruncateMidRegion,
    TruncateMidTrailer,
    UnknownRegionKind,
}

impl SegmentMutation {
    fn apply(self, bytes: &mut Vec<u8>) {
        match self {
            Self::BadMagic => bytes[0] ^= 1,
            Self::UnknownFamily => bytes[8..10].copy_from_slice(&u16::MAX.to_le_bytes()),
            Self::Version(version) => bytes[10..12].copy_from_slice(&version.to_le_bytes()),
            Self::RegionVersion(version) => {
                let offset = entry_offset(CODES_ENTRY) + 2;
                bytes[offset..offset + 2].copy_from_slice(&version.to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
            Self::HeaderChecksum => {
                let checksum_offset = read_u64(bytes, 16) as usize - 8;
                bytes[checksum_offset] ^= 1;
            }
            Self::RegionDirectoryChecksum => {
                let checksum_offset = entry_offset(CODES_ENTRY) + 24;
                bytes[checksum_offset] ^= 1;
                rewrite_segment_envelope(bytes);
            }
            Self::FileChecksum => {
                let trailer = bytes.len() - FILE_TRAILER_LEN;
                bytes[trailer] ^= 1;
            }
            Self::ChunkChecksum => {
                let table = region_offset(bytes, CHECKSUM_TABLE_ENTRY);
                let count = read_u32(bytes, table) as usize;
                let checksum = (0..count)
                    .map(|index| table + 8 + index * 16)
                    .find(|offset| {
                        u16::from_le_bytes(bytes[*offset..*offset + 2].try_into().unwrap())
                            == RegionKind::VectorCodes.id()
                            && read_u32(bytes, *offset + 4) == 0
                    })
                    .expect("VectorCodes chunk zero");
                bytes[checksum + 8] ^= 1;
                rewrite_region_checksum(bytes, CHECKSUM_TABLE_ENTRY);
            }
            Self::RegionOffsetPastEof => {
                let past_eof = bytes.len().next_multiple_of(REGION_ALIGNMENT) + REGION_ALIGNMENT;
                let offset = entry_offset(COLUMNS_ENTRY) + 8;
                bytes[offset..offset + 8].copy_from_slice(&(past_eof as u64).to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
            Self::RegionEndOverflow => {
                let offset = entry_offset(COLUMNS_ENTRY) + 16;
                bytes[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
            Self::RegionLengthPastEof => {
                let length = bytes.len() as u64;
                let offset = entry_offset(COLUMNS_ENTRY) + 16;
                bytes[offset..offset + 8].copy_from_slice(&length.to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
            Self::OverlappingRegions => {
                let first = read_u64(bytes, entry_offset(COLUMNS_ENTRY) + 8);
                let offset = entry_offset(ALIVE_ENTRY) + 8;
                bytes[offset..offset + 8].copy_from_slice(&first.to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
            Self::RegionLengthMismatch => {
                let offset = entry_offset(CODES_ENTRY) + 16;
                bytes[offset..offset + 8]
                    .copy_from_slice(&(VECTOR_HEADER_LEN as u64).to_le_bytes());
                rewrite_region_checksum(bytes, CODES_ENTRY);
            }
            Self::RequiredRegionAbsent => {
                let offset = entry_offset(CODES_ENTRY);
                bytes[offset..offset + 2].copy_from_slice(&(UNKNOWN_REGION_KIND - 1).to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
            Self::DuplicateRegionKind => {
                let offset = entry_offset(ALIVE_ENTRY);
                bytes[offset..offset + 2].copy_from_slice(&RegionKind::Columns.id().to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
            Self::VectorRowCountMismatch => {
                let offset = region_offset(bytes, CODES_ENTRY) + 24;
                bytes[offset..offset + 4].copy_from_slice(&2_u32.to_le_bytes());
                rewrite_region_checksum(bytes, CODES_ENTRY);
            }
            Self::RowStrideMismatch => {
                let offset = region_offset(bytes, CODES_ENTRY) + 8;
                bytes[offset..offset + 4].copy_from_slice(&3_u32.to_le_bytes());
                rewrite_region_checksum(bytes, CODES_ENTRY);
            }
            Self::FactorStrideMismatch => {
                let offset = region_offset(bytes, FACTORS_ENTRY) + 12;
                bytes[offset..offset + 2].copy_from_slice(&8_u16.to_le_bytes());
                rewrite_region_checksum(bytes, FACTORS_ENTRY);
            }
            Self::RetiredScheme(scheme) => {
                bytes[56..58].copy_from_slice(&scheme.to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
            Self::TransformKind => {
                let offset = region_offset(bytes, CODES_ENTRY) + 14;
                bytes[offset..offset + 2].copy_from_slice(&1_u16.to_le_bytes());
                rewrite_region_checksum(bytes, CODES_ENTRY);
            }
            Self::TransformSeed => {
                let offset = region_offset(bytes, CODES_ENTRY) + 16;
                bytes[offset..offset + 8].copy_from_slice(&1_u64.to_le_bytes());
                rewrite_region_checksum(bytes, CODES_ENTRY);
            }
            Self::WrongObject => {
                bytes[FILE_HEADER_LEN] ^= 1;
                rewrite_segment_envelope(bytes);
            }
            Self::TruncateZero => bytes.truncate(0),
            Self::TruncateMidHeader => bytes.truncate(FILE_HEADER_LEN / 2),
            Self::TruncateMidDirectory => {
                bytes.truncate(DIRECTORY_START + REGION_ENTRY_LEN / 2);
            }
            Self::TruncateMidRegion => {
                let offset = region_offset(bytes, COLUMNS_ENTRY);
                let length = region_length(bytes, COLUMNS_ENTRY);
                bytes.truncate(offset + length / 2);
            }
            Self::TruncateMidTrailer => bytes.truncate(bytes.len() - FILE_TRAILER_LEN / 2),
            Self::UnknownRegionKind => {
                let offset = entry_offset(RESCORE_ENTRY);
                bytes[offset..offset + 2].copy_from_slice(&UNKNOWN_REGION_KIND.to_le_bytes());
                rewrite_segment_envelope(bytes);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum SegmentAction {
    Open,
    ValidateAll,
    Bit4Codes,
    Bit4Factors,
    VectorCodesChunk,
    ValidateUnknown,
}

#[derive(Clone, Copy, Debug)]
enum ExpectedSegment {
    Format {
        check: FormatCheck,
        artifact: &'static str,
        detail: &'static str,
    },
    Geometry(&'static str),
    MissingRegion(RegionKind),
    WrongObject,
    UnknownRegion(u16),
}

#[derive(Clone, Copy, Debug)]
struct SegmentCase {
    name: &'static str,
    mutation: SegmentMutation,
    action: SegmentAction,
    expected: ExpectedSegment,
}

const SEGMENT_CASES: &[SegmentCase] = &[
    SegmentCase {
        name: "segment_bad_magic",
        mutation: SegmentMutation::BadMagic,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::Magic,
            artifact: "damaged.zseg",
            detail: "expected",
        },
    },
    SegmentCase {
        name: "segment_unknown_family_id",
        mutation: SegmentMutation::UnknownFamily,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::Family,
            artifact: "damaged.zseg",
            detail: "got 65535",
        },
    },
    SegmentCase {
        name: "segment_version_below_range",
        mutation: SegmentMutation::Version(0),
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::Version,
            artifact: "damaged.zseg",
            detail: "outside accepted range",
        },
    },
    SegmentCase {
        name: "segment_version_above_range",
        mutation: SegmentMutation::Version(2),
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::Version,
            artifact: "damaged.zseg",
            detail: "outside accepted range",
        },
    },
    SegmentCase {
        name: "segment_region_version_below_range",
        mutation: SegmentMutation::RegionVersion(0),
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::Version,
            artifact: "damaged.zseg",
            detail: "outside accepted range",
        },
    },
    SegmentCase {
        name: "segment_region_version_above_range",
        mutation: SegmentMutation::RegionVersion(2),
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::Version,
            artifact: "damaged.zseg",
            detail: "outside accepted range",
        },
    },
    SegmentCase {
        name: "segment_header_checksum_wrong",
        mutation: SegmentMutation::HeaderChecksum,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::BlockChecksum,
            artifact: "damaged.zseg",
            detail: "header expected",
        },
    },
    SegmentCase {
        name: "segment_region_directory_checksum_wrong",
        mutation: SegmentMutation::RegionDirectoryChecksum,
        action: SegmentAction::ValidateAll,
        expected: ExpectedSegment::Format {
            check: FormatCheck::BlockChecksum,
            artifact: "region-3",
            detail: "expected",
        },
    },
    SegmentCase {
        name: "segment_file_trailer_checksum_wrong",
        mutation: SegmentMutation::FileChecksum,
        action: SegmentAction::ValidateAll,
        expected: ExpectedSegment::Format {
            check: FormatCheck::FileChecksum,
            artifact: "segment:",
            detail: "expected",
        },
    },
    SegmentCase {
        name: "segment_per_chunk_checksum_wrong",
        mutation: SegmentMutation::ChunkChecksum,
        action: SegmentAction::VectorCodesChunk,
        expected: ExpectedSegment::Format {
            check: FormatCheck::BlockChecksum,
            artifact: "chunk-0",
            detail: "expected",
        },
    },
    SegmentCase {
        name: "segment_region_offset_past_eof",
        mutation: SegmentMutation::RegionOffsetPastEof,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::BlockLength,
            artifact: "damaged.zseg",
            detail: "maximum",
        },
    },
    SegmentCase {
        name: "segment_region_offset_plus_length_overflows_u64",
        mutation: SegmentMutation::RegionEndOverflow,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Geometry("region end overflow"),
    },
    SegmentCase {
        name: "segment_region_length_past_eof",
        mutation: SegmentMutation::RegionLengthPastEof,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::BlockLength,
            artifact: "damaged.zseg",
            detail: "maximum",
        },
    },
    SegmentCase {
        name: "segment_two_regions_overlap",
        mutation: SegmentMutation::OverlappingRegions,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Geometry("before previous end"),
    },
    SegmentCase {
        name: "segment_region_declared_length_disagrees_with_geometry",
        mutation: SegmentMutation::RegionLengthMismatch,
        action: SegmentAction::Bit4Codes,
        expected: ExpectedSegment::Geometry("Bit4 code stride/length"),
    },
    SegmentCase {
        name: "segment_required_region_absent",
        mutation: SegmentMutation::RequiredRegionAbsent,
        action: SegmentAction::Bit4Codes,
        expected: ExpectedSegment::MissingRegion(RegionKind::VectorCodes),
    },
    SegmentCase {
        name: "segment_duplicate_region_kind",
        mutation: SegmentMutation::DuplicateRegionKind,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Geometry("duplicate region kind"),
    },
    SegmentCase {
        name: "segment_header_vector_row_count_mismatch",
        mutation: SegmentMutation::VectorRowCountMismatch,
        action: SegmentAction::Bit4Codes,
        expected: ExpectedSegment::Geometry("header scheme/dims/rows"),
    },
    SegmentCase {
        name: "segment_row_stride_inconsistent_with_dims",
        mutation: SegmentMutation::RowStrideMismatch,
        action: SegmentAction::Bit4Codes,
        expected: ExpectedSegment::Geometry("Bit4 code stride/length"),
    },
    SegmentCase {
        name: "segment_factor_stride_inconsistent_with_scheme",
        mutation: SegmentMutation::FactorStrideMismatch,
        action: SegmentAction::Bit4Factors,
        expected: ExpectedSegment::Geometry("Bit4 factor stride"),
    },
    SegmentCase {
        name: "segment_retired_scheme_3",
        mutation: SegmentMutation::RetiredScheme(3),
        action: SegmentAction::Open,
        expected: ExpectedSegment::Geometry("scheme id 3 is permanently retired"),
    },
    SegmentCase {
        name: "segment_retired_scheme_5",
        mutation: SegmentMutation::RetiredScheme(5),
        action: SegmentAction::Open,
        expected: ExpectedSegment::Geometry("scheme id 5 is permanently retired"),
    },
    SegmentCase {
        name: "segment_transform_kind_nonzero",
        mutation: SegmentMutation::TransformKind,
        action: SegmentAction::Bit4Codes,
        expected: ExpectedSegment::Geometry("v1 transform must be identity/0"),
    },
    SegmentCase {
        name: "segment_transform_seed_nonzero",
        mutation: SegmentMutation::TransformSeed,
        action: SegmentAction::Bit4Codes,
        expected: ExpectedSegment::Geometry("v1 transform must be identity/0"),
    },
    SegmentCase {
        name: "segment_wrong_object_identity",
        mutation: SegmentMutation::WrongObject,
        action: SegmentAction::Open,
        expected: ExpectedSegment::WrongObject,
    },
    SegmentCase {
        name: "segment_zero_byte_file",
        mutation: SegmentMutation::TruncateZero,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::Length,
            artifact: "damaged.zseg",
            detail: "empty segment file",
        },
    },
    SegmentCase {
        name: "segment_truncated_mid_header",
        mutation: SegmentMutation::TruncateMidHeader,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::Length,
            artifact: "damaged.zseg",
            detail: "header bytes",
        },
    },
    SegmentCase {
        name: "segment_truncated_mid_directory",
        mutation: SegmentMutation::TruncateMidDirectory,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::FileLength,
            artifact: "damaged.zseg",
            detail: "declared",
        },
    },
    SegmentCase {
        name: "segment_truncated_mid_region",
        mutation: SegmentMutation::TruncateMidRegion,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::FileLength,
            artifact: "damaged.zseg",
            detail: "declared",
        },
    },
    SegmentCase {
        name: "segment_truncated_mid_trailer",
        mutation: SegmentMutation::TruncateMidTrailer,
        action: SegmentAction::Open,
        expected: ExpectedSegment::Format {
            check: FormatCheck::FileLength,
            artifact: "damaged.zseg",
            detail: "declared",
        },
    },
    SegmentCase {
        name: "segment_unknown_region_kind_skips_cleanly",
        mutation: SegmentMutation::UnknownRegionKind,
        action: SegmentAction::ValidateUnknown,
        expected: ExpectedSegment::UnknownRegion(UNKNOWN_REGION_KIND),
    },
];

fn run_segment_action(
    path: &Path,
    id: SegmentId,
    action: SegmentAction,
) -> Result<Option<Vec<u16>>, SegmentError> {
    let reader = SegmentReader::open(&StdVfs, path, id)?;
    match action {
        SegmentAction::Open => Ok(None),
        SegmentAction::ValidateAll => reader.validate_all().map(|()| None),
        SegmentAction::Bit4Codes => reader.bit4_codes().map(|_| None),
        SegmentAction::Bit4Factors => reader.bit4_factors().map(|_| None),
        SegmentAction::VectorCodesChunk => reader
            .region_chunk(RegionKind::VectorCodes, 0)
            .map(|_| None),
        SegmentAction::ValidateUnknown => {
            reader.validate_all()?;
            Ok(Some(reader.unknown_region_ids()))
        }
    }
}

fn assert_segment_result(case: SegmentCase, result: Result<Option<Vec<u16>>, SegmentError>) {
    match (case.expected, result) {
        (
            ExpectedSegment::Format {
                check,
                artifact,
                detail,
            },
            Err(SegmentError::Format(error)),
        ) => {
            assert_eq!(error.check(), check, "{} returned {error}", case.name);
            assert!(
                error.artifact().contains(artifact),
                "{} named artifact {}",
                case.name,
                error.artifact()
            );
            assert!(
                error.detail().contains(detail),
                "{} returned detail {}",
                case.name,
                error.detail()
            );
        }
        (ExpectedSegment::Geometry(detail), Err(SegmentError::Geometry(actual))) => {
            assert!(
                actual.contains(detail),
                "{} returned geometry {actual}",
                case.name
            );
        }
        (ExpectedSegment::MissingRegion(expected), Err(SegmentError::MissingRegion(actual))) => {
            assert_eq!(
                actual, expected,
                "{} returned wrong missing region",
                case.name
            );
        }
        (ExpectedSegment::WrongObject, Err(SegmentError::WrongObject { .. })) => {}
        (ExpectedSegment::UnknownRegion(expected), Ok(Some(actual))) => {
            assert_eq!(
                actual,
                vec![expected],
                "{} returned wrong unknown ids",
                case.name
            );
        }
        (expected, actual) => panic!("{} expected {expected:?}, got {actual:?}", case.name),
    }
}

#[test]
fn segment_corruption_matrix_returns_the_specific_typed_result() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("damaged.zseg");
    let id = SegmentId::new(7, [7; 10]);
    let valid = one_row_segment(id);
    assert_eq!(
        validate_segment_bytes(&valid).expect("valid segment").id,
        id
    );

    for case in SEGMENT_CASES {
        let mut damaged = valid.clone();
        case.mutation.apply(&mut damaged);
        fs::write(&path, damaged).expect("damaged segment write");
        assert_segment_result(*case, run_segment_action(&path, id, case.action));
    }
}

#[derive(Clone, Copy, Debug)]
enum ManifestMutation {
    BadMagic,
    Version(u16),
    BlockChecksum,
    FileChecksum,
    LogSeqAhead,
    MissingSegmentReference,
    TruncateZero,
    TruncateMidHeader,
    TruncateMidBlockLength,
    TruncateMidPayload,
    TruncateMidBlockChecksum,
    TruncateMidTrailer,
}

impl ManifestMutation {
    fn apply(self, bytes: &mut Vec<u8>) {
        match self {
            Self::BadMagic => bytes[0] ^= 1,
            Self::Version(version) => bytes[10..12].copy_from_slice(&version.to_le_bytes()),
            Self::BlockChecksum => {
                let checksum = bytes.len() - FILE_TRAILER_LEN - 8;
                bytes[checksum] ^= 1;
                rewrite_file_checksum(bytes);
            }
            Self::FileChecksum => {
                let trailer = bytes.len() - FILE_TRAILER_LEN;
                bytes[trailer] ^= 1;
            }
            Self::LogSeqAhead => {
                let log_seq = FILE_HEADER_LEN + 8 + 8;
                bytes[log_seq..log_seq + 8].copy_from_slice(&2_u64.to_le_bytes());
                rewrite_framed_checksums(bytes);
            }
            Self::MissingSegmentReference => {
                let segment_id = FILE_HEADER_LEN + 8 + 32 + 24;
                bytes[segment_id] ^= 1;
                rewrite_framed_checksums(bytes);
            }
            Self::TruncateZero => bytes.truncate(0),
            Self::TruncateMidHeader => bytes.truncate(FILE_HEADER_LEN / 2),
            Self::TruncateMidBlockLength => bytes.truncate(FILE_HEADER_LEN + 4),
            Self::TruncateMidPayload => {
                let payload_length = read_u64(bytes, FILE_HEADER_LEN) as usize;
                bytes.truncate(FILE_HEADER_LEN + 8 + payload_length / 2);
            }
            Self::TruncateMidBlockChecksum => {
                let payload_length = read_u64(bytes, FILE_HEADER_LEN) as usize;
                bytes.truncate(FILE_HEADER_LEN + 8 + payload_length + 4);
            }
            Self::TruncateMidTrailer => bytes.truncate(bytes.len() - FILE_TRAILER_LEN / 2),
        }
    }
}

#[derive(Debug)]
enum ManifestMutationExpectation {
    Field {
        name: &'static str,
        target: Range<usize>,
        neighbours: Vec<(&'static str, Range<usize>)>,
    },
    Truncation {
        expected_len: usize,
    },
}

#[derive(Debug)]
struct ManifestMutationGuard {
    before: Vec<u8>,
    expectation: ManifestMutationExpectation,
}

impl ManifestMutationGuard {
    fn new(mutation: ManifestMutation, bytes: &[u8], manifest: &Manifest) -> Self {
        let payload_start = FILE_HEADER_LEN + 8;
        let expectation = match mutation {
            ManifestMutation::BadMagic => ManifestMutationExpectation::Field {
                name: "manifest magic prefix",
                target: 0..4,
                neighbours: vec![("manifest magic suffix", 4..8), ("format family", 8..10)],
            },
            ManifestMutation::Version(_) => ManifestMutationExpectation::Field {
                name: "format version",
                target: 10..12,
                neighbours: vec![("format family", 8..10), ("format flags", 12..16)],
            },
            ManifestMutation::BlockChecksum => {
                let payload_len = read_u64(bytes, FILE_HEADER_LEN) as usize;
                let checksum = payload_start + payload_len;
                ManifestMutationExpectation::Field {
                    name: "manifest block checksum",
                    target: checksum..checksum + 8,
                    neighbours: vec![("payload tail", checksum - 4..checksum)],
                }
            }
            ManifestMutation::FileChecksum => {
                let checksum = bytes.len() - FILE_TRAILER_LEN;
                ManifestMutationExpectation::Field {
                    name: "manifest file checksum",
                    target: checksum..bytes.len(),
                    neighbours: vec![("block checksum", checksum - 8..checksum)],
                }
            }
            ManifestMutation::LogSeqAhead => ManifestMutationExpectation::Field {
                name: "manifest log sequence",
                target: payload_start + 8..payload_start + 16,
                neighbours: vec![
                    ("manifest generation", payload_start..payload_start + 8),
                    ("segment count", payload_start + 16..payload_start + 20),
                ],
            },
            ManifestMutation::MissingSegmentReference => {
                let segment = manifest.segments.first().expect("manifest segment");
                let segment_start = bytes
                    .windows(segment.id.as_bytes().len())
                    .position(|window| window == segment.id.as_bytes())
                    .expect("encoded segment id");
                ManifestMutationExpectation::Field {
                    name: "segment-id prefix",
                    target: segment_start..segment_start + 4,
                    neighbours: vec![
                        ("epoch alias suffix", segment_start - 4..segment_start),
                        ("segment-id successor", segment_start + 4..segment_start + 8),
                    ],
                }
            }
            ManifestMutation::TruncateZero => {
                ManifestMutationExpectation::Truncation { expected_len: 0 }
            }
            ManifestMutation::TruncateMidHeader => ManifestMutationExpectation::Truncation {
                expected_len: FILE_HEADER_LEN / 2,
            },
            ManifestMutation::TruncateMidBlockLength => ManifestMutationExpectation::Truncation {
                expected_len: FILE_HEADER_LEN + 4,
            },
            ManifestMutation::TruncateMidPayload => {
                let payload_len = read_u64(bytes, FILE_HEADER_LEN) as usize;
                ManifestMutationExpectation::Truncation {
                    expected_len: payload_start + payload_len / 2,
                }
            }
            ManifestMutation::TruncateMidBlockChecksum => {
                let payload_len = read_u64(bytes, FILE_HEADER_LEN) as usize;
                ManifestMutationExpectation::Truncation {
                    expected_len: payload_start + payload_len + 4,
                }
            }
            ManifestMutation::TruncateMidTrailer => ManifestMutationExpectation::Truncation {
                expected_len: bytes.len() - FILE_TRAILER_LEN / 2,
            },
        };
        Self {
            before: bytes.to_vec(),
            expectation,
        }
    }

    fn assert_applied(&self, case: &str, after: &[u8]) {
        match &self.expectation {
            ManifestMutationExpectation::Field {
                name,
                target,
                neighbours,
            } => {
                assert_eq!(
                    after.len(),
                    self.before.len(),
                    "{case} mutation guard: byte length moved while changing {name}"
                );
                assert_ne!(
                    &self.before[target.clone()],
                    &after[target.clone()],
                    "{case} mutation guard: intended {name} field at {}..{} was unchanged",
                    target.start,
                    target.end
                );
                for (neighbour_name, neighbour) in neighbours {
                    assert_eq!(
                        &self.before[neighbour.clone()],
                        &after[neighbour.clone()],
                        "{case} mutation guard: neighbouring {neighbour_name} field at {}..{} moved",
                        neighbour.start,
                        neighbour.end
                    );
                }
            }
            ManifestMutationExpectation::Truncation { expected_len } => {
                assert_eq!(
                    after.len(),
                    *expected_len,
                    "{case} mutation guard: truncation ended at the wrong offset"
                );
                assert_eq!(
                    after,
                    &self.before[..*expected_len],
                    "{case} mutation guard: bytes before the truncation offset moved"
                );
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ManifestAction {
    Decode,
    Load { durable: u64 },
    Open { durable: u64 },
}

#[derive(Clone, Copy, Debug)]
enum ExpectedManifest {
    Format {
        check: FormatCheck,
        detail: &'static str,
    },
    AheadOfLog {
        snapshot: u64,
        durable: u64,
    },
    MissingSegment,
}

#[derive(Clone, Copy, Debug)]
struct ManifestCase {
    name: &'static str,
    mutation: ManifestMutation,
    action: ManifestAction,
    expected: ExpectedManifest,
}

const MANIFEST_CASES: &[ManifestCase] = &[
    ManifestCase {
        name: "manifest_bad_magic",
        mutation: ManifestMutation::BadMagic,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::Magic,
            detail: "expected",
        },
    },
    ManifestCase {
        name: "manifest_wrong_version",
        mutation: ManifestMutation::Version(1),
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::Version,
            detail: "outside accepted range",
        },
    },
    ManifestCase {
        name: "manifest_block_checksum_mismatch",
        mutation: ManifestMutation::BlockChecksum,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::BlockChecksum,
            detail: "expected",
        },
    },
    ManifestCase {
        name: "manifest_file_checksum_mismatch",
        mutation: ManifestMutation::FileChecksum,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::FileChecksum,
            detail: "expected",
        },
    },
    ManifestCase {
        name: "manifest_log_seq_ahead_of_log",
        mutation: ManifestMutation::LogSeqAhead,
        action: ManifestAction::Load { durable: 1 },
        expected: ExpectedManifest::AheadOfLog {
            snapshot: 2,
            durable: 1,
        },
    },
    ManifestCase {
        name: "manifest_segment_reference_does_not_resolve",
        mutation: ManifestMutation::MissingSegmentReference,
        action: ManifestAction::Open { durable: 1 },
        expected: ExpectedManifest::MissingSegment,
    },
    ManifestCase {
        name: "manifest_zero_byte_file",
        mutation: ManifestMutation::TruncateZero,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::Length,
            detail: "header bytes",
        },
    },
    ManifestCase {
        name: "manifest_truncated_mid_header",
        mutation: ManifestMutation::TruncateMidHeader,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::Length,
            detail: "header bytes",
        },
    },
    ManifestCase {
        name: "manifest_truncated_mid_block_length",
        mutation: ManifestMutation::TruncateMidBlockLength,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::FileLength,
            detail: "declared",
        },
    },
    ManifestCase {
        name: "manifest_truncated_mid_payload",
        mutation: ManifestMutation::TruncateMidPayload,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::FileLength,
            detail: "declared",
        },
    },
    ManifestCase {
        name: "manifest_truncated_mid_block_checksum",
        mutation: ManifestMutation::TruncateMidBlockChecksum,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::FileLength,
            detail: "declared",
        },
    },
    ManifestCase {
        name: "manifest_truncated_mid_trailer",
        mutation: ManifestMutation::TruncateMidTrailer,
        action: ManifestAction::Decode,
        expected: ExpectedManifest::Format {
            check: FormatCheck::FileLength,
            detail: "declared",
        },
    },
];

struct FixedLog(u64);

impl DurableLog for FixedLog {
    fn durable_end(&self) -> u64 {
        self.0
    }
}

fn run_manifest_action(
    directory: &Path,
    bytes: &[u8],
    action: ManifestAction,
) -> Result<(), ManifestError> {
    let path = directory.join(MANIFEST_FILE);
    fs::write(&path, bytes).expect("damaged manifest write");
    match action {
        ManifestAction::Decode => decode_manifest("damaged.manifest", bytes).map(|_| ()),
        ManifestAction::Load { durable } => load_manifest(&StdVfs, &path, durable).map(|_| ()),
        ManifestAction::Open { durable } => {
            open_manifest(&StdVfs, directory, &FixedLog(durable), &HashSet::new()).map(|_| ())
        }
    }
}

fn assert_manifest_result(case: ManifestCase, result: Result<(), ManifestError>) {
    match (case.expected, result) {
        (ExpectedManifest::Format { check, detail }, Err(ManifestError::Format(error))) => {
            assert_eq!(error.check(), check, "{} returned {error}", case.name);
            assert!(
                error.detail().contains(detail),
                "{} returned detail {}",
                case.name,
                error.detail()
            );
        }
        (
            ExpectedManifest::AheadOfLog { snapshot, durable },
            Err(ManifestError::AheadOfLog {
                snapshot: actual_snapshot,
                durable: actual_durable,
            }),
        ) => {
            assert_eq!(actual_snapshot, snapshot, "{} snapshot", case.name);
            assert_eq!(actual_durable, durable, "{} durable end", case.name);
        }
        (
            ExpectedManifest::MissingSegment,
            Err(ManifestError::Segment(SegmentError::Io { source, .. })),
        ) => {
            assert_eq!(
                source.kind(),
                ErrorKind::NotFound,
                "{} returned {source}",
                case.name
            );
        }
        (expected, actual) => panic!("{} expected {expected:?}, got {actual:?}", case.name),
    }
}

#[test]
fn manifest_corruption_matrix_returns_the_specific_typed_error() {
    let directory = tempdir().expect("tempdir");
    let id = SegmentId::new(8, [8; 10]);
    let segment = one_row_segment(id);
    let meta: SegmentMeta = validate_segment_bytes(&segment).expect("valid segment");
    fs::write(directory.path().join(id.file_name()), &segment).expect("valid segment write");
    let manifest = Manifest {
        generation: 1,
        log_seq: 1,
        segments: vec![meta],
        epochs: Vec::new(),
        epoch_alias: None,
        schema: Schema::new(Vec::new()).expect("schema"),
    };
    let valid = encode_manifest(&manifest).expect("manifest");
    assert_eq!(
        decode_manifest("valid.manifest", &valid).expect("valid"),
        manifest
    );

    for case in MANIFEST_CASES {
        let mut damaged = valid.clone();
        let guard = ManifestMutationGuard::new(case.mutation, &valid, &manifest);
        case.mutation.apply(&mut damaged);
        guard.assert_applied(case.name, &damaged);
        assert_manifest_result(
            *case,
            run_manifest_action(directory.path(), &damaged, case.action),
        );
    }
}
