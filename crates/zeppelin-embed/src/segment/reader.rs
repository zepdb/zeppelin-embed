//! Read-only memory-mapped segment validation and lazy region access.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use xxhash_rust::xxh3::xxh3_64;

use crate::format::frame::{
    FILE_HEADER_LEN, FILE_TRAILER_LEN, FormatCheck, FormatError, decode_header, read_u16, read_u32,
    read_u64,
};
use crate::format::{FormatFamily, FormatRegistry};
use crate::graph::block::{GraphNodeBlocks, ValidatedGraphNodeBlocks, decode_node_blocks};
use crate::ingest::{DocId, DocumentVersion, Revision};
use crate::meta::{AliveSet, ColumnStore};
use crate::quant::Bit4Factors;
use crate::vfs::Vfs;

use super::layout::{
    CHECKSUM_CHUNK_BYTES, Cursor, Int8Factors, REGION_ALIGNMENT, REGION_ENTRY_LEN, RegionEntry,
    RegionKind, SEGMENT_PREFIX_LEN, VECTOR_HEADER_LEN, align_up, decode_alive, decode_columns,
    decode_vector_header,
};
use super::{SegmentError, SegmentId, SegmentMeta};

const MAX_SEGMENT_HEADER_BYTES: usize = 1024 * 1024;

thread_local! {
    static DATA_READ_AUDIT: std::cell::RefCell<Option<Arc<AtomicU64>>> = const {
        std::cell::RefCell::new(None)
    };
}

pub(crate) struct DataReadAuditGuard {
    previous: Option<Arc<AtomicU64>>,
}

impl Drop for DataReadAuditGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        DATA_READ_AUDIT.with(|slot| {
            slot.replace(previous);
        });
    }
}

pub(crate) fn install_data_read_audit(counter: Option<Arc<AtomicU64>>) -> DataReadAuditGuard {
    let previous = DATA_READ_AUDIT.with(|slot| slot.replace(counter));
    DataReadAuditGuard { previous }
}

fn account_data_read(bytes: usize) {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    DATA_READ_AUDIT.with(|slot| {
        if let Some(counter) = slot.borrow().as_ref() {
            let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(bytes))
            });
        }
    });
}

struct MappedFile {
    _file: File,
    pointer: NonNull<u8>,
    length: usize,
}

impl MappedFile {
    fn open(path: &Path) -> Result<Self, SegmentError> {
        let file = File::open(path).map_err(|error| SegmentError::io(path, error))?;
        let length = usize::try_from(
            file.metadata()
                .map_err(|error| SegmentError::io(path, error))?
                .len(),
        )
        .map_err(|_| SegmentError::Geometry("mapped file length exceeds usize".to_owned()))?;
        if length == 0 {
            return Err(FormatError::new(
                path.display().to_string(),
                FormatCheck::Length,
                "empty segment file",
            )
            .into());
        }
        // SAFETY: `file` is an open regular-file descriptor, length is non-zero,
        // and the mapping is read-only/private. The mapping owns its lifetime and
        // is released exactly once in `Drop`; closing the descriptor is permitted.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(SegmentError::io(path, std::io::Error::last_os_error()));
        }
        let pointer = NonNull::new(mapped.cast::<u8>()).ok_or_else(|| {
            SegmentError::Geometry("mmap returned a null non-failure pointer".to_owned())
        })?;
        Ok(Self {
            _file: file,
            pointer,
            length,
        })
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: the read-only mapping is valid for `length` bytes and remains
        // alive for the returned borrow through `&self`.
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // SAFETY: this is the exact pointer/length pair returned by successful
        // `mmap`, and `Drop` runs once for the sole mapping owner.
        let _ = unsafe { libc::munmap(self.pointer.as_ptr().cast(), self.length) };
    }
}

// SAFETY: the mapping is immutable for its entire lifetime.
unsafe impl Send for MappedFile {}
// SAFETY: all shared access exposes read-only slices.
unsafe impl Sync for MappedFile {}

struct ParsedHeader {
    meta: SegmentMeta,
    header_length: usize,
    entries: Vec<RegionEntry>,
}

/// Validated read-only memory mapping with lazy region checksum verification.
pub struct SegmentReader {
    mapping: MappedFile,
    meta: SegmentMeta,
    header_length: usize,
    entries: Vec<RegionEntry>,
    // Inline cached graph metadata is covered by this reader's exactly
    // accounted snapshot slot; its heap-backed scratch is charged to Cache.
    pub(crate) graph_search_cache: crate::lifecycle::graph_cache::SegmentGraphSearchCache,
}

