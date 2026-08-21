//! In-memory segment assembly and atomic publication.

use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::xxh3_64;

use crate::format::frame::{FILE_HEADER_LEN, FILE_MAGIC, FILE_TRAILER_LEN};
use crate::format::{FormatFamily, FormatRegistry};
use crate::meta::{AliveSet, ColumnStore};
use crate::quant::Bit4Factors;
use crate::vfs::{PART_A_ORDERED_SYNC, Vfs};

use super::layout::{
    CHECKSUM_CHUNK_BYTES, Int8Factors, REGION_ALIGNMENT, REGION_ENTRY_LEN, RegionEntry, RegionKind,
    SEGMENT_PREFIX_LEN, VECTOR_HEADER_LEN, VectorHeader, align_up, encode_alive, encode_columns,
    encode_vector_header,
};
use super::{SegmentError, SegmentId, SegmentMeta};

/// Borrowed factor records for a supported per-segment quantization scheme.
#[derive(Clone, Copy, Debug)]
pub enum SegmentFactors<'a> {
    /// Permanent 12-byte Bit4 factor records.
    Bit4(&'a [Bit4Factors]),
    /// Permanent 8-byte affine Int8 factor records.
    Int8(&'a [Int8Factors]),
}

/// Complete in-memory data sealed into one immutable segment.
#[derive(Clone, Copy, Debug)]
pub struct SegmentBuild<'a> {
    /// Sortable immutable identity.
    pub id: SegmentId,
    /// Permanent per-segment quantization scheme id.
    pub scheme: u16,
    /// Logical vector dimension.
    pub dims: u32,
    /// Contiguous row-major packed codes with no row padding.
    pub codes: &'a [u8],
    /// Contiguous factor records matching `scheme`.
    pub factors: SegmentFactors<'a>,
    /// Contiguous row-major f32 exact-rescore source.
    pub rescore: &'a [f32],
    /// Typed metadata arrays aligned to row ids.
    pub columns: &'a ColumnStore,
    /// Alive/tombstone state aligned to row ids.
    pub alive: &'a AliveSet,
}

struct RegionBytes {
    kind: RegionKind,
    family: FormatFamily,
    bytes: Vec<u8>,
}

