//! Generation-tagged slot table: the handle state machine, independent of
//! the engine so it can be model-checked under loom with a stand-in payload.
//!
//! A handle is `(generation << 32) | index`. Generation zero is reserved, so
//! handle zero is permanently invalid. Every close bumps the generation, so a
//! reused slot never honours a stale handle.

use std::collections::HashMap;

use zeppelin_embed::epoch::EpochIdentity;

use crate::abi::ZeErrorCode;
use crate::error::FfiError;
use crate::sync::{Arc, Mutex};

struct Slot<S, P> {
    generation: u32,
    payload: Option<Arc<S>>,
    epoch: Option<EpochIdentity>,
    writer: Arc<Mutex<()>>,
    purge_tokens: Arc<Mutex<HashMap<u64, P>>>,
    last_error: String,
    poisoned: bool,
    closing: bool,
}

/// Everything a caller may touch after a successful lookup. The registry
/// lock is released before the caller works, so the payload is kept alive by
/// its own `Arc` rather than by the slot.
pub(crate) struct Access<S, P> {
    pub(crate) store: Arc<S>,
    /// Identity declared when the handle was opened.
    pub(crate) epoch: Option<EpochIdentity>,
    pub(crate) writer: Arc<Mutex<()>>,
    pub(crate) purge_tokens: Arc<Mutex<HashMap<u64, P>>>,
}

pub(crate) enum CloseAccess<S> {
    /// The slot was poisoned; it has already been released and recycled.
    Poisoned,
    /// The caller now owns the close and must call `finish_close`.
    Store(Arc<S>),
}

pub(crate) struct SlotTable<S, P> {
    slots: Vec<Slot<S, P>>,
}

pub(crate) fn decode(value: u64) -> Result<(usize, u32), FfiError> {
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

pub(crate) fn encode(index: usize, generation: u32) -> Result<u64, FfiError> {
    let index = u32::try_from(index).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "handle registry exhausted its u32 slot address space",
        )
    })?;
    Ok((u64::from(generation) << 32) | u64::from(index))
}

pub(crate) fn bump_generation(generation: &mut u32) {
    *generation = generation.checked_add(1).unwrap_or(0);
}

fn never_allocated() -> FfiError {
    FfiError::new(
        ZeErrorCode::ZeErrInvalidHandle,
        "handle slot was never allocated",
    )
}

fn closed_or_stale() -> FfiError {
    FfiError::new(
        ZeErrorCode::ZeErrClosed,
        "handle is closed or belongs to a stale generation",
    )
}

