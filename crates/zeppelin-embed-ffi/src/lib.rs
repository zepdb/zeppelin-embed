//! Stable C ABI for the Zeppelin embedded search engine.
//!
//! Every exported function catches unwinding panics. The crate deliberately
//! does not install a panic hook because a hook is process-global policy owned
//! by the embedding host.

#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unwrap_used,
    unsafe_op_in_unsafe_fn
)]
#![warn(missing_docs)]

mod abi;
mod error;
mod marshal;
mod registry;

use std::any::Any;
use std::ffi::c_char;
use std::mem::{align_of, size_of};
use std::path::Path;
use std::time::Duration;

use error::FfiError;
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{
    AccessMode, CancelToken, Deadline, GraphSearchOptions, OpenOptions, QueryControl,
    SearchOptions, SearchTier, Store, StoreState,
};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus};

pub use abi::*;

macro_rules! ffi_entry {
    ($handle:expr, $panic_value:expr, $body:block) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(value) => value,
            Err(payload) => {
                crate::registry::poison($handle, crate::panic_message(payload));
                $panic_value
            }
        }
    }};
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload crossed the ABI boundary".to_owned()
    }
}

fn finish(handle: Option<ZeHandle>, result: Result<(), FfiError>) -> ZeErrorCode {
    match result {
        Ok(()) => ZeErrorCode::Ok,
        Err(error) => {
            if error.code != ZeErrorCode::Poisoned {
                registry::set_error(handle, error.message);
            }
            error.code
        }
    }
}

fn scalar_output<T>(pointer: *mut T) -> Result<(), FfiError> {
    if pointer.is_null() {
        return Err(FfiError::invalid("output pointer is null"));
    }
    if pointer.align_offset(align_of::<T>()) != 0 {
        return Err(FfiError::invalid("output pointer is misaligned"));
    }
    Ok(())
}

fn bool_u32(value: bool) -> u32 {
    u32::from(value)
}

fn usize_u64(value: usize, field: &'static str) -> Result<u64, FfiError> {
    u64::try_from(value).map_err(|_| {
        FfiError::new(
            ZeErrorCode::Internal,
            format!("{field} does not fit the ABI u64 field"),
        )
    })
}

fn doc_id(value: ZeDocId) -> DocId {
    DocId::new((u128::from(value.high) << 64) | u128::from(value.low))
}

fn ffi_doc_id(value: DocId) -> ZeDocId {
    let value = value.get();
    ZeDocId {
        high: (value >> 64) as u64,
        low: value as u64,
    }
}

fn parse_access(value: i32) -> Result<AccessMode, FfiError> {
    match value {
        0 => Ok(AccessMode::ReadWrite),
        1 => Ok(AccessMode::ReadOnly),
        _ => Err(FfiError::invalid(
            "access_mode discriminant is out of range",
        )),
    }
}

fn parse_durability(value: i32) -> Result<DurabilityMode, FfiError> {
    match value {
        0 => Ok(DurabilityMode::Derived),
        1 => Ok(DurabilityMode::Durable),
        2 => Ok(DurabilityMode::Attached),
        _ => Err(FfiError::invalid(
            "durability_mode discriminant is out of range",
        )),
    }
}

fn parse_commit_tier(value: i32) -> Result<CommitTier, FfiError> {
    match value {
        0 => Ok(CommitTier::None),
        1 => Ok(CommitTier::Ordered),
        2 => Ok(CommitTier::Durable),
        _ => Err(FfiError::invalid(
            "commit_tier discriminant is out of range",
        )),
    }
}

fn parse_graph_profile(value: i32) -> Result<GraphSearchProfile, FfiError> {
    match value {
        0 => Ok(GraphSearchProfile::SiftClass),
        1 => Ok(GraphSearchProfile::Angular),
        _ => Err(FfiError::invalid(
            "graph_profile discriminant is out of range",
        )),
    }
}