/// Encodes one complete segment in RAM, validating every cross-region shape first.
pub fn encode_segment(build: SegmentBuild<'_>) -> Result<Vec<u8>, SegmentError> {
    let row_count = build.columns.row_count();
    if build.alive.row_count() != row_count {
        return Err(SegmentError::Geometry(format!(
            "alive rows {}, column rows {row_count}",
            build.alive.row_count()
        )));
    }
    FormatRegistry::require_scheme(build.scheme)
        .map_err(|error| SegmentError::Geometry(error.to_string()))?;
    let dims = usize::try_from(build.dims)
        .map_err(|_| SegmentError::Geometry("dims exceeds usize".to_owned()))?;
    let rows = row_count as usize;
    let (row_stride, factor_stride, factor_bytes) = encode_factors(build, rows)?;
    let expected_codes = row_stride
        .checked_mul(rows)
        .ok_or_else(|| SegmentError::Geometry("code length overflow".to_owned()))?;
    if build.codes.len() != expected_codes {
        return Err(SegmentError::Geometry(format!(
            "code bytes {}, expected {expected_codes}",
            build.codes.len()
        )));
    }
    let expected_rescore = dims
        .checked_mul(rows)
        .ok_or_else(|| SegmentError::Geometry("rescore length overflow".to_owned()))?;
    if build.rescore.len() != expected_rescore {
        return Err(SegmentError::Geometry(format!(
            "rescore values {}, expected {expected_rescore}",
            build.rescore.len()
        )));
    }

    let common_header = VectorHeader {
        scheme: build.scheme,
        vector_space_id: 0,
        dims: build.dims,
        row_stride_bytes: u32::try_from(row_stride)
            .map_err(|_| SegmentError::Geometry("row stride exceeds u32".to_owned()))?,
        factor_stride_bytes: factor_stride,
        transform_kind: 0,
        transform_seed: 0,
        row_count,
        reserved: 0,
    };
    common_header.validate()?;
    let mut codes = Vec::with_capacity(VECTOR_HEADER_LEN.saturating_add(build.codes.len()));
    encode_vector_header(common_header, &mut codes);
    codes.extend_from_slice(build.codes);

    let mut factors = Vec::with_capacity(VECTOR_HEADER_LEN.saturating_add(factor_bytes.len()));
    encode_vector_header(common_header, &mut factors);
    factors.extend_from_slice(&factor_bytes);

    let rescore_stride = dims
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| SegmentError::Geometry("rescore stride overflow".to_owned()))?;
    let rescore_header = VectorHeader {
        scheme: 0,
        vector_space_id: 0,
        dims: build.dims,
        row_stride_bytes: u32::try_from(rescore_stride)
            .map_err(|_| SegmentError::Geometry("rescore stride exceeds u32".to_owned()))?,
        factor_stride_bytes: 0,
        transform_kind: 0,
        transform_seed: 0,
        row_count,
        reserved: 0,
    };
    let mut rescore =
        Vec::with_capacity(VECTOR_HEADER_LEN.saturating_add(build.rescore.len().saturating_mul(4)));
    encode_vector_header(rescore_header, &mut rescore);
    for value in build.rescore {
        rescore.extend_from_slice(&value.to_bits().to_le_bytes());
    }

    let regions = vec![
        RegionBytes {
            kind: RegionKind::Columns,
            family: FormatFamily::Columns,
            bytes: encode_columns(build.columns)?,
        },
        RegionBytes {
            kind: RegionKind::Alive,
            family: FormatFamily::Alive,
            bytes: encode_alive(build.alive),
        },
        RegionBytes {
            kind: RegionKind::VectorFactors,
            family: FormatFamily::VectorFactors,
            bytes: factors,
        },
        RegionBytes {
            kind: RegionKind::VectorCodes,
            family: FormatFamily::VectorCodes,
            bytes: codes,
        },
        RegionBytes {
            kind: RegionKind::VectorRescore,
            family: FormatFamily::VectorRescore,
            bytes: rescore,
        },
    ];
    encode_regions(build, &regions)
}

fn encode_factors(
    build: SegmentBuild<'_>,
    rows: usize,
) -> Result<(usize, u16, Vec<u8>), SegmentError> {
    match build.factors {
        SegmentFactors::Bit4(factors) => {
            if build.scheme != 4 {
                return Err(SegmentError::Geometry(format!(
                    "Bit4 factors require scheme 4, got {}",
                    build.scheme
                )));
            }
            if factors.len() != rows {
                return Err(SegmentError::Geometry(format!(
                    "factor rows {}, expected {rows}",
                    factors.len()
                )));
            }
            let row_stride = (build.dims as usize).div_ceil(2);
            let mut bytes = Vec::with_capacity(factors.len().saturating_mul(12));
            for factor in factors {
                for field in factor.persisted_fields() {
                    bytes.extend_from_slice(&field.to_bits().to_le_bytes());
                }
            }
            Ok((row_stride, 12, bytes))
        }
        SegmentFactors::Int8(factors) => {
            if build.scheme != 2 {
                return Err(SegmentError::Geometry(format!(
                    "Int8 factors require scheme 2, got {}",
                    build.scheme
                )));
            }
            if factors.len() != rows {
                return Err(SegmentError::Geometry(format!(
                    "factor rows {}, expected {rows}",
                    factors.len()
                )));
            }
            let mut bytes = Vec::with_capacity(factors.len().saturating_mul(8));
            for factor in factors {
                bytes.extend_from_slice(&factor.scale.to_bits().to_le_bytes());
                bytes.extend_from_slice(&factor.offset.to_bits().to_le_bytes());
            }
            Ok((build.dims as usize, 8, bytes))
        }
    }
}

