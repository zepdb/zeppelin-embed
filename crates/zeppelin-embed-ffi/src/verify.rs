//! `ze_verify`: read-only end-to-end store verification over the C ABI.

use std::mem::{align_of, size_of};
use std::path::Path;

use zeppelin_embed::verify::{Finding, FindingKind, VerifyError, verify_store};

use crate::error::FfiError;
use crate::{ZeErrorCode, finish, marshal, registry, run_named_panic_probe};

/// `manifest.ze` is absent, but the WAL or segment files prove a committed
/// snapshot existed and data it covered is now unreachable.
pub const ZE_VERIFY_MANIFEST_MISSING: u32 = 1;
/// The manifest frame, checksum, or payload failed to decode.
pub const ZE_VERIFY_MANIFEST_CORRUPT: u32 = 2;
/// The manifest covers WAL sequences that the WAL does not hold.
pub const ZE_VERIFY_MANIFEST_AHEAD_OF_WAL: u32 = 3;
/// A segment the manifest references does not exist.
pub const ZE_VERIFY_SEGMENT_MISSING: u32 = 4;
/// A segment header, length, identity, or file trailer failed validation.
pub const ZE_VERIFY_SEGMENT_CORRUPT: u32 = 5;
/// A segment header disagrees with the manifest's record of it.
pub const ZE_VERIFY_SEGMENT_MISMATCH: u32 = 6;
/// A segment region's checksum does not match its bytes.
pub const ZE_VERIFY_SEGMENT_REGION_CORRUPT: u32 = 7;
/// A checksum-valid region failed its decoder or cross-structure checks.
pub const ZE_VERIFY_SEGMENT_INDEX_INVALID: u32 = 8;
/// The WAL is absent although the manifest covers WAL sequences.
pub const ZE_VERIFY_WAL_MISSING: u32 = 9;
/// The WAL file header is truncated or invalid.
pub const ZE_VERIFY_WAL_HEADER_CORRUPT: u32 = 10;
/// A WAL record failed framing, checksum, or sequence validation.
pub const ZE_VERIFY_WAL_RECORD_CORRUPT: u32 = 11;
/// A checksum-valid WAL record cannot be replayed into the store.
pub const ZE_VERIFY_WAL_RECORD_INVALID: u32 = 12;
/// A store file exists but could not be read.
pub const ZE_VERIFY_UNREADABLE: u32 = 13;
/// The pending purge intent `purge.ze` failed its frame or decoder.
pub const ZE_VERIFY_PURGE_INTENT_CORRUPT: u32 = 14;
/// A required graph object is absent.
pub const ZE_VERIFY_GRAPH_OBJECT_MISSING: u32 = 15;
/// A required graph object failed length, identity or checksum validation.
pub const ZE_VERIFY_GRAPH_OBJECT_CORRUPT: u32 = 16;
/// A graph checkpoint or inventory failed semantic validation.
pub const ZE_VERIFY_GRAPH_INVENTORY_INVALID: u32 = 17;

/// Store verification request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeVerifyRequest {
    /// Caller-provided `sizeof(ZeVerifyRequest)`.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned UTF-8 store-directory path, without interior NUL bytes.
    pub path: *const u8,
    /// Number of path bytes.
    pub path_len: usize,
}

/// One damaged artifact. Every byte is owned by the containing result arena.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeVerifyFinding {
    /// One of the `ZE_VERIFY_*` kinds. Kinds are append-only.
    pub kind: u32,
    /// `1` when `offset` is meaningful, otherwise `0`.
    pub has_offset: u32,
    /// Byte offset of the damage inside `file`.
    pub offset: u64,
    /// UTF-8 file name relative to the store directory.
    pub file: *const u8,
    /// Number of file-name bytes.
    pub file_len: usize,
    /// UTF-8 decoder detail.
    pub detail: *const u8,
    /// Number of detail bytes.
    pub detail_len: usize,
}

/// Callee-owned verification report; release with `ze_verify_result_free`.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeVerifyResult {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Caller sets zero; callee returns an opaque allocation generation.
    pub abi_reserved: u32,
    /// Callee-owned finding array, or null when `finding_count` is zero.
    pub findings: *mut ZeVerifyFinding,
    /// Number of findings; zero means no damage was found.
    pub finding_count: usize,
    /// Generation of the decoded manifest, or zero without one.
    pub generation: u64,
    /// Segments the manifest references.
    pub segments_checked: u64,
    /// WAL records that passed checksum and sequence validation.
    pub wal_records_checked: u64,
}

