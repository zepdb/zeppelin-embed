//! Read-only memory-mapped segment validation and lazy region access.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
#[cfg(any(test, feature = "test-support"))]
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static SEGMENT_COST_AUDIT: std::cell::RefCell<Option<Arc<SegmentCostCounters>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct SegmentCostCounters {
    identity_hash_bytes: AtomicU64,
    rescore_hash_bytes: AtomicU64,
    int8_factor_decode_bytes: AtomicU64,
    postings_hash_bytes: AtomicU64,
    postings_decode_bytes: AtomicU64,
    columns_hash_bytes: AtomicU64,
    columns_decode_bytes: AtomicU64,
    alive_hash_bytes: AtomicU64,
    alive_decode_bytes: AtomicU64,
}

/// Test-only deterministic costs observed while reading immutable segments.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SegmentCostSnapshot {
    /// Bytes hashed while validating document identities.
    pub identity_hash_bytes: u64,
    /// Bytes hashed while validating exact-rescore rows.
    pub rescore_hash_bytes: u64,
    /// Int8 factor bytes decoded for scan validation.
    pub int8_factor_decode_bytes: u64,
    /// Bytes hashed while validating sealed postings.
    pub postings_hash_bytes: u64,
    /// Sealed-postings bytes heap-decoded into query-ready state.
    pub postings_decode_bytes: u64,
    /// Bytes hashed while validating typed columns.
    pub columns_hash_bytes: u64,
    /// Typed-column bytes heap-decoded into query-ready state.
    pub columns_decode_bytes: u64,
    /// Bytes hashed while validating alive state.
    pub alive_hash_bytes: u64,
    /// Alive-state bytes heap-decoded into query-ready state.
    pub alive_decode_bytes: u64,
}

/// Test-only scoped collector for deterministic segment read-path costs.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Default)]
pub struct SegmentCostAudit {
    counters: Arc<SegmentCostCounters>,
}

#[cfg(any(test, feature = "test-support"))]
struct SegmentCostAuditGuard {
    previous: Option<Arc<SegmentCostCounters>>,
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for SegmentCostAuditGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        SEGMENT_COST_AUDIT.with(|slot| {
            slot.replace(previous);
        });
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SegmentCostAudit {
    /// Creates a zeroed cost collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs one operation with this collector installed on the calling thread.
    pub fn measure<T>(&self, operation: impl FnOnce() -> T) -> T {
        let previous =
            SEGMENT_COST_AUDIT.with(|slot| slot.replace(Some(Arc::clone(&self.counters))));
        let _guard = SegmentCostAuditGuard { previous };
        operation()
    }

    /// Returns the exact accumulated byte counters.
    #[must_use]
    pub fn snapshot(&self) -> SegmentCostSnapshot {
        SegmentCostSnapshot {
            identity_hash_bytes: self.counters.identity_hash_bytes.load(Ordering::Relaxed),
            rescore_hash_bytes: self.counters.rescore_hash_bytes.load(Ordering::Relaxed),
            int8_factor_decode_bytes: self
                .counters
                .int8_factor_decode_bytes
                .load(Ordering::Relaxed),
            postings_hash_bytes: self.counters.postings_hash_bytes.load(Ordering::Relaxed),
            postings_decode_bytes: self.counters.postings_decode_bytes.load(Ordering::Relaxed),
            columns_hash_bytes: self.counters.columns_hash_bytes.load(Ordering::Relaxed),
            columns_decode_bytes: self.counters.columns_decode_bytes.load(Ordering::Relaxed),
            alive_hash_bytes: self.counters.alive_hash_bytes.load(Ordering::Relaxed),
            alive_decode_bytes: self.counters.alive_decode_bytes.load(Ordering::Relaxed),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
fn account_region_hash(kind: RegionKind, bytes: usize) {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    SEGMENT_COST_AUDIT.with(|slot| {
        let Some(counters) = slot.borrow().as_ref().cloned() else {
            return;
        };
        let counter = match kind {
            RegionKind::DocumentVersions => &counters.identity_hash_bytes,
            RegionKind::VectorRescore => &counters.rescore_hash_bytes,
            RegionKind::Postings => &counters.postings_hash_bytes,
            RegionKind::Columns => &counters.columns_hash_bytes,
            RegionKind::Alive => &counters.alive_hash_bytes,
            _ => return,
        };
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(bytes))
        });
    });
}

#[cfg(any(test, feature = "test-support"))]
fn account_region_decode(kind: RegionKind, bytes: usize) {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    SEGMENT_COST_AUDIT.with(|slot| {
        let Some(counters) = slot.borrow().as_ref().cloned() else {
            return;
        };
        let counter = match kind {
            RegionKind::VectorFactors => &counters.int8_factor_decode_bytes,
            RegionKind::Postings => &counters.postings_decode_bytes,
            RegionKind::Columns => &counters.columns_decode_bytes,
            RegionKind::Alive => &counters.alive_decode_bytes,
            _ => return,
        };
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(bytes))
        });
    });
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn account_active_postings_decode(bytes: usize) {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    SEGMENT_COST_AUDIT.with(|slot| {
        if let Some(counters) = slot.borrow().as_ref() {
            let _ = counters.postings_decode_bytes.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |current| Some(current.saturating_add(bytes)),
            );
        }
    });
}

