//! Graph handles and atomic structured batch C boundary.
mod batch;
use crate::error::FfiError;
use crate::slots::{Access, CloseAccess, SlotTable};
use crate::sync::{Arc, TryLockError};
use crate::sync::{Mutex, MutexGuard};
use crate::{ZeErrorCode, ZeGraphControl, ZeGraphHandle, ZeGraphOpenRequest, marshal};
use std::sync::OnceLock;
use zeppelin_embed::epoch::EmbeddingTower;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl};
use zeppelin_embed::property_graph::{GraphStore, GraphStoreError, GraphStoreErrorKind};

use crate::graph_result::conversion::{ConversionError, ProducerError, apply_and_settle};
use crate::graph_result::{
    GraphResultRegistry, OperationOutcome, OwnerError, SuccessfulOutcome, WriteInterrupted,
    empty_response,
};
use crate::{ZeGraphBatchRequest, ZeGraphDisposition, ZeGraphResponse};
use batch::{MAX_BATCH_ITEMS, Pool, with_batch};
use zeppelin_embed::property_graph::staging::StageError;

static RESPONSES: GraphResultRegistry = GraphResultRegistry::new(4096);

/// Serializes every registry-touching graph call. The ZE-128 registry gate
/// is non-blocking (contention is `OwnerError::Busy`); at the C boundary a
/// second concurrent caller must wait, not fail. Poison is recovered: the
/// registry has its own consistency and `free` re-validates every field.
fn response_gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Writes only the disposition after `begin_response` validates the pointer.
fn set_disposition(out: *mut ZeGraphResponse, disposition: ZeGraphDisposition) {
    // SAFETY: begin_response checked a writable, aligned response descriptor.
    unsafe {
        std::ptr::addr_of_mut!((*out).disposition).write(disposition as u32);
    }
}

const GRAPH_INDEX_BIT: u64 = 1 << 30;
const TEXT_INDEX_BIT: u64 = 1 << 31;
const MAX_RESIDENT_BYTES: u64 = 256 * 1024 * 1024;
/// One open native graph store and the document interpretation it was opened
/// with, which node vectors are validated against.
pub(crate) struct GraphHandleState {
    store: GraphStore,
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

fn lookup(handle: ZeGraphHandle) -> Result<crate::slots::Access<GraphHandleState, ()>, FfiError> {
    lock_handles()?.lookup(internal(handle.token)?, |_| false)
}

/// A second concurrent structured write on one handle is busy.
fn with_graph_writer<T>(
    handle: ZeGraphHandle,
    operation: impl FnOnce(&Access<GraphHandleState, ()>) -> Result<T, FfiError>,
) -> Result<T, FfiError> {
    let access = lookup(handle)?;
    let lock = Arc::clone(&access.writer);
    let guard = match lock.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrBusy,
                "another FFI writer call is active on this handle",
            ));
        }
        Err(TryLockError::Poisoned(_)) => {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrSynchronization,
                "per-handle writer mutex is poisoned",
            ));
        }
    };
    let result = operation(&access);
    drop(guard);
    result
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

/// Checks the caller's response descriptor and empties it before anything
/// else runs, so every return leaves it in a defined state. A refusal after
/// this point reports `NotCommitted` until a later stage knows better.
fn begin_response(out: *mut ZeGraphResponse) -> Result<(), FfiError> {
    marshal::validate_output(out)
        .map_err(|error| invalid(format!("graph response: {}", error.0)))?;
    if marshal::read_abi_size(out.cast_const()) as usize != std::mem::size_of::<ZeGraphResponse>() {
        return Err(invalid(format!(
            "graph response abi_size must be exactly {}",
            std::mem::size_of::<ZeGraphResponse>()
        )));
    }
    let mut empty = empty_response();
    empty.disposition = ZeGraphDisposition::ZeGraphDispositionNotCommitted as u32;
    marshal::write_output(out, empty);
    Ok(())
}