impl SegmentReader {
    /// Memory-maps a segment and validates only its bounded header/directory.
    pub fn open(path: &Path, expected_id: SegmentId) -> Result<Self, SegmentError> {
        let mapping = MappedFile::open(path)?;
        let artifact = path.display().to_string();
        let parsed = parse_segment_header(
            &artifact,
            mapping.as_bytes(),
            mapping.length as u64,
            expected_id,
        )?;
        Ok(Self {
            mapping,
            meta: parsed.meta,
            header_length: parsed.header_length,
            entries: parsed.entries,
            graph_search_cache: crate::lifecycle::graph_cache::SegmentGraphSearchCache::new(),
        })
    }

    pub(crate) fn open_accounted(
        path: &Path,
        expected: &SegmentMeta,
        before_directory_allocation: impl FnOnce(usize) -> Result<(), crate::lifecycle::StoreError>,
    ) -> Result<Self, crate::lifecycle::StoreError> {
        let mapping = MappedFile::open(path).map_err(crate::lifecycle::StoreError::Segment)?;
        let artifact = path.display().to_string();
        let region_count =
            preflight_region_count(&artifact, mapping.as_bytes(), mapping.length as u64)
                .map_err(crate::lifecycle::StoreError::Segment)?;
        before_directory_allocation(region_count)?;
        let parsed = parse_segment_header(
            &artifact,
            mapping.as_bytes(),
            mapping.length as u64,
            expected.id,
        )
        .map_err(crate::lifecycle::StoreError::Segment)?;
        if !parsed.meta.same_segment_file(expected) {
            return Err(crate::lifecycle::StoreError::Segment(
                SegmentError::Geometry(format!(
                    "manifest metadata {expected:?}, mapped header metadata {:?}",
                    parsed.meta
                )),
            ));
        }
        Ok(Self {
            mapping,
            meta: expected.clone(),
            header_length: parsed.header_length,
            entries: parsed.entries,
            graph_search_cache: crate::lifecycle::graph_cache::SegmentGraphSearchCache::new(),
        })
    }

    /// Returns immutable manifest-visible metadata decoded from the header.
    #[must_use]
    pub const fn meta(&self) -> &SegmentMeta {
        &self.meta
    }

    /// Returns exact header bytes touched by an O(manifest) open.
    #[must_use]
    pub const fn header_length(&self) -> usize {
        self.header_length
    }

    /// Returns the exact virtual byte length of this immutable file mapping.
    #[must_use]
    pub(crate) const fn mapped_bytes(&self) -> usize {
        self.mapping.length
    }

    /// Returns mapped bytes whose intersecting pages are resident per `mincore`.
    pub(crate) fn mapped_resident_bytes(&self) -> std::io::Result<u64> {
        crate::sys::memory::mincore_resident_bytes(self.mapping.as_bytes())
    }

    /// Returns the validated region directory, including unknown skippable kinds.
    #[must_use]
    pub fn directory(&self) -> &[RegionEntry] {
        &self.entries
    }

    /// Returns unknown region ids without treating them as corruption.
    #[must_use]
    pub fn unknown_region_ids(&self) -> Vec<u16> {
        self.entries
            .iter()
            .filter_map(|entry| {
                RegionKind::from_id(entry.kind)
                    .is_none()
                    .then_some(entry.kind)
            })
            .collect()
    }

    /// Validates a complete region and returns its original mmap-backed bytes.
    pub fn region(&self, kind: RegionKind) -> Result<&[u8], SegmentError> {
        let entry = self.entry(kind)?;
        let bytes = self.region_slice(entry)?;
        let actual = xxh3_64(bytes);
        if actual != entry.checksum {
            return Err(FormatError::new(
                format!("segment:{}:{kind:?}", self.meta.id),
                FormatCheck::BlockChecksum,
                format!("expected {:#018x}, computed {actual:#018x}", entry.checksum),
            )
            .into());
        }
        Ok(bytes)
    }

    /// Validates one 64-KB chunk and returns only that mmap-backed chunk.
    pub fn region_chunk(&self, kind: RegionKind, chunk_index: u32) -> Result<&[u8], SegmentError> {
        let entry = self.entry(kind)?;
        let region = self.region_slice(entry)?;
        let start = (chunk_index as usize)
            .checked_mul(CHECKSUM_CHUNK_BYTES)
            .ok_or_else(|| SegmentError::Geometry("chunk offset overflow".to_owned()))?;
        let end = start.saturating_add(CHECKSUM_CHUNK_BYTES).min(region.len());
        let chunk = region.get(start..end).ok_or_else(|| {
            SegmentError::Geometry(format!("chunk {chunk_index} is outside {kind:?}"))
        })?;
        if chunk.is_empty() {
            return Err(SegmentError::Geometry(format!(
                "chunk {chunk_index} is outside {kind:?}"
            )));
        }
        let expected = self.chunk_checksum(kind, chunk_index)?;
        let actual = xxh3_64(chunk);
        if actual != expected {
            return Err(FormatError::new(
                format!("segment:{}:{kind:?}:chunk-{chunk_index}", self.meta.id),
                FormatCheck::BlockChecksum,
                format!("expected {expected:#018x}, computed {actual:#018x}"),
            )
            .into());
        }
        Ok(chunk)
    }

