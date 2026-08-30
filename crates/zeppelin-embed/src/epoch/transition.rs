//! Atomic published-alias transitions and explicit epoch reclamation.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::lifecycle::durability::SyncRequirement;
use crate::lifecycle::{PublishedSnapshot, Store, StoreError, StoreState};
use crate::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use crate::segment::SegmentId;
use crate::vfs::Vfs;

use super::{EpochId, EpochIdentity};

/// Typed rejection from alias switching or explicit epoch reclamation.
#[derive(Debug)]
pub enum EpochTransitionError {
    /// Store lifecycle, persistence, or filesystem work failed.
    Store(StoreError),
    /// The requested alias is not a registered epoch with retained segments.
    EpochUnavailable {
        /// Alias that cannot be published.
        target: EpochIdentity,
    },
    /// The target epoch does not contain exactly the published live revisions.
    IncompleteEpoch {
        /// Alias that was refused before the manifest commit point.
        target: EpochIdentity,
        /// Live source revisions absent from the target epoch.
        missing_documents: usize,
        /// Live target revisions absent from the source, including duplicates.
        unexpected_documents: usize,
    },
    /// A live epoch row lacks the stable document identity needed for completeness checks.
    MissingDocumentIdentity {
        /// Embedding epoch whose row could not be identified.
        epoch: EpochId,
        /// Segment containing the unidentified live row.
        segment_id: SegmentId,
        /// Local live row with no document/revision value.
        row: u32,
    },
    /// No retained segment belongs to the requested embedding epoch.
    EmbeddingEpochUnavailable {
        /// Embedding epoch that cannot be dropped.
        target: EpochId,
    },
    /// The published epoch cannot be dropped while queries name it.
    PublishedEpoch {
        /// Embedding epoch that remains published.
        target: EpochId,
    },
    /// A transition was attempted before the active WAL state was sealed.
    UnsealedWrites {
        /// Rows still held by the active segment.
        active_rows: usize,
        /// Largest sequence already absorbed by the manifest.
        absorbed_through: u64,
        /// Largest sequence durable in the WAL.
        durable_end: u64,
    },
    /// Summing the reclaimed segment bytes exceeded `u64`.
    ReclaimedBytesOverflow,
}

impl std::fmt::Display for EpochTransitionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::EpochUnavailable { target } => write!(
                formatter,
                "epoch alias ({}, {}) has no retained segment set",
                target.embedding, target.tokenizer
            ),
            Self::IncompleteEpoch {
                target,
                missing_documents,
                unexpected_documents,
            } => write!(
                formatter,
                "epoch alias ({}, {}) is incomplete: {missing_documents} published revisions missing and {unexpected_documents} unexpected target revisions",
                target.embedding, target.tokenizer
            ),
            Self::MissingDocumentIdentity {
                epoch,
                segment_id,
                row,
            } => write!(
                formatter,
                "embedding epoch {epoch} segment {segment_id} live row {row} has no document identity"
            ),
            Self::EmbeddingEpochUnavailable { target } => {
                write!(
                    formatter,
                    "embedding epoch {target} has no retained segments"
                )
            }
            Self::PublishedEpoch { target } => {
                write!(
                    formatter,
                    "published embedding epoch {target} cannot be dropped"
                )
            }
            Self::UnsealedWrites {
                active_rows,
                absorbed_through,
                durable_end,
            } => write!(
                formatter,
                "epoch transition requires a sealed WAL: active rows {active_rows}, manifest absorbed through {absorbed_through}, durable WAL end {durable_end}"
            ),
            Self::ReclaimedBytesOverflow => {
                formatter.write_str("dropped epoch segment bytes exceed u64")
            }
        }
    }
}

impl std::error::Error for EpochTransitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::EpochUnavailable { .. }
            | Self::IncompleteEpoch { .. }
            | Self::MissingDocumentIdentity { .. }
            | Self::EmbeddingEpochUnavailable { .. }
            | Self::PublishedEpoch { .. }
            | Self::UnsealedWrites { .. }
            | Self::ReclaimedBytesOverflow => None,
        }
    }
}

impl From<StoreError> for EpochTransitionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Result of one atomic published-alias transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpochAliasReport {
    generation: u64,
    previous: EpochIdentity,
    published: EpochIdentity,
    manifest_committed: bool,
}

impl EpochAliasReport {
    /// Returns the committed or unchanged generation.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Returns the alias visible before this call.
    #[must_use]
    pub const fn previous(self) -> EpochIdentity {
        self.previous
    }

