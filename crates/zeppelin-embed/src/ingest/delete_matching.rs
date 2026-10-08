//! Delete every live document matching a predicate in one mutation, then
//! remove the deleted bytes from every store file (ZE-217).

use crate::lifecycle::{QueryError, Store, StoreError, StoreState};
use crate::meta::Predicate;
use crate::planner::PlanError;

use super::{DocId, IngestError, PurgeError};

/// Outcome of [`Store::delete_matching`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteMatchingReport {
    deleted_ids: Vec<DocId>,
    generation: u64,
}

impl DeleteMatchingReport {
    /// Ascending ids of every deleted document; empty when nothing matched.
    #[must_use]
    pub fn deleted_ids(&self) -> &[DocId] {
        &self.deleted_ids
    }

    /// Store generation when the call returned. Unchanged when nothing
    /// matched; otherwise the generation after the physical purge.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Why [`Store::delete_matching`] failed.
#[derive(Debug)]
pub enum DeleteMatchingError {
    /// The predicate does not fit the store schema. Nothing changed.
    Predicate(PlanError),
    /// Resolving the matching documents failed. Nothing changed.
    Query(QueryError),
    /// The tombstone mutation failed. Nothing changed.
    Delete(IngestError),
    /// Admission, the purge intent, or the physical purge failed. When the
    /// documents were already tombstoned, a writable open finishes the purge.
    Purge(PurgeError),
}

impl std::fmt::Display for DeleteMatchingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Predicate(error) => write!(formatter, "delete filter: {error}"),
            Self::Query(error) => write!(formatter, "delete match: {error}"),
            Self::Delete(error) => write!(formatter, "delete: {error}"),
            Self::Purge(error) => write!(formatter, "delete purge: {error}"),
        }
    }
}

impl std::error::Error for DeleteMatchingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Predicate(error) => Some(error),
            Self::Query(error) => Some(error),
            Self::Delete(error) => Some(error),
            Self::Purge(error) => Some(error),
        }
    }
}

impl From<StoreError> for DeleteMatchingError {
    fn from(error: StoreError) -> Self {
        Self::Purge(PurgeError::Store(error))
    }
}

impl Store {
    /// Deletes every live document whose current version matches
    /// `predicate`, atomically, and physically removes their bytes.
    ///
    /// The whole call holds the maintenance, state, writer and WAL locks, so
    /// no other write lands between resolving the matches and deleting them.
    /// The order is: resolve the ids; durably record a purge intent for them;
    /// tombstone them all in one WAL record at one generation (readers see all
    /// of them or none of them); then run the physical purge, which rewrites
    /// every affected segment and the WAL. When this returns, no deleted byte
    /// remains in any store file. A crash before the intent is durable deletes
    /// nothing; after it, the next writable open completes the purge.
    ///
    /// A cascade (ZE-225) extends the id set between resolution and
    /// `delete_and_purge_locked`, inside the same held locks.
    pub fn delete_matching(
        &self,
        predicate: &Predicate,
    ) -> Result<DeleteMatchingReport, DeleteMatchingError> {
        crate::planner::validate_predicate(predicate, &self.schema)
            .map_err(DeleteMatchingError::Predicate)?;
        let _maintenance = self
            .maintenance
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "delete matching",
            })?;
        #[cfg(feature = "graph-cypher")]
        let mut native_writer = self.native_document_writer()?;
        #[cfg(feature = "graph-cypher")]
        let native_deleted = if let Some(writer) =
            native_writer.as_mut().and_then(|slot| slot.as_mut())
        {
            let ids = {
                let active = self
                    .active
                    .lock()
                    .map_err(|_| StoreError::Synchronization {
                        component: "active segment",
                    })?;
                let current = active.as_ref().ok_or(StoreError::Closed)?;
                let snapshot = self
                    .snapshot
                    .read()
                    .map_err(|_| StoreError::Synchronization {
                        component: "published snapshot",
                    })?;
                let snapshot = snapshot.as_ref().ok_or(StoreError::Closed)?;
                self.live_document_ids_matching(&current.segment, snapshot.segments(), predicate)
                    .map_err(DeleteMatchingError::Query)?
            };
            // Release the snapshot read guard before the mixed writer publishes.
            if !ids.is_empty()
                && self
                    .documents_have_native_nodes(&ids)
                    .map_err(DeleteMatchingError::Delete)?
            {
                self.prepare_purge_intent(
                    &ids,
                    self.available_purge_bytes()
                        .map_err(DeleteMatchingError::Purge)?,
                    self.vfs.as_ref(),
                )
                .map_err(DeleteMatchingError::Purge)?;
                self.delete_native_documents_locked(&super::DeleteBatch::new(ids.clone()), writer)
                    .map_err(DeleteMatchingError::Delete)?;
                Some(ids)
            } else {
                None
            }
        } else {
            None
        };
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing.into()),
            StoreState::Closed => return Err(StoreError::Closed.into()),
        }
        let writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        if writer_lock.is_none() {
            return Err(StoreError::ReadOnly.into());
        }
        self.require_no_snapshot_views()?;
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_mut().ok_or(StoreError::ReadOnly)?;
        let (generation, active) = {
            let active = self
                .active
                .lock()
                .map_err(|_| StoreError::Synchronization {
                    component: "active segment",
                })?;
            let current = active.as_ref().ok_or(StoreError::Closed)?;
            (current.generation, std::sync::Arc::clone(&current.segment))
        };
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        let ids = self
            .live_document_ids_matching(&active, snapshot.segments(), predicate)
            .map_err(DeleteMatchingError::Query)?;
        #[cfg(feature = "graph-cypher")]
        let (ids, already_deleted) = match native_deleted {
            Some(ids) => (ids, true),
            None => (ids, false),
        };
        #[cfg(not(feature = "graph-cypher"))]
        let already_deleted = false;
        drop(snapshot);
        drop(active);
        let report = if ids.is_empty() {
            DeleteMatchingReport {
                deleted_ids: ids,
                generation,
            }
        } else {
            self.delete_and_purge_locked(writer, ids, already_deleted)?
        };
        drop(wal);
        drop(writer_lock);
        drop(state);
        Ok(report)
    }

    /// Records the purge intent for `ids`, tombstones them in one mutation
    /// and completes the physical purge. The caller holds the maintenance,
    /// state, writer and WAL locks; `ids` are ascending, unique and non-empty.
    fn delete_and_purge_locked(
        &self,
        writer: &mut super::StoreWal,
        ids: Vec<DocId>,
        already_deleted: bool,
    ) -> Result<DeleteMatchingReport, DeleteMatchingError> {
        let vfs = self.vfs.as_ref();
        let available = self
            .available_purge_bytes()
            .map_err(DeleteMatchingError::Purge)?;
        let token = self
            .schedule_purge_locked(&ids, available, vfs, writer)
            .map_err(DeleteMatchingError::Purge)?;
        if !already_deleted && let Err(error) = self.delete_with_writer(writer, &ids, &[]) {
            // The tombstones did not commit, so the intent must not purge
            // these documents at the next open.
            self.abandon_scheduled_purge_locked(vfs, writer)
                .map_err(DeleteMatchingError::Purge)?;
            return Err(DeleteMatchingError::Delete(error));
        }
        let purged = self
            .complete_physical_purge_locked(vfs, writer, token)
            .map_err(DeleteMatchingError::Purge)?;
        Ok(DeleteMatchingReport {
            deleted_ids: ids,
            generation: purged.generation(),
        })
    }
}