fn parse_search_options(request: ZeSearchRequest) -> Result<SearchOptions, FfiError> {
    let profile = parse_graph_profile(request.graph_profile)?;
    let scan = ScanOptions {
        thread_budget: request.thread_budget,
    };
    let tier = match request.search_tier {
        0 => SearchTier::Auto,
        1 => SearchTier::Scan,
        2 => {
            let graph = GraphSearchOptions::new(profile).with_seed(request.graph_seed);
            let graph = if request.graph_ef == 0 {
                graph
            } else {
                graph.with_ef(request.graph_ef)
            };
            SearchTier::Graph(graph)
        }
        _ => {
            return Err(FfiError::invalid(
                "search_tier discriminant is out of range",
            ));
        }
    };
    Ok(SearchOptions::new(scan).with_tier(tier))
}

fn query_control(request: ZeSearchRequest) -> Result<QueryControl, FfiError> {
    if request.cancel_token != 0 && request.deadline_ns != 0 {
        return Err(FfiError::invalid(
            "search accepts either a cancel token or a deadline, not both",
        ));
    }
    if request.cancel_token != 0 {
        return registry::lookup_cancel(request.cancel_token).map(QueryControl::Cancel);
    }
    if request.deadline_ns != 0 {
        return Deadline::after(Duration::from_nanos(request.deadline_ns))
            .map(QueryControl::Deadline)
            .map_err(|error| FfiError::invalid(error.to_string()));
    }
    Ok(QueryControl::Cancel(CancelToken::new()))
}

fn empty_search_result(abi_size: u32) -> ZeSearchResult {
    ZeSearchResult {
        abi_size,
        abi_reserved: 0,
        hits: std::ptr::null_mut(),
        hit_count: 0,
        generation: 0,
        dims_touched: 0,
        bytes_read: 0,
        threads_used: 0,
        graph_segments_traversed: 0,
        graph_validations: 0,
        graph_entry_seed_discoveries: 0,
        graph_visited_epoch_clears: 0,
        graph_candidates_scored: 0,
        graph_candidates_rescored: 0,
        graph_segments_pruned_by_bound: 0,
    }
}

#[cfg(feature = "abi-panic-probe")]
fn named_panic_probe() -> &'static std::sync::Mutex<Option<&'static str>> {
    static PROBE: std::sync::OnceLock<std::sync::Mutex<Option<&'static str>>> =
        std::sync::OnceLock::new();
    PROBE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Arms a one-shot named panic used only by the ABI wrapper contract tests.
#[cfg(feature = "abi-panic-probe")]
pub fn arm_abi_panic_probe(entry_point: &'static str) {
    if let Ok(mut probe) = named_panic_probe().lock() {
        *probe = Some(entry_point);
    }
}

#[cfg(feature = "abi-panic-probe")]
fn run_named_panic_probe(entry_point: &str) {
    let armed = named_panic_probe()
        .lock()
        .ok()
        .and_then(|mut probe| probe.take());
    if armed == Some(entry_point) {
        std::panic::resume_unwind(Box::new(format!("abi panic probe in {entry_point}")));
    }
}

#[cfg(not(feature = "abi-panic-probe"))]
fn run_named_panic_probe(_entry_point: &str) {}

#[cfg(feature = "abi-panic-probe")]
fn run_panic_probe(request: *const ZeSearchRequest) {
    const PROBE: u32 = 0x5041_4e49;
    if request.is_null() || request.align_offset(align_of::<ZeSearchRequest>()) != 0 {
        return;
    }
    let reserved = unsafe { std::ptr::read(request.cast::<u32>().add(1)) };
    if reserved == PROBE {
        std::panic::resume_unwind(Box::new("abi panic probe"));
    }
}

#[cfg(not(feature = "abi-panic-probe"))]
fn run_panic_probe(_request: *const ZeSearchRequest) {}

/// Returns the frozen ABI version, currently `1`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_abi_version() -> u32 {
    ffi_entry!(None, 0, { 1 })
}