fn region_hash(_kind: RegionKind, bytes: &[u8]) -> u64 {
    #[cfg(any(test, feature = "test-support"))]
    account_region_hash(_kind, bytes.len());
    xxh3_64(bytes)
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
    fn open(file: File, path: &Path) -> Result<Self, SegmentError> {
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

    /// Flips `mask` into one byte of the private mapping through copy-on-write,
    /// leaving the file untouched. Test support only: it models bit rot under a
    /// reader that already validated the page, deterministically on every
    /// platform (a file write is not visible through a private mapping on
    /// macOS). `&mut self` proves no `as_bytes` borrow is live during the write.
    #[cfg(any(test, feature = "test-support"))]
    fn corrupt_byte(&mut self, offset: usize, mask: u8) -> std::io::Result<()> {
        if offset >= self.length {
            return Err(std::io::Error::other(
                "corruption offset is outside the mapping",
            ));
        }
        let page_size_raw = unsafe {
            // SAFETY: `_SC_PAGESIZE` takes no pointer arguments and has no preconditions.
            libc::sysconf(libc::_SC_PAGESIZE)
        };
        let page_size = usize::try_from(page_size_raw)
            .ok()
            .filter(|size| *size != 0)
            .ok_or_else(|| std::io::Error::other("sysconf returned an invalid page size"))?;
        let page_offset = offset - offset % page_size;
        let page = self
            .pointer
            .as_ptr()
            .wrapping_add(page_offset)
            .cast::<libc::c_void>();
        // SAFETY: `page` is the page-aligned start of a page inside this live
        // private mapping, so the kernel may grant copy-on-write access to it.
        if unsafe { libc::mprotect(page, page_size, libc::PROT_READ | libc::PROT_WRITE) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `offset < length` keeps the byte inside the mapping, the page
        // is writable, and `&mut self` guarantees no shared view of the bytes.
        unsafe {
            let byte = self.pointer.as_ptr().add(offset);
            byte.write(byte.read() ^ mask);
        }
        // SAFETY: the same page range as above, returned to read-only.
        if unsafe { libc::mprotect(page, page_size, libc::PROT_READ) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
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

impl crate::graph::search::RescoreValidator for SegmentReader {
    fn validate_rows(&self, rows: &[u32]) -> Result<(), String> {
        self.validate_rescore_rows(rows).map_err(|source| {
            format!(
                "exact scores unavailable for segment {}: {source}",
                self.meta.id
            )
        })
    }
}

struct ParsedHeader {
    meta: SegmentMeta,
    header_length: usize,
    entries: Vec<RegionEntry>,
}

/// Validated read-only memory mapping with lazy region checksum verification.
pub struct SegmentReader {
    mapping: MappedFile,
    meta: SegmentMeta,
    #[cfg(any(test, feature = "test-support"))]
    storage_store_directory: PathBuf,
    header_length: usize,
    entries: Vec<RegionEntry>,
    document_versions_validation: OnceLock<Result<(), DocumentVersionValidationError>>,
    // Set by the query path once the stored-text region has passed its checksum
    // and every row offset has been walked; the public `stored_text` never
    // consults it and verifies on every call.
    stored_text_validated: OnceLock<()>,
    // Set only by the get path after the stored-metadata region has passed its
    // checksum and full row-offset walk.
    stored_metadata_validated: OnceLock<()>,
    vector_codes_validated: OnceLock<()>,
    vector_factors_validated: OnceLock<()>,
    rescore_valid_chunks: Mutex<Box<[u64]>>,
    query_accounting: Option<Arc<crate::lifecycle::stats::Accounting>>,
    int8_factors_cache: OnceLock<CachedQueryValue<Arc<Vec<crate::scan::Int8Factors>>>>,
    postings_cache: OnceLock<CachedQueryValue<Arc<crate::fts::sealed::SealedSegment>>>,
    columns_cache: OnceLock<CachedQueryValue<Arc<ColumnStore>>>,
    alive_cache: OnceLock<CachedQueryValue<Arc<AliveSet>>>,
    // Sealed rows ordered by persisted document identity so an exact revision
    // resolves by binary search instead of a full row scan.
    document_version_index_cache: OnceLock<CachedQueryValue<Arc<Box<[u32]>>>>,
    // Inline cached graph metadata is covered by this reader's exactly
    // accounted snapshot slot; its heap-backed scratch is charged to Cache.
    pub(crate) graph_search_cache: crate::lifecycle::graph_cache::SegmentGraphSearchCache,
    // Query-independent half of the fusion vector ceiling. The enclosure is a
    // property of this sealed segment's immutable factor or rescore bytes, so
    // it is walked once per reader and folded with each query's norm after.
    // Two f64 held inline, covered by the accounted snapshot slot.
    vector_ceiling_norm_range: OnceLock<crate::graph::search::GraphSegmentNormRange>,
}

struct CachedQueryValue<T> {
    value: T,
    _accounting: Option<crate::lifecycle::stats::AccountedCounter>,
}

#[derive(Clone, Debug)]
enum DocumentVersionValidationError {
    Geometry(String),
    Checksum,
}

impl DocumentVersionValidationError {
    fn into_segment_error(self, segment_id: SegmentId) -> SegmentError {
        match self {
            Self::Geometry(detail) => SegmentError::Geometry(detail),
            Self::Checksum => FormatError::new(
                format!("segment:{segment_id}:document-versions"),
                FormatCheck::BlockChecksum,
                "document-version region checksum mismatch",
            )
            .into(),
        }
    }
}

/// Validated mmap-backed opaque metadata rows.
#[derive(Clone, Copy, Debug)]
pub struct StoredMetadataRows<'a> {
    offsets: &'a [u8],
    bytes: &'a [u8],
    row_count: usize,
}

impl<'a> StoredMetadataRows<'a> {
    /// Returns the number of dense metadata rows.
    #[must_use]
    pub const fn row_count(self) -> usize {
        self.row_count
    }

    /// Returns one row's original opaque bytes.
    #[must_use]
    pub fn row(self, row: usize) -> Option<&'a [u8]> {
        if row >= self.row_count {
            return None;
        }
        let start_index = row.checked_mul(8)?;
        let end_index = row.checked_add(1)?.checked_mul(8)?;
        let start = self
            .offsets
            .get(start_index..start_index.checked_add(8)?)?
            .try_into()
            .ok()
            .map(u64::from_le_bytes)
            .and_then(|value| usize::try_from(value).ok())?;
        let end = self
            .offsets
            .get(end_index..end_index.checked_add(8)?)?
            .try_into()
            .ok()
            .map(u64::from_le_bytes)
            .and_then(|value| usize::try_from(value).ok())?;
        self.bytes.get(start..end)
    }
}

/// Validated mmap-backed optional UTF-8 source rows.
#[derive(Clone, Copy, Debug)]
pub struct StoredTextRows<'a> {
    present: &'a [u8],
    offsets: &'a [u8],
    bytes: &'a [u8],
    row_count: usize,
}

impl<'a> StoredTextRows<'a> {
    /// Returns the number of dense text rows.
    #[must_use]
    pub const fn row_count(self) -> usize {
        self.row_count
    }

    /// Returns `None` for an invalid row, `Some(None)` for an absent field,
    /// and `Some(Some(text))` for a present UTF-8 field (including empty text).
    #[must_use]
    pub fn row(self, row: usize) -> Option<Option<&'a str>> {
        if row >= self.row_count {
            return None;
        }
        let end_index = row.checked_mul(8)?;
        let end = self
            .offsets
            .get(end_index..end_index.checked_add(8)?)?
            .try_into()
            .ok()
            .map(u64::from_le_bytes)
            .and_then(|value| usize::try_from(value).ok())?;
        let start = if row == 0 {
            0
        } else {
            let start_index = row.checked_sub(1)?.checked_mul(8)?;
            self.offsets
                .get(start_index..start_index.checked_add(8)?)?
                .try_into()
                .ok()
                .map(u64::from_le_bytes)
                .and_then(|value| usize::try_from(value).ok())?
        };
        let byte = *self.present.get(row / 8)?;
        if byte & (1 << (row % 8)) == 0 {
            return (start == end).then_some(None);
        }
        let text = std::str::from_utf8(self.bytes.get(start..end)?).ok()?;
        Some(Some(text))
    }
}

/// Tri-state cache for query-path checksum verification: 0 unset,
/// 1 disabled, 2 enabled. Read from `ZE_VERIFY_QUERY_CHECKSUMS` once.
static QUERY_CHECKSUMS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Forces query-path checksum verification on or off.
///
/// Verification is opt-in and off by default, so a test that asserts the
/// verifying behaviour must turn it on rather than assume it.
#[cfg(any(test, feature = "test-support"))]
pub fn set_query_checksum_verification(enabled: bool) {
    QUERY_CHECKSUMS.store(
        if enabled { 2 } else { 1 },
        std::sync::atomic::Ordering::Relaxed,
    );
}

impl SegmentReader {
    /// Memory-maps a segment and validates only its bounded header/directory.
    pub fn open(vfs: &dyn Vfs, path: &Path, expected_id: SegmentId) -> Result<Self, SegmentError> {
        let file = vfs
            .open_for_map(path)
            .map_err(|error| SegmentError::io(path, error))?;
        let mapping = MappedFile::open(file, path)?;
        let artifact = path.display().to_string();
        let parsed = parse_segment_header(
            &artifact,
            mapping.as_bytes(),
            mapping.length as u64,
            expected_id,
        )?;
        let rescore_valid_chunks = rescore_validation_words(&parsed.entries)?;
        Ok(Self {
            mapping,
            meta: parsed.meta,
            #[cfg(any(test, feature = "test-support"))]
            storage_store_directory: path.parent().unwrap_or(Path::new("")).to_path_buf(),
            header_length: parsed.header_length,
            entries: parsed.entries,
            document_versions_validation: OnceLock::new(),
            stored_text_validated: OnceLock::new(),
            stored_metadata_validated: OnceLock::new(),
            vector_codes_validated: OnceLock::new(),
            vector_factors_validated: OnceLock::new(),
            rescore_valid_chunks: Mutex::new(vec![0_u64; rescore_valid_chunks].into_boxed_slice()),
            query_accounting: None,
            int8_factors_cache: OnceLock::new(),
            postings_cache: OnceLock::new(),
            columns_cache: OnceLock::new(),
            alive_cache: OnceLock::new(),
            document_version_index_cache: OnceLock::new(),
            graph_search_cache: crate::lifecycle::graph_cache::SegmentGraphSearchCache::new(),
            vector_ceiling_norm_range: OnceLock::new(),
        })
    }

    pub(crate) fn open_accounted(
        vfs: &dyn Vfs,
        path: &Path,
        expected: &SegmentMeta,
        accounting: &Arc<crate::lifecycle::stats::Accounting>,
        mut before_reader_allocation: impl FnMut(usize) -> Result<(), crate::lifecycle::StoreError>,
    ) -> Result<Self, crate::lifecycle::StoreError> {
        let file = vfs
            .open_for_map(path)
            .map_err(|error| SegmentError::io(path, error))
            .map_err(crate::lifecycle::StoreError::Segment)?;
        let mapping =
            MappedFile::open(file, path).map_err(crate::lifecycle::StoreError::Segment)?;
        let artifact = path.display().to_string();
        let region_count =
            preflight_region_count(&artifact, mapping.as_bytes(), mapping.length as u64)
                .map_err(crate::lifecycle::StoreError::Segment)?;
        let directory_bytes = region_count
            .checked_mul(std::mem::size_of::<RegionEntry>())
            .ok_or(crate::lifecycle::StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "snapshot",
            })?;
        before_reader_allocation(directory_bytes)?;
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
        let rescore_valid_chunks = rescore_validation_words(&parsed.entries)
            .map_err(crate::lifecycle::StoreError::Segment)?;
        let validation_bytes = rescore_valid_chunks
            .checked_mul(std::mem::size_of::<u64>())
            .ok_or(crate::lifecycle::StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "snapshot",
            })?;
        before_reader_allocation(validation_bytes)?;
        Ok(Self {
            mapping,
            meta: expected.clone(),
            #[cfg(any(test, feature = "test-support"))]
            storage_store_directory: path.parent().unwrap_or(Path::new("")).to_path_buf(),
            header_length: parsed.header_length,
            entries: parsed.entries,
            document_versions_validation: OnceLock::new(),
            stored_text_validated: OnceLock::new(),
            stored_metadata_validated: OnceLock::new(),
            vector_codes_validated: OnceLock::new(),
            vector_factors_validated: OnceLock::new(),
            rescore_valid_chunks: Mutex::new(vec![0_u64; rescore_valid_chunks].into_boxed_slice()),
            query_accounting: Some(Arc::clone(accounting)),
            int8_factors_cache: OnceLock::new(),
            postings_cache: OnceLock::new(),
            columns_cache: OnceLock::new(),
            alive_cache: OnceLock::new(),
            document_version_index_cache: OnceLock::new(),
            graph_search_cache: crate::lifecycle::graph_cache::SegmentGraphSearchCache::new(),
            vector_ceiling_norm_range: OnceLock::new(),
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

    #[cfg(test)]
    pub(crate) fn retained_validation_bytes(&self) -> usize {
        self.rescore_valid_chunks
            .lock()
            .map_or(0, |words| std::mem::size_of_val(words.as_ref()))
    }

    /// Flips `mask` into one mapped byte through copy-on-write, leaving the
    /// segment file untouched, so a test can corrupt what an already-validated
    /// reader sees. `&mut self` guarantees no region view is live meanwhile.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn corrupt_mapped_byte_for_test(&mut self, offset: usize, mask: u8) -> std::io::Result<()> {
        self.mapping.corrupt_byte(offset, mask)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn retained_query_view_bytes(&self) -> u64 {
        let int8_factors = self.int8_factors_cache.get().and_then(|cached| {
            cached
                .value
                .capacity()
                .checked_mul(std::mem::size_of::<crate::scan::Int8Factors>())
                .and_then(arc_resident_bytes::<Vec<crate::scan::Int8Factors>>)
        });
        let postings = self.postings_cache.get().and_then(|cached| {
            cached
                .value
                .resident_bytes()
                .ok()
                .and_then(arc_resident_bytes::<crate::fts::sealed::SealedSegment>)
        });
        let columns = self.columns_cache.get().and_then(|cached| {
            cached
                .value
                .resident_bytes()
                .and_then(arc_resident_bytes::<ColumnStore>)
        });
        let alive = self.alive_cache.get().and_then(|cached| {
            cached
                .value
                .resident_bytes()
                .and_then(arc_resident_bytes::<AliveSet>)
        });
        let document_version_index = self.document_version_index_cache.get().and_then(|cached| {
            cached
                .value
                .len()
                .checked_mul(std::mem::size_of::<u32>())
                .and_then(arc_resident_bytes::<Box<[u32]>>)
        });
        int8_factors
            .into_iter()
            .chain(postings)
            .chain(columns)
            .chain(alive)
            .chain(document_version_index)
            .fold(0_u64, |total, bytes| {
                total.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX))
            })
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
        let actual = region_hash(kind, bytes);
        if actual != entry.checksum {
            #[cfg(any(test, feature = "test-support"))]
            crate::lifecycle::record_storage_segment_checksum_fault(
                &self.storage_store_directory,
                self.meta.id,
                kind,
                0,
                entry.checksum,
                actual,
            );
            return Err(FormatError::checksum_mismatch(
                format!("segment:{}:{kind:?}", self.meta.id),
                FormatCheck::BlockChecksum,
                entry.checksum,
                actual,
            )
            .into());
        }
        Ok(bytes)
    }

    /// Validates and returns one source region by its numeric directory kind.
    pub(crate) fn region_by_id(&self, kind: u16) -> Result<&[u8], SegmentError> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.kind == kind)
            .ok_or_else(|| {
                SegmentError::Geometry(format!("segment is missing region kind {kind}"))
            })?;
        let bytes = self.region_slice(entry)?;
        let actual = xxh3_64(bytes);
        if actual != entry.checksum {
            return Err(FormatError::checksum_mismatch(
                format!("segment:{}:region-{kind}", self.meta.id),
                FormatCheck::BlockChecksum,
                entry.checksum,
                actual,
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
        let actual = region_hash(kind, chunk);
        if actual != expected {
            #[cfg(any(test, feature = "test-support"))]
            crate::lifecycle::record_storage_segment_checksum_fault(
                &self.storage_store_directory,
                self.meta.id,
                kind,
                chunk_index,
                expected,
                actual,
            );
            return Err(FormatError::checksum_mismatch(
                format!("segment:{}:{kind:?}:chunk-{chunk_index}", self.meta.id),
                FormatCheck::BlockChecksum,
                expected,
                actual,
            )
            .into());
        }
        Ok(chunk)
    }

    /// Returns packed Bit4 code bytes exactly as passed to the batch kernel.
    pub fn bit4_codes(&self) -> Result<&[u8], SegmentError> {
        let (header, payload) = self.vector_payload(RegionKind::VectorCodes)?;
        self.bit4_codes_from(header, payload)
    }

    fn bit4_codes_unchecked(&self) -> Result<&[u8], SegmentError> {
        let (header, payload) = self.vector_payload_unchecked(RegionKind::VectorCodes)?;
        self.bit4_codes_from(header, payload)
    }

    fn bit4_codes_from<'a>(
        &'a self,
        header: super::layout::VectorHeader,
        payload: &'a [u8],
    ) -> Result<&'a [u8], SegmentError> {
        if self.meta.scheme != 4 {
            return Err(SegmentError::Geometry(format!(
                "Bit4 codes requested for scheme {}",
                self.meta.scheme
            )));
        }
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

    /// Casts validated full-precision code rows directly from the mapping.
    pub fn f32_codes(&self) -> Result<&[f32], SegmentError> {
        let (header, payload) = self.vector_payload(RegionKind::VectorCodes)?;
        self.f32_codes_from(header, payload)
    }

    /// Validate-once twin of [`Self::f32_codes`].
    pub(crate) fn query_f32_codes(&self) -> Result<&[f32], SegmentError> {
        self.validate_vector_region_once(RegionKind::VectorCodes, &self.vector_codes_validated)?;
        let (header, payload) = self.vector_payload_unchecked(RegionKind::VectorCodes)?;
        self.f32_codes_from(header, payload)
    }

    fn f32_codes_from<'a>(
        &'a self,
        header: super::layout::VectorHeader,
        payload: &'a [u8],
    ) -> Result<&'a [f32], SegmentError> {
        if self.meta.scheme != 0 {
            return Err(SegmentError::Geometry(format!(
                "F32 codes requested for scheme {}",
                self.meta.scheme
            )));
        }
        let expected = (self.meta.dims as usize)
            .checked_mul(self.meta.row_count as usize)
            .ok_or_else(|| SegmentError::Geometry("F32 code length overflow".to_owned()))?;
        let expected_bytes = expected
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| SegmentError::Geometry("F32 code byte length overflow".to_owned()))?;
        let expected_stride = (self.meta.dims as usize)
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| SegmentError::Geometry("F32 code stride overflow".to_owned()))?;
        if header.row_stride_bytes as usize != expected_stride || payload.len() != expected_bytes {
            return Err(SegmentError::Geometry(format!(
                "F32 code stride/length {}/{}, expected {expected_stride}/{expected_bytes}",
                header.row_stride_bytes,
                payload.len()
            )));
        }
        cast_slice::<f32>(payload, expected, "F32 codes")
    }

    /// Casts signed-byte vector codes directly from the validated mmap region.
    pub fn int8_codes(&self) -> Result<&[i8], SegmentError> {
        let (header, payload) = self.vector_payload(RegionKind::VectorCodes)?;
        self.int8_codes_from(header, payload)
    }

    fn int8_codes_unchecked(&self) -> Result<&[i8], SegmentError> {
        let (header, payload) = self.vector_payload_unchecked(RegionKind::VectorCodes)?;
        self.int8_codes_from(header, payload)
    }

    fn int8_codes_from<'a>(
        &'a self,
        header: super::layout::VectorHeader,
        payload: &'a [u8],
    ) -> Result<&'a [i8], SegmentError> {
        if self.meta.scheme != 2 {
            return Err(SegmentError::Geometry(format!(
                "Int8 codes requested for scheme {}",
                self.meta.scheme
            )));
        }
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
        let (header, payload) = self.vector_payload(RegionKind::VectorFactors)?;
        self.bit4_factors_from(header, payload)
    }

    fn bit4_factors_unchecked(&self) -> Result<&[Bit4Factors], SegmentError> {
        let (header, payload) = self.vector_payload_unchecked(RegionKind::VectorFactors)?;
        self.bit4_factors_from(header, payload)
    }

    fn bit4_factors_from<'a>(
        &'a self,
        header: super::layout::VectorHeader,
        payload: &'a [u8],
    ) -> Result<&'a [Bit4Factors], SegmentError> {
        if self.meta.scheme != 4 {
            return Err(SegmentError::Geometry(format!(
                "Bit4 factors requested for scheme {}",
                self.meta.scheme
            )));
        }
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
        let (header, payload) = self.vector_payload(RegionKind::VectorFactors)?;
        self.int8_factors_from(header, payload)
    }

    fn int8_factors_unchecked(&self) -> Result<&[Int8Factors], SegmentError> {
        let (header, payload) = self.vector_payload_unchecked(RegionKind::VectorFactors)?;
        self.int8_factors_from(header, payload)
    }

    fn int8_factors_from<'a>(
        &'a self,
        header: super::layout::VectorHeader,
        payload: &'a [u8],
    ) -> Result<&'a [Int8Factors], SegmentError> {
        if self.meta.scheme != 2 {
            return Err(SegmentError::Geometry(format!(
                "Int8 factors requested for scheme {}",
                self.meta.scheme
            )));
        }
        if header.factor_stride_bytes != 8 {
            return Err(SegmentError::Geometry(format!(
                "Int8 factor stride {}, expected 8",
                header.factor_stride_bytes
            )));
        }
        #[cfg(any(test, feature = "test-support"))]
        account_region_decode(RegionKind::VectorFactors, payload.len());
        cast_slice::<Int8Factors>(payload, self.meta.row_count as usize, "Int8 factors")
    }

    /// Reports whether query paths verify region checksums at all.
    ///
    /// Owner decision 2026-09-04: verification is opt-in and off by
    /// default. A sealed segment is immutable and its checksums are
    /// written and verified at seal; re-verifying at query time defends
    /// only against on-disk corruption after publication, which is what
    /// `Store::health()` and the diagnostics paths are for. Those paths
    /// still verify unconditionally. Set `ZE_VERIFY_QUERY_CHECKSUMS=1`
    /// to restore per-reader verification on the query path.
    fn query_checksums_enabled() -> bool {
        match QUERY_CHECKSUMS.load(std::sync::atomic::Ordering::Relaxed) {
            0 => {
                let enabled =
                    std::env::var_os("ZE_VERIFY_QUERY_CHECKSUMS").is_some_and(|value| value != "0");
                QUERY_CHECKSUMS.store(
                    if enabled { 2 } else { 1 },
                    std::sync::atomic::Ordering::Relaxed,
                );
                enabled
            }
            1 => false,
            _ => true,
        }
    }

    /// Verifies a vector region's checksum once per reader, then serves it
    /// from the mapping without re-hashing.
    ///
    /// The verifying accessors re-hash the whole region on every call. On
    /// a query path that is ruinous: hybrid cross-fill called the
    /// verifying rescore accessor once per cross-filled document and
    /// spent 219 ms of a 221 ms query re-hashing 181 MB. Query paths use
    /// these twins; validation, diagnostics and maintenance keep the
    /// verifying accessors.
    fn validate_vector_region_once(
        &self,
        kind: RegionKind,
        gate: &OnceLock<()>,
    ) -> Result<(), SegmentError> {
        if gate.get().is_some() || !Self::query_checksums_enabled() {
            return Ok(());
        }
        let _ = self.region(kind)?;
        let _ = gate.set(());
        Ok(())
    }

    /// Validate-once twin of [`Self::bit4_codes`].
    pub(crate) fn query_bit4_codes(&self) -> Result<&[u8], SegmentError> {
        self.validate_vector_region_once(RegionKind::VectorCodes, &self.vector_codes_validated)?;
        self.bit4_codes_unchecked()
    }

    /// Validate-once twin of [`Self::int8_codes`].
    pub(crate) fn query_int8_codes(&self) -> Result<&[i8], SegmentError> {
        self.validate_vector_region_once(RegionKind::VectorCodes, &self.vector_codes_validated)?;
        self.int8_codes_unchecked()
    }

    /// Validate-once twin of [`Self::int8_factors`] returning the raw slice.
    pub(crate) fn query_int8_factors_slice(&self) -> Result<&[Int8Factors], SegmentError> {
        self.validate_vector_region_once(
            RegionKind::VectorFactors,
            &self.vector_factors_validated,
        )?;
        self.int8_factors_unchecked()
    }

    /// Returns this reader's remembered query-independent norm enclosure.
    ///
    /// `None` means no query has walked the region yet. The enclosure is
    /// derived only from immutable sealed bytes, so a remembered value is the
    /// same value any later walk would produce.
    pub(crate) fn cached_vector_ceiling_norm_range(
        &self,
    ) -> Option<crate::graph::search::GraphSegmentNormRange> {
        self.vector_ceiling_norm_range.get().copied()
    }

    /// Remembers the enclosure a query walked out of this segment's bytes.
    pub(crate) fn remember_vector_ceiling_norm_range(
        &self,
        range: crate::graph::search::GraphSegmentNormRange,
    ) {
        if self.vector_ceiling_norm_range.set(range).is_err() {
            // A concurrent walk won. It read the same immutable bytes with the
            // same arithmetic, so its enclosure is this enclosure.
        }
    }

    /// Validate-once twin of [`Self::bit4_factors`].
    pub(crate) fn query_bit4_factors(&self) -> Result<&[Bit4Factors], SegmentError> {
        self.validate_vector_region_once(
            RegionKind::VectorFactors,
            &self.vector_factors_validated,
        )?;
        self.bit4_factors_unchecked()
    }

    pub(crate) fn query_int8_factors(
        &self,
    ) -> Result<Arc<Vec<crate::scan::Int8Factors>>, crate::lifecycle::StoreError> {
        if let Some(cached) = self.int8_factors_cache.get() {
            return Ok(Arc::clone(&cached.value));
        }
        let factors = self
            .int8_factors()
            .map_err(crate::lifecycle::StoreError::Segment)?
            .iter()
            .map(|factor| crate::scan::Int8Factors::new(factor.scale, factor.offset))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                crate::lifecycle::StoreError::Segment(SegmentError::Geometry(
                    "Int8 factor is not finite and non-negative".to_owned(),
                ))
            })?;
        let resident_bytes = factors
            .capacity()
            .checked_mul(std::mem::size_of::<crate::scan::Int8Factors>())
            .and_then(arc_resident_bytes::<Vec<crate::scan::Int8Factors>>)
            .ok_or(crate::lifecycle::StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "snapshot",
            })?;
        let cached = CachedQueryValue {
            value: Arc::new(factors),
            _accounting: self.account_query_cache(resident_bytes)?,
        };
        if self.int8_factors_cache.set(cached).is_err() {
            // A concurrent initializer won. Its value and reservation are authoritative.
        }
        self.int8_factors_cache
            .get()
            .map(|cached| Arc::clone(&cached.value))
            .ok_or(crate::lifecycle::StoreError::Synchronization {
                component: "Int8 factors query cache",
            })
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

    pub(crate) fn query_rescore_f32(&self) -> Result<&[f32], SegmentError> {
        self.validate_rescore_byte_range(0, VECTOR_HEADER_LEN)?;
        let (header, payload) = self.vector_payload_unchecked(RegionKind::VectorRescore)?;
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

    pub(crate) fn validate_rescore_rows(&self, rows: &[u32]) -> Result<(), SegmentError> {
        let row_bytes = (self.meta.dims as usize)
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| SegmentError::Geometry("rescore row byte length overflow".to_owned()))?;
        for row in rows {
            if *row >= self.meta.row_count {
                return Err(SegmentError::Geometry(format!(
                    "rescore row {row} is outside {} rows",
                    self.meta.row_count
                )));
            }
            let start = (*row as usize)
                .checked_mul(row_bytes)
                .and_then(|offset| offset.checked_add(VECTOR_HEADER_LEN))
                .ok_or_else(|| SegmentError::Geometry("rescore row offset overflow".to_owned()))?;
            self.validate_rescore_byte_range(start, row_bytes)?;
        }
        Ok(())
    }

    /// Decodes the checksummed metadata region into typed column arrays.
    pub fn columns(&self) -> Result<ColumnStore, SegmentError> {
        let region = self.region(RegionKind::Columns)?;
        #[cfg(any(test, feature = "test-support"))]
        account_region_decode(RegionKind::Columns, region.len());
        let columns = decode_columns(region)?;
        if columns.row_count() != self.meta.row_count {
            return Err(SegmentError::Geometry(format!(
                "column rows {}, header rows {}",
                columns.row_count(),
                self.meta.row_count
            )));
        }
        Ok(columns)
    }

    pub(crate) fn query_columns(&self) -> Result<Arc<ColumnStore>, crate::lifecycle::StoreError> {
        if let Some(cached) = self.columns_cache.get() {
            return Ok(Arc::clone(&cached.value));
        }
        let region = self
            .region(RegionKind::Columns)
            .map_err(crate::lifecycle::StoreError::Segment)?;
        #[cfg(any(test, feature = "test-support"))]
        account_region_decode(RegionKind::Columns, region.len());
        let mut columns = decode_columns(region).map_err(crate::lifecycle::StoreError::Segment)?;
        if columns.row_count() != self.meta.row_count {
            return Err(crate::lifecycle::StoreError::Segment(
                SegmentError::Geometry(format!(
                    "column rows {}, header rows {}",
                    columns.row_count(),
                    self.meta.row_count
                )),
            ));
        }
        columns.compact_for_cache();
        let resident_bytes = columns
            .resident_bytes()
            .and_then(arc_resident_bytes::<ColumnStore>)
            .ok_or(crate::lifecycle::StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "snapshot",
            })?;
        let cached = CachedQueryValue {
            value: Arc::new(columns),
            _accounting: self.account_query_cache(resident_bytes)?,
        };
        if self.columns_cache.set(cached).is_err() {
            // A concurrent initializer won. Its value and reservation are authoritative.
        }
        self.columns_cache
            .get()
            .map(|cached| Arc::clone(&cached.value))
            .ok_or(crate::lifecycle::StoreError::Synchronization {
                component: "columns query cache",
            })
    }

    /// Decodes the optional whole-segment lexical region.
    pub fn postings(&self) -> Result<Option<crate::fts::sealed::SealedSegment>, SegmentError> {
        if !self
            .entries
            .iter()
            .any(|entry| entry.kind == RegionKind::Postings.id())
        {
            return Ok(None);
        }
        let region = self.region(RegionKind::Postings)?;
        #[cfg(any(test, feature = "test-support"))]
        account_region_decode(RegionKind::Postings, region.len());
        let postings = crate::fts::sealed::SealedSegment::decode_region(region)?;
        if postings.row_count() != self.meta.row_count {
            return Err(SegmentError::Geometry(format!(
                "postings rows {}, header rows {}",
                postings.row_count(),
                self.meta.row_count
            )));
        }
        Ok(Some(postings))
    }

    /// Directory identity for already verified lexical payloads. A remapped
    /// reader may reuse statistics only after its normal postings validation.
    pub(crate) fn lexical_postings_identity(&self) -> Result<(u64, u64), SegmentError> {
        let entry = self.entry(RegionKind::Postings)?;
        Ok((entry.length, entry.checksum))
    }

    pub(crate) fn query_postings(
        &self,
    ) -> Result<Option<Arc<crate::fts::sealed::SealedSegment>>, crate::lifecycle::StoreError> {
        if !self
            .entries
            .iter()
            .any(|entry| entry.kind == RegionKind::Postings.id())
        {
            return Ok(None);
        }
        if let Some(cached) = self.postings_cache.get() {
            return Ok(Some(Arc::clone(&cached.value)));
        }
        let region = self
            .region(RegionKind::Postings)
            .map_err(crate::lifecycle::StoreError::Segment)?;
        #[cfg(any(test, feature = "test-support"))]
        account_region_decode(RegionKind::Postings, region.len());
        let postings = crate::fts::sealed::SealedSegment::decode_region(region)
            .map_err(SegmentError::from)
            .map_err(crate::lifecycle::StoreError::Segment)?;
        if postings.row_count() != self.meta.row_count {
            return Err(crate::lifecycle::StoreError::Segment(
                SegmentError::Geometry(format!(
                    "postings rows {}, header rows {}",
                    postings.row_count(),
                    self.meta.row_count
                )),
            ));
        }
        let resident_bytes = postings
            .resident_bytes()
            .map_err(SegmentError::from)
            .map_err(crate::lifecycle::StoreError::Segment)?
            .checked_add(std::mem::size_of::<crate::fts::sealed::SealedSegment>())
            .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
            .ok_or(crate::lifecycle::StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "snapshot",
            })?;
        let mut cache_accounting = self
            .query_accounting
            .as_ref()
            .map(|accounting| {
                crate::lifecycle::stats::AccountedCounter::new(
                    accounting,
                    crate::lifecycle::stats::AllocationComponent::Snapshot,
                )
            })
            .transpose()?;
        if let Some(counter) = cache_accounting.as_mut() {
            counter.set(resident_bytes)?;
        }
        let cached = CachedQueryValue {
            value: Arc::new(postings),
            _accounting: cache_accounting,
        };
        if self.postings_cache.set(cached).is_err() {
            // A concurrent initializer won. Its value and reservation are authoritative.
        }
        self.postings_cache
            .get()
            .map(|cached| Some(Arc::clone(&cached.value)))
            .ok_or(crate::lifecycle::StoreError::Synchronization {
                component: "postings query cache",
            })
    }

    /// Decodes the checksummed alive/tombstone region.
    pub fn alive(&self) -> Result<AliveSet, SegmentError> {
        let region = self.region(RegionKind::Alive)?;
        #[cfg(any(test, feature = "test-support"))]
        account_region_decode(RegionKind::Alive, region.len());
        let alive = decode_alive(region)?;
        if alive.row_count() != self.meta.row_count {
            return Err(SegmentError::Geometry(format!(
                "alive rows {}, header rows {}",
                alive.row_count(),
                self.meta.row_count
            )));
        }
        Ok(alive)
    }

    pub(crate) fn query_alive(&self) -> Result<Arc<AliveSet>, crate::lifecycle::StoreError> {
        if let Some(cached) = self.alive_cache.get() {
            return Ok(Arc::clone(&cached.value));
        }
        let region = self
            .region(RegionKind::Alive)
            .map_err(crate::lifecycle::StoreError::Segment)?;
        #[cfg(any(test, feature = "test-support"))]
        account_region_decode(RegionKind::Alive, region.len());
        let mut alive = decode_alive(region).map_err(crate::lifecycle::StoreError::Segment)?;
        if alive.row_count() != self.meta.row_count {
            return Err(crate::lifecycle::StoreError::Segment(
                SegmentError::Geometry(format!(
                    "alive rows {}, header rows {}",
                    alive.row_count(),
                    self.meta.row_count
                )),
            ));
        }
        alive.compact_for_cache();
        let resident_bytes = alive
            .resident_bytes()
            .and_then(arc_resident_bytes::<AliveSet>)
            .ok_or(crate::lifecycle::StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "snapshot",
            })?;
        let cached = CachedQueryValue {
            value: Arc::new(alive),
            _accounting: self.account_query_cache(resident_bytes)?,
        };
        if self.alive_cache.set(cached).is_err() {
            // A concurrent initializer won. Its value and reservation are authoritative.
        }
        self.alive_cache
            .get()
            .map(|cached| Arc::clone(&cached.value))
            .ok_or(crate::lifecycle::StoreError::Synchronization {
                component: "alive query cache",
            })
    }

    /// Resolves the checksum-validated document-version region once, or
    /// `None` for segments sealed before the region existed.
    fn document_versions_region(&self) -> Result<Option<&[u8]>, SegmentError> {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.kind == RegionKind::DocumentVersions.id())
        else {
            return Ok(None);
        };
        let validation = self.document_versions_validation.get_or_init(|| {
            let bytes = self
                .region_slice(entry)
                .map_err(|error| DocumentVersionValidationError::Geometry(error.to_string()))?;
            if region_hash(RegionKind::DocumentVersions, bytes) != entry.checksum {
                return Err(DocumentVersionValidationError::Checksum);
            }
            let expected = (self.meta.row_count as usize)
                .checked_mul(24)
                .ok_or_else(|| {
                    DocumentVersionValidationError::Geometry(
                        "document-version length overflow".to_owned(),
                    )
                })?;
            if bytes.len() != expected {
                return Err(DocumentVersionValidationError::Geometry(format!(
                    "document-version bytes {}, expected {expected}",
                    bytes.len()
                )));
            }
            Ok(())
        });
        if let Err(error) = validation {
            return Err(error.clone().into_segment_error(self.meta.id));
        }
        self.region_slice_unaccounted(entry).map(Some)
    }

    /// Decodes one optional sealed-row document identity directly from the mapping.
    pub fn document_version(&self, row: usize) -> Result<Option<DocumentVersion>, SegmentError> {
        let Some(bytes) = self.document_versions_region()? else {
            return Ok(None);
        };
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

    /// Finds the sealed row that holds exactly `version`.
    ///
    /// `None` means the segment predates document versions or holds no row
    /// with that exact document id and revision.
    pub fn row_for_document_version(
        &self,
        version: DocumentVersion,
    ) -> Result<Option<usize>, SegmentError> {
        let Some(bytes) = self.document_versions_region()? else {
            return Ok(None);
        };
        let needle = encode_document_version(version);
        Ok(bytes
            .chunks_exact(DOCUMENT_VERSION_BYTES)
            .position(|entry| entry == needle))
    }

    /// Finds the sealed row that holds exactly `version` through the cached
    /// identity-ordered row permutation, building it on first use.
    ///
    /// Equal identities resolve to the lowest row, matching a forward scan.
    pub(crate) fn query_row_for_document_version(
        &self,
        version: DocumentVersion,
    ) -> Result<Option<usize>, crate::lifecycle::StoreError> {
        let Some(bytes) = self
            .document_versions_region()
            .map_err(crate::lifecycle::StoreError::Segment)?
        else {
            return Ok(None);
        };
        let Some(index) = self.query_document_version_index(bytes)? else {
            return Ok(None);
        };
        let needle = encode_document_version(version);
        let position =
            index.partition_point(|&row| document_version_entry(bytes, row) < needle.as_slice());
        Ok(index
            .get(position)
            .copied()
            .filter(|&row| document_version_entry(bytes, row) == needle)
            .map(|row| row as usize))
    }

    /// Finds every live sealed row for `doc_id` through the cached
    /// identity-ordered row permutation, building it on first use.
    pub(crate) fn query_rows_for_doc_id(
        &self,
        doc_id: DocId,
    ) -> Result<Vec<usize>, crate::lifecycle::StoreError> {
        let Some(bytes) = self
            .document_versions_region()
            .map_err(crate::lifecycle::StoreError::Segment)?
        else {
            return Ok(Vec::new());
        };
        let Some(index) = self.query_document_version_index(bytes)? else {
            return Ok(Vec::new());
        };
        let needle = doc_id.get().to_le_bytes();
        let start = index.partition_point(|&row| document_id_entry(bytes, row) < needle.as_slice());
        let end = index.partition_point(|&row| document_id_entry(bytes, row) <= needle.as_slice());
        let matching = index.get(start..end).ok_or_else(|| {
            crate::lifecycle::StoreError::Segment(SegmentError::Geometry(
                "document-id index range is invalid".to_owned(),
            ))
        })?;
        let alive = self.query_alive()?;
        let mut rows = Vec::new();
        rows.try_reserve_exact(matching.len()).map_err(|_| {
            crate::lifecycle::StoreError::AllocationFailed {
                needed: matching
                    .len()
                    .checked_mul(std::mem::size_of::<usize>())
                    .and_then(|bytes| u64::try_from(bytes).ok())
                    .unwrap_or(u64::MAX),
                component: "document-id rows",
            }
        })?;
        for row in matching {
            if alive.is_alive(*row) {
                rows.push(*row as usize);
            }
        }
        Ok(rows)
    }

    fn query_document_version_index(
        &self,
        bytes: &[u8],
    ) -> Result<Option<Arc<Box<[u32]>>>, crate::lifecycle::StoreError> {
        if let Some(cached) = self.document_version_index_cache.get() {
            return Ok(Some(Arc::clone(&cached.value)));
        }
        let mut rows: Vec<u32> = (0..self.meta.row_count).collect();
        rows.sort_unstable_by(|&left, &right| {
            document_version_entry(bytes, left)
                .cmp(document_version_entry(bytes, right))
                .then(left.cmp(&right))
        });
        let index = rows.into_boxed_slice();
        let resident_bytes = index
            .len()
            .checked_mul(std::mem::size_of::<u32>())
            .and_then(arc_resident_bytes::<Box<[u32]>>)
            .ok_or(crate::lifecycle::StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "snapshot",
            })?;
        let cached = CachedQueryValue {
            value: Arc::new(index),
            _accounting: self.account_query_cache(resident_bytes)?,
        };
        if self.document_version_index_cache.set(cached).is_err() {
            // A concurrent initializer won. Its value and reservation are authoritative.
        }
        self.document_version_index_cache
            .get()
            .map(|cached| Some(Arc::clone(&cached.value)))
            .ok_or(crate::lifecycle::StoreError::Synchronization {
                component: "document-version index cache",
            })
    }

    /// Returns validated opaque metadata rows, or `None` for older segments.
    pub fn stored_metadata(&self) -> Result<Option<StoredMetadataRows<'_>>, SegmentError> {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.kind == RegionKind::StoredMetadata.id())
        else {
            return Ok(None);
        };
        let region = self.region(RegionKind::StoredMetadata)?;
        let declared_rows = region
            .get(..4)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| {
                SegmentError::Geometry("stored-metadata header is truncated".to_owned())
            })?;
        let reserved = region
            .get(4..8)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| {
                SegmentError::Geometry("stored-metadata reserved field is truncated".to_owned())
            })?;
        if declared_rows != self.meta.row_count || reserved != 0 {
            return Err(SegmentError::Geometry(format!(
                "stored-metadata rows/reserved {declared_rows}/{reserved}, expected {}/0",
                self.meta.row_count
            )));
        }
        let row_count = declared_rows as usize;
        let offset_bytes = row_count
            .checked_add(1)
            .and_then(|count| count.checked_mul(8))
            .ok_or_else(|| SegmentError::Geometry("stored-metadata offsets overflow".to_owned()))?;
        let offsets_end = 8_usize.checked_add(offset_bytes).ok_or_else(|| {
            SegmentError::Geometry("stored-metadata offset end overflow".to_owned())
        })?;
        let offsets = region.get(8..offsets_end).ok_or_else(|| {
            SegmentError::Geometry("stored-metadata offsets are truncated".to_owned())
        })?;
        let bytes = region.get(offsets_end..).ok_or_else(|| {
            SegmentError::Geometry("stored-metadata payload is truncated".to_owned())
        })?;
        let view = StoredMetadataRows {
            offsets,
            bytes,
            row_count,
        };
        let first = view
            .offsets
            .get(..8)
            .and_then(|value| value.try_into().ok())
            .map(u64::from_le_bytes)
            .ok_or_else(|| {
                SegmentError::Geometry("stored-metadata first offset is missing".to_owned())
            })?;
        if first != 0 {
            return Err(SegmentError::Geometry(format!(
                "stored-metadata first offset is {first}, expected zero"
            )));
        }
        let mut previous = 0_usize;
        for row in 0..row_count {
            let value = view.row(row).ok_or_else(|| {
                SegmentError::Geometry(format!("stored-metadata row {row} is invalid"))
            })?;
            previous = previous.checked_add(value.len()).ok_or_else(|| {
                SegmentError::Geometry("stored-metadata length overflow".to_owned())
            })?;
        }
        if previous != bytes.len() {
            return Err(SegmentError::Geometry(format!(
                "stored-metadata final offset {previous}, bytes {}",
                bytes.len()
            )));
        }
        let _ = entry;
        Ok(Some(view))
    }

    /// Returns validated optional UTF-8 source rows, or `None` for older segments.
    ///
    /// Every call verifies the region checksum and walks every row offset.
    pub fn stored_text(&self) -> Result<Option<StoredTextRows<'_>>, SegmentError> {
        let Some(_) = self
            .entries
            .iter()
            .find(|entry| entry.kind == RegionKind::StoredText.id())
        else {
            return Ok(None);
        };
        let region = self.region(RegionKind::StoredText)?;
        let view = stored_text_view(region, self.meta.row_count)?;
        let StoredTextRows {
            offsets,
            bytes,
            row_count,
            ..
        } = view;
        let mut previous = 0_usize;
        for row in 0..row_count {
            let value = view.row(row).ok_or_else(|| {
                SegmentError::Geometry(format!("stored-text row {row} is invalid"))
            })?;
            let end_index = row.checked_mul(8).ok_or_else(|| {
                SegmentError::Geometry("stored-text offset index overflow".to_owned())
            })?;
            let end = offsets
                .get(end_index..end_index + 8)
                .and_then(|value| value.try_into().ok())
                .map(u64::from_le_bytes)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    SegmentError::Geometry(format!("stored-text offset row {row} is invalid"))
                })?;
            if end < previous || end > bytes.len() {
                return Err(SegmentError::Geometry(format!(
                    "stored-text offset row {row} is outside {previous}..={} bytes",
                    bytes.len()
                )));
            }
            if value.is_none() && end != previous {
                return Err(SegmentError::Geometry(format!(
                    "stored-text absent row {row} owns bytes {previous}..{end}"
                )));
            }
            previous = end;
        }
        if previous != bytes.len() {
            return Err(SegmentError::Geometry(format!(
                "stored-text final offset {previous}, bytes {}",
                bytes.len()
            )));
        }
        Ok(Some(view))
    }

    /// Query-path twin of [`Self::stored_text`]: the checksum and the full
    /// row-offset walk run once per reader, and later calls rebuild the view
    /// from the fixed header geometry alone. Like the document-version region,
    /// later reads come from the mapping without re-verification. Nothing is
    /// retained beyond the validation fact, so there is nothing to account.
    pub(crate) fn query_stored_text(&self) -> Result<Option<StoredTextRows<'_>>, SegmentError> {
        if self.stored_text_validated.get().is_some() || !Self::query_checksums_enabled() {
            let Some(entry) = self
                .entries
                .iter()
                .find(|entry| entry.kind == RegionKind::StoredText.id())
            else {
                return Ok(None);
            };
            let region = self.region_slice(entry)?;
            return stored_text_view(region, self.meta.row_count).map(Some);
        }
        let view = self.stored_text()?;
        if view.is_some() {
            // A concurrent validator may have set this first; same fact either way.
            let _ = self.stored_text_validated.set(());
        }
        Ok(view)
    }

    /// Query-path twin of [`Self::stored_metadata`]: checksum and row-offset
    /// validation run once per reader, then later calls rebuild only the view.
    pub(crate) fn query_stored_metadata(
        &self,
    ) -> Result<Option<StoredMetadataRows<'_>>, SegmentError> {
        if self.stored_metadata_validated.get().is_some() || !Self::query_checksums_enabled() {
            let Some(entry) = self
                .entries
                .iter()
                .find(|entry| entry.kind == RegionKind::StoredMetadata.id())
            else {
                return Ok(None);
            };
            let region = self.region_slice(entry)?;
            return stored_metadata_view(region, self.meta.row_count).map(Some);
        }
        let view = self.stored_metadata()?;
        if view.is_some() {
            let _ = self.stored_metadata_validated.set(());
        }
        Ok(view)
    }
}

