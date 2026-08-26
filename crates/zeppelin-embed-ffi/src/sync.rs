//! Synchronization primitives, swapped for loom's under `cfg(loom)` so the
//! handle state machine can be model-checked with the same source. loom's
//! `try_lock` returns the std `TryLockResult`, so that error type is shared.

pub(crate) use std::sync::TryLockError;

#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Mutex, MutexGuard};
#[cfg(not(loom))]
pub(crate) use std::sync::{Arc, Mutex, MutexGuard};