    /// Returns the alias visible after this call.
    #[must_use]
    pub const fn published(self) -> EpochIdentity {
        self.published
    }

    /// Returns whether this call crossed the manifest commit point.
    #[must_use]
    pub const fn manifest_committed(self) -> bool {
        self.manifest_committed
    }
}

/// Result of one explicit embedding-epoch drop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropEpochReport {
    generation: u64,
    segments_dropped: Vec<SegmentId>,
    bytes_reclaimed: u64,
}

impl DropEpochReport {
    /// Returns the generation committed without the dropped segments.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the segment ids omitted by the committed manifest.
    #[must_use]
    pub fn segments_dropped(&self) -> &[SegmentId] {
        &self.segments_dropped
    }

    /// Returns the exact segment-file bytes unlinked after commit.
    #[must_use]
    pub const fn bytes_reclaimed(&self) -> u64 {
        self.bytes_reclaimed
    }
}

impl Store {
    /// Atomically publishes a registered epoch whose segments are retained.
    pub fn switch_epoch_alias(
        &self,
        target: EpochIdentity,
    ) -> Result<EpochAliasReport, EpochTransitionError> {
        self.switch_epoch_alias_on_vfs(target, self.vfs.as_ref())
    }

    fn switch_epoch_alias_on_vfs(
        &self,
        target: EpochIdentity,
        vfs: &dyn Vfs,
    ) -> Result<EpochAliasReport, EpochTransitionError> {
        let _maintenance = self
            .maintenance
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "epoch transition",
            })?;
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
        let wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let durable_end = wal.as_ref().ok_or(StoreError::ReadOnly)?.durable_end();
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let active_state = active.as_mut().ok_or(StoreError::Closed)?;
        let manifest_path = self.directory.join(MANIFEST_FILE);
        let mut manifest =
            load_manifest(vfs, &manifest_path, durable_end).map_err(StoreError::Manifest)?;
        require_absorbed(active_state.segment.row_count(), &manifest, durable_end)?;
        let previous = manifest
            .epoch_alias
            .ok_or(EpochTransitionError::EpochUnavailable { target })?;
        if previous == target {
            return Ok(EpochAliasReport {
                generation: active_state.generation.max(manifest.generation),
                previous,
                published: target,
                manifest_committed: false,
            });
        }
        let registered = manifest
            .epochs
            .iter()
            .any(|epoch| epoch.id == target.embedding && epoch.tokenizer == target.tokenizer);
        let retained = manifest
            .segments
            .iter()
            .any(|segment| segment.epoch_id == Some(target.embedding));
        if !registered || !retained {
            return Err(EpochTransitionError::EpochUnavailable { target });
        }
        let generation = active_state
            .generation
            .max(manifest.generation)
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        manifest.generation = generation;
        manifest.epoch_alias = Some(target);
        let remapped =
            PublishedSnapshot::from_manifest(vfs, &self.directory, &manifest, &self.accounting)?;
        let source_documents = live_document_versions(&remapped, previous.embedding)?;
        let target_documents = live_document_versions(&remapped, target.embedding)?;
        let missing_documents = multiset_difference(&source_documents, &target_documents);
        let unexpected_documents = multiset_difference(&target_documents, &source_documents);
        if missing_documents != 0 || unexpected_documents != 0 {
            return Err(EpochTransitionError::IncompleteEpoch {
                target,
                missing_documents,
                unexpected_documents,
            });
        }
        commit_manifest(vfs, &self.directory, &manifest, self.durability_policy)
            .map_err(StoreError::Manifest)?;
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let prior_snapshot = published.replace(Arc::new(remapped));
        active_state.generation = generation;
        drop(published);
        if let Some(snapshot) = prior_snapshot.as_ref() {
            let drain_result = snapshot.drain_readers(self.reader_drain_timeout);
            self.epoch_alias.store(Some(target));
            drain_result?;
        } else {
            self.epoch_alias.store(Some(target));
        }
        drop(prior_snapshot);
        drop(active);
        drop(wal);
        drop(writer_lock);
        drop(state);
        Ok(EpochAliasReport {
            generation,
            previous,
            published: target,
            manifest_committed: true,
        })
    }

    /// Explicitly drops every immutable segment belonging to an old epoch.
    pub fn drop_epoch(&self, target: EpochId) -> Result<DropEpochReport, EpochTransitionError> {
        self.drop_epoch_on_vfs(target, self.vfs.as_ref())
    }

    /// Test-support seam for observing the commit-then-unlink protocol.
    #[doc(hidden)]
    pub fn drop_epoch_on_vfs(
        &self,
        target: EpochId,
        vfs: &dyn Vfs,
    ) -> Result<DropEpochReport, EpochTransitionError> {
        let _maintenance = self
            .maintenance
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "drop epoch",
            })?;
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
        let wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let durable_end = wal.as_ref().ok_or(StoreError::ReadOnly)?.durable_end();
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let active_state = active.as_mut().ok_or(StoreError::Closed)?;
        let manifest_path = self.directory.join(MANIFEST_FILE);
        let mut manifest =
            load_manifest(vfs, &manifest_path, durable_end).map_err(StoreError::Manifest)?;
        require_absorbed(active_state.segment.row_count(), &manifest, durable_end)?;
        if manifest
            .epoch_alias
            .is_some_and(|identity| identity.embedding == target)
        {
            return Err(EpochTransitionError::PublishedEpoch { target });
        }
        let mut retained = Vec::with_capacity(manifest.segments.len());
        let mut dropped = Vec::new();
        let mut bytes_reclaimed = 0_u64;
        for segment in manifest.segments {
            if segment.epoch_id == Some(target) {
                bytes_reclaimed = bytes_reclaimed
                    .checked_add(segment.file_size)
                    .ok_or(EpochTransitionError::ReclaimedBytesOverflow)?;
                dropped.push(segment);
            } else {
                retained.push(segment);
            }
        }
        if dropped.is_empty() {
            return Err(EpochTransitionError::EmbeddingEpochUnavailable { target });
        }
        let generation = active_state
            .generation
            .max(manifest.generation)
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        manifest.generation = generation;
        manifest.segments = retained;
        let remapped =
            PublishedSnapshot::from_manifest(vfs, &self.directory, &manifest, &self.accounting)?;
        commit_manifest(vfs, &self.directory, &manifest, self.durability_policy)
            .map_err(StoreError::Manifest)?;
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let prior_snapshot = published.replace(Arc::new(remapped));
        active_state.generation = generation;
        drop(published);
        drop(prior_snapshot);

        for segment in &dropped {
            let path = self.directory.join(segment.id.file_name());
            vfs.delete(&path)
                .map_err(|source| StoreError::Io { path, source })?;
        }
        if let SyncRequirement::Sync(kind) = self.durability_policy.directory_sync() {
            vfs.sync(&self.directory, kind)
                .map_err(|source| StoreError::Io {
                    path: self.directory.clone(),
                    source,
                })?;
        }
        let segments_dropped = dropped.iter().map(|segment| segment.id).collect();
        drop(active);
        drop(wal);
        drop(writer_lock);
        drop(state);
        Ok(DropEpochReport {
            generation,
            segments_dropped,
            bytes_reclaimed,
        })
    }
}

