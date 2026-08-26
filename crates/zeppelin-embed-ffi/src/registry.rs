use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};

use zeppelin_embed::epoch::EpochIdentity;
use zeppelin_embed::ingest::PurgeToken;
use zeppelin_embed::lifecycle::{CancelToken, Store};

use crate::abi::{ZeCancelToken, ZeErrorCode, ZeHandle};
use crate::error::FfiError;

struct Slot {
    generation: u32,
    store: Option<Arc<Store>>,
    epoch: Option<EpochIdentity>,
    writer: Arc<Mutex<()>>,
    purge_tokens: Arc<Mutex<HashMap<u64, PurgeToken>>>,
    last_error: String,
    poisoned: bool,
    closing: bool,
}

pub(crate) struct HandleAccess {
    pub(crate) store: Arc<Store>,
    /// Identity declared when the handle was opened; attached to every
    /// batch the engine requires a declaration for.
    pub(crate) epoch: Option<EpochIdentity>,
    pub(crate) writer: Arc<Mutex<()>>,
    pub(crate) purge_tokens: Arc<Mutex<HashMap<u64, PurgeToken>>>,
}

pub(crate) enum CloseAccess {
    Poisoned,
    Store(Arc<Store>),
}

#[derive(Clone)]
struct CancelSlot {
    generation: u32,
    token: Option<CancelToken>,
}

fn handles() -> &'static Mutex<Vec<Slot>> {
    static HANDLES: OnceLock<Mutex<Vec<Slot>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(Vec::new()))
}

fn cancels() -> &'static Mutex<Vec<CancelSlot>> {
    static CANCELS: OnceLock<Mutex<Vec<CancelSlot>>> = OnceLock::new();
    CANCELS.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResultAllocation {
    length: usize,
    element: std::any::TypeId,
}

fn result_allocations() -> &'static Mutex<HashMap<usize, ResultAllocation>> {
    static ALLOCATIONS: OnceLock<Mutex<HashMap<usize, ResultAllocation>>> = OnceLock::new();
    ALLOCATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn global_error() -> &'static Mutex<String> {
    static GLOBAL_ERROR: OnceLock<Mutex<String>> = OnceLock::new();
    GLOBAL_ERROR.get_or_init(|| Mutex::new(String::new()))
}

fn lock_handles() -> Result<MutexGuard<'static, Vec<Slot>>, FfiError> {
    handles().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "global handle registry mutex is poisoned",
        )
    })
}

fn decode(value: u64) -> Result<(usize, u32), FfiError> {
    if value == 0 {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "handle zero is permanently invalid",
        ));
    }
    let generation = u32::try_from(value >> 32)
        .map_err(|_| FfiError::new(ZeErrorCode::ZeErrInvalidHandle, "invalid handle generation"))?;
    if generation == 0 {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "handle generation zero is permanently reserved",
        ));
    }
    let index = usize::try_from(value & u64::from(u32::MAX))
        .map_err(|_| FfiError::new(ZeErrorCode::ZeErrInvalidHandle, "invalid handle slot"))?;
    Ok((index, generation))
}

fn encode(index: usize, generation: u32) -> Result<u64, FfiError> {
    let index = u32::try_from(index).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "handle registry exhausted its u32 slot address space",
        )
    })?;
    Ok((u64::from(generation) << 32) | u64::from(index))
}

fn bump_generation(generation: &mut u32) {
    *generation = generation.checked_add(1).unwrap_or(0);
}

pub(crate) fn insert_store(
    store: Store,
    epoch: Option<EpochIdentity>,
) -> Result<ZeHandle, FfiError> {
    let mut registry = lock_handles()?;
    if let Some((index, slot)) = registry
        .iter_mut()
        .enumerate()
        .find(|(_, slot)| slot.store.is_none() && !slot.closing && slot.generation != 0)
    {
        slot.store = Some(Arc::new(store));
        slot.epoch = epoch;
        slot.writer = Arc::new(Mutex::new(()));
        slot.purge_tokens = Arc::new(Mutex::new(HashMap::new()));
        slot.last_error.clear();
        slot.poisoned = false;
        return encode(index, slot.generation);
    }
    let index = registry.len();
    registry.try_reserve(1).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "handle registry allocation failed",
        )
    })?;
    registry.push(Slot {
        generation: 1,
        store: Some(Arc::new(store)),
        epoch,
        writer: Arc::new(Mutex::new(())),
        purge_tokens: Arc::new(Mutex::new(HashMap::new())),
        last_error: String::new(),
        poisoned: false,
        closing: false,
    });
    encode(index, 1)
}

