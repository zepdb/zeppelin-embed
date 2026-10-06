//! Graph handles, structured batches and Cypher C boundary.
use crate::{ZeGraphCompileLimits, ZeGraphCypherRequest};
use zeppelin_embed::property_graph::query::completed::{GraphQueryOptions, Outcome};
use zeppelin_embed_cypher::{CompileLimits, ErrorKind, StatementError};
mod batch;
mod completion;
mod options;
mod plan;
mod values;
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
        store_error_code(error.kind(), statement)
    };
    FfiError::new(code, error.to_string())
}

fn store_error_code(kind: GraphStoreErrorKind, statement: bool) -> ZeErrorCode {
    match kind {
        GraphStoreErrorKind::Unsupported => ZeErrorCode::ZeErrUnsupported,
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
        GraphStoreErrorKind::Internal => ZeErrorCode::ZeErrInternal,
        GraphStoreErrorKind::Storage => ZeErrorCode::ZeErrIo,
        GraphStoreErrorKind::WriteIndeterminate => ZeErrorCode::ZeErrIndeterminateCommit,
        _ => ZeErrorCode::ZeErrInternal,
    }
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
    open_declared(request, out_handle, None)
}

pub(crate) fn open_with_relationship_types(
    request: *const ZeGraphOpenRequest,
    rules: *const crate::ZeGraphRelationshipType,
    rule_count: usize,
    out_handle: *mut ZeGraphHandle,
) -> Result<(), FfiError> {
    use zeppelin_embed::property_graph::catalog::{OnDelete, RelationshipRule};
    use zeppelin_embed::property_graph::{GraphName, MAX_GRAPH_CHANGES, MAX_GRAPH_INPUT_BYTES};
    if rule_count > MAX_GRAPH_CHANGES {
        return Err(invalid("too many relationship type declarations"));
    }
    let rules = marshal::read_slice(rules, rule_count).map_err(|error| invalid(error.0))?;
    let mut declared = Vec::new();
    declared
        .try_reserve_exact(rule_count)
        .map_err(|_| invalid("relationship declaration allocation"))?;
    let mut bytes = 0usize;
    for rule in rules {
        rule.validate_header()
            .map_err(|_| invalid("relationship type descriptor header"))?;
        if rule.reserved != 0 {
            return Err(invalid("relationship type reserved field"));
        }
        bytes = bytes
            .checked_add(9)
            .and_then(|n| n.checked_add(rule.name.count))
            .ok_or_else(|| invalid("relationship declaration size"))?;
        if bytes > MAX_GRAPH_INPUT_BYTES {
            return Err(invalid("relationship declaration size"));
        }
        let name = marshal::read_slice(rule.name.data, rule.name.count)
            .map_err(|error| invalid(error.0))?;
        declared.push(RelationshipRule {
            relationship_type: GraphName::new(utf8(name, "relationship type")?)
                .map_err(|_| invalid("relationship type name"))?,
            on_delete: match rule.on_delete {
                1 => OnDelete::Restrict,
                2 => OnDelete::Cascade,
                _ => return Err(invalid("on_delete must be 1 restrict or 2 cascade")),
            },
        });
    }
    open_declared(request, out_handle, Some(&declared))
}

