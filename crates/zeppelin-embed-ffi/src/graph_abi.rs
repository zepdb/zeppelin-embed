//! Graph handle ownership and open/close validation.
mod batch;
use crate::error::FfiError;
use crate::slots::{CloseAccess, SlotTable};
use crate::sync::{Mutex, MutexGuard};
use crate::{ZeErrorCode, ZeGraphControl, ZeGraphHandle, ZeGraphOpenRequest, marshal};
use std::sync::OnceLock;
use zeppelin_embed::epoch::EmbeddingTower;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl};
use zeppelin_embed::property_graph::{GraphStore, GraphStoreError, GraphStoreErrorKind};
const GRAPH_INDEX_BIT: u64 = 1 << 30;
const TEXT_INDEX_BIT: u64 = 1 << 31;
const MAX_RESIDENT_BYTES: u64 = 256 * 1024 * 1024;
/// One open native graph store and the document interpretation it was opened
/// with, which node vectors are validated against.
pub(crate) struct GraphHandleState {
    store: GraphStore,
    #[allow(dead_code)] // Retained for vector validation by the later apply slice.
    document: Option<EmbeddingTower>,
}

fn handles() -> &'static Mutex<SlotTable<GraphHandleState, ()>> {
    static HANDLES: OnceLock<Mutex<SlotTable<GraphHandleState, ()>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(SlotTable::new()))
}

fn lock_handles() -> Result<MutexGuard<'static, SlotTable<GraphHandleState, ()>>, FfiError> {
    handles().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "graph handle registry mutex is poisoned",
        )
    })
}

/// True for a token minted by [`open`]; such a token names no legacy or text store.
pub(crate) const fn is_graph_handle(handle: u64) -> bool {
    handle & GRAPH_INDEX_BIT != 0 && handle & TEXT_INDEX_BIT == 0
}

fn internal(handle: u64) -> Result<u64, FfiError> {
    if !is_graph_handle(handle) {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "handle does not name a graph store",
        ));
    }
    Ok(handle & !GRAPH_INDEX_BIT)
}

pub(crate) fn set_error(handle: u64, message: String) -> Option<String> {
    let Ok(internal) = internal(handle) else {
        return Some(message);
    };
    match handles().lock() {
        Ok(mut table) => table.set_error(internal, message),
        Err(_) => Some(message),
    }
}

pub(crate) fn poison(handle: u64, message: String) -> Option<String> {
    let Ok(internal) = internal(handle) else {
        return Some(message);
    };
    let mut table = match handles().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    table.poison(internal, message)
}

pub(crate) fn last_error(handle: u64) -> Result<String, FfiError> {
    lock_handles()?.last_error(internal(handle)?)
}

fn invalid(message: impl Into<String>) -> FfiError {
    FfiError::invalid(message)
}

/// Reads one graph request descriptor, whose `abi_size` must be exactly this
/// version's size: graph descriptors are fixed-stride, never prefix-extended.
fn read_exact<T: Copy>(pointer: *const T, size: fn(&T) -> u32, what: &str) -> Result<T, FfiError> {
    let value =
        marshal::read_struct(pointer).map_err(|error| invalid(format!("{what}: {}", error.0)))?;
    if size(&value) as usize != std::mem::size_of::<T>() {
        return Err(invalid(format!(
            "{what} abi_size must be exactly {}",
            std::mem::size_of::<T>()
        )));
    }
    Ok(value)
}

fn utf8<'p>(bytes: &'p [u8], what: &str) -> Result<&'p str, FfiError> {
    std::str::from_utf8(bytes).map_err(|_| invalid(format!("{what} is not valid UTF-8")))
}

#[allow(dead_code)] // Shared control decoder for subsequent graph entries.
fn read_control(pointer: *const ZeGraphControl) -> Result<QueryControl, FfiError> {
    if pointer.is_null() {
        return Ok(QueryControl::Cancel(CancelToken::new()));
    }
    let control = read_exact(pointer, |control| control.abi_size, "graph control")?;
    if control.cancel_token != 0 && control.deadline_ns != 0 {
        return Err(invalid(
            "graph control accepts either a cancel token or a deadline, not both",
        ));
    }
    crate::query_control_for(control.cancel_token, control.deadline_ns)
}