fn stored_metadata_view(
    region: &[u8],
    header_rows: u32,
) -> Result<StoredMetadataRows<'_>, SegmentError> {
    let declared_rows = region
        .get(..4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| SegmentError::Geometry("stored-metadata header is truncated".to_owned()))?;
    let reserved = region
        .get(4..8)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| {
            SegmentError::Geometry("stored-metadata reserved field is truncated".to_owned())
        })?;
    if declared_rows != header_rows || reserved != 0 {
        return Err(SegmentError::Geometry(format!(
            "stored-metadata rows/reserved {declared_rows}/{reserved}, expected {header_rows}/0"
        )));
    }
    let row_count = declared_rows as usize;
    let offset_bytes = row_count
        .checked_add(1)
        .and_then(|count| count.checked_mul(8))
        .ok_or_else(|| SegmentError::Geometry("stored-metadata offsets overflow".to_owned()))?;
    let offsets_end = 8_usize
        .checked_add(offset_bytes)
        .ok_or_else(|| SegmentError::Geometry("stored-metadata offset end overflow".to_owned()))?;
    let offsets = region.get(8..offsets_end).ok_or_else(|| {
        SegmentError::Geometry("stored-metadata offsets are truncated".to_owned())
    })?;
    let bytes = region
        .get(offsets_end..)
        .ok_or_else(|| SegmentError::Geometry("stored-metadata payload is truncated".to_owned()))?;
    Ok(StoredMetadataRows {
        offsets,
        bytes,
        row_count,
    })
}