    /// Returns packed Bit4 code bytes exactly as passed to the batch kernel.
    pub fn bit4_codes(&self) -> Result<&[u8], SegmentError> {
        if self.meta.scheme != 4 {
            return Err(SegmentError::Geometry(format!(
                "Bit4 codes requested for scheme {}",
                self.meta.scheme
            )));
        }
        let (header, payload) = self.vector_payload(RegionKind::VectorCodes)?;
        let expected = (self.meta.dims as usize)
            .div_ceil(2)
            .checked_mul(self.meta.row_count as usize)
            .ok_or_else(|| SegmentError::Geometry("Bit4 code length overflow".to_owned()))?;
        if header.row_stride_bytes as usize != (self.meta.dims as usize).div_ceil(2)
            || payload.len() != expected
        {
            return Err(SegmentError::Geometry(format!(
                "Bit4 code stride/length {}/{}, expected {}/{}",
                header.row_stride_bytes,
                payload.len(),
                (self.meta.dims as usize).div_ceil(2),
                expected
            )));
        }
        Ok(payload)
    }

    /// Casts signed-byte vector codes directly from the validated mmap region.
    pub fn int8_codes(&self) -> Result<&[i8], SegmentError> {
        if self.meta.scheme != 2 {
            return Err(SegmentError::Geometry(format!(
                "Int8 codes requested for scheme {}",
                self.meta.scheme
            )));
        }
        let (header, payload) = self.vector_payload(RegionKind::VectorCodes)?;
        let expected = (self.meta.dims as usize)
            .checked_mul(self.meta.row_count as usize)
            .ok_or_else(|| SegmentError::Geometry("Int8 code length overflow".to_owned()))?;
        if header.row_stride_bytes as usize != self.meta.dims as usize || payload.len() != expected
        {
            return Err(SegmentError::Geometry(format!(
                "Int8 code stride/length {}/{}, expected {}/{}",
                header.row_stride_bytes,
                payload.len(),
                self.meta.dims,
                expected
            )));
        }
        cast_slice::<i8>(payload, expected, "Int8 codes")
    }

    /// Casts the validated factor region directly to permanent Bit4 records.
    pub fn bit4_factors(&self) -> Result<&[Bit4Factors], SegmentError> {
        if self.meta.scheme != 4 {
            return Err(SegmentError::Geometry(format!(
                "Bit4 factors requested for scheme {}",
                self.meta.scheme
            )));
        }
        let (header, payload) = self.vector_payload(RegionKind::VectorFactors)?;
        if header.factor_stride_bytes != 12 {
            return Err(SegmentError::Geometry(format!(
                "Bit4 factor stride {}, expected 12",
                header.factor_stride_bytes
            )));
        }
        cast_slice::<Bit4Factors>(payload, self.meta.row_count as usize, "Bit4 factors")
    }

    /// Casts a validated Int8 factor region directly to permanent records.
    pub fn int8_factors(&self) -> Result<&[Int8Factors], SegmentError> {
        if self.meta.scheme != 2 {
            return Err(SegmentError::Geometry(format!(
                "Int8 factors requested for scheme {}",
                self.meta.scheme
            )));
        }
        let (header, payload) = self.vector_payload(RegionKind::VectorFactors)?;
        if header.factor_stride_bytes != 8 {
            return Err(SegmentError::Geometry(format!(
                "Int8 factor stride {}, expected 8",
                header.factor_stride_bytes
            )));
        }
        cast_slice::<Int8Factors>(payload, self.meta.row_count as usize, "Int8 factors")
    }

    /// Casts validated f32 exact-rescore rows directly from the mmap.
    pub fn rescore_f32(&self) -> Result<&[f32], SegmentError> {
        let (header, payload) = self.vector_payload(RegionKind::VectorRescore)?;
        if header.scheme != 0 {
            return Err(SegmentError::Geometry(format!(
                "v1 rescore scheme must be F32/0, got {}",
                header.scheme
            )));
        }
        let count = (self.meta.row_count as usize)
            .checked_mul(self.meta.dims as usize)
            .ok_or_else(|| SegmentError::Geometry("rescore count overflow".to_owned()))?;
        cast_slice::<f32>(payload, count, "f32 rescore")
    }

    /// Decodes the checksummed metadata region into typed column arrays.
    pub fn columns(&self) -> Result<ColumnStore, SegmentError> {
        let columns = decode_columns(self.region(RegionKind::Columns)?)?;
        if columns.row_count() != self.meta.row_count {
            return Err(SegmentError::Geometry(format!(
                "column rows {}, header rows {}",
                columns.row_count(),
                self.meta.row_count
            )));
        }
        Ok(columns)
    }