fn store_error(error: &GraphStoreError, statement: bool) -> FfiError {
    let code = match error.kind() {
        GraphStoreErrorKind::LegacyStore => ZeErrorCode::ZeErrStoreKind,
        GraphStoreErrorKind::Busy => ZeErrorCode::ZeErrStoreBusy,
        GraphStoreErrorKind::ReadOnly | GraphStoreErrorKind::Unavailable => {
            ZeErrorCode::ZeErrAccessMode
        }
        GraphStoreErrorKind::InvalidRequest => ZeErrorCode::ZeErrInvalidArgument,
        GraphStoreErrorKind::Constraint if statement => ZeErrorCode::ZeErrEndpoint,
        GraphStoreErrorKind::Constraint => ZeErrorCode::ZeErrKeyConflict,
        GraphStoreErrorKind::Limit => ZeErrorCode::ZeErrBudgetExceeded,
        GraphStoreErrorKind::Cancelled => ZeErrorCode::ZeErrCancelled,
        GraphStoreErrorKind::Timeout => ZeErrorCode::ZeErrTimeout,
        GraphStoreErrorKind::Closed => ZeErrorCode::ZeErrClosed,
        GraphStoreErrorKind::Corruption => ZeErrorCode::ZeErrCorrupt,
        GraphStoreErrorKind::Storage => ZeErrorCode::ZeErrIo,
        GraphStoreErrorKind::WriteIndeterminate => ZeErrorCode::ZeErrIndeterminateCommit,
    };
    FfiError::new(code, error.to_string())
}

pub(crate) fn open(
    request: *const ZeGraphOpenRequest,
    out_handle: *mut ZeGraphHandle,
) -> Result<(), FfiError> {
    crate::scalar_output(out_handle)?;
    let request = read_exact(request, |request| request.abi_size, "graph open request")?;
    let path = marshal::read_slice(request.path.data, request.path.count)
        .map_err(|error| invalid(format!("graph path: {}", error.0)))?;
    if path.is_empty() {
        return Err(invalid("graph path must not be empty"));
    }
    if path.contains(&0) {
        return Err(invalid("graph path contains an interior NUL byte"));
    }
    let path = utf8(path, "graph path")?;
    if request.tokenizer_profile != 0 {
        return Err(invalid(
            "tokenizer_profile must be 0, the general-purpose tokenizer",
        ));
    }
    if request.max_resident_bytes == 0 || request.max_resident_bytes > MAX_RESIDENT_BYTES {
        return Err(invalid(
            "max_resident_bytes must be between 1 and 268435456",
        ));
    }
    if !request.control.is_null() {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrUnsupported,
            "graph open does not accept controls yet; pass a null control",
        ));
    }
    let document = if request.document_tower.is_null() {
        None
    } else {
        if request
            .document_tower
            .align_offset(std::mem::align_of::<crate::ZeEmbeddingTower>())
            != 0
        {
            return Err(invalid("document_tower pointer is misaligned"));
        }
        Some(crate::parse_tower(marshal::read_value(
            request.document_tower,
        ))?)
    };
    let options = OpenOptions::new()
        .with_reader_drain_timeout(std::time::Duration::from_millis(
            request.reader_drain_timeout_ms,
        ))
        .with_max_resident_bytes(request.max_resident_bytes);
    use crate::ZeGraphOpenMode as M;
    let store = match request.mode {
        mode if mode == M::ZeGraphOpenCreate as u32 => {
            GraphStore::create(path, options, document.clone())
        }
        mode if mode == M::ZeGraphOpenReadWrite as u32 => {
            GraphStore::open(path, options, document.clone())
        }
        mode if mode == M::ZeGraphOpenReadOnly as u32 => {
            GraphStore::open_read_only(path, options, document.clone())
        }
        _ => {
            return Err(invalid(
                "graph open mode must be 0 create, 1 read-write or 2 read-only",
            ));
        }
    }
    .map_err(|error| store_error(&error, false))?;
    let raw = lock_handles()?.insert(GraphHandleState { store, document }, None)?;
    marshal::write_scalar(
        out_handle,
        ZeGraphHandle {
            token: raw | GRAPH_INDEX_BIT,
        },
    );
    Ok(())
}

/// Closes one graph store and releases its handle. Responses stay valid.
pub(crate) fn close(handle: ZeGraphHandle) -> Result<(), FfiError> {
    let internal = internal(handle.token)?;
    let access = lock_handles()?.begin_close(internal)?;
    match access {
        CloseAccess::Poisoned => Err(FfiError::new(
            ZeErrorCode::ZeErrPoisoned,
            "graph handle is poisoned",
        )),
        CloseAccess::Store(state) => {
            let close = state
                .store
                .close()
                .map_err(|error| store_error(&error, false));
            let release = lock_handles()?.finish_close(internal);
            close.and(release)
        }
    }
}