fn encode_regions(
    build: SegmentBuild<'_>,
    regions: &[RegionBytes],
) -> Result<Vec<u8>, SegmentError> {
    let region_count = regions.len().saturating_add(1);
    let header_length = FILE_HEADER_LEN
        .checked_add(SEGMENT_PREFIX_LEN)
        .and_then(|value| value.checked_add(region_count.saturating_mul(REGION_ENTRY_LEN)))
        .and_then(|value| value.checked_add(8))
        .ok_or_else(|| SegmentError::Geometry("header length overflow".to_owned()))?;
    let mut next_offset = align_up(header_length, REGION_ALIGNMENT)?;
    let mut entries = Vec::with_capacity(region_count);
    for region in regions.iter() {
        let length = u64::try_from(region.bytes.len())
            .map_err(|_| SegmentError::Geometry("region length exceeds u64".to_owned()))?;
        entries.push(RegionEntry {
            kind: region.kind.id(),
            version: current_version(region.family)?,
            reserved: 0,
            offset: next_offset as u64,
            length,
            checksum: xxh3_64(&region.bytes),
        });
        next_offset = next_offset
            .checked_add(region.bytes.len())
            .ok_or_else(|| SegmentError::Geometry("region end overflow".to_owned()))?;
        next_offset = align_up(next_offset, REGION_ALIGNMENT)?;
    }

    let checksum_table = encode_checksum_table(regions);
    entries.push(RegionEntry {
        kind: RegionKind::ChecksumTable.id(),
        version: current_version(FormatFamily::ChecksumTable)?,
        reserved: 0,
        offset: next_offset as u64,
        length: checksum_table.len() as u64,
        checksum: xxh3_64(&checksum_table),
    });
    let file_without_trailer = next_offset
        .checked_add(checksum_table.len())
        .ok_or_else(|| SegmentError::Geometry("file length overflow".to_owned()))?;
    let file_length = file_without_trailer
        .checked_add(FILE_TRAILER_LEN)
        .ok_or_else(|| SegmentError::Geometry("file trailer overflow".to_owned()))?;

    let mut output = Vec::with_capacity(file_length);
    output.extend_from_slice(&FILE_MAGIC);
    output.extend_from_slice(&FormatFamily::Segment.id().to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&(header_length as u64).to_le_bytes());
    output.extend_from_slice(&(file_length as u64).to_le_bytes());
    output.extend_from_slice(build.id.as_bytes());
    output.extend_from_slice(&build.columns.row_count().to_le_bytes());
    output.extend_from_slice(&(region_count as u16).to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&build.scheme.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&build.dims.to_le_bytes());
    for entry in &entries {
        output.extend_from_slice(&entry.kind.to_le_bytes());
        output.extend_from_slice(&entry.version.to_le_bytes());
        output.extend_from_slice(&entry.reserved.to_le_bytes());
        output.extend_from_slice(&entry.offset.to_le_bytes());
        output.extend_from_slice(&entry.length.to_le_bytes());
        output.extend_from_slice(&entry.checksum.to_le_bytes());
    }
    let header_checksum = xxh3_64(&output);
    output.extend_from_slice(&header_checksum.to_le_bytes());
    if output.len() != header_length {
        return Err(SegmentError::Geometry(format!(
            "encoded header length {}, declared {header_length}",
            output.len()
        )));
    }
    let first_offset = entries
        .first()
        .and_then(|entry| usize::try_from(entry.offset).ok())
        .ok_or_else(|| SegmentError::Geometry("missing first region offset".to_owned()))?;
    output.resize(first_offset, 0);
    for (region, entry) in regions.iter().zip(&entries) {
        let offset = usize::try_from(entry.offset)
            .map_err(|_| SegmentError::Geometry("region offset exceeds usize".to_owned()))?;
        if output.len() != offset {
            return Err(SegmentError::Geometry(format!(
                "region {:?} offset {offset}, current length {}",
                region.kind,
                output.len()
            )));
        }
        output.extend_from_slice(&region.bytes);
        output.resize(align_up(output.len(), REGION_ALIGNMENT)?, 0);
    }
    let checksum_entry = entries.last().ok_or_else(|| {
        SegmentError::Geometry("missing checksum-table directory entry".to_owned())
    })?;
    let checksum_offset = usize::try_from(checksum_entry.offset)
        .map_err(|_| SegmentError::Geometry("checksum offset exceeds usize".to_owned()))?;
    if output.len() != checksum_offset {
        return Err(SegmentError::Geometry(format!(
            "checksum offset {checksum_offset}, current length {}",
            output.len()
        )));
    }
    output.extend_from_slice(&checksum_table);
    let whole_file_checksum = xxh3_64(&output);
    output.extend_from_slice(&whole_file_checksum.to_le_bytes());
    if output.len() != file_length {
        return Err(SegmentError::Geometry(format!(
            "encoded file length {}, declared {file_length}",
            output.len()
        )));
    }
    Ok(output)
}