impl<S, P> SlotTable<S, P> {
    pub(crate) const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    pub(crate) fn insert(
        &mut self,
        payload: S,
        epoch: Option<EpochIdentity>,
    ) -> Result<u64, FfiError> {
        if let Some((index, slot)) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.payload.is_none() && !slot.closing && slot.generation != 0)
        {
            slot.payload = Some(Arc::new(payload));
            slot.epoch = epoch;
            slot.writer = Arc::new(Mutex::new(()));
            slot.purge_tokens = Arc::new(Mutex::new(HashMap::new()));
            slot.last_error.clear();
            slot.poisoned = false;
            return encode(index, slot.generation);
        }
        let index = self.slots.len();
        self.slots.try_reserve(1).map_err(|_| {
            FfiError::new(
                ZeErrorCode::ZeErrOutOfMemory,
                "handle registry allocation failed",
            )
        })?;
        self.slots.push(Slot {
            generation: 1,
            payload: Some(Arc::new(payload)),
            epoch,
            writer: Arc::new(Mutex::new(())),
            purge_tokens: Arc::new(Mutex::new(HashMap::new())),
            last_error: String::new(),
            poisoned: false,
            closing: false,
        });
        encode(index, 1)
    }

    pub(crate) fn lookup(&self, handle: u64) -> Result<Access<S, P>, FfiError> {
        let (index, generation) = decode(handle)?;
        let slot = self.slots.get(index).ok_or_else(never_allocated)?;
        if slot.generation != generation || slot.payload.is_none() {
            return Err(closed_or_stale());
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
        let store = slot.payload.as_ref().cloned().ok_or_else(closed_or_stale)?;
        Ok(Access {
            store,
            epoch: slot.epoch,
            writer: Arc::clone(&slot.writer),
            purge_tokens: Arc::clone(&slot.purge_tokens),
        })
    }

    pub(crate) fn begin_close(&mut self, handle: u64) -> Result<CloseAccess<S>, FfiError> {
        let (index, generation) = decode(handle)?;
        let slot = self.slots.get_mut(index).ok_or_else(never_allocated)?;
        if slot.generation != generation || slot.payload.is_none() {
            return Err(closed_or_stale());
        }
        if slot.poisoned {
            slot.payload = None;
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
        let store = slot.payload.as_ref().cloned().ok_or_else(closed_or_stale)?;
        Ok(CloseAccess::Store(store))
    }

    pub(crate) fn finish_close(&mut self, handle: u64) -> Result<(), FfiError> {
        let (index, generation) = decode(handle)?;
        let slot = self.slots.get_mut(index).ok_or_else(never_allocated)?;
        if slot.generation != generation {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrClosed,
                "handle became stale while closing",
            ));
        }
        slot.payload = None;
        slot.epoch = None;
        slot.closing = false;
        slot.poisoned = false;
        slot.purge_tokens = Arc::new(Mutex::new(HashMap::new()));
        bump_generation(&mut slot.generation);
        Ok(())
    }

    /// Records a message on a live slot; returns the message back when no
    /// slot matches so the caller can fall back to the global error.
    pub(crate) fn set_error(&mut self, handle: u64, message: String) -> Option<String> {
        match decode(handle) {
            Ok((index, generation)) => match self.slots.get_mut(index) {
                Some(slot) if slot.generation == generation => {
                    slot.last_error = message;
                    None
                }
                _ => Some(message),
            },
            Err(_) => Some(message),
        }
    }

    /// Poisons a live slot; returns the message back when the slot is not
    /// live so the caller can fall back to the global error.
    pub(crate) fn poison(&mut self, handle: u64, message: String) -> Option<String> {
        match decode(handle) {
            Ok((index, generation)) => match self.slots.get_mut(index) {
                Some(slot) if slot.generation == generation && slot.payload.is_some() => {
                    slot.poisoned = true;
                    slot.last_error = message;
                    None
                }
                _ => Some(message),
            },
            Err(_) => Some(message),
        }
    }

    pub(crate) fn last_error(&self, handle: u64) -> Result<String, FfiError> {
        let (index, generation) = decode(handle)?;
        let slot = self.slots.get(index).ok_or_else(never_allocated)?;
        if slot.generation != generation || slot.payload.is_none() {
            return Err(closed_or_stale());
        }
        Ok(slot.last_error.clone())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn error_code<T>(result: Result<T, FfiError>) -> ZeErrorCode {
        match result {
            Ok(_) => unreachable!("operation unexpectedly succeeded"),
            Err(error) => error.code,
        }
    }

    #[test]
    fn handle_encoding_rejects_reserved_and_exhausted_values() {
        assert_eq!(
            decode(0).expect_err("zero handle").code,
            ZeErrorCode::ZeErrInvalidHandle
        );
        assert_eq!(
            decode(7).expect_err("zero generation").code,
            ZeErrorCode::ZeErrInvalidHandle
        );
        assert_eq!(
            decode(encode(9, 3).expect("encode")).expect("decode"),
            (9, 3)
        );
        assert_eq!(
            encode(usize::MAX, 1).expect_err("slot overflow").code,
            ZeErrorCode::ZeErrOutOfMemory
        );
        let mut generation = u32::MAX;
        bump_generation(&mut generation);
        assert_eq!(generation, 0);
    }

    #[test]
    fn slot_table_exercises_close_poison_error_and_reuse_states() {
        let mut table = SlotTable::<u32, u64>::new();
        let first = table.insert(7, None).expect("insert first slot");
        let access = table.lookup(first).expect("lookup first slot");
        assert_eq!(*access.store, 7);
        assert!(access.epoch.is_none());
        assert!(access.purge_tokens.lock().expect("tokens").is_empty());
        drop(access);

        assert_eq!(
            error_code(table.lookup(encode(99, 1).expect("unknown slot"))),
            ZeErrorCode::ZeErrInvalidHandle
        );
        assert!(table.set_error(first, "first error".to_owned()).is_none());
        assert_eq!(table.last_error(first).expect("last error"), "first error");
        assert_eq!(
            table.set_error(0, "global".to_owned()),
            Some("global".to_owned())
        );
        assert_eq!(
            table.poison(0, "global panic".to_owned()),
            Some("global panic".to_owned())
        );
        assert!(table.poison(first, "panic".to_owned()).is_none());
        assert_eq!(error_code(table.lookup(first)), ZeErrorCode::ZeErrPoisoned);
        assert!(matches!(
            table.begin_close(first).expect("release poisoned slot"),
            CloseAccess::Poisoned
        ));
        assert_eq!(error_code(table.lookup(first)), ZeErrorCode::ZeErrClosed);

        let second = table.insert(11, None).expect("reuse slot");
        assert_ne!(second, first);
        assert_eq!(*table.lookup(second).expect("lookup reused slot").store, 11);
        assert!(matches!(
            table.begin_close(second).expect("begin close"),
            CloseAccess::Store(_)
        ));
        assert_eq!(error_code(table.lookup(second)), ZeErrorCode::ZeErrClosing);
        assert_eq!(
            error_code(table.begin_close(second)),
            ZeErrorCode::ZeErrClosing
        );
        table.finish_close(second).expect("finish close");
        assert_eq!(
            table.finish_close(second).expect_err("stale finish").code,
            ZeErrorCode::ZeErrClosed
        );
        assert_eq!(
            table.last_error(second).expect_err("stale error").code,
            ZeErrorCode::ZeErrClosed
        );
        assert_eq!(
            table.poison(second, "stale".to_owned()),
            Some("stale".to_owned())
        );
    }
}