/// Opens a store and writes a new generation-tagged handle.
#[unsafe(no_mangle)]
pub extern "C" fn ze_open(request: *const ZeOpenRequest, out_handle: *mut ZeHandle) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::Panic, {
        finish(
            None,
            (|| {
                let request = marshal::read_struct(request)?;
                scalar_output(out_handle)?;
                let path = marshal::utf8_without_nul(request.path, request.path_len)?;
                if path.is_empty() {
                    return Err(FfiError::invalid("store path must not be empty"));
                }
                let access = parse_access(request.access_mode)?;
                let durability = parse_durability(request.durability_mode)?;
                let tier = parse_commit_tier(request.commit_tier)?;
                let options = match access {
                    AccessMode::ReadWrite => OpenOptions::new(),
                    AccessMode::ReadOnly => OpenOptions::read_only(),
                }
                .with_durability(durability, tier)
                .with_reader_drain_timeout(Duration::from_millis(request.reader_drain_timeout_ms))
                .with_max_resident_bytes(request.max_resident_bytes)
                .with_max_temp_bytes(request.max_temp_bytes);
                let store = Store::open(Path::new(path), options).map_err(FfiError::store)?;
                let handle = registry::insert_store(store)?;
                marshal::write_scalar(out_handle, handle);
                Ok(())
            })(),
        )
    })
}

/// Closes a handle; a poisoned handle is released and returns `Poisoned`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_close(handle: ZeHandle) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_close");
        match registry::begin_close(handle) {
            Ok(registry::CloseAccess::Poisoned) => ZeErrorCode::Poisoned,
            Ok(registry::CloseAccess::Store(store)) => {
                let close_result = store.close().map_err(FfiError::store);
                let release_result = registry::finish_close(handle);
                finish(Some(handle), close_result.and(release_result))
            }
            Err(error) => finish(Some(handle), Err(error)),
        }
    })
}