/// Walks the store directory at `request.path` and reports every damaged
/// manifest, segment, WAL, and pending purge-intent artifact. The store is never modified: no file
/// is created, written, renamed, locked, or removed, so this is safe to run
/// after an unclean shutdown and before any open. Damage is reported as
/// findings with `ZE_OK`; an error code means the walk could not start
/// (`ZE_ERR_NOT_FOUND` for a missing path, `ZE_ERR_IO` for a path that is not
/// a directory or cannot be listed). A store another process is writing can
/// report a write that is in flight.
#[unsafe(no_mangle)]
pub extern "C" fn ze_verify(
    request: *const ZeVerifyRequest,
    out_result: *mut ZeVerifyResult,
) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_verify");
        finish(
            None,
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_result)?;
                if request.abi_reserved != 0 {
                    return Err(FfiError::invalid(
                        "verify request reserved field is nonzero",
                    ));
                }
                let path = marshal::utf8_without_nul(request.path, request.path_len)?;
                if path.is_empty() {
                    return Err(FfiError::invalid("store path must not be empty"));
                }
                let report = verify_store(Path::new(path)).map_err(verify_error)?;
                let (findings, allocation_generation) = publish_findings(&report.findings)?;
                marshal::write_output(
                    out_result,
                    ZeVerifyResult {
                        abi_size,
                        abi_reserved: allocation_generation,
                        findings,
                        finding_count: report.findings.len(),
                        generation: report.generation,
                        segments_checked: report.segments_checked,
                        wal_records_checked: report.wal_records_checked,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Releases the single arena owned by a verification result. A zeroed result
/// is accepted as a successful no-op.
#[unsafe(no_mangle)]
pub extern "C" fn ze_verify_result_free(result: *mut ZeVerifyResult) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::ZeErrPanic, {
        run_named_panic_probe("ze_verify_result_free");
        finish(
            None,
            (|| {
                if result.is_null() {
                    return Err(FfiError::invalid("verify result pointer is null"));
                }
                if result.align_offset(align_of::<ZeVerifyResult>()) != 0 {
                    return Err(FfiError::invalid("verify result pointer is misaligned"));
                }
                let abi_size = marshal::read_abi_size(result);
                if abi_size == 0 {
                    let zeroed = marshal::read_value(result);
                    if zeroed.findings.is_null() && zeroed.finding_count == 0 {
                        return Ok(());
                    }
                    return Err(FfiError::invalid(
                        "zero-sized verify result contains an allocation",
                    ));
                }
                marshal::validate_abi_size::<ZeVerifyResult>(abi_size)?;
                let current = marshal::read_value(result);
                if current.findings.is_null() != (current.finding_count == 0) {
                    return Err(FfiError::invalid(
                        "verify result pointer and finding count disagree",
                    ));
                }
                registry::take_registered_result(
                    current.findings.cast::<u64>(),
                    current.finding_count,
                    current.abi_reserved,
                )?;
                marshal::write_output(result, empty_result(abi_size));
                Ok(())
            })(),
        )
    })
}

fn empty_result(abi_size: u32) -> ZeVerifyResult {
    ZeVerifyResult {
        abi_size,
        abi_reserved: 0,
        findings: std::ptr::null_mut(),
        finding_count: 0,
        generation: 0,
        segments_checked: 0,
        wal_records_checked: 0,
    }
}

fn verify_error(error: VerifyError) -> FfiError {
    let code = match &error {
        VerifyError::NotFound { .. } => ZeErrorCode::ZeErrNotFound,
        VerifyError::NotDirectory { .. } | VerifyError::Io { .. } => ZeErrorCode::ZeErrIo,
        VerifyError::Tokenizer(_) => ZeErrorCode::ZeErrInternal,
    };
    FfiError::new(code, error.to_string())
}