/// Records a known outcome on an error response, which owns nothing.
fn set_outcome(out: *mut ZeGraphResponse, outcome: OperationOutcome) {
    use ZeGraphDisposition as D;
    let (disposition, changed) = match outcome {
        OperationOutcome::NotCommitted => (D::ZeGraphDispositionNotCommitted, None),
        OperationOutcome::Indeterminate => (D::ZeGraphDispositionIndeterminate, None),
        OperationOutcome::Success(SuccessfulOutcome::Read) => {
            (D::ZeGraphDispositionNotApplicable, None)
        }
        OperationOutcome::Success(SuccessfulOutcome::Committed(generation)) => {
            (D::ZeGraphDispositionCommitted, Some(generation.get()))
        }
        OperationOutcome::Success(SuccessfulOutcome::Replayed) => {
            (D::ZeGraphDispositionReplayed, None)
        }
        OperationOutcome::Success(SuccessfulOutcome::NoOp) => (D::ZeGraphDispositionNoOp, None),
    };
    let mut response = empty_response();
    response.disposition = disposition as u32;
    response.has_changed_generation = u32::from(changed.is_some());
    response.changed_generation = changed.unwrap_or(0);
    marshal::write_output(out, response);
}

fn stage_code(error: &StageError) -> ZeErrorCode {
    match error {
        StageError::Lifecycle(error) => error.into(),
        StageError::Canonical(error) => error.into(),
        StageError::Endpoint | StageError::IncidentRelationship => ZeErrorCode::ZeErrEndpoint,
        StageError::MissingEntity => ZeErrorCode::ZeErrNotFound,
        StageError::DeletedEntity => ZeErrorCode::ZeErrDeletedEntity,
        StageError::Limit => ZeErrorCode::ZeErrBudgetExceeded,
        StageError::Cancelled => ZeErrorCode::ZeErrCancelled,
        StageError::IdentityOverflow => ZeErrorCode::ZeErrIdentityOverflow,
        StageError::InvalidLimits | StageError::InvalidInput | StageError::Catalog(_) => {
            ZeErrorCode::ZeErrInvalidArgument
        }
        StageError::Memory(error) => crate::error::FfiError::store_kind_code(error.kind()),
        StageError::ViewMismatch | StageError::NativeStorage(_) => ZeErrorCode::ZeErrCorrupt,
    }
}

/// Maps a graph store refusal onto the append-only C codes. A structured
/// staging refusal keeps its precise code; a graph rule with no finer
/// classification is `ZE_ERR_ENDPOINT` for statements (incident and entity
/// rules) and `ZE_ERR_KEY_CONFLICT` otherwise.
fn store_error(error: &GraphStoreError, statement: bool) -> FfiError {
    let code = if let Some(stage) = error.stage_error() {
        stage_code(stage)
    } else {
        match error.kind() {
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
        }
    };
    FfiError::new(code, error.to_string())
}

fn owner_code(error: &OwnerError) -> ZeErrorCode {
    match error {
        OwnerError::Limit | OwnerError::Memory(_) | OwnerError::RegistryFull => {
            ZeErrorCode::ZeErrBudgetExceeded
        }
        OwnerError::Allocation | OwnerError::TokenExhausted => ZeErrorCode::ZeErrOutOfMemory,
        OwnerError::Busy => ZeErrorCode::ZeErrBusy,
        OwnerError::Poisoned => ZeErrorCode::ZeErrSynchronization,
        OwnerError::InvalidOwner | OwnerError::InvalidShape => ZeErrorCode::ZeErrInvalidArgument,
        OwnerError::Runtime(_) => ZeErrorCode::ZeErrInternal,
    }
}

