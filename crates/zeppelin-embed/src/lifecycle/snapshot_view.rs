//! In-place read-only views: shared data, independent handle lifetime.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

use super::{LexicalIndexCache, Store, StoreError, StoreLock, StoreState};

pub(super) struct SnapshotPin {
    count: Arc<AtomicU64>,
    // A source closed before its views must not permit a new writer to reclaim
    // their retired files through open-time orphan cleanup.
    _writer: Arc<StoreLock>,
}

impl Drop for SnapshotPin {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Store {
    /// Opens an in-place read-only view of one complete generation.
    ///
    /// Requires a writable source. Active data and sealed mappings are shared;
    /// no store files are created or copied. The view remains valid after the
    /// source closes, and must itself be closed to release its retention pin.
    /// Physical purge is busy while any view is open. Retired segment paths are
    /// retained until ordinary orphan cleanup on the next writable open.
    pub fn open_snapshot(&self) -> Result<Self, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let writer = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        let writer = Arc::clone(writer.as_ref().ok_or(StoreError::ReadOnly)?);
        let active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let active = active.as_ref().ok_or(StoreError::Closed)?;
        let published = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let published = published.as_ref().ok_or(StoreError::Closed)?;
        let snapshot = published.fork_read_view(&self.accounting)?;
        #[cfg(feature = "graph-cypher")]
        let native_graph = super::native_graph::NativeGraphPublication::new(
            &self.accounting,
            published.graph_enabled,
        )?;
        self.snapshot_pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| StoreError::GenerationOverflow)?;
        let pin = SnapshotPin {
            count: Arc::clone(&self.snapshot_pins),
            _writer: writer,
        };
        Ok(Self {
            private_preparation: None,
            directory: self.directory.clone(),
            open_migrations: super::OpenMigrations {
                generation: active.generation,
                ..super::OpenMigrations::default()
            },
            vfs: Arc::clone(&self.vfs),
            clock: Arc::clone(&self.clock),
            state: Mutex::new(StoreState::Open),
            state_changed: Condvar::new(),
            background: Mutex::new(None),
            query_pool: Mutex::new(None),
            lexical_worker: Mutex::new(None),
            #[cfg(feature = "graph-cypher")]
            native_graph,
            snapshot: RwLock::new(Some(Arc::new(snapshot))),
            active: Mutex::new(Some(crate::ingest::ActiveState {
                generation: active.generation,
                segment: Arc::clone(&active.segment),
            })),
            wal_writer: Mutex::new(None),
            writer_lock: Mutex::new(None),
            logical_writer_lock: Mutex::new(None),
            reclamation_pin: Mutex::new(
                self.reclamation_pin
                    .lock()
                    .map_err(|_| StoreError::Synchronization {
                        component: "reclamation pin",
                    })?
                    .clone(),
            ),
            snapshot_pins: Arc::clone(&self.snapshot_pins),
            snapshot_pin: Mutex::new(Some(pin)),
            maintenance: Mutex::new(()),
            health_state: Mutex::new(crate::diag::HealthState::default()),
            durability_policy: self.durability_policy,
            reader_drain_timeout: self.reader_drain_timeout,
            accounting: Arc::clone(&self.accounting),
            active_queries: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-seams"))]
            text_materialization_work: super::materialize::TestMaterializationWork::default(),
            epoch: self.epoch.clone(),
            epoch_alias: crate::epoch::EpochAliasCell::new(self.epoch_alias.load()),
            tokenizer: self.tokenizer.clone(),
            schema: self.schema.clone(),
            lexical_index_cache: LexicalIndexCache::new(),
            #[cfg(any(test, feature = "test-seams"))]
            ingest_retention_fault_controller: None,
            #[cfg(any(test, feature = "test-seams"))]
            hybrid_leg_fault: Mutex::new(None),
            #[cfg(any(test, feature = "test-seams"))]
            hybrid_execution_receipt: Mutex::new(None),
            #[cfg(any(test, feature = "test-seams"))]
            metadata_test_controller: None,
            #[cfg(any(test, feature = "test-seams"))]
            vector_fault_controller: None,
            #[cfg(any(test, feature = "test-seams"))]
            kernel_fault_controller: None,
            #[cfg(any(test, feature = "test-seams"))]
            vector_seal_scheme: None,
            #[cfg(test)]
            teardown_probe: Arc::new(super::close::TeardownProbe::new()),
        })
    }

    pub(crate) fn has_snapshot_views(&self) -> bool {
        self.snapshot_pins.load(Ordering::Acquire) != 0
    }

    pub(crate) fn require_no_snapshot_views(&self) -> Result<(), StoreError> {
        if self.has_snapshot_views() {
            return Err(StoreError::SnapshotViewsOpen);
        }
        Ok(())
    }
}