#[cfg(all(test, loom))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod loom_model {
    //! Exhaustive interleavings of close against readers, writers, poison,
    //! and slot reuse. Run with `RUSTFLAGS="--cfg loom" cargo test -p
    //! zeppelin-embed-ffi --lib slots::loom_model`.

    use super::*;
    use crate::sync::TryLockError;
    use loom::thread;

    type Table = Arc<Mutex<SlotTable<u32, ()>>>;

    fn table_with_one_handle() -> (Table, u64) {
        let table = Arc::new(Mutex::new(SlotTable::<u32, ()>::new()));
        let handle = table.lock().unwrap().insert(7, None).unwrap();
        (table, handle)
    }

    fn close(table: &Table, handle: u64) -> ZeErrorCode {
        let begun = table.lock().unwrap().begin_close(handle);
        match begun {
            Ok(CloseAccess::Store(store)) => {
                assert_eq!(*store, 7);
                drop(store);
                match table.lock().unwrap().finish_close(handle) {
                    Ok(()) => ZeErrorCode::ZeOk,
                    Err(error) => error.code,
                }
            }
            Ok(CloseAccess::Poisoned) => ZeErrorCode::ZeErrPoisoned,
            Err(error) => error.code,
        }
    }

    fn read(table: &Table, handle: u64) -> ZeErrorCode {
        let looked_up = table.lock().unwrap().lookup(handle);
        match looked_up {
            Ok(access) => {
                // The payload outlives a concurrent close through its Arc.
                assert_eq!(*access.store, 7);
                ZeErrorCode::ZeOk
            }
            Err(error) => error.code,
        }
    }

    fn write(table: &Table, handle: u64) -> ZeErrorCode {
        let looked_up = table.lock().unwrap().lookup(handle);
        match looked_up {
            Ok(access) => match access.writer.try_lock() {
                Ok(guard) => {
                    assert_eq!(*access.store, 7);
                    drop(guard);
                    ZeErrorCode::ZeOk
                }
                Err(TryLockError::WouldBlock) => ZeErrorCode::ZeErrBusy,
                Err(TryLockError::Poisoned(_)) => ZeErrorCode::ZeErrSynchronization,
            },
            Err(error) => error.code,
        }
    }

    fn typed_lifecycle(code: ZeErrorCode) {
        assert!(
            matches!(
                code,
                ZeErrorCode::ZeOk | ZeErrorCode::ZeErrClosed | ZeErrorCode::ZeErrClosing
            ),
            "{code:?}"
        );
    }

    #[test]
    fn close_races_a_reader_and_a_writer_and_every_outcome_is_typed() {
        loom::model(|| {
            let (table, handle) = table_with_one_handle();
            let reader = {
                let table = Arc::clone(&table);
                thread::spawn(move || read(&table, handle))
            };
            let writer = {
                let table = Arc::clone(&table);
                thread::spawn(move || write(&table, handle))
            };
            let closed = close(&table, handle);
            typed_lifecycle(reader.join().unwrap());
            typed_lifecycle(writer.join().unwrap());
            assert_eq!(closed, ZeErrorCode::ZeOk, "the sole close always wins");
            assert_eq!(read(&table, handle), ZeErrorCode::ZeErrClosed);
        });
    }

    #[test]
    fn two_closes_race_and_exactly_one_wins() {
        loom::model(|| {
            let (table, handle) = table_with_one_handle();
            let other = {
                let table = Arc::clone(&table);
                thread::spawn(move || close(&table, handle))
            };
            let mine = close(&table, handle);
            let theirs = other.join().unwrap();
            let outcomes = [mine, theirs];
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|code| **code == ZeErrorCode::ZeOk)
                    .count(),
                1,
                "{outcomes:?}"
            );
            assert!(outcomes.iter().all(|code| matches!(
                code,
                ZeErrorCode::ZeOk | ZeErrorCode::ZeErrClosed | ZeErrorCode::ZeErrClosing
            )));
        });
    }

    #[test]
    fn two_writers_race_and_at_most_one_holds_the_writer_slot() {
        loom::model(|| {
            let (table, handle) = table_with_one_handle();
            let other = {
                let table = Arc::clone(&table);
                thread::spawn(move || write(&table, handle))
            };
            let mine = write(&table, handle);
            let theirs = other.join().unwrap();
            assert!(matches!(mine, ZeErrorCode::ZeOk | ZeErrorCode::ZeErrBusy));
            assert!(matches!(theirs, ZeErrorCode::ZeOk | ZeErrorCode::ZeErrBusy));
        });
    }

    #[test]
    fn poison_racing_close_never_leaks_into_the_recycled_slot() {
        loom::model(|| {
            let (table, handle) = table_with_one_handle();
            let poisoner = {
                let table = Arc::clone(&table);
                thread::spawn(move || {
                    table
                        .lock()
                        .unwrap()
                        .poison(handle, "caught panic".to_owned())
                        .is_none()
                })
            };
            let closed = close(&table, handle);
            let landed_in_slot = poisoner.join().unwrap();
            assert!(matches!(
                closed,
                ZeErrorCode::ZeOk | ZeErrorCode::ZeErrPoisoned
            ));
            assert_eq!(read(&table, handle), ZeErrorCode::ZeErrClosed);
            let reused = table.lock().unwrap().insert(7, None).unwrap();
            assert_ne!(reused, handle, "reuse bumps the generation");
            assert_eq!(
                read(&table, reused),
                ZeErrorCode::ZeOk,
                "poison landed in slot: {landed_in_slot}"
            );
            assert_eq!(read(&table, handle), ZeErrorCode::ZeErrClosed);
        });
    }
}
