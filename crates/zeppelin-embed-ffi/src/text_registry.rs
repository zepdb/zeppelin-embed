use std::sync::OnceLock;

use zeppelin_embed_text::TextStore;

use crate::abi::{ZeErrorCode, ZeHandle};
use crate::error::FfiError;
use crate::slots::{CloseAccess, SlotTable};
use crate::sync::{Arc, Mutex, MutexGuard, TryLockError};

const TEXT_INDEX_BIT: u64 = 1_u64 << 31;

pub(crate) type TextHandleAccess = crate::slots::Access<TextStore, ()>;

fn handles() -> &'static Mutex<SlotTable<TextStore, ()>> {
    static HANDLES: OnceLock<Mutex<SlotTable<TextStore, ()>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(SlotTable::new()))
}

fn lock_handles() -> Result<MutexGuard<'static, SlotTable<TextStore, ()>>, FfiError> {
    handles().lock().map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrSynchronization,
            "text handle registry mutex is poisoned",
        )
    })
}

fn external(raw: ZeHandle) -> ZeHandle {
    raw | TEXT_INDEX_BIT
}

fn internal(handle: ZeHandle) -> Result<ZeHandle, FfiError> {
    if !is_text_handle(handle) {
        return Err(FfiError::new(
            ZeErrorCode::ZeErrInvalidHandle,
            "handle does not name a text store",
        ));
    }
    Ok(handle & !TEXT_INDEX_BIT)
}

pub(crate) const fn is_text_handle(handle: ZeHandle) -> bool {
    handle & TEXT_INDEX_BIT != 0
}

pub(crate) fn insert(store: TextStore) -> Result<ZeHandle, FfiError> {
    let raw = lock_handles()?.insert(store, None)?;
    Ok(external(raw))
}

pub(crate) fn lookup(handle: ZeHandle) -> Result<TextHandleAccess, FfiError> {
    lock_handles()?.lookup(internal(handle)?)
}

pub(crate) fn with_writer<T>(
    handle: ZeHandle,
    operation: impl FnOnce(&TextHandleAccess) -> Result<T, FfiError>,
) -> Result<T, FfiError> {
    let access = lookup(handle)?;
    let lock = Arc::clone(&access.writer);
    let guard = match lock.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrBusy,
                "another text writer call is active on this handle",
            ));
        }
        Err(TryLockError::Poisoned(_)) => {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrSynchronization,
                "text writer mutex is poisoned",
            ));
        }
    };
    let result = operation(&access);
    drop(guard);
    result
}

pub(crate) fn close(handle: ZeHandle) -> Result<(), FfiError> {
    let internal = internal(handle)?;
    let access = lock_handles()?.begin_close(internal)?;
    match access {
        CloseAccess::Poisoned => Err(FfiError::new(
            ZeErrorCode::ZeErrPoisoned,
            "text handle is poisoned",
        )),
        CloseAccess::Store(store) => {
            let close = store.close().map_err(FfiError::text);
            let release = lock_handles()?.finish_close(internal);
            close.and(release)
        }
    }
}

pub(crate) fn set_error(handle: Option<ZeHandle>, message: String) -> Option<String> {
    let Some(handle) = handle else {
        return Some(message);
    };
    let internal = match internal(handle) {
        Ok(handle) => handle,
        Err(_) => return Some(message),
    };
    match handles().lock() {
        Ok(mut table) => table.set_error(internal, message),
        Err(_) => Some(message),
    }
}

pub(crate) fn poison(handle: Option<ZeHandle>, message: String) -> Option<String> {
    let Some(handle) = handle else {
        return Some(message);
    };
    let internal = match internal(handle) {
        Ok(handle) => handle,
        Err(_) => return Some(message),
    };
    let mut table = match handles().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    };
    table.poison(internal, message)
}

pub(crate) fn last_error(handle: ZeHandle) -> Result<String, FfiError> {
    lock_handles()?.last_error(internal(handle)?)
}