/// Reads the explicit store lifecycle state.
#[unsafe(no_mangle)]
pub extern "C" fn ze_state(handle: ZeHandle, out_report: *mut ZeStateReport) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_state");
        finish(
            Some(handle),
            (|| {
                let abi_size = marshal::validate_output(out_report)?;
                let access = registry::lookup(handle)?;
                let state = match access.store.state().map_err(FfiError::store)? {
                    StoreState::Open => 0,
                    StoreState::Closing => 1,
                    StoreState::Closed => 2,
                };
                marshal::write_output(
                    out_report,
                    ZeStateReport {
                        abi_size,
                        abi_reserved: 0,
                        state,
                        reserved: 0,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Reads exact resource counters for an open store.
#[unsafe(no_mangle)]
pub extern "C" fn ze_stats(handle: ZeHandle, out_report: *mut ZeStatsReport) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_stats");
        finish(
            Some(handle),
            (|| {
                let abi_size = marshal::validate_output(out_report)?;
                let access = registry::lookup(handle)?;
                let stats = access.store.stats().map_err(FfiError::store)?;
                marshal::write_output(
                    out_report,
                    ZeStatsReport {
                        abi_size,
                        abi_reserved: 0,
                        resident_owned_bytes: stats.resident_owned_bytes,
                        mapped_bytes: stats.mapped_bytes,
                        mapped_resident_bytes: stats.mapped_resident_bytes,
                        segment_bytes: stats.segment_bytes,
                        active_segment_bytes: stats.active_segment_bytes,
                        active_row_count: stats.active_row_count,
                        tombstone_count: stats.tombstone_count,
                        tombstone_bytes: stats.tombstone_bytes,
                        wal_bytes: stats.wal_bytes,
                        cache_bytes: stats.cache_bytes,
                        temporary_bytes: stats.temporary_bytes,
                        query_pool_bytes: stats.query_pool_bytes,
                        open_files: stats.open_files,
                        active_queries: stats.active_queries,
                        active_snapshot_leases: stats.active_snapshot_leases,
                        phys_footprint: stats.phys_footprint.unwrap_or(0),
                        has_phys_footprint: bool_u32(stats.phys_footprint.is_some()),
                        reserved: 0,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Atomically ingests caller-owned document records.
#[unsafe(no_mangle)]
pub extern "C" fn ze_ingest(
    handle: ZeHandle,
    request: *const ZeIngestRequest,
    out_report: *mut ZeMutationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_ingest");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                if request.dimension == 0 {
                    return Err(FfiError::invalid("ingest dimension must be nonzero"));
                }
                let vector_bytes = request
                    .dimension
                    .checked_mul(size_of::<f32>())
                    .ok_or_else(|| FfiError::invalid("ingest dimension byte length overflows"))?;
                let records = marshal::read_slice(request.documents, request.document_count)?;
                if records.is_empty() {
                    return Err(FfiError::new(
                        ZeErrorCode::EmptyBatch,
                        "ingest batch is empty",
                    ));
                }
                let mut documents = Vec::new();
                documents.try_reserve_exact(records.len()).map_err(|_| {
                    FfiError::new(
                        ZeErrorCode::OutOfMemory,
                        "ingest document allocation failed",
                    )
                })?;
                for record in records {
                    let record = marshal::read_struct(record as *const ZeIngestDocument)?;
                    if record.vector_len != request.dimension {
                        return Err(FfiError::new(
                            ZeErrorCode::DimensionMismatch,
                            "ingest vector length does not match request dimension",
                        ));
                    }
                    marshal::checked_buffer_len(record.vector_len, size_of::<f32>(), vector_bytes)?;
                    let vector = marshal::copy_slice(record.vector, record.vector_len)?;
                    let metadata = marshal::copy_slice(record.metadata, record.metadata_len)?;
                    documents.push(
                        IngestDocument::new(
                            DocumentVersion::new(
                                doc_id(record.doc_id),
                                Revision::new(record.revision),
                            ),
                            vector,
                        )
                        .with_timestamp(record.timestamp)
                        .with_metadata(metadata),
                    );
                }
                registry::with_writer(handle, |access| {
                    let ack = access
                        .store
                        .ingest(IngestBatch::new(documents))
                        .map_err(FfiError::ingest)?;
                    marshal::write_output(
                        out_report,
                        ZeMutationReport {
                            abi_size,
                            abi_reserved: 0,
                            sequence: ack.seq().get(),
                            generation: ack.generation(),
                        },
                    );
                    Ok(())
                })
            })(),
        )
    })
}

/// Atomically tombstones caller-owned document identifiers.
#[unsafe(no_mangle)]
pub extern "C" fn ze_delete(
    handle: ZeHandle,
    request: *const ZeDeleteRequest,
    out_report: *mut ZeMutationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_delete");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let ids = marshal::read_slice(request.doc_ids, request.doc_id_count)?;
                if ids.is_empty() {
                    return Err(FfiError::new(
                        ZeErrorCode::EmptyBatch,
                        "delete batch is empty",
                    ));
                }
                let ids = ids.iter().copied().map(doc_id).collect::<Vec<_>>();
                registry::with_writer(handle, |access| {
                    let ack = access
                        .store
                        .delete(DeleteBatch::new(ids))
                        .map_err(FfiError::ingest)?;
                    marshal::write_output(
                        out_report,
                        ZeMutationReport {
                            abi_size,
                            abi_reserved: 0,
                            sequence: ack.seq().get(),
                            generation: ack.generation(),
                        },
                    );
                    Ok(())
                })
            })(),
        )
    })
}

