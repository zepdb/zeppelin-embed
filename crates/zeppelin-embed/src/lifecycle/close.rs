//! Ordered store teardown.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;

use super::{Store, StoreError, StoreState};

#[cfg(test)]
pub(crate) struct TeardownProbe {
    sequence: AtomicU64,
    background_stopped: AtomicU64,
    snapshot_released: AtomicU64,
}

#[cfg(test)]
impl TeardownProbe {
    pub(crate) const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            background_stopped: AtomicU64::new(0),
            snapshot_released: AtomicU64::new(0),
        }
    }

    fn record_background_stopped(&self) {
        let event = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let _ =
            self.background_stopped
                .compare_exchange(0, event, Ordering::SeqCst, Ordering::SeqCst);
    }

    pub(crate) fn record_snapshot_released(&self) {
        let event = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let _ =
            self.snapshot_released
                .compare_exchange(0, event, Ordering::SeqCst, Ordering::SeqCst);
    }

    pub(crate) fn events(&self) -> (u64, u64) {
        (
            self.background_stopped.load(Ordering::SeqCst),
            self.snapshot_released.load(Ordering::SeqCst),
        )
    }
}

struct BackgroundControl {
    stop: Mutex<bool>,
    changed: Condvar,
}

pub(crate) struct BackgroundThread {
    control: Arc<BackgroundControl>,
    handle: Option<JoinHandle<()>>,
    #[cfg(test)]
    teardown_probe: Option<Arc<TeardownProbe>>,
}

impl BackgroundThread {
    pub(crate) fn start() -> Result<Self, StoreError> {
        static NEXT_THREAD: AtomicU64 = AtomicU64::new(1);

        let control = Arc::new(BackgroundControl {
            stop: Mutex::new(false),
            changed: Condvar::new(),
        });
        let worker_control = Arc::clone(&control);
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let thread_id = NEXT_THREAD.fetch_add(1, Ordering::Relaxed);
        let handle = std::thread::Builder::new()
            .name(format!("ze-lifecycle-{thread_id}"))
            .spawn(move || {
                if ready_tx.send(()).is_err() {
                    return;
                }
                let Ok(mut stop) = worker_control.stop.lock() else {
                    return;
                };
                while !*stop {
                    let Ok(next) = worker_control.changed.wait(stop) else {
                        return;
                    };
                    stop = next;
                }
            })
            .map_err(|source| StoreError::BackgroundStart { source })?;
        if ready_rx.recv().is_err() {
            let mut background = Self {
                control,
                handle: Some(handle),
                #[cfg(test)]
                teardown_probe: None,
            };
            background.stop_best_effort();
            return Err(StoreError::BackgroundHandshake);
        }
        Ok(Self {
            control,
            handle: Some(handle),
            #[cfg(test)]
            teardown_probe: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_teardown_probe(&mut self, probe: Arc<TeardownProbe>) {
        self.teardown_probe = Some(probe);
    }

    fn stop_and_join(mut self) -> Result<(), StoreError> {
        {
            let mut stop = self
                .control
                .stop
                .lock()
                .map_err(|_| StoreError::Synchronization {
                    component: "lifecycle thread",
                })?;
            *stop = true;
            self.control.changed.notify_all();
        }
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let result = handle
            .join()
            .map_err(|_| StoreError::BackgroundThreadPanicked);
        #[cfg(test)]
        if result.is_ok()
            && let Some(probe) = &self.teardown_probe
        {
            probe.record_background_stopped();
        }
        result
    }

    fn stop_best_effort(&mut self) {
        if let Ok(mut stop) = self.control.stop.lock() {
            *stop = true;
            self.control.changed.notify_all();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        #[cfg(test)]
        if let Some(probe) = &self.teardown_probe {
            probe.record_background_stopped();
        }
    }
}

impl Drop for BackgroundThread {
    fn drop(&mut self) {
        self.stop_best_effort();
    }
}

impl Store {
    /// Synchronously tears down this handle and releases writer ownership.
    ///
    /// Concurrent callers wait for the one `Open -> Closing -> Closed`
    /// transition and observe the same completed close. Admitted reads may run
    /// through the configured bounded drain grace period. At that deadline
    /// their published snapshot is marked cancelled, and close waits for those
    /// cooperative reads to observe cancellation and release their `Arc`
    /// before it unmaps segments. Calling `close` after completion is
    /// successful and has no further effect.
    pub fn close(&self) -> Result<(), StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        loop {
            match *state {
                StoreState::Open => {
                    *state = StoreState::Closing;
                    break;
                }
                StoreState::Closing => {
                    state = self
                        .state_changed
                        .wait(state)
                        .map_err(|_| StoreError::Synchronization { component: "state" })?;
                }
                StoreState::Closed => return Ok(()),
            }
        }
        drop(state);

        let mut background = self
            .background
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "lifecycle thread",
            })?;
        let stopped_background = background.take();
        drop(background);
        let background_result = stopped_background.map_or(Ok(()), |thread| thread.stop_and_join());

        let mut snapshot = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let released_snapshot = snapshot.take();
        drop(snapshot);
        if let Some(snapshot) = released_snapshot.as_ref() {
            snapshot.drain_readers(self.reader_drain_timeout)?;
        }
        drop(released_snapshot);

        let mut writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        let released = writer_lock.take();
        drop(writer_lock);
        drop(released);

        let mut state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        *state = StoreState::Closed;
        self.state_changed.notify_all();
        background_result
    }

    /// Runs close ordering during `Drop` without propagating failures or
    /// waiting on reader cooperation. Existing leases are cancelled and keep
    /// their snapshot mapping alive only until the lease itself is dropped.
    pub(crate) fn close_best_effort(&mut self) {
        let state = match self.state.get_mut() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *state == StoreState::Closed {
            return;
        }
        *state = StoreState::Closing;

        let background_slot = match self.background.get_mut() {
            Ok(background) => background,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(mut background) = background_slot.take() {
            background.stop_best_effort();
        }

        let snapshot_slot = match self.snapshot.get_mut() {
            Ok(snapshot) => snapshot,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(snapshot) = snapshot_slot.take() {
            snapshot.cancel_readers();
            drop(snapshot);
        }

        let writer_slot = match self.writer_lock.get_mut() {
            Ok(writer_lock) => writer_lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        let released = writer_slot.take();
        drop(released);
        *state = StoreState::Closed;
        self.state_changed.notify_all();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use crate::lifecycle::{OpenOptions, Store};

    #[test]
    fn background_thread_stops_before_snapshot_is_released_on_drop() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        let probe = Arc::clone(&store.teardown_probe);

        drop(store);

        let (background_stopped, snapshot_released) = probe.events();
        assert!(background_stopped > 0, "background stop was not observed");
        assert!(snapshot_released > 0, "snapshot release was not observed");
        assert!(
            background_stopped < snapshot_released,
            "snapshot released at event {snapshot_released} before background stopped at event {background_stopped}"
        );
    }
}