pub(crate) fn lookup(handle: ZeHandle) -> Result<HandleAccess, FfiError> {
    let (index, generation) = decode(handle)?;
    let registry = lock_handles()?;
    let slot = registry.get(index).ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "handle slot was never allocated",
        )
    })?;
    if slot.generation != generation || slot.store.is_none() {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "handle is closed or belongs to a stale generation",
        ));
    }
    if slot.poisoned {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrPoisoned,
            "handle was poisoned by a caught panic",
        ));
    }
    if slot.closing {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrClosing,
            "handle is closing",
        ));
    }
    let store = slot.store.as_ref().cloned().ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "handle store has already been released",
        )
    })?;
    Ok(HandleAccess {
        store,
        epoch: slot.epoch,
        writer: Arc::clone(&slot.writer),
        purge_tokens: Arc::clone(&slot.purge_tokens),
    })
}

pub(crate) fn with_writer<T>(
    handle: ZeHandle,
    operation: impl FnOnce(&HandleAccess) -> Result<T, FfiError>,
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

pub(crate) fn begin_close(handle: ZeHandle) -> Result<CloseAccess, FfiError> {
    let (index, generation) = decode(handle)?;
    let mut registry = lock_handles()?;
    let slot = registry.get_mut(index).ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "handle slot was never allocated",
        )
    })?;
    if slot.generation != generation || slot.store.is_none() {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "handle is closed or belongs to a stale generation",
        ));
    }
    if slot.poisoned {
        slot.store = None;
        slot.epoch = None;
        slot.closing = false;
        slot.poisoned = false;
        slot.purge_tokens = Arc::new(Mutex::new(HashMap::new()));
        bump_generation(&mut slot.generation);
        return Ok(CloseAccess::Poisoned);
    }
    if slot.closing {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrClosing,
            "handle is already closing",
        ));
    }
    slot.closing = true;
    let store = slot.store.as_ref().cloned().ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "handle store has already been released",
        )
    })?;
    Ok(CloseAccess::Store(store))
}

pub(crate) fn finish_close(handle: ZeHandle) -> Result<(), FfiError> {
    let (index, generation) = decode(handle)?;
    let mut registry = lock_handles()?;
    let slot = registry.get_mut(index).ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "handle slot was never allocated",
        )
    })?;
    if slot.generation != generation {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "handle became stale while closing",
        ));
    }
    slot.store = None;
    slot.epoch = None;
    slot.closing = false;
    slot.poisoned = false;
    slot.purge_tokens = Arc::new(Mutex::new(HashMap::new()));
    bump_generation(&mut slot.generation);
    Ok(())
}

pub(crate) fn set_error(handle: Option<ZeHandle>, message: String) {
    if let Some(handle) = handle
        && let Ok((index, generation)) = decode(handle)
        && let Ok(mut registry) = handles().lock()
        && let Some(slot) = registry.get_mut(index)
        && slot.generation == generation
    {
        slot.last_error = message;
        return;
    }
    if let Ok(mut error) = global_error().lock() {
        *error = message;
    }
}