/// Parses the stored-text header geometry into a row view without walking rows.
fn stored_text_view(region: &[u8], header_rows: u32) -> Result<StoredTextRows<'_>, SegmentError> {
    {
        let declared_rows = region
            .get(..4)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| SegmentError::Geometry("stored-text header is truncated".to_owned()))?;
        let reserved = region
            .get(4..8)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| {
                SegmentError::Geometry("stored-text reserved field is truncated".to_owned())
            })?;
        if declared_rows != header_rows || reserved != 0 {
            return Err(SegmentError::Geometry(format!(
                "stored-text rows/reserved {declared_rows}/{reserved}, expected {header_rows}/0"
            )));
        }
        let row_count = declared_rows as usize;
        let bitmap_len = row_count.div_ceil(8);
        let bitmap_end = 8_usize
            .checked_add(bitmap_len)
            .ok_or_else(|| SegmentError::Geometry("stored-text bitmap overflow".to_owned()))?;
        let offsets_len = row_count
            .checked_mul(8)
            .ok_or_else(|| SegmentError::Geometry("stored-text offsets overflow".to_owned()))?;
        let offsets_end = bitmap_end
            .checked_add(offsets_len)
            .ok_or_else(|| SegmentError::Geometry("stored-text offset end overflow".to_owned()))?;
        let present = region.get(8..bitmap_end).ok_or_else(|| {
            SegmentError::Geometry("stored-text presence bitmap is truncated".to_owned())
        })?;
        let offsets = region.get(bitmap_end..offsets_end).ok_or_else(|| {
            SegmentError::Geometry("stored-text offsets are truncated".to_owned())
        })?;
        let bytes = region
            .get(offsets_end..)
            .ok_or_else(|| SegmentError::Geometry("stored-text payload is truncated".to_owned()))?;
        if !row_count.is_multiple_of(8) {
            let used = row_count % 8;
            let padding_mask = !((1_u8 << used) - 1);
            if present.last().is_some_and(|byte| byte & padding_mask != 0) {
                return Err(SegmentError::Geometry(
                    "stored-text presence padding bits are nonzero".to_owned(),
                ));
            }
        }
        Ok(StoredTextRows {
            present,
            offsets,
            bytes,
            row_count,
        })
    }
}