fn kind_code(kind: FindingKind) -> Result<u32, FfiError> {
    Ok(match kind {
        FindingKind::ManifestMissing => ZE_VERIFY_MANIFEST_MISSING,
        FindingKind::ManifestCorrupt => ZE_VERIFY_MANIFEST_CORRUPT,
        FindingKind::ManifestAheadOfWal => ZE_VERIFY_MANIFEST_AHEAD_OF_WAL,
        FindingKind::SegmentMissing => ZE_VERIFY_SEGMENT_MISSING,
        FindingKind::SegmentCorrupt => ZE_VERIFY_SEGMENT_CORRUPT,
        FindingKind::SegmentMismatch => ZE_VERIFY_SEGMENT_MISMATCH,
        FindingKind::SegmentRegionCorrupt => ZE_VERIFY_SEGMENT_REGION_CORRUPT,
        FindingKind::SegmentIndexInvalid => ZE_VERIFY_SEGMENT_INDEX_INVALID,
        FindingKind::WalMissing => ZE_VERIFY_WAL_MISSING,
        FindingKind::WalHeaderCorrupt => ZE_VERIFY_WAL_HEADER_CORRUPT,
        FindingKind::WalRecordCorrupt => ZE_VERIFY_WAL_RECORD_CORRUPT,
        FindingKind::WalRecordInvalid => ZE_VERIFY_WAL_RECORD_INVALID,
        FindingKind::Unreadable => ZE_VERIFY_UNREADABLE,
        FindingKind::PurgeIntentCorrupt => ZE_VERIFY_PURGE_INTENT_CORRUPT,
        #[cfg(feature = "graph-cypher")]
        FindingKind::GraphObjectMissing => ZE_VERIFY_GRAPH_OBJECT_MISSING,
        #[cfg(feature = "graph-cypher")]
        FindingKind::GraphObjectCorrupt => ZE_VERIFY_GRAPH_OBJECT_CORRUPT,
        #[cfg(feature = "graph-cypher")]
        FindingKind::GraphInventoryInvalid => ZE_VERIFY_GRAPH_INVENTORY_INVALID,
        // `FindingKind` is non-exhaustive. A kind without a code is a build
        // that forgot to extend this table; refuse the report loudly rather
        // than drop or relabel the finding.
        _ => {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrInternal,
                format!("verify finding kind {} has no ABI code", kind.as_str()),
            ));
        }
    })
}

/// Copies every finding and its strings into one u64-aligned arena.
fn publish_findings(findings: &[Finding]) -> Result<(*mut ZeVerifyFinding, u32), FfiError> {
    if findings.is_empty() {
        return Ok((std::ptr::null_mut(), 0));
    }
    let entry_bytes = findings
        .len()
        .checked_mul(size_of::<ZeVerifyFinding>())
        .ok_or_else(|| FfiError::invalid("verify finding bytes overflow"))?;
    let text_bytes = findings.iter().try_fold(0_usize, |total, finding| {
        total
            .checked_add(finding.file.len())
            .and_then(|total| total.checked_add(finding.detail.len()))
            .ok_or_else(|| FfiError::invalid("verify text bytes overflow"))
    })?;
    let arena_bytes = entry_bytes
        .checked_add(text_bytes)
        .ok_or_else(|| FfiError::invalid("verify arena bytes overflow"))?;
    let kinds = findings
        .iter()
        .map(|finding| kind_code(finding.kind))
        .collect::<Result<Vec<_>, _>>()?;
    let words = arena_bytes.div_ceil(size_of::<u64>());
    let mut arena = Vec::<u64>::new();
    arena.try_reserve_exact(words).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "verify result arena allocation failed",
        )
    })?;
    arena.resize(words, 0);
    let mut arena = arena.into_boxed_slice();
    let arena_pointer = arena.as_mut_ptr();
    let entries = arena_pointer.cast::<ZeVerifyFinding>();
    // SAFETY: the arena holds `entry_bytes` of entries followed by
    // `text_bytes` of string storage; every write below stays inside it.
    let mut text = unsafe { arena_pointer.cast::<u8>().add(entry_bytes) };
    for (index, (finding, kind)) in findings.iter().zip(kinds).enumerate() {
        unsafe {
            let file = text;
            std::ptr::copy_nonoverlapping(finding.file.as_ptr(), file, finding.file.len());
            let detail = file.add(finding.file.len());
            std::ptr::copy_nonoverlapping(finding.detail.as_ptr(), detail, finding.detail.len());
            text = detail.add(finding.detail.len());
            std::ptr::write(
                entries.add(index),
                ZeVerifyFinding {
                    kind,
                    has_offset: u32::from(finding.offset.is_some()),
                    offset: finding.offset.unwrap_or(0),
                    file,
                    file_len: finding.file.len(),
                    detail,
                    detail_len: finding.detail.len(),
                },
            );
        }
    }
    let generation = registry::register_result_arena(arena_pointer, arena.len(), findings.len())?;
    let _raw = Box::into_raw(arena);
    Ok((entries, generation))
}