    /// Decodes the checksummed alive/tombstone region.
    pub fn alive(&self) -> Result<AliveSet, SegmentError> {
        let alive = decode_alive(self.region(RegionKind::Alive)?)?;
        if alive.row_count() != self.meta.row_count {
            return Err(SegmentError::Geometry(format!(
                "alive rows {}, header rows {}",
                alive.row_count(),
                self.meta.row_count
            )));
        }
        Ok(alive)
    }

    /// Decodes one optional sealed-row document identity directly from the mapping.
    pub fn document_version(&self, row: usize) -> Result<Option<DocumentVersion>, SegmentError> {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.kind == RegionKind::DocumentVersions.id())
        else {
            return Ok(None);
        };
        let bytes = self.region_slice(entry)?;
        if xxh3_64(bytes) != entry.checksum {
            return Err(FormatError::new(
                format!("segment:{}:document-versions", self.meta.id),
                FormatCheck::BlockChecksum,
                "document-version region checksum mismatch",
            )
            .into());
        }
        let expected = (self.meta.row_count as usize)
            .checked_mul(24)
            .ok_or_else(|| SegmentError::Geometry("document-version length overflow".to_owned()))?;
        if bytes.len() != expected {
            return Err(SegmentError::Geometry(format!(
                "document-version bytes {}, expected {expected}",
                bytes.len()
            )));
        }
        let start = row
            .checked_mul(24)
            .ok_or_else(|| SegmentError::Geometry("document-version row overflow".to_owned()))?;
        let doc_end = start
            .checked_add(16)
            .ok_or_else(|| SegmentError::Geometry("document id end overflow".to_owned()))?;
        let revision_end = doc_end
            .checked_add(8)
            .ok_or_else(|| SegmentError::Geometry("revision end overflow".to_owned()))?;
        let doc_id = bytes
            .get(start..doc_end)
            .and_then(|value| value.try_into().ok())
            .map(u128::from_le_bytes)
            .ok_or_else(|| SegmentError::Geometry(format!("document id row {row} is missing")))?;
        let revision = bytes
            .get(doc_end..revision_end)
            .and_then(|value| value.try_into().ok())
            .map(u64::from_le_bytes)
            .ok_or_else(|| SegmentError::Geometry(format!("revision row {row} is missing")))?;
        Ok(Some(DocumentVersion::new(
            DocId::new(doc_id),
            Revision::new(revision),
        )))
    }

    /// Returns the validated, mmap-backed fixed-stride graph node-block region.
    pub fn graph_node_blocks(&self) -> Result<GraphNodeBlocks<'_>, SegmentError> {
        let blocks = decode_node_blocks(self.region(RegionKind::GraphNodeBlocks)?)?;
        if blocks.layout().dims() != self.meta.dims {
            return Err(SegmentError::Geometry(format!(
                "graph dimensions {}, segment dimensions {}",
                blocks.layout().dims(),
                self.meta.dims
            )));
        }
        if blocks.node_count() != self.meta.row_count {
            return Err(SegmentError::Geometry(format!(
                "graph nodes {}, segment rows {}",
                blocks.node_count(),
                self.meta.row_count
            )));
        }
        Ok(blocks)
    }

    pub(crate) fn bind_validated_graph_node_blocks(
        &self,
        descriptor: ValidatedGraphNodeBlocks,
    ) -> Result<GraphNodeBlocks<'_>, SegmentError> {
        let entry = self.entry(RegionKind::GraphNodeBlocks)?;
        let bytes = self.region_slice(entry)?;
        descriptor.bind(bytes).map_err(SegmentError::Graph)
    }

    /// Validates every region, alignment padding, and the whole-file trailer.
    pub fn validate_all(&self) -> Result<(), SegmentError> {
        for entry in &self.entries {
            let bytes = self.region_slice(entry)?;
            let actual = xxh3_64(bytes);
            if actual != entry.checksum {
                return Err(FormatError::new(
                    format!("segment:{}:region-{}", self.meta.id, entry.kind),
                    FormatCheck::BlockChecksum,
                    format!("expected {:#018x}, computed {actual:#018x}", entry.checksum),
                )
                .into());
            }
        }
        let bytes = self.mapping.as_bytes();
        let trailer_start = bytes.len().checked_sub(FILE_TRAILER_LEN).ok_or_else(|| {
            FormatError::new(
                format!("segment: {}", self.meta.id),
                FormatCheck::Length,
                "missing file trailer",
            )
        })?;
        let expected = read_u64("segment", bytes, trailer_start)?;
        let checksummed = bytes.get(..trailer_start).ok_or_else(|| {
            FormatError::new("segment", FormatCheck::Length, "invalid trailer position")
        })?;
        let actual = xxh3_64(checksummed);
        if actual != expected {
            return Err(FormatError::new(
                format!("segment: {}", self.meta.id),
                FormatCheck::FileChecksum,
                format!("expected {expected:#018x}, computed {actual:#018x}"),
            )
            .into());
        }
        Ok(())
    }

    fn vector_payload(
        &self,
        kind: RegionKind,
    ) -> Result<(super::layout::VectorHeader, &[u8]), SegmentError> {
        let region = self.region(kind)?;
        let header_bytes = region.get(..VECTOR_HEADER_LEN).ok_or_else(|| {
            SegmentError::Geometry(format!("{kind:?} is shorter than vector header"))
        })?;
        let header = decode_vector_header(header_bytes)?;
        if header.dims != self.meta.dims
            || header.row_count != self.meta.row_count
            || (kind != RegionKind::VectorRescore && header.scheme != self.meta.scheme)
        {
            return Err(SegmentError::Geometry(format!(
                "{kind:?} header scheme/dims/rows {}/{}/{}, segment {}/{}/{}",
                header.scheme,
                header.dims,
                header.row_count,
                self.meta.scheme,
                self.meta.dims,
                self.meta.row_count
            )));
        }
        let payload = region
            .get(VECTOR_HEADER_LEN..)
            .ok_or_else(|| SegmentError::Geometry(format!("{kind:?} payload offset is invalid")))?;
        Ok((header, payload))
    }

    fn entry(&self, kind: RegionKind) -> Result<&RegionEntry, SegmentError> {
        self.entries
            .iter()
            .find(|entry| entry.kind == kind.id())
            .ok_or(SegmentError::MissingRegion(kind))
    }

    fn region_slice(&self, entry: &RegionEntry) -> Result<&[u8], SegmentError> {
        let start = usize::try_from(entry.offset)
            .map_err(|_| SegmentError::Geometry("region offset exceeds usize".to_owned()))?;
        let length = usize::try_from(entry.length)
            .map_err(|_| SegmentError::Geometry("region length exceeds usize".to_owned()))?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| SegmentError::Geometry("region end overflow".to_owned()))?;
        let bytes = self.mapping.as_bytes().get(start..end).ok_or_else(|| {
            SegmentError::Geometry(format!(
                "region {} range {start}..{end} exceeds file {}",
                entry.kind, self.mapping.length
            ))
        })?;
        account_data_read(bytes.len());
        Ok(bytes)
    }

    fn chunk_checksum(&self, kind: RegionKind, chunk_index: u32) -> Result<u64, SegmentError> {
        let table = self.region(RegionKind::ChecksumTable)?;
        let mut cursor = Cursor::new("checksum-table", table);
        let count = cursor.u32()?;
        let chunk_size = cursor.u32()?;
        if chunk_size as usize != CHECKSUM_CHUNK_BYTES {
            return Err(SegmentError::Geometry(format!(
                "checksum chunk size {chunk_size}, expected {CHECKSUM_CHUNK_BYTES}"
            )));
        }
        let mut found = None;
        for _ in 0..count {
            let entry_kind = cursor.u16()?;
            let reserved = cursor.u16()?;
            let entry_chunk = cursor.u32()?;
            let checksum = cursor.u64()?;
            if reserved != 0 {
                return Err(SegmentError::Geometry(
                    "checksum-table reserved field is non-zero".to_owned(),
                ));
            }
            if entry_kind == kind.id() && entry_chunk == chunk_index {
                found = Some(checksum);
            }
        }
        cursor.finish().map_err(SegmentError::Geometry)?;
        found.ok_or_else(|| {
            SegmentError::Geometry(format!(
                "checksum table has no {kind:?} chunk {chunk_index}"
            ))
        })
    }
}