impl SegmentReader {
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

    fn vector_payload_unchecked(
        &self,
        kind: RegionKind,
    ) -> Result<(super::layout::VectorHeader, &[u8]), SegmentError> {
        let entry = self.entry(kind)?;
        let region = self.region_slice_unaccounted(entry)?;
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

    fn validate_rescore_byte_range(&self, start: usize, length: usize) -> Result<(), SegmentError> {
        let entry = self.entry(RegionKind::VectorRescore)?;
        let region_length = usize::try_from(entry.length).map_err(|_| {
            SegmentError::Geometry("rescore region length exceeds usize".to_owned())
        })?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| SegmentError::Geometry("rescore byte range overflow".to_owned()))?;
        if end > region_length {
            return Err(SegmentError::Geometry(format!(
                "rescore byte range {start}..{end} exceeds {region_length}"
            )));
        }
        if length == 0 || !Self::query_checksums_enabled() {
            return Ok(());
        }
        let first_chunk = start / CHECKSUM_CHUNK_BYTES;
        let last_chunk = end.saturating_sub(1) / CHECKSUM_CHUNK_BYTES;
        let mut validated = self.rescore_valid_chunks.lock().map_err(|_| {
            SegmentError::Geometry("rescore validation state is poisoned".to_owned())
        })?;
        for chunk in first_chunk..=last_chunk {
            let word = chunk / u64::BITS as usize;
            let bit = chunk % u64::BITS as usize;
            let mask = 1_u64 << bit;
            let state = validated.get_mut(word).ok_or_else(|| {
                SegmentError::Geometry(format!("rescore checksum chunk {chunk} is untracked"))
            })?;
            if *state & mask != 0 {
                continue;
            }
            let chunk_index = u32::try_from(chunk).map_err(|_| {
                SegmentError::Geometry("rescore checksum chunk exceeds u32".to_owned())
            })?;
            self.region_chunk(RegionKind::VectorRescore, chunk_index)?;
            *state |= mask;
        }
        Ok(())
    }