fn require_absorbed(
    active_rows: usize,
    manifest: &crate::manifest::Manifest,
    durable_end: u64,
) -> Result<(), EpochTransitionError> {
    if active_rows != 0 || manifest.log_seq != durable_end {
        return Err(EpochTransitionError::UnsealedWrites {
            active_rows,
            absorbed_through: manifest.log_seq,
            durable_end,
        });
    }
    Ok(())
}

fn live_document_versions(
    snapshot: &PublishedSnapshot,
    epoch: EpochId,
) -> Result<BTreeMap<(u128, u64), usize>, EpochTransitionError> {
    let mut documents = BTreeMap::new();
    for segment in snapshot
        .all_segments()
        .iter()
        .filter(|segment| segment.meta().epoch_id == Some(epoch))
    {
        let alive = segment.alive().map_err(StoreError::Segment)?;
        for row in alive.iter_alive() {
            let version = segment
                .document_version(row as usize)
                .map_err(StoreError::Segment)?
                .ok_or(EpochTransitionError::MissingDocumentIdentity {
                    epoch,
                    segment_id: segment.meta().id,
                    row,
                })?;
            let key = (version.doc_id().get(), version.revision().get());
            let count = documents.entry(key).or_insert(0_usize);
            *count = count.saturating_add(1);
        }
    }
    Ok(documents)
}

fn multiset_difference(
    left: &BTreeMap<(u128, u64), usize>,
    right: &BTreeMap<(u128, u64), usize>,
) -> usize {
    left.iter().fold(0_usize, |difference, (document, count)| {
        difference.saturating_add(count.saturating_sub(*right.get(document).unwrap_or(&0)))
    })
}