/// Reads and validates only one bounded segment header through a VFS decorator.
pub fn validate_header_with_vfs(
    vfs: &dyn Vfs,
    path: &Path,
    expected: &SegmentMeta,
) -> Result<usize, SegmentError> {
    let actual_length = vfs
        .open(path)
        .map_err(|error| SegmentError::io(path, error))?;
    let mut header_bytes = vfs
        .read_range(path, 0, FILE_HEADER_LEN)
        .map_err(|error| SegmentError::io(path, error))?;
    let artifact = path.display().to_string();
    let fixed = decode_header(&artifact, FormatFamily::Segment, &header_bytes)?;
    let header_length = usize::try_from(fixed.header_length)
        .map_err(|_| SegmentError::Geometry("segment header length exceeds usize".to_owned()))?;
    validate_bounded_header_length(&artifact, header_length, actual_length)?;
    let remaining = header_length.saturating_sub(FILE_HEADER_LEN);
    if remaining > 0 {
        let tail = vfs
            .read_range(path, FILE_HEADER_LEN as u64, remaining)
            .map_err(|error| SegmentError::io(path, error))?;
        header_bytes.extend_from_slice(&tail);
    }
    let parsed = parse_segment_header(&artifact, &header_bytes, actual_length, expected.id)?;
    if !parsed.meta.same_segment_file(expected) {
        return Err(SegmentError::Geometry(format!(
            "manifest metadata {expected:?}, header metadata {:?}",
            parsed.meta
        )));
    }
    Ok(header_length)
}

