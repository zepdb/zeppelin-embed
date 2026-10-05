//! Ordered store teardown.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

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
        self.close_with_final_writer(|| {})
    }

    /// Run one facade-owned final write after admission stops, before releasing
    /// writer ownership. The facade retains any typed failure; teardown always
    /// runs, and repeated/concurrent close never repeats the callback.
    pub(crate) fn close_with_final_writer(
        &self,
        before_close: impl FnOnce(),
    ) -> Result<(), StoreError> {
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
        let reader_deadline = Instant::now().checked_add(self.reader_drain_timeout);
        before_close();

        let mut background = self
            .background
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "lifecycle thread",
            })?;
        let stopped_background = background.take();
        drop(background);
        let background_result = stopped_background.map_or(Ok(()), |thread| thread.stop_and_join());

        #[cfg(feature = "graph-cypher")]
        let native_graph_result = self
            .native_graph
            .drain_writer_for_close()
            .and_then(|()| self.native_graph.drain_and_clear(reader_deadline));
        #[cfg(not(feature = "graph-cypher"))]
        let native_graph_result = Ok(());

        let mut snapshot = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let released_snapshot = snapshot.take();
        drop(snapshot);
        if let Some(snapshot) = released_snapshot.as_ref() {
            let remaining = reader_deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_default();
            snapshot.drain_readers(remaining)?;
        }
        let mut query_pool = self
            .query_pool
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "query pool",
            })?;
        let stopped_query_pool = query_pool.take();
        drop(query_pool);
        let query_pool_result = stopped_query_pool.map_or(Ok(()), |pool| pool.stop_and_join());
        let mut lexical_worker =
            self.lexical_worker
                .lock()
                .map_err(|_| StoreError::Synchronization {
                    component: "pooled lexical worker",
                })?;
        let stopped_lexical_worker = lexical_worker.take();
        drop(lexical_worker);
        let lexical_worker_result =
            stopped_lexical_worker.map_or(Ok(()), |worker| worker.stop_and_join());
        // Admission has stopped and readers/workers have drained. Drop only
        // the cache owner; any retained query assembly keeps its own charge.
        let lexical_cache_result = self
            .lexical_index_cache
            .entry
            .lock()
            .map(|mut entry| {
                drop(entry.take());
            })
            .map_err(|_| StoreError::Synchronization {
                component: "lexical index cache",
            });
        drop(released_snapshot);

        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let released_active = active.take();
        drop(active);
        drop(released_active);
        self.snapshot_pin
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "snapshot pin",
            })?
            .take();

        let mut wal_writer = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let released_wal = wal_writer.take();
        drop(wal_writer);
        drop(released_wal);

        let mut writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        let released = writer_lock.take();
        drop(writer_lock);
        drop(released);
        drop(
            self.logical_writer_lock
                .lock()
                .map_err(|_| StoreError::Synchronization {
                    component: "logical writer lock",
                })?
                .take(),
        );

        let mut state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        self.reclamation_pin
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "reclamation pin",
            })?
            .take();
        *state = StoreState::Closed;
        self.state_changed.notify_all();
        background_result
            .and(native_graph_result)
            .and(query_pool_result)
            .and(lexical_worker_result)
            .and(lexical_cache_result)
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

        #[cfg(feature = "graph-cypher")]
        self.native_graph.cancel_and_clear_best_effort();

        let snapshot_slot = match self.snapshot.get_mut() {
            Ok(snapshot) => snapshot,
            Err(poisoned) => poisoned.into_inner(),
        };
        let released_snapshot = snapshot_slot.take();
        if let Some(snapshot) = released_snapshot.as_ref() {
            snapshot.cancel_readers();
        }

        let query_pool_slot = match self.query_pool.get_mut() {
            Ok(query_pool) => query_pool,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(pool) = query_pool_slot.take() {
            let _ = pool.stop_and_join();
        }
        let lexical_worker_slot = match self.lexical_worker.get_mut() {
            Ok(worker) => worker,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(worker) = lexical_worker_slot.take() {
            worker.stop_best_effort();
        }
        let lexical_cache = match self.lexical_index_cache.entry.get_mut() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        drop(lexical_cache.take());
        drop(released_snapshot);

        let active_slot = match self.active.get_mut() {
            Ok(active) => active,
            Err(poisoned) => poisoned.into_inner(),
        };
        drop(active_slot.take());

        let wal_slot = match self.wal_writer.get_mut() {
            Ok(wal) => wal,
            Err(poisoned) => poisoned.into_inner(),
        };
        drop(wal_slot.take());

        let writer_slot = match self.writer_lock.get_mut() {
            Ok(writer_lock) => writer_lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        let released = writer_slot.take();
        drop(released);
        let logical = match self.logical_writer_lock.get_mut() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        drop(logical.take());
        let pin = match self.reclamation_pin.get_mut() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        drop(pin.take());
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