/// Searches active and immutable store state with optional cancellation.
#[unsafe(no_mangle)]
pub extern "C" fn ze_search(
    handle: ZeHandle,
    request: *const ZeSearchRequest,
    out_result: *mut ZeSearchResult,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_search");
        run_panic_probe(request);
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_result)?;
                marshal::write_output(out_result, empty_search_result(abi_size));
                if request.k == 0 {
                    return Err(FfiError::invalid("search k must be nonzero"));
                }
                if request.k > ZE_MAX_K {
                    return Err(FfiError::invalid("search k exceeds ZE_MAX_K"));
                }
                if request.dimension == 0 || request.vector_len != request.dimension {
                    return Err(FfiError::new(
                        ZeErrorCode::DimensionMismatch,
                        "search vector length does not match a nonzero dimension",
                    ));
                }
                let vector_bytes = request
                    .dimension
                    .checked_mul(size_of::<f32>())
                    .ok_or_else(|| FfiError::invalid("search dimension byte length overflows"))?;
                marshal::checked_buffer_len(request.vector_len, size_of::<f32>(), vector_bytes)?;
                let vector = marshal::copy_slice(request.vector, request.vector_len)?;
                let options = parse_search_options(request)?;
                let control = query_control(request)?;
                let access = registry::lookup(handle)?;
                let outcome = access
                    .store
                    .search(SearchRequest::new(&vector), request.k, options, control)
                    .map_err(FfiError::query)?;
                let mut hits = Vec::new();
                hits.try_reserve_exact(outcome.candidates.len())
                    .map_err(|_| {
                        FfiError::new(ZeErrorCode::OutOfMemory, "search hit allocation failed")
                    })?;
                for candidate in outcome.candidates {
                    let (source_kind, segment_id) = match candidate.row_id().source() {
                        RowSource::Active => (0, [0_u8; 16]),
                        RowSource::Sealed(segment) => (1, *segment.as_bytes()),
                    };
                    let (has_document, id, revision) = match candidate.document() {
                        Some(document) => {
                            (1, ffi_doc_id(document.doc_id()), document.revision().get())
                        }
                        None => (0, ZeDocId { high: 0, low: 0 }, 0),
                    };
                    hits.push(ZeSearchHit {
                        source_kind,
                        reserved: 0,
                        segment_id,
                        local_row: candidate.row_id().local_row(),
                        has_document,
                        doc_id: id,
                        revision,
                        score: candidate.score(),
                        reserved_tail: 0,
                    });
                }
                let mut hits = hits.into_boxed_slice();
                let hit_count = hits.len();
                let hit_pointer = if hits.is_empty() {
                    std::ptr::null_mut()
                } else {
                    hits.as_mut_ptr()
                };
                registry::register_result(hit_pointer, hit_count)?;
                if !hits.is_empty() {
                    let _raw = Box::into_raw(hits);
                }
                marshal::write_output(
                    out_result,
                    ZeSearchResult {
                        abi_size,
                        abi_reserved: 0,
                        hits: hit_pointer,
                        hit_count,
                        generation: outcome.generation,
                        dims_touched: outcome.stats.dims_touched,
                        bytes_read: outcome.stats.bytes_read,
                        threads_used: usize_u64(outcome.stats.threads_used, "threads_used")?,
                        graph_segments_traversed: usize_u64(
                            outcome.graph_stats.segments_traversed,
                            "graph_segments_traversed",
                        )?,
                        graph_validations: usize_u64(
                            outcome.graph_stats.graph_validations,
                            "graph_validations",
                        )?,
                        graph_entry_seed_discoveries: usize_u64(
                            outcome.graph_stats.entry_seed_discoveries,
                            "graph_entry_seed_discoveries",
                        )?,
                        graph_visited_epoch_clears: usize_u64(
                            outcome.graph_stats.visited_epoch_clears,
                            "graph_visited_epoch_clears",
                        )?,
                        graph_candidates_scored: usize_u64(
                            outcome.graph_stats.candidates_scored,
                            "graph_candidates_scored",
                        )?,
                        graph_candidates_rescored: usize_u64(
                            outcome.graph_stats.candidates_rescored,
                            "graph_candidates_rescored",
                        )?,
                        graph_segments_pruned_by_bound: usize_u64(
                            outcome.graph_stats.segments_pruned_by_bound,
                            "graph_segments_pruned_by_bound",
                        )?,
                    },
                );
                Ok(())
            })(),
        )
    })
}

/// Seals the active segment with an optional cancellation token.
#[unsafe(no_mangle)]
pub extern "C" fn ze_seal(
    handle: ZeHandle,
    request: *const ZeSealRequest,
    out_report: *mut ZeGenerationReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_seal");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let cancel = if request.cancel_token == 0 {
                    None
                } else {
                    Some(registry::lookup_cancel(request.cancel_token)?)
                };
                registry::with_writer(handle, |access| {
                    let generation = match cancel.as_ref() {
                        Some(cancel) => access.store.seal_with_cancel(cancel),
                        None => access.store.seal(),
                    }
                    .map_err(FfiError::store)?;
                    marshal::write_output(
                        out_report,
                        ZeGenerationReport {
                            abi_size,
                            abi_reserved: 0,
                            generation,
                        },
                    );
                    Ok(())
                })
            })(),
        )
    })
}