/// Fuzzing/parser seam that validates arbitrary complete segment bytes without mapping.
pub fn validate_segment_bytes(bytes: &[u8]) -> Result<SegmentMeta, SegmentError> {
    let id_bytes: [u8; 16] = bytes
        .get(FILE_HEADER_LEN..FILE_HEADER_LEN.saturating_add(16))
        .ok_or_else(|| {
            FormatError::new(
                "segment-bytes",
                FormatCheck::Length,
                format!("missing segment id in {} bytes", bytes.len()),
            )
        })?
        .try_into()
        .map_err(|_| {
            FormatError::new("segment-bytes", FormatCheck::Length, "invalid segment id")
        })?;
    let id = SegmentId::from_bytes(id_bytes);
    let actual_length = u64::try_from(bytes.len())
        .map_err(|_| SegmentError::Geometry("byte slice length exceeds u64".to_owned()))?;
    let parsed = parse_segment_header("segment-bytes", bytes, actual_length, id)?;
    for entry in &parsed.entries {
        let start = usize::try_from(entry.offset)
            .map_err(|_| SegmentError::Geometry("region offset exceeds usize".to_owned()))?;
        let length = usize::try_from(entry.length)
            .map_err(|_| SegmentError::Geometry("region length exceeds usize".to_owned()))?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| SegmentError::Geometry("region end overflow".to_owned()))?;
        let region = bytes.get(start..end).ok_or_else(|| {
            FormatError::new(
                "segment-bytes",
                FormatCheck::BlockLength,
                format!("region {} range {start}..{end} is truncated", entry.kind),
            )
        })?;
        let actual = xxh3_64(region);
        if actual != entry.checksum {
            return Err(FormatError::new(
                format!("segment-bytes:region-{}", entry.kind),
                FormatCheck::BlockChecksum,
                format!("expected {:#018x}, computed {actual:#018x}", entry.checksum),
            )
            .into());
        }
    }
    let trailer = bytes.len().checked_sub(FILE_TRAILER_LEN).ok_or_else(|| {
        FormatError::new("segment-bytes", FormatCheck::Length, "missing file trailer")
    })?;
    let expected = read_u64("segment-bytes", bytes, trailer)?;
    let checksummed = bytes.get(..trailer).ok_or_else(|| {
        FormatError::new(
            "segment-bytes",
            FormatCheck::Length,
            "invalid trailer offset",
        )
    })?;
    let actual = xxh3_64(checksummed);
    if actual != expected {
        return Err(FormatError::new(
            "segment-bytes",
            FormatCheck::FileChecksum,
            format!("expected {expected:#018x}, computed {actual:#018x}"),
        )
        .into());
    }
    Ok(parsed.meta)
}

fn preflight_region_count(
    artifact: &str,
    bytes: &[u8],
    actual_file_length: u64,
) -> Result<usize, SegmentError> {
    let fixed = decode_header(artifact, FormatFamily::Segment, bytes)?;
    if fixed.file_length != actual_file_length {
        return Err(FormatError::new(
            artifact,
            FormatCheck::FileLength,
            format!(
                "declared {}, actual {actual_file_length}",
                fixed.file_length
            ),
        )
        .into());
    }
    let header_length = usize::try_from(fixed.header_length)
        .map_err(|_| SegmentError::Geometry("header length exceeds usize".to_owned()))?;
    validate_bounded_header_length(artifact, header_length, actual_file_length)?;
    let header_bytes = bytes.get(..header_length).ok_or_else(|| {
        FormatError::new(
            artifact,
            FormatCheck::Length,
            format!("declared header {header_length}, available {}", bytes.len()),
        )
    })?;
    Ok(usize::from(read_u16(
        artifact,
        header_bytes,
        FILE_HEADER_LEN.saturating_add(20),
    )?))
}