fn encode_checksum_table(regions: &[RegionBytes]) -> Vec<u8> {
    let entry_count = regions
        .iter()
        .map(|region| region.bytes.len().div_ceil(CHECKSUM_CHUNK_BYTES))
        .sum::<usize>();
    let mut output = Vec::with_capacity(8_usize.saturating_add(entry_count.saturating_mul(16)));
    output.extend_from_slice(&(entry_count as u32).to_le_bytes());
    output.extend_from_slice(&(CHECKSUM_CHUNK_BYTES as u32).to_le_bytes());
    for region in regions {
        for (chunk_index, chunk) in region.bytes.chunks(CHECKSUM_CHUNK_BYTES).enumerate() {
            output.extend_from_slice(&region.kind.id().to_le_bytes());
            output.extend_from_slice(&0_u16.to_le_bytes());
            output.extend_from_slice(&(chunk_index as u32).to_le_bytes());
            output.extend_from_slice(&xxh3_64(chunk).to_le_bytes());
        }
    }
    output
}

fn current_version(family: FormatFamily) -> Result<u16, SegmentError> {
    FormatRegistry::families()
        .iter()
        .find(|spec| spec.family == family)
        .map(|spec| spec.current_version)
        .ok_or_else(|| SegmentError::Geometry(format!("family {} is not registered", family.id())))
}

/// Writes a temp file, synchronizes it, renames it into place, then syncs the directory.
pub fn write_segment(
    vfs: &dyn Vfs,
    directory: &Path,
    build: SegmentBuild<'_>,
) -> Result<SegmentMeta, SegmentError> {
    let bytes = encode_segment(build)?;
    let final_path = directory.join(build.id.file_name());
    let temporary_path = temporary_path(directory, build.id);
    vfs.write(&temporary_path, &bytes)
        .map_err(|error| SegmentError::io(&temporary_path, error))?;
    vfs.sync(&temporary_path, PART_A_ORDERED_SYNC)
        .map_err(|error| SegmentError::io(&temporary_path, error))?;
    vfs.rename(&temporary_path, &final_path)
        .map_err(|error| SegmentError::io(&final_path, error))?;
    vfs.sync(directory, PART_A_ORDERED_SYNC)
        .map_err(|error| SegmentError::io(directory, error))?;
    Ok(SegmentMeta {
        id: build.id,
        row_count: build.columns.row_count(),
        scheme: build.scheme,
        dims: build.dims,
        file_size: bytes.len() as u64,
    })
}

fn temporary_path(directory: &Path, id: SegmentId) -> PathBuf {
    directory.join(format!(".{}.tmp", id.file_name()))
}