fn write_partition_report(
    output: *mut ZePartitionReport,
    abi_size: u32,
    report: zeppelin_embed::ingest::DropPartitionReport,
) -> Result<(), FfiError> {
    marshal::write_output(
        output,
        ZePartitionReport {
            abi_size,
            abi_reserved: 0,
            generation: report.generation(),
            segments_dropped: usize_u64(report.segments_dropped().len(), "segments_dropped")?,
            bytes_reclaimed: report.bytes_reclaimed(),
            straddlers_skipped: usize_u64(report.straddlers_skipped().len(), "straddlers_skipped")?,
            is_no_op: bool_u32(report.is_no_op()),
            reserved: 0,
        },
    );
    Ok(())
}

/// Drops immutable segments wholly contained by a timestamp range.
#[unsafe(no_mangle)]
pub extern "C" fn ze_drop_partition(
    handle: ZeHandle,
    request: *const ZeDropPartitionRequest,
    out_report: *mut ZePartitionReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_drop_partition");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                if request.start_ts >= request.end_ts {
                    return Err(FfiError::invalid(
                        "partition range must be a nonempty half-open interval",
                    ));
                }
                registry::with_writer(handle, |access| {
                    let report = access
                        .store
                        .drop_partition(request.start_ts..request.end_ts)
                        .map_err(FfiError::store)?;
                    write_partition_report(out_report, abi_size, report)
                })
            })(),
        )
    })
}

/// Applies a positive retention window at a caller-supplied timestamp.
#[unsafe(no_mangle)]
pub extern "C" fn ze_apply_retention(
    handle: ZeHandle,
    request: *const ZeRetentionRequest,
    out_report: *mut ZePartitionReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_apply_retention");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let policy = zeppelin_embed::ingest::RetentionPolicy::new(request.window)
                    .map_err(|error| FfiError::invalid(error.to_string()))?;
                registry::with_writer(handle, |access| {
                    let report = access
                        .store
                        .apply_retention(policy, request.now_ts)
                        .map_err(FfiError::store)?;
                    write_partition_report(out_report, abi_size, report)
                })
            })(),
        )
    })
}

/// Schedules physical removal of document ids and returns an opaque token.
#[unsafe(no_mangle)]
pub extern "C" fn ze_purge(
    handle: ZeHandle,
    request: *const ZePurgeRequest,
    out_report: *mut ZePurgeTokenReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_purge");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                let ids = marshal::read_slice(request.doc_ids, request.doc_id_count)?;
                if ids.is_empty() {
                    return Err(FfiError::new(
                        ZeErrorCode::EmptyBatch,
                        "purge id batch is empty",
                    ));
                }
                let ids = ids.iter().copied().map(doc_id).collect::<Vec<_>>();
                registry::with_writer(handle, |access| {
                    let token = access.store.purge(&ids).map_err(FfiError::purge)?;
                    let token_id = token.id();
                    access
                        .purge_tokens
                        .lock()
                        .map_err(|_| {
                            FfiError::new(
                                ZeErrorCode::Synchronization,
                                "purge-token registry mutex is poisoned",
                            )
                        })?
                        .insert(token_id, token.clone());
                    marshal::write_output(
                        out_report,
                        ZePurgeTokenReport {
                            abi_size,
                            abi_reserved: 0,
                            token_id,
                            generation: token.generation(),
                            unknown_id_count: usize_u64(
                                token.unknown_ids().len(),
                                "unknown_id_count",
                            )?,
                            is_no_op: bool_u32(token.is_no_op()),
                            reserved: 0,
                        },
                    );
                    Ok(())
                })
            })(),
        )
    })
}