fn parse_segment_header(
    artifact: &str,
    bytes: &[u8],
    actual_file_length: u64,
    expected_id: SegmentId,
) -> Result<ParsedHeader, SegmentError> {
    let fixed = decode_header(artifact, FormatFamily::Segment, bytes)?;
    if fixed.file_length != actual_file_length {
        return Err(FormatError::new(
            artifact,
            FormatCheck::FileLength,
            format!(
                "declared {}, actual {actual_file_length}",
                fixed.file_length
            ),
        )
        .into());
    }
    let header_length = usize::try_from(fixed.header_length)
        .map_err(|_| SegmentError::Geometry("header length exceeds usize".to_owned()))?;
    validate_bounded_header_length(artifact, header_length, actual_file_length)?;
    let header_bytes = bytes.get(..header_length).ok_or_else(|| {
        FormatError::new(
            artifact,
            FormatCheck::Length,
            format!("declared header {header_length}, available {}", bytes.len()),
        )
    })?;
    let id_bytes: [u8; 16] = header_bytes
        .get(FILE_HEADER_LEN..FILE_HEADER_LEN.saturating_add(16))
        .ok_or_else(|| FormatError::new(artifact, FormatCheck::Length, "missing segment id"))?
        .try_into()
        .map_err(|_| FormatError::new(artifact, FormatCheck::Length, "invalid segment id"))?;
    let actual_id = SegmentId::from_bytes(id_bytes);
    if actual_id != expected_id {
        return Err(SegmentError::WrongObject {
            artifact: artifact.to_owned(),
            expected: expected_id,
            actual: actual_id,
        });
    }
    let row_count = read_u32(artifact, header_bytes, FILE_HEADER_LEN.saturating_add(16))?;
    let region_count = usize::from(read_u16(
        artifact,
        header_bytes,
        FILE_HEADER_LEN.saturating_add(20),
    )?);
    if read_u16(artifact, header_bytes, FILE_HEADER_LEN.saturating_add(22))? != 0 {
        return Err(SegmentError::Geometry(
            "segment prefix reserved field is non-zero".to_owned(),
        ));
    }
    let scheme = read_u16(artifact, header_bytes, FILE_HEADER_LEN.saturating_add(24))?;
    FormatRegistry::require_scheme(scheme)
        .map_err(|error| SegmentError::Geometry(error.to_string()))?;
    if read_u16(artifact, header_bytes, FILE_HEADER_LEN.saturating_add(26))? != 0 {
        return Err(SegmentError::Geometry(
            "segment scheme padding is non-zero".to_owned(),
        ));
    }
    let dims = read_u32(artifact, header_bytes, FILE_HEADER_LEN.saturating_add(28))?;
    let expected_header_length = FILE_HEADER_LEN
        .checked_add(SEGMENT_PREFIX_LEN)
        .and_then(|value| value.checked_add(region_count.saturating_mul(REGION_ENTRY_LEN)))
        .and_then(|value| value.checked_add(8))
        .ok_or_else(|| SegmentError::Geometry("header length overflow".to_owned()))?;
    if header_length != expected_header_length {
        return Err(FormatError::new(
            artifact,
            FormatCheck::HeaderLength,
            format!("declared {header_length}, directory implies {expected_header_length}"),
        )
        .into());
    }
    let checksum_offset = header_length.saturating_sub(8);
    let expected_checksum = read_u64(artifact, header_bytes, checksum_offset)?;
    let checksum_bytes = header_bytes.get(..checksum_offset).ok_or_else(|| {
        FormatError::new(
            artifact,
            FormatCheck::Length,
            "invalid header checksum offset",
        )
    })?;
    let actual_checksum = xxh3_64(checksum_bytes);
    if actual_checksum != expected_checksum {
        return Err(FormatError::new(
            artifact,
            FormatCheck::BlockChecksum,
            format!("header expected {expected_checksum:#018x}, computed {actual_checksum:#018x}"),
        )
        .into());
    }

    let mut entries: Vec<RegionEntry> = Vec::with_capacity(region_count);
    let directory_start = FILE_HEADER_LEN.saturating_add(SEGMENT_PREFIX_LEN);
    for position in 0..region_count {
        let offset = directory_start
            .checked_add(position.saturating_mul(REGION_ENTRY_LEN))
            .ok_or_else(|| SegmentError::Geometry("directory offset overflow".to_owned()))?;
        let entry = RegionEntry {
            kind: read_u16(artifact, header_bytes, offset)?,
            version: read_u16(artifact, header_bytes, offset.saturating_add(2))?,
            reserved: read_u32(artifact, header_bytes, offset.saturating_add(4))?,
            offset: read_u64(artifact, header_bytes, offset.saturating_add(8))?,
            length: read_u64(artifact, header_bytes, offset.saturating_add(16))?,
            checksum: read_u64(artifact, header_bytes, offset.saturating_add(24))?,
        };
        if entry.reserved != 0 {
            return Err(SegmentError::Geometry(format!(
                "region {} reserved field is non-zero",
                entry.kind
            )));
        }
        if entries.iter().any(|existing| existing.kind == entry.kind) {
            return Err(SegmentError::Geometry(format!(
                "duplicate region kind {}",
                entry.kind
            )));
        }
        validate_entry(artifact, &entry, header_length, actual_file_length)?;
        if let Some(kind) = RegionKind::from_id(entry.kind)
            && let Some(family) = kind.family()
        {
            FormatRegistry::require(family.id(), entry.version).map_err(|error| {
                FormatError::new(artifact, FormatCheck::Version, error.to_string())
            })?;
        }
        entries.push(entry);
    }
    validate_non_overlapping(&entries)?;
    Ok(ParsedHeader {
        meta: SegmentMeta {
            id: actual_id,
            row_count,
            scheme,
            dims,
            file_size: actual_file_length,
            clustering_key_range: super::ClusteringKeyRange::Unstamped,
        },
        header_length,
        entries,
    })
}

