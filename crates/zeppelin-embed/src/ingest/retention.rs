//! Whole-segment partition retention.

use std::ops::Range;
use std::sync::Arc;

use crate::lifecycle::durability::SyncRequirement;
use crate::lifecycle::{PublishedSnapshot, Store, StoreError, StoreState};
use crate::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use crate::segment::{ClusteringKeyRange, SegmentId, SegmentMeta};
use crate::vfs::{StdVfs, Vfs};

/// Pure timestamp-window policy evaluated only when a host calls it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    window: i64,
}

/// Typed rejection of a non-positive retention window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicyError {
    window: i64,
}

impl std::fmt::Display for RetentionPolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "retention window {} must be positive",
            self.window
        )
    }
}

impl std::error::Error for RetentionPolicyError {}

impl RetentionPolicy {
    /// Creates a positive retention window in the caller's timestamp unit.
    pub const fn new(window: i64) -> Result<Self, RetentionPolicyError> {
        if window <= 0 {
            Err(RetentionPolicyError { window })
        } else {
            Ok(Self { window })
        }
    }

    /// Purely decides the half-open old-key range outside the live window.
    #[must_use]
    pub const fn partition_to_drop(self, now_ts: i64) -> Range<i64> {
        i64::MIN..now_ts.saturating_sub(self.window)
    }
}

/// Result of one explicit whole-segment partition drop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropPartitionReport {
    generation: u64,
    manifest_committed: bool,
    segments_dropped: Vec<SegmentId>,
    bytes_reclaimed: u64,
    straddlers_skipped: Vec<SegmentId>,
}

impl DropPartitionReport {
    /// Returns the committed or unchanged generation observed by this call.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns whether no manifest commit and no unlink were needed.
    #[must_use]
    pub const fn is_no_op(&self) -> bool {
        !self.manifest_committed
    }

    /// Returns immutable segments omitted by the committed manifest.
    #[must_use]
    pub fn segments_dropped(&self) -> &[SegmentId] {
        &self.segments_dropped
    }

    /// Returns the exact immutable file bytes unlinked after commit.
    #[must_use]
    pub const fn bytes_reclaimed(&self) -> u64 {
        self.bytes_reclaimed
    }

    /// Returns overlapping segments retained because they crossed a boundary.
    #[must_use]
    pub fn straddlers_skipped(&self) -> &[SegmentId] {
        &self.straddlers_skipped
    }
}

impl Store {
    /// Drops immutable segments wholly contained by the half-open timestamp range.
    pub fn drop_partition(&self, key_range: Range<i64>) -> Result<DropPartitionReport, StoreError> {
        self.drop_partition_on_vfs(key_range, &StdVfs)
    }

    /// Evaluates a retention policy now and explicitly invokes partition drop.
    pub fn apply_retention(
        &self,
        policy: RetentionPolicy,
        now_ts: i64,
    ) -> Result<DropPartitionReport, StoreError> {
        self.drop_partition(policy.partition_to_drop(now_ts))
    }