fn open_declared(
    request: *const ZeGraphOpenRequest,
    out_handle: *mut ZeGraphHandle,
    rules: Option<&[zeppelin_embed::property_graph::catalog::RelationshipRule<'_>]>,
) -> Result<(), FfiError> {
    crate::scalar_output(out_handle)?;
    let request = read_exact(request, |request| request.abi_size, "graph open request")?;
    if rules.is_some() && request.mode != 0 {
        return Err(invalid(
            "relationship types can only be declared at creation",
        ));
    }
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
        mode if mode == M::ZeGraphOpenCreate as u32 => match rules {
            Some(rules) => {
                GraphStore::create_with_relationship_types(path, options, document.clone(), rules)
            }
            None => GraphStore::create(path, options, document.clone()),
        },
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
    if items.len() > MAX_BATCH_ITEMS {
        return Err(invalid("graph batch must hold at most 16384 items"));
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

fn compile_code(kind: ErrorKind) -> ZeErrorCode {
    match kind {
        ErrorKind::Syntax => ZeErrorCode::ZeErrQuerySyntax,
        ErrorKind::Unsupported | ErrorKind::SearchContext => ZeErrorCode::ZeErrQueryUnsupported,
        ErrorKind::InvalidParameterUse | ErrorKind::Parameter => ZeErrorCode::ZeErrParameter,
        ErrorKind::Type => ZeErrorCode::ZeErrType,
        ErrorKind::UnknownVariable | ErrorKind::DuplicateVariable => ZeErrorCode::ZeErrScope,
        ErrorKind::DeletedEntity => ZeErrorCode::ZeErrDeletedEntity,
        ErrorKind::Plan(error) => error.into(),
        ErrorKind::Limit(_) => ZeErrorCode::ZeErrBudgetExceeded,
        ErrorKind::Resource(resource) => {
            use zeppelin_embed_cypher::ResourceError as R;
            match resource {
                R::Cancelled | R::ReadCancelled => ZeErrorCode::ZeErrCancelled,
                R::Timeout => ZeErrorCode::ZeErrTimeout,
                R::WorkLimit | R::Memory => ZeErrorCode::ZeErrBudgetExceeded,
                R::Allocation => ZeErrorCode::ZeErrOutOfMemory,
                R::Control => ZeErrorCode::ZeErrInternal,
            }
        }
        ErrorKind::BindingInvariant => ZeErrorCode::ZeErrInternal,
        ErrorKind::InvalidLiteral
        | ErrorKind::InvalidRange
        | ErrorKind::DuplicateProperty
        | ErrorKind::InvalidLimits
        | ErrorKind::RelationshipUniqueness => ZeErrorCode::ZeErrInvalidArgument,
    }
}

fn compile_limits(pointer: *const ZeGraphCompileLimits) -> Result<CompileLimits, FfiError> {
    if pointer.is_null() {
        return Ok(CompileLimits::default());
    }
    let limits = read_exact(pointer, |limits| limits.abi_size, "graph compile limits")?;
    limits
        .validate_shape()
        .map_err(|_| invalid("graph compile limits widen the documented profile ceilings"))?;
    Ok(CompileLimits {
        text_bytes: limits.text_bytes as usize,
        tokens: limits.tokens as usize,
        ast_nodes: limits.ast_nodes as usize,
        depth: limits.depth as usize,
        parameters: limits.parameters as usize,
        columns: limits.columns as usize,
        list_depth: limits.list_depth as usize,
        path_hops: limits.path_hops,
    })
}

/// Compiles and runs one statement of the documented Cypher profile.
pub(crate) fn cypher(
    handle: ZeGraphHandle,
    request: *const ZeGraphCypherRequest,
    out: *mut ZeGraphResponse,
) -> Result<(), FfiError> {
    cypher_with_row_limit(handle, request, 0, out)
}

pub(crate) fn cypher_with_row_limit(
    handle: ZeGraphHandle,
    request: *const ZeGraphCypherRequest,
    result_row_limit: u32,
    out: *mut ZeGraphResponse,
) -> Result<(), FfiError> {
    begin_response(out)?;
    let options = GraphQueryOptions::default()
        .with_result_row_limit(if result_row_limit == 0 {
            1024
        } else {
            result_row_limit as usize
        })
        .map_err(|_| invalid("result_row_limit must be 0 (default 1024) or 1..=65536"))?;
    let request = read_exact(request, |request| request.abi_size, "graph cypher request")?;
    if request.abi_reserved != 0 {
        return Err(invalid("graph cypher reserved must be zero"));
    }
    if request.query.count > 65_536 {
        return Err(invalid("graph query text exceeds 65536 bytes"));
    }
    let text = marshal::read_slice(request.query.data, request.query.count)
        .map_err(|error| invalid(format!("graph query text: {}", error.0)))?;
    let text = utf8(text, "graph query text")?;
    let bindings = marshal::read_slice(request.parameters, request.parameter_count)
        .map_err(|error| invalid(format!("graph parameters: {}", error.0)))?;
    let pool = if request.parameter_pool.is_null() {
        None
    } else {
        Some(Pool::read(request.parameter_pool, "graph parameter pool")?)
    };
    let limits = compile_limits(request.compile_limits)?;
    let control = read_control(request.control)?;
    let access = lookup(handle)?;
    let store = &access.store.store;
    let options = options::query(request.options, access.store.document.as_ref(), options)?;
    values::with_parameters_accounted(
        bindings,
        pool.as_ref(),
        &control,
        |bindings, parameter_bytes| {
            let (_parameter_charge, options) = charge_parameters(store, options, parameter_bytes)?;
            let boundary = completion::Boundary::new(&access, out);
            let result = zeppelin_embed_cypher::execute_with_boundary(
                store.statement_store(),
                &control,
                &options,
                text,
                bindings,
                limits,
                Some(&boundary),
            );
            if let Some(error) = boundary.take_error() {
                set_outcome(out, OperationOutcome::NotCommitted);
                return Err(error);
            }
            let result = match result {
                Ok(result) => result,
                Err(StatementError::Compile(error)) => {
                    set_outcome(out, OperationOutcome::NotCommitted);
                    return Err(FfiError::new(
                        compile_code(error.kind),
                        format!("cypher: {error}"),
                    ));
                }
                Err(StatementError::Query(error)) => {
                    let error = GraphStoreError::from(error);
                    if error.nothing_committed() {
                        set_outcome(out, OperationOutcome::NotCommitted);
                    }
                    return Err(store_error(&error, true));
                }
            };
            set_outcome(
                out,
                OperationOutcome::Success(outcome_of(result.metadata().outcome)),
            );
            crate::run_named_panic_probe("ze_graph_cypher:after-execute");
            let response = boundary.publish(&result)?;
            marshal::write_output(out, response);
            Ok(())
        },
    )
}

fn outcome_of(outcome: Outcome) -> SuccessfulOutcome {
    match outcome {
        Outcome::Read => SuccessfulOutcome::Read,
        Outcome::Committed { changed } => std::num::NonZeroU64::new(changed.get())
            .map_or(SuccessfulOutcome::NoOp, SuccessfulOutcome::Committed),
        Outcome::Replayed => SuccessfulOutcome::Replayed,
        Outcome::NoOp => SuccessfulOutcome::NoOp,
    }
}

// These two frozen descriptors use their second u32 as data, not abi_reserved.
fn validate_maintenance_descriptor<T>(pointer: *const T) -> Result<(), FfiError> {
    if pointer.is_null() || pointer.align_offset(std::mem::align_of::<T>()) != 0 {
        return Err(invalid(
            "graph maintenance descriptor is null or misaligned",
        ));
    }
    if marshal::read_abi_size(pointer) as usize != std::mem::size_of::<T>() {
        return Err(invalid("graph maintenance descriptor abi_size mismatch"));
    }
    Ok(())
}

pub(crate) fn set_maintenance_policy(
    handle: ZeGraphHandle,
    policy: *const crate::ZeGraphMaintenancePolicy,
) -> Result<(), FfiError> {
    with_graph_writer(handle, |access| {
        validate_maintenance_descriptor(policy)?;
        let policy = marshal::read_value(policy);
        if policy.automatic > 1 || policy.reclaim_after_bytes < 1024 * 1024 {
            return Err(invalid(
                "automatic must be 0 or 1 and reclaim_after_bytes at least 1 MiB",
            ));
        }
        access
            .store
            .store
            .set_maintenance_policy(zeppelin_embed::property_graph::GraphMaintenancePolicy {
                automatic: policy.automatic == 1,
                reclaim_after_bytes: policy.reclaim_after_bytes,
            })
            .map_err(|error| store_error(&error, false))
    })
}

pub(crate) fn maintain(
    handle: ZeGraphHandle,
    control: *const ZeGraphControl,
    out: *mut crate::ZeGraphMaintainReport,
) -> Result<(), FfiError> {
    with_graph_writer(handle, |access| {
        validate_maintenance_descriptor(out.cast_const())?;
        let size = std::mem::size_of::<crate::ZeGraphMaintainReport>() as u32;
        let empty = crate::ZeGraphMaintainReport {
            abi_size: size,
            cycle_complete: 0,
            generation: 0,
            replaced_physical_refs: 0,
            new_pack_bytes: 0,
            relocated_bytes: 0,
            drained_packs: 0,
            reclaimed_bytes: 0,
            removed_bytes: 0,
        };
        // SAFETY: descriptor validation checked size and alignment; caller provides writable storage.
        unsafe {
            out.write(empty);
        }
        let control = read_control(control)?;
        let report = access
            .store
            .store
            .maintain(&control)
            .map_err(|error| store_error(&error, false))?;
        // SAFETY: same validated caller-owned report, with no retained pointers.
        unsafe {
            out.write(crate::ZeGraphMaintainReport {
                abi_size: size,
                cycle_complete: u32::from(report.cycle_complete),
                generation: report.generation.get(),
                replaced_physical_refs: report.replaced_physical_refs,
                new_pack_bytes: report.new_pack_bytes,
                relocated_bytes: report.relocated_bytes,
                drained_packs: u64::from(report.drained_packs),
                reclaimed_bytes: report.reclaimed_bytes,
                removed_bytes: report.removed_bytes,
            });
        }
        Ok(())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn ze200_internal_maps_to_existing_ffi_code() {
        for statement in [false, true] {
            assert_eq!(
                store_error_code(GraphStoreErrorKind::Internal, statement),
                ZeErrorCode::ZeErrInternal
            );
        }
    }

    #[test]
    fn an_unsupported_platform_maps_to_ze_err_unsupported() {
        for statement in [false, true] {
            assert_eq!(
                store_error_code(GraphStoreErrorKind::Unsupported, statement),
                ZeErrorCode::ZeErrUnsupported
            );
        }
    }

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
        assert_eq!(
            set_maintenance_policy(handle, std::ptr::null())
                .unwrap_err()
                .code,
            ZeErrorCode::ZeErrBusy
        );
        assert_eq!(
            maintain(handle, std::ptr::null(), std::ptr::null_mut())
                .unwrap_err()
                .code,
            ZeErrorCode::ZeErrBusy
        );
        drop(held);
        with_graph_writer(handle, |_| Ok(())).unwrap();
        drop(access);
        close(handle).unwrap();
    }
}

/// Converts bounded honest-pointer identity arrays before store admission.
fn get_ids<A: Copy, B: TryFrom<A>>(pointer: *const A, count: usize) -> Result<Vec<B>, FfiError> {
    if count > 65_536 {
        return Err(invalid("graph get ID count exceeds 65536"));
    }
    let source = marshal::read_slice(pointer, count).map_err(|e| invalid(e.0))?;
    let mut ids = Vec::new();
    ids.try_reserve_exact(count)
        .map_err(|_| FfiError::new(ZeErrorCode::ZeErrOutOfMemory, "graph get IDs"))?;
    for id in source {
        ids.push(B::try_from(*id).map_err(|_| invalid("graph get ID must be nonzero"))?);
    }
    Ok(ids)
}

pub(crate) fn get_nodes(
    handle: ZeGraphHandle,
    request: *const crate::ZeGraphGetNodesRequest,
    out: *mut ZeGraphResponse,
) -> Result<(), FfiError> {
    begin_response(out)?;
    let request = read_exact(request, |r| r.abi_size, "graph get nodes")?;
    request
        .validate_header()
        .map_err(|_| invalid("graph get nodes header"))?;
    if request.include_text > 1 || request.include_vector > 1 {
        return Err(invalid("graph get selection flags must be 0 or 1"));
    }
    let limits = options::limits(request.limits, GraphQueryOptions::default())?;
    let ids = get_ids(request.ids, request.id_count)?;
    let control = read_control(request.control)?;
    let access = lookup(handle)?;
    let _gate = response_gate();
    let response = crate::graph_result::conversion::run_get_nodes_with_limits(
        &RESPONSES,
        &access.store.store,
        &ids,
        zeppelin_embed::property_graph::GraphGetOptions {
            text: request.include_text == 1,
            vector: request.include_vector == 1,
        },
        &control,
        &limits,
    )
    .map_err(|e| producer_error(&e, false))?;
    marshal::write_output(out, response);
    Ok(())
}

pub(crate) fn get_relationships(
    handle: ZeGraphHandle,
    request: *const crate::ZeGraphGetRelsRequest,
    out: *mut ZeGraphResponse,
) -> Result<(), FfiError> {
    begin_response(out)?;
    let request = read_exact(request, |r| r.abi_size, "graph get relationships")?;
    request
        .validate_header()
        .map_err(|_| invalid("graph get relationships header"))?;
    let limits = options::limits(request.limits, GraphQueryOptions::default())?;
    let ids = get_ids(request.ids, request.id_count)?;
    let control = read_control(request.control)?;
    let access = lookup(handle)?;
    let _gate = response_gate();
    let response = crate::graph_result::conversion::run_get_relationships_with_limits(
        &RESPONSES,
        &access.store.store,
        &ids,
        &control,
        &limits,
    )
    .map_err(|e| producer_error(&e, false))?;
    marshal::write_output(out, response);
    Ok(())
}

pub(crate) fn query(
    handle: ZeGraphHandle,
    request: *const crate::ZeGraphQueryRequest,
    out: *mut ZeGraphResponse,
) -> Result<(), FfiError> {
    begin_response(out)?;
    let request = read_exact(request, |r| r.abi_size, "graph query")?;
    request
        .validate_header()
        .map_err(|_| invalid("graph query header"))?;
    let bindings = marshal::read_slice(request.parameters, request.parameter_count)
        .map_err(|e| invalid(e.0))?;
    let pool = if request.parameter_pool.is_null() {
        None
    } else {
        Some(Pool::read(request.parameter_pool, "query parameter pool")?)
    };
    let control = read_control(request.control)?;
    values::with_parameters_backed(
        bindings,
        pool.as_ref(),
        &control,
        |bindings, backing, scratch_bytes, _retained_bytes| {
            plan::with_plan(request.plan, bindings, backing, |plan, _writes| {
                let access = lookup(handle)?;
                let options = options::query(
                    request.options,
                    access.store.document.as_ref(),
                    GraphQueryOptions::default().with_slot_column_names(),
                )?;
                let (_parameter_charge, options) =
                    charge_parameters(&access.store.store, options, scratch_bytes)?;
                let boundary = completion::Boundary::new(&access, out);
                let result = access
                    .store
                    .store
                    .query_with_boundary(&control, &options, plan, &boundary);
                if let Some(error) = boundary.take_error() {
                    set_outcome(out, OperationOutcome::NotCommitted);
                    return Err(error);
                }
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        if error.nothing_committed() {
                            set_outcome(out, OperationOutcome::NotCommitted);
                        }
                        return Err(store_error(&error, true));
                    }
                };
                set_outcome(
                    out,
                    OperationOutcome::Success(outcome_of(result.metadata().outcome)),
                );
                crate::run_named_panic_probe("ze_graph_query:after-execute");
                marshal::write_output(out, boundary.publish(&result)?);
                Ok(())
            })
        },
    )
}

fn charge_parameters(
    store: &GraphStore,
    options: GraphQueryOptions,
    bytes: usize,
) -> Result<
    (
        zeppelin_embed::property_graph::resources::GraphReservation,
        GraphQueryOptions,
    ),
    FfiError,
> {
    let remaining = options.memory_limit().checked_sub(bytes).ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrBudgetExceeded,
            "parameter backing exceeds query memory",
        )
    })?;
    let resources = store.resources().map_err(|e| store_error(&e, false))?;
    let charge = resources
        .reserve(bytes)
        .map_err(|e| FfiError::new(ZeErrorCode::ZeErrOutOfMemory, e.to_string()))?;
    let options = options
        .with_limits(remaining, options.runtime_limits())
        .map_err(|_| invalid("parameter query memory"))?;
    Ok((charge, options))
}