pub(crate) fn poison(handle: Option<ZeHandle>, message: String) {
    if let Some(handle) = handle
        && let Ok((index, generation)) = decode(handle)
    {
        let mut registry = match handles().lock() {
            Ok(registry) => registry,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(slot) = registry.get_mut(index)
            && slot.generation == generation
            && slot.store.is_some()
        {
            slot.poisoned = true;
            slot.last_error = message;
            return;
        }
    }
    let mut error = match global_error().lock() {
        Ok(error) => error,
        Err(poisoned) => poisoned.into_inner(),
    };
    *error = message;
}

pub(crate) fn last_error(handle: ZeHandle) -> Result<String, FfiError> {
    if handle == 0 {
        return global_error()
            .lock()
            .map(|message| message.clone())
            .map_err(|_| {
                FfiError::new(
                    ZeErrorCode::ZeErrSynchronization,
                    "global last-error mutex is poisoned",
                )
            });
    }
    let (index, generation) = decode(handle)?;
    let registry = lock_handles()?;
    let slot = registry.get(index).ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "handle slot was never allocated",
        )
    })?;
    if slot.generation != generation || slot.store.is_none() {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "handle is closed or belongs to a stale generation",
        ));
    }
    Ok(slot.last_error.clone())
}

pub(crate) fn insert_cancel(token: CancelToken) -> Result<ZeCancelToken, FfiError> {
    let mut registry = cancels().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "cancel registry mutex is poisoned",
        )
    })?;
    if let Some((index, slot)) = registry
        .iter_mut()
        .enumerate()
        .find(|(_, slot)| slot.token.is_none() && slot.generation != 0)
    {
        slot.token = Some(token);
        return encode(index, slot.generation);
    }
    let index = registry.len();
    registry.try_reserve(1).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "cancel registry allocation failed",
        )
    })?;
    registry.push(CancelSlot {
        generation: 1,
        token: Some(token),
    });
    encode(index, 1)
}

pub(crate) fn lookup_cancel(handle: ZeCancelToken) -> Result<CancelToken, FfiError> {
    let (index, generation) = decode(handle)?;
    let registry = cancels().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "cancel registry mutex is poisoned",
        )
    })?;
    let slot = registry.get(index).ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "cancel-token slot was never allocated",
        )
    })?;
    if slot.generation != generation || slot.token.is_none() {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "cancel token is closed or stale",
        ));
    }
    slot.token.as_ref().cloned().ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "cancel token has already been released",
        )
    })
}

pub(crate) fn free_cancel(handle: ZeCancelToken) -> Result<(), FfiError> {
    let (index, generation) = decode(handle)?;
    let mut registry = cancels().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "cancel registry mutex is poisoned",
        )
    })?;
    let slot = registry.get_mut(index).ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "cancel-token slot was never allocated",
        )
    })?;
    if slot.generation != generation || slot.token.is_none() {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrClosed,
            "cancel token is closed or stale",
        ));
    }
    slot.token = None;
    bump_generation(&mut slot.generation);
    Ok(())
}

pub(crate) fn register_result<T: 'static>(pointer: *mut T, length: usize) -> Result<(), FfiError> {
    if length == 0 {
        return Ok(());
    }
    result_allocations()
        .lock()
        .map_err(|_| {
            FfiError::new(
                ZeErrorCode::ZeErrSynchronization,
                "result allocation registry mutex is poisoned",
            )
        })?
        .insert(
            pointer as usize,
            ResultAllocation {
                length,
                element: std::any::TypeId::of::<T>(),
            },
        );
    Ok(())
}

pub(crate) fn take_result<T: 'static>(pointer: *mut T, length: usize) -> Result<(), FfiError> {
    if pointer.is_null() && length == 0 {
        return Ok(());
    }
    if pointer.is_null() || length == 0 {
        return Err(FfiError::invalid("result pointer and length disagree"));
    }
    let expected = ResultAllocation {
        length,
        element: std::any::TypeId::of::<T>(),
    };
    let mut allocations = result_allocations().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "result allocation registry mutex is poisoned",
        )
    })?;
    // Compare before removing: a mismatched free must leave the genuine
    // registration in place so the correct free can still succeed.
    if allocations.get(&(pointer as usize)) != Some(&expected) {
        return Err(FfiError::invalid(
            "result was already freed or was not allocated by this ABI",
        ));
    }
    allocations.remove(&(pointer as usize));
    drop(allocations);
    let slice = std::ptr::slice_from_raw_parts_mut(pointer, length);
    unsafe { drop(Box::from_raw(slice)) };
    Ok(())
}
