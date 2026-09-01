use std::collections::HashMap;
use std::sync::OnceLock;

use zeppelin_embed::epoch::EpochIdentity;
use zeppelin_embed::ingest::PurgeToken;
use zeppelin_embed::lifecycle::{CancelToken, Store};

use crate::abi::{ZeCancelToken, ZeErrorCode, ZeHandle};
use crate::error::FfiError;
use crate::slots::SlotTable;
use crate::sync::{Arc, Mutex, MutexGuard, TryLockError};

pub(crate) use crate::slots::{bump_generation, decode, encode};

pub(crate) type HandleAccess = crate::slots::Access<Store, PurgeToken>;
pub(crate) type CloseAccess = crate::slots::CloseAccess<Store>;

#[derive(Clone)]
struct CancelSlot {
    generation: u32,
    token: Option<CancelToken>,
}

fn handles() -> &'static Mutex<SlotTable<Store, PurgeToken>> {
    static HANDLES: OnceLock<Mutex<SlotTable<Store, PurgeToken>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(SlotTable::new()))
}

fn cancels() -> &'static Mutex<Vec<CancelSlot>> {
    static CANCELS: OnceLock<Mutex<Vec<CancelSlot>>> = OnceLock::new();
    CANCELS.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResultAllocation {
    length: usize,
    element: std::any::TypeId,
    generation: u32,
}

struct ResultAllocations {
    allocations: HashMap<usize, ResultAllocation>,
    next_generation: u32,
}

impl ResultAllocations {
    fn new() -> Self {
        Self {
            allocations: HashMap::new(),
            next_generation: 1,
        }
    }

    fn register<T: 'static>(&mut self, pointer: *mut T, length: usize) -> Result<u32, FfiError> {
        if self.allocations.contains_key(&(pointer as usize)) {
            return Err(FfiError::invalid("result allocation is already registered"));
        }
        let generation = self.next_generation;
        if generation == 0 {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrOutOfMemory,
                "result allocation generations are exhausted",
            ));
        }
        bump_generation(&mut self.next_generation);
        self.allocations.insert(
            pointer as usize,
            ResultAllocation {
                length,
                element: std::any::TypeId::of::<T>(),
                generation,
            },
        );
        Ok(generation)
    }

    fn take<T: 'static>(
        &mut self,
        pointer: *mut T,
        length: usize,
        generation: u32,
    ) -> Result<(), FfiError> {
        let expected = ResultAllocation {
            length,
            element: std::any::TypeId::of::<T>(),
            generation,
        };
        if self.allocations.get(&(pointer as usize)) != Some(&expected) {
            return Err(FfiError::invalid(
                "result was already freed or was not allocated by this ABI",
            ));
        }
        self.allocations.remove(&(pointer as usize));
        Ok(())
    }
}

fn result_allocations() -> &'static Mutex<ResultAllocations> {
    static ALLOCATIONS: OnceLock<Mutex<ResultAllocations>> = OnceLock::new();
    ALLOCATIONS.get_or_init(|| Mutex::new(ResultAllocations::new()))
}

fn global_error() -> &'static Mutex<String> {
    static GLOBAL_ERROR: OnceLock<Mutex<String>> = OnceLock::new();
    GLOBAL_ERROR.get_or_init(|| Mutex::new(String::new()))
}

fn lock_handles() -> Result<MutexGuard<'static, SlotTable<Store, PurgeToken>>, FfiError> {
    handles().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "global handle registry mutex is poisoned",
        )
    })
}

pub(crate) fn insert_store(
    store: Store,
    epoch: Option<EpochIdentity>,
) -> Result<ZeHandle, FfiError> {
    lock_handles()?.insert(store, epoch)
}

pub(crate) fn lookup(handle: ZeHandle) -> Result<HandleAccess, FfiError> {
    lock_handles()?.lookup(handle)
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
    lock_handles()?.begin_close(handle)
}

pub(crate) fn finish_close(handle: ZeHandle) -> Result<(), FfiError> {
    lock_handles()?.finish_close(handle)
}

pub(crate) fn set_error(handle: Option<ZeHandle>, message: String) {
    let message = match handle {
        Some(handle) => match handles().lock() {
            Ok(mut table) => match table.set_error(handle, message) {
                Some(message) => message,
                None => return,
            },
            Err(_) => return,
        },
        None => message,
    };
    if let Ok(mut error) = global_error().lock() {
        *error = message;
    }
}

pub(crate) fn poison(handle: Option<ZeHandle>, message: String) {
    let message = match handle {
        Some(handle) => {
            let mut table = match handles().lock() {
                Ok(table) => table,
                Err(poisoned) => poisoned.into_inner(),
            };
            match table.poison(handle, message) {
                Some(message) => message,
                None => return,
            }
        }
        None => message,
    };
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
    lock_handles()?.last_error(handle)
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
        let handle = encode(index, slot.generation)?;
        slot.token = Some(token);
        return Ok(handle);
    }
    let index = registry.len();
    let handle = encode(index, 1)?;
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
    Ok(handle)
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