/// Waits until a scheduled purge has removed every reachable physical byte.
#[unsafe(no_mangle)]
pub extern "C" fn ze_await_physical_purge(
    handle: ZeHandle,
    request: *const ZeAwaitPurgeRequest,
    out_report: *mut ZePurgeReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_await_physical_purge");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                registry::with_writer(handle, |access| {
                    let token = access
                        .purge_tokens
                        .lock()
                        .map_err(|_| {
                            FfiError::new(
                                ZeErrorCode::Synchronization,
                                "purge-token registry mutex is poisoned",
                            )
                        })?
                        .get(&request.token_id)
                        .cloned()
                        .ok_or_else(|| {
                            FfiError::invalid("purge token is unknown for this handle")
                        })?;
                    let report = access
                        .store
                        .await_physical_purge(token)
                        .map_err(FfiError::purge)?;
                    access
                        .purge_tokens
                        .lock()
                        .map_err(|_| {
                            FfiError::new(
                                ZeErrorCode::Synchronization,
                                "purge-token registry mutex is poisoned",
                            )
                        })?
                        .remove(&request.token_id);
                    marshal::write_output(
                        out_report,
                        ZePurgeReport {
                            abi_size,
                            abi_reserved: 0,
                            generation: report.generation(),
                            segments_rewritten: usize_u64(
                                report.segments_rewritten(),
                                "segments_rewritten",
                            )?,
                            unknown_id_count: usize_u64(
                                report.unknown_ids().len(),
                                "unknown_id_count",
                            )?,
                            wal_rewritten: bool_u32(report.wal_rewritten()),
                            is_no_op: bool_u32(report.is_no_op()),
                        },
                    );
                    Ok(())
                })
            })(),
        )
    })
}

/// Runs due tier transitions within caller-supplied work budgets.
#[unsafe(no_mangle)]
pub extern "C" fn ze_maintain(
    handle: ZeHandle,
    request: *const ZeMaintainRequest,
    out_report: *mut ZeMaintainReport,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        run_named_panic_probe("ze_maintain");
        finish(
            Some(handle),
            (|| {
                let request = marshal::read_struct(request)?;
                let abi_size = marshal::validate_output(out_report)?;
                registry::with_writer(handle, |access| {
                    let report = access.store.maintain(MaintenanceBudget {
                        wall_time: Duration::from_nanos(request.wall_time_ns),
                        bytes: request.bytes,
                    });
                    let status = match report.status {
                        MaintenanceStatus::Complete => 0,
                        MaintenanceStatus::BudgetExhausted => 1,
                        MaintenanceStatus::Failed(error) => {
                            return Err(FfiError::maintenance(error));
                        }
                    };
                    marshal::write_output(
                        out_report,
                        ZeMaintainReport {
                            abi_size,
                            abi_reserved: 0,
                            graphs_built: report.graphs_built,
                            bytes_consumed: report.bytes_consumed,
                            checkpoints_resumed: report.checkpoints_resumed,
                            status,
                            reserved: 0,
                        },
                    );
                    Ok(())
                })
            })(),
        )
    })
}

/// Copies the per-handle or process-global last error into caller-owned memory.
#[unsafe(no_mangle)]
pub extern "C" fn ze_last_error_message(
    handle: ZeHandle,
    buffer: *mut c_char,
    capacity: usize,
    written: *mut usize,
) -> ZeErrorCode {
    ffi_entry!(Some(handle), ZeErrorCode::Panic, {
        finish(
            if handle == 0 { None } else { Some(handle) },
            (|| {
                scalar_output(written)?;
                let message = registry::last_error(handle)?;
                marshal::write_scalar(written, message.len());
                if capacity == 0 {
                    return Ok(());
                }
                if buffer.is_null() {
                    return Err(FfiError::invalid(
                        "last-error buffer is null with nonzero capacity",
                    ));
                }
                let required = message
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| FfiError::invalid("last-error length overflow"))?;
                if capacity < required {
                    return Err(FfiError::invalid("last-error buffer capacity is too small"));
                }
                marshal::copy_nul_terminated(&message, buffer);
                Ok(())
            })(),
        )
    })
}