    fn account_query_cache(
        &self,
        bytes: usize,
    ) -> Result<Option<crate::lifecycle::stats::AccountedCounter>, crate::lifecycle::StoreError>
    {
        let Some(accounting) = &self.query_accounting else {
            return Ok(None);
        };
        let mut counter = crate::lifecycle::stats::AccountedCounter::new(
            accounting,
            crate::lifecycle::stats::AllocationComponent::Snapshot,
        )?;
        counter.set(bytes)?;
        Ok(Some(counter))
    }

    fn entry(&self, kind: RegionKind) -> Result<&RegionEntry, SegmentError> {
        self.entries
            .iter()
            .find(|entry| entry.kind == kind.id())
            .ok_or(SegmentError::MissingRegion(kind))
    }

    fn region_slice(&self, entry: &RegionEntry) -> Result<&[u8], SegmentError> {
        let bytes = self.region_slice_unaccounted(entry)?;
        account_data_read(bytes.len());
        Ok(bytes)
    }

    fn region_slice_unaccounted(&self, entry: &RegionEntry) -> Result<&[u8], SegmentError> {
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

/// Persisted width of one document-version row: a little-endian `u128`
/// document id followed by a little-endian `u64` revision.
const DOCUMENT_VERSION_BYTES: usize = 24;

fn encode_document_version(version: DocumentVersion) -> [u8; DOCUMENT_VERSION_BYTES] {
    let doc_id = version.doc_id().get().to_le_bytes();
    let revision = version.revision().get().to_le_bytes();
    let mut bytes = [0_u8; DOCUMENT_VERSION_BYTES];
    for (slot, byte) in bytes.iter_mut().zip(doc_id.into_iter().chain(revision)) {
        *slot = byte;
    }
    bytes
}

/// Returns the persisted identity bytes of one sealed row. The region length
/// is validated as exactly `row_count * 24` before any caller reaches this,
/// so the empty fallback is unreachable and sorts consistently if it were not.
fn document_version_entry(bytes: &[u8], row: u32) -> &[u8] {
    (row as usize)
        .checked_mul(DOCUMENT_VERSION_BYTES)
        .and_then(|start| bytes.get(start..start.checked_add(DOCUMENT_VERSION_BYTES)?))
        .unwrap_or(&[])
}

fn document_id_entry(bytes: &[u8], row: u32) -> &[u8] {
    document_version_entry(bytes, row)
        .get(..std::mem::size_of::<u128>())
        .unwrap_or(&[])
}

fn arc_resident_bytes<T>(deep_bytes: usize) -> Option<usize> {
    deep_bytes
        .checked_add(std::mem::size_of::<T>())?
        .checked_add(2 * std::mem::size_of::<usize>())
}

fn rescore_validation_words(entries: &[RegionEntry]) -> Result<usize, SegmentError> {
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.kind == RegionKind::VectorRescore.id())
    else {
        return Ok(0);
    };
    let length = usize::try_from(entry.length)
        .map_err(|_| SegmentError::Geometry("rescore region length exceeds usize".to_owned()))?;
    let chunks = length.div_ceil(CHECKSUM_CHUNK_BYTES);
    Ok(chunks.div_ceil(u64::BITS as usize))
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
    #[cfg(any(test, feature = "test-support"))]
    let actual_family = header_bytes
        .get(8..10)
        .and_then(|family| <[u8; 2]>::try_from(family).ok())
        .map(u16::from_le_bytes);
    let fixed =
        decode_header(&artifact, FormatFamily::Segment, &header_bytes).map_err(|error| {
            #[cfg(any(test, feature = "test-support"))]
            crate::lifecycle::record_storage_segment_format_fault(
                &error,
                actual_family,
                expected.id,
            );
            SegmentError::from(error)
        })?;
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
    #[cfg(any(test, feature = "test-support"))]
    let actual_family = bytes
        .get(8..10)
        .and_then(|family| <[u8; 2]>::try_from(family).ok())
        .map(u16::from_le_bytes);
    let fixed = decode_header(artifact, FormatFamily::Segment, bytes).map_err(|error| {
        #[cfg(any(test, feature = "test-support"))]
        crate::lifecycle::record_storage_segment_format_fault(&error, actual_family, expected_id);
        SegmentError::from(error)
    })?;
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
        #[cfg(any(test, feature = "test-support"))]
        crate::lifecycle::record_storage_segment_identity_fault(artifact, expected_id, actual_id);
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
            epoch_id: None,
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