pub(crate) fn register_result<T: 'static>(pointer: *mut T, length: usize) -> Result<u32, FfiError> {
    if length == 0 {
        return Ok(0);
    }
    result_allocations()
        .lock()
        .map_err(|_| {
            FfiError::new(
                ZeErrorCode::ZeErrSynchronization,
                "result allocation registry mutex is poisoned",
            )
        })?
        .register(pointer, length)
}

pub(crate) fn take_result<T: 'static>(
    pointer: *mut T,
    length: usize,
    generation: u32,
) -> Result<(), FfiError> {
    if pointer.is_null() && length == 0 {
        return if generation == 0 {
            Ok(())
        } else {
            Err(FfiError::invalid(
                "empty result has a nonzero allocation generation",
            ))
        };
    }
    if pointer.is_null() || length == 0 {
        return Err(FfiError::invalid("result pointer and length disagree"));
    }
    let mut allocations = result_allocations().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "result allocation registry mutex is poisoned",
        )
    })?;
    allocations.take(pointer, length, generation)?;
    drop(allocations);
    let slice = std::ptr::slice_from_raw_parts_mut(pointer, length);
    unsafe { drop(Box::from_raw(slice)) };
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn cancel_registry_rejects_stale_tokens_and_reuses_the_slot() {
        let first = insert_cancel(CancelToken::new()).expect("insert cancel token");
        let token = lookup_cancel(first).expect("lookup cancel token");
        token.cancel();
        lookup_cancel(first).expect("lookup cancelled token");
        free_cancel(first).expect("free cancel token");
        assert_eq!(
            lookup_cancel(first).expect_err("stale lookup").code,
            ZeErrorCode::ZeErrClosed
        );
        assert_eq!(
            free_cancel(first).expect_err("double free").code,
            ZeErrorCode::ZeErrClosed
        );
        let second = insert_cancel(CancelToken::new()).expect("reuse cancel slot");
        assert_ne!(second, first);
        free_cancel(second).expect("free reused token");
        assert_eq!(
            lookup_cancel(encode(99, 1).expect("unknown cancel slot"))
                .expect_err("unknown cancel slot")
                .code,
            ZeErrorCode::ZeErrInvalidHandle
        );
        assert_eq!(
            free_cancel(encode(99, 1).expect("unknown free slot"))
                .expect_err("unknown free slot")
                .code,
            ZeErrorCode::ZeErrInvalidHandle
        );
    }

    #[test]
    fn result_registry_preserves_a_valid_allocation_after_hostile_frees() {
        assert_eq!(
            register_result(std::ptr::null_mut::<u32>(), 0).expect("empty registration"),
            0
        );
        take_result(std::ptr::null_mut::<u32>(), 0, 0).expect("empty free");
        assert_eq!(
            take_result(std::ptr::null_mut::<u32>(), 1, 0)
                .expect_err("null nonempty")
                .code,
            ZeErrorCode::ZeErrInvalidArgument
        );

        let values = vec![3_u32, 5, 8].into_boxed_slice();
        let length = values.len();
        let pointer = Box::into_raw(values).cast::<u32>();
        let generation = register_result(pointer, length).expect("register result");
        assert_eq!(
            take_result(pointer, length - 1, generation)
                .expect_err("wrong length")
                .code,
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(
            take_result(pointer.cast::<u8>(), length, generation)
                .expect_err("wrong type")
                .code,
            ZeErrorCode::ZeErrInvalidArgument
        );
        take_result(pointer, length, generation).expect("correct free");
        assert_eq!(
            take_result(pointer, length, generation)
                .expect_err("double free")
                .code,
            ZeErrorCode::ZeErrInvalidArgument
        );
    }

    #[test]
    fn stale_free_after_address_reuse_is_rejected() {
        let first = vec![()].into_boxed_slice();
        let length = first.len();
        let pointer = Box::into_raw(first).cast::<()>();
        let first_generation = register_result(pointer, length).expect("register first result");
        take_result(pointer, length, first_generation).expect("free first result");

        let reused = vec![()].into_boxed_slice();
        let reused_pointer = Box::into_raw(reused).cast::<()>();
        assert_eq!(
            reused_pointer, pointer,
            "ZST address is deterministically reused"
        );
        let reused_generation =
            register_result(reused_pointer, length).expect("register reused result");
        assert_ne!(reused_generation, first_generation);

        assert_eq!(
            take_result(pointer, length, first_generation)
                .expect_err("reject stale free")
                .code,
            ZeErrorCode::ZeErrInvalidArgument
        );
        take_result(reused_pointer, length, reused_generation).expect("free reused result");
    }

    #[test]
    fn global_last_error_round_trips_without_a_handle() {
        set_error(None, "global failure".to_owned());
        assert_eq!(last_error(0).expect("global last error"), "global failure");
        poison(None, "global panic".to_owned());
        assert_eq!(last_error(0).expect("global panic error"), "global panic");
        poison(
            Some(encode(99, 1).expect("unknown poison slot")),
            "unknown handle panic".to_owned(),
        );
        assert_eq!(
            last_error(0).expect("unknown handle panic"),
            "unknown handle panic"
        );
    }
}