fn validate_bounded_header_length(
    artifact: &str,
    header_length: usize,
    actual_file_length: u64,
) -> Result<(), SegmentError> {
    if !(FILE_HEADER_LEN + SEGMENT_PREFIX_LEN + 8..=MAX_SEGMENT_HEADER_BYTES)
        .contains(&header_length)
    {
        return Err(FormatError::new(
            artifact,
            FormatCheck::HeaderLength,
            format!("header length {header_length} is outside bounded v1 range"),
        )
        .into());
    }
    let file_length = usize::try_from(actual_file_length)
        .map_err(|_| SegmentError::Geometry("file length exceeds usize".to_owned()))?;
    if header_length > file_length.saturating_sub(FILE_TRAILER_LEN) {
        return Err(FormatError::new(
            artifact,
            FormatCheck::HeaderLength,
            format!("header {header_length} exceeds file {file_length}"),
        )
        .into());
    }
    Ok(())
}

fn validate_entry(
    artifact: &str,
    entry: &RegionEntry,
    header_length: usize,
    file_length: u64,
) -> Result<(), SegmentError> {
    let offset = usize::try_from(entry.offset)
        .map_err(|_| SegmentError::Geometry("region offset exceeds usize".to_owned()))?;
    let length = usize::try_from(entry.length)
        .map_err(|_| SegmentError::Geometry("region length exceeds usize".to_owned()))?;
    let minimum = align_up(header_length, REGION_ALIGNMENT)?;
    if offset < minimum || !offset.is_multiple_of(REGION_ALIGNMENT) {
        return Err(SegmentError::Geometry(format!(
            "region {} offset {offset} is below/aligned from {minimum}",
            entry.kind
        )));
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| SegmentError::Geometry("region end overflow".to_owned()))?;
    let maximum = usize::try_from(file_length)
        .map_err(|_| SegmentError::Geometry("file length exceeds usize".to_owned()))?
        .checked_sub(FILE_TRAILER_LEN)
        .ok_or_else(|| SegmentError::Geometry("file is shorter than trailer".to_owned()))?;
    if end > maximum {
        return Err(FormatError::new(
            artifact,
            FormatCheck::BlockLength,
            format!("region {} ends at {end}, maximum {maximum}", entry.kind),
        )
        .into());
    }
    Ok(())
}

fn validate_non_overlapping(entries: &[RegionEntry]) -> Result<(), SegmentError> {
    for (position, entry) in entries.iter().enumerate() {
        let entry_end = entry
            .offset
            .checked_add(entry.length)
            .ok_or_else(|| SegmentError::Geometry("region u64 end overflow".to_owned()))?;
        for other in entries.iter().skip(position.saturating_add(1)) {
            let other_end = other
                .offset
                .checked_add(other.length)
                .ok_or_else(|| SegmentError::Geometry("region u64 end overflow".to_owned()))?;
            if entry.offset < other_end && other.offset < entry_end {
                let (previous, current, previous_end) = if entry.offset <= other.offset {
                    (entry, other, entry_end)
                } else {
                    (other, entry, other_end)
                };
                return Err(SegmentError::Geometry(format!(
                    "region {} begins {} before previous end {previous_end} for region {}",
                    current.kind, current.offset, previous.kind
                )));
            }
        }
    }
    Ok(())
}

fn cast_slice<'a, T>(
    bytes: &'a [u8],
    count: usize,
    artifact: &str,
) -> Result<&'a [T], SegmentError> {
    #[cfg(target_endian = "big")]
    {
        let _ = (bytes, count);
        return Err(SegmentError::Geometry(format!(
            "{artifact} zero-copy requires little-endian target"
        )));
    }
    #[cfg(target_endian = "little")]
    {
        let expected = count
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| SegmentError::Geometry(format!("{artifact} byte length overflow")))?;
        if bytes.len() != expected {
            return Err(SegmentError::Geometry(format!(
                "{artifact} bytes {}, expected {expected}",
                bytes.len()
            )));
        }
        if !(bytes.as_ptr() as usize).is_multiple_of(std::mem::align_of::<T>()) {
            return Err(SegmentError::Geometry(format!(
                "{artifact} address is not {}-byte aligned",
                std::mem::align_of::<T>()
            )));
        }
        // SAFETY: byte length and alignment were checked above; T is one of the
        // pinned repr(C) all-f32 record types (or f32), and the checksum plus
        // little-endian target makes the mapped representation valid.
        Ok(unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<T>(), count) })
    }
}