    /// Test-support seam for counting filesystem work during partition drop.
    #[doc(hidden)]
    pub fn drop_partition_on_vfs(
        &self,
        key_range: Range<i64>,
        vfs: &dyn Vfs,
    ) -> Result<DropPartitionReport, StoreError> {
        let _data_read_audit =
            crate::segment::reader::install_data_read_audit(vfs.segment_data_read_counter());
        let _maintenance = self
            .maintenance
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "partition retention",
            })?;
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        if writer_lock.is_none() {
            return Err(StoreError::ReadOnly);
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
        if key_range.start >= key_range.end {
            return Ok(no_op_report(active_state.generation, Vec::new()));
        }
        let manifest_path = self.directory.join(MANIFEST_FILE);
        let mut manifest = match vfs.open(&manifest_path) {
            Ok(_) => {
                load_manifest(vfs, &manifest_path, durable_end).map_err(StoreError::Manifest)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(no_op_report(active_state.generation, Vec::new()));
            }
            Err(source) => {
                return Err(StoreError::Io {
                    path: manifest_path,
                    source,
                });
            }
        };
        let selection = select_segments(manifest.segments, &key_range)?;
        if selection.dropped.is_empty() {
            return Ok(no_op_report(
                active_state.generation.max(manifest.generation),
                selection.straddlers,
            ));
        }
        let generation = active_state
            .generation
            .max(manifest.generation)
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        manifest.generation = generation;
        manifest.segments = selection.retained;
        manifest.epochs = self.epoch_registry(&manifest.epochs);
        // Construct and validate the replacement snapshot before crossing the
        // manifest commit point. After commit, publication is an in-memory
        // pointer swap and cannot fail on segment I/O or accounting budget.
        let remapped =
            PublishedSnapshot::from_manifest(&self.directory, &manifest, &self.accounting)?;
        commit_manifest(vfs, &self.directory, &manifest, self.durability_policy)
            .map_err(StoreError::Manifest)?;

        // Store admission holds `state` across the manifest commit and this
        // publication, so a query admitted after the commit can only pin the
        // replacement snapshot that omits every dropped segment.
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let previous = published.replace(Arc::new(remapped));
        active_state.generation = generation;
        drop(published);
        drop(previous);

        for segment in &selection.dropped {
            let path = self.directory.join(segment.id.file_name());
            // Queries admitted before the commit own their snapshot's open
            // segment descriptor and mmap. POSIX unlink removes only the path;
            // the inode remains alive until the final pinned descriptor/mapping
            // is released, so their in-flight reads remain valid.
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
        let dropped_ids = selection.dropped.iter().map(|segment| segment.id).collect();
        drop(active);
        drop(wal);
        drop(writer_lock);
        drop(state);
        Ok(DropPartitionReport {
            generation,
            manifest_committed: true,
            segments_dropped: dropped_ids,
            bytes_reclaimed: selection.bytes_reclaimed,
            straddlers_skipped: selection.straddlers,
        })
    }
}

fn no_op_report(generation: u64, straddlers_skipped: Vec<SegmentId>) -> DropPartitionReport {
    DropPartitionReport {
        generation,
        manifest_committed: false,
        segments_dropped: Vec::new(),
        bytes_reclaimed: 0,
        straddlers_skipped,
    }
}

struct SegmentSelection {
    retained: Vec<SegmentMeta>,
    dropped: Vec<SegmentMeta>,
    straddlers: Vec<SegmentId>,
    bytes_reclaimed: u64,
}

fn select_segments(
    segments: Vec<SegmentMeta>,
    requested: &Range<i64>,
) -> Result<SegmentSelection, StoreError> {
    let mut retained = Vec::with_capacity(segments.len());
    let mut dropped = Vec::new();
    let mut straddlers = Vec::new();
    let mut bytes_reclaimed = 0_u64;
    for segment in segments {
        match segment.clustering_key_range {
            // The empty set is safely contained by every non-empty requested
            // range, so all-tombstoned segments can be reclaimed without a
            // numeric sentinel pretending to be a timestamp.
            ClusteringKeyRange::Empty => {
                bytes_reclaimed = bytes_reclaimed
                    .checked_add(segment.file_size)
                    .ok_or(StoreError::PartitionBytesOverflow)?;
                dropped.push(segment);
            }
            ClusteringKeyRange::Bounded { min_ts, max_ts }
                if min_ts >= requested.start && max_ts < requested.end =>
            {
                bytes_reclaimed = bytes_reclaimed
                    .checked_add(segment.file_size)
                    .ok_or(StoreError::PartitionBytesOverflow)?;
                dropped.push(segment);
            }
            ClusteringKeyRange::Bounded { min_ts, max_ts }
                if max_ts >= requested.start && min_ts < requested.end =>
            {
                straddlers.push(segment.id);
                retained.push(segment);
            }
            ClusteringKeyRange::Unstamped | ClusteringKeyRange::Bounded { .. } => {
                retained.push(segment);
            }
        }
    }
    Ok(SegmentSelection {
        retained,
        dropped,
        straddlers,
        bytes_reclaimed,
    })
}