/// Returns a static NUL-terminated symbolic name for one numeric error code.
#[unsafe(no_mangle)]
pub extern "C" fn ze_error_code_name(code: i32) -> *const c_char {
    ffi_entry!(None, std::ptr::null(), {
        let bytes: &'static [u8] = match code {
            0 => b"ZE_OK\0",
            1 => b"ZE_ERR_INVALID_ARGUMENT\0",
            2 => b"ZE_ERR_INVALID_HANDLE\0",
            3 => b"ZE_ERR_CLOSED\0",
            4 => b"ZE_ERR_CLOSING\0",
            5 => b"ZE_ERR_POISONED\0",
            6 => b"ZE_ERR_PANIC\0",
            7 => b"ZE_ERR_BUSY\0",
            8 => b"ZE_ERR_STORE_BUSY\0",
            9 => b"ZE_ERR_IO\0",
            10 => b"ZE_ERR_CORRUPT\0",
            11 => b"ZE_ERR_UNSUPPORTED\0",
            12 => b"ZE_ERR_CANCELLED\0",
            13 => b"ZE_ERR_TIMEOUT\0",
            14 => b"ZE_ERR_OUT_OF_MEMORY\0",
            15 => b"ZE_ERR_BUDGET_EXCEEDED\0",
            16 => b"ZE_ERR_EMPTY_BATCH\0",
            17 => b"ZE_ERR_STALE_REVISION\0",
            18 => b"ZE_ERR_DIMENSION_MISMATCH\0",
            19 => b"ZE_ERR_NOT_FOUND\0",
            20 => b"ZE_ERR_SYNCHRONIZATION\0",
            21 => b"ZE_ERR_ACCESS_MODE\0",
            22 => b"ZE_ERR_INTERNAL\0",
            _ => b"ZE_ERR_UNKNOWN\0",
        };
        bytes.as_ptr().cast::<c_char>()
    })
}

/// Creates an active generation-tagged cancellation token.
#[unsafe(no_mangle)]
pub extern "C" fn ze_cancel_token_create(out_token: *mut ZeCancelToken) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::Panic, {
        finish(
            None,
            (|| {
                scalar_output(out_token)?;
                let token = registry::insert_cancel(CancelToken::new())?;
                marshal::write_scalar(out_token, token);
                Ok(())
            })(),
        )
    })
}

/// Requests cancellation; repeated requests are harmless.
#[unsafe(no_mangle)]
pub extern "C" fn ze_cancel_token_cancel(token: ZeCancelToken) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::Panic, {
        finish(
            None,
            registry::lookup_cancel(token).map(|token| token.cancel()),
        )
    })
}

/// Releases a cancellation token; a stale second free returns `Closed`.
#[unsafe(no_mangle)]
pub extern "C" fn ze_cancel_token_free(token: ZeCancelToken) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::Panic, {
        finish(None, registry::free_cancel(token))
    })
}

/// Releases a callee-owned hit array; a zeroed result is a successful no-op.
#[unsafe(no_mangle)]
pub extern "C" fn ze_search_result_free(result: *mut ZeSearchResult) -> ZeErrorCode {
    ffi_entry!(None, ZeErrorCode::Panic, {
        finish(
            None,
            (|| {
                if result.is_null() {
                    return Err(FfiError::invalid("search result pointer is null"));
                }
                if result.align_offset(align_of::<ZeSearchResult>()) != 0 {
                    return Err(FfiError::invalid("search result pointer is misaligned"));
                }
                let abi_size = marshal::read_abi_size(result);
                if abi_size == 0 {
                    let zeroed = marshal::read_value(result);
                    if zeroed.hits.is_null() && zeroed.hit_count == 0 {
                        return Ok(());
                    }
                    return Err(FfiError::invalid(
                        "zero-sized search result contains an allocation",
                    ));
                }
                let abi_size = marshal::validate_output(result)?;
                let current = marshal::read_value(result);
                registry::take_result(current.hits, current.hit_count)?;
                marshal::write_output(result, empty_search_result(abi_size));
                Ok(())
            })(),
        )
    })
}

const _: () = {
    assert!(size_of::<ZeOpenRequest>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
    assert!(size_of::<ZeSearchResult>() <= ZE_ABI_MAX_STRUCT_SIZE as usize);
};