fn producer_error(error: &ProducerError, statement: bool) -> FfiError {
    match error {
        ProducerError::Store(error) => store_error(error, statement),
        ProducerError::Conversion(ConversionError::Owner(owner)) => FfiError::new(
            owner_code(owner),
            format!("graph response could not be built: {owner}"),
        ),
        ProducerError::Conversion(other) => FfiError::new(
            ZeErrorCode::ZeErrInternal,
            format!("graph response could not be built: {other:?}"),
        ),
    }
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

/// Applies one atomic structured batch and publishes its receipts.
pub(crate) fn apply(
    handle: ZeGraphHandle,
    request: *const ZeGraphBatchRequest,
    out: *mut ZeGraphResponse,
) -> Result<(), FfiError> {
    begin_response(out)?;
    let request = read_exact(request, |request| request.abi_size, "graph batch request")?;
    let items = marshal::read_slice(request.items, request.item_count)
        .map_err(|error| invalid(format!("graph batch items: {}", error.0)))?;
    if items.is_empty() || items.len() > MAX_BATCH_ITEMS {
        return Err(invalid("graph batch must hold between 1 and 16384 items"));
    }
    let pool = Pool::read(request.pool, "graph batch pool")?;
    let control = read_control(request.control)?;
    let guarded = with_graph_writer(handle, |access| {
        let _gate = response_gate();
        with_batch(&pool, items, access.store.document.as_ref(), |writes| {
            set_disposition(out, ZeGraphDisposition::ZeGraphDispositionIndeterminate);
            apply_and_settle(&RESPONSES, &access.store.store, writes, &control)
        })
    })?;
    match guarded.value {
        Ok(Ok(response)) => {
            marshal::write_output(out, response);
            Ok(())
        }
        Ok(Err(error)) => {
            set_outcome(out, guarded.outcome);
            Err(producer_error(&error, false))
        }
        Err(WriteInterrupted::Panicked) => {
            set_outcome(out, guarded.outcome);
            let message = "a panic interrupted the graph write; its outcome is reported in the response disposition".to_owned();
            let _ = poison(handle.token, message.clone());
            Err(FfiError::new(ZeErrorCode::ZeErrPanic, message))
        }
    }
}

pub(crate) fn free(response: *mut ZeGraphResponse) -> Result<(), FfiError> {
    let _gate = response_gate();
    crate::scalar_output(response)?;
    if marshal::read_abi_size(response.cast_const()) as usize
        != std::mem::size_of::<ZeGraphResponse>()
    {
        return Err(invalid(format!(
            "graph response abi_size must be exactly {}",
            std::mem::size_of::<ZeGraphResponse>()
        )));
    }
    // SAFETY: the pointer is non-null and aligned (checked above), and the
    // caller guarantees it names a writable response for this call.
    let root = unsafe { &mut *response };
    if root.owner_token == 0 {
        let mut probe = *root;
        probe.disposition = 0;
        probe.has_changed_generation = 0;
        probe.changed_generation = 0;
        RESPONSES.free(&mut probe).map_err(|error| {
            FfiError::new(owner_code(&error), format!("graph response free: {error}"))
        })?;
        *root = empty_response();
        return Ok(());
    }
    RESPONSES
        .free(root)
        .map(|_| ())
        .map_err(|error| FfiError::new(owner_code(&error), format!("graph response free: {error}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn a_second_writer_on_the_same_graph_handle_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph");
        let bytes = path.to_str().unwrap().as_bytes();
        let request = ZeGraphOpenRequest {
            abi_size: std::mem::size_of::<ZeGraphOpenRequest>() as u32,
            abi_reserved: 0,
            path: crate::ZeGraphBytes {
                data: bytes.as_ptr(),
                count: bytes.len(),
            },
            mode: 0,
            tokenizer_profile: 0,
            document_tower: std::ptr::null(),
            reader_drain_timeout_ms: 250,
            max_resident_bytes: MAX_RESIDENT_BYTES,
            control: std::ptr::null(),
        };
        let mut handle = ZeGraphHandle { token: 0 };
        open(&request, &mut handle).unwrap();
        let access = lookup(handle).unwrap();
        let held = access.writer.lock().unwrap();
        assert_eq!(
            with_graph_writer(handle, |_| Ok(())).unwrap_err().code,
            ZeErrorCode::ZeErrBusy
        );
        drop(held);
        with_graph_writer(handle, |_| Ok(())).unwrap();
        drop(access);
        close(handle).unwrap();
    }
}
