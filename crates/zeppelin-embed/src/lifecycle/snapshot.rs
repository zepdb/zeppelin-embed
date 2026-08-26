//! Generation-stamped immutable segment snapshots.

use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::manifest::Manifest;
use crate::manifest::io::commit_manifest;
use crate::manifest::io::{DurableLog, MANIFEST_FILE, load_manifest};
use crate::meta::{AliveSet, ColumnStore};
use crate::quant::Bit4Factors;
use crate::segment::SegmentId;
use crate::segment::layout::Int8Factors;
use crate::segment::reader::SegmentReader;
use crate::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use crate::vfs::{StdVfs, Vfs};
use crate::wal::WalReader;

#[cfg(test)]
use super::close::TeardownProbe;
use super::stats::{Accounted, Accounting, AllocationComponent, MappingReservation};
use super::{Store, StoreError, StoreState};

#[cfg(test)]
struct SnapshotReleaseProbe(Option<Arc<TeardownProbe>>);

#[cfg(test)]
impl Drop for SnapshotReleaseProbe {
    fn drop(&mut self) {
        if let Some(probe) = &self.0 {
            probe.record_snapshot_released();
        }
    }
}

const STORE_WAL_FILE: &str = "wal.ze";

/// Owned vector-factor buffers for one already-built in-memory segment.
///
/// This is an ownership-transfer seam for sealing, not an ingest API. Task 10
/// supplies the mutation path that eventually builds these buffers.
pub enum InMemorySegmentFactors {
    /// Permanent 12-byte Bit4 factor records.
    Bit4(Vec<Bit4Factors>),
    /// Permanent 8-byte affine Int8 factor records.
    Int8(Vec<Int8Factors>),
}

/// One complete already-built segment whose vector buffers can be adopted by a store.
///
/// Metadata and alive state remain caller-borrowed during the synchronous seal.
/// The store takes ownership only of the three vector buffers whose exact
/// capacities become part of `resident_owned_bytes`.
pub struct InMemorySegment<'a> {
    /// Sortable immutable identity.
    pub id: SegmentId,
    /// Permanent per-segment quantization scheme id.
    pub scheme: u16,
    /// Logical vector dimension.
    pub dims: u32,
    /// Contiguous row-major packed codes with no row padding.
    pub codes: Vec<u8>,
    /// Contiguous factor records matching `scheme`.
    pub factors: InMemorySegmentFactors,
    /// Contiguous row-major f32 exact-rescore source.
    pub rescore: Vec<f32>,
    /// Typed metadata arrays aligned to row ids.
    pub columns: &'a ColumnStore,
    /// Alive/tombstone state aligned to row ids.
    pub alive: &'a AliveSet,
}

enum PreparedFactors {
    Bit4(Accounted<Vec<Bit4Factors>>),
    Int8(Accounted<Vec<Int8Factors>>),
}

/// Store-owned, exactly accounted anonymous buffers waiting to be sealed.
pub struct PreparedSegment<'a> {
    accounting: Arc<Accounting>,
    id: SegmentId,
    scheme: u16,
    dims: u32,
    codes: Accounted<Vec<u8>>,
    factors: PreparedFactors,
    rescore: Accounted<Vec<f32>>,
    columns: &'a ColumnStore,
    alive: &'a AliveSet,
    resident_bytes: u64,
}

impl PreparedSegment<'_> {
    /// Returns the exact anonymous allocation capacity transferred to the store.
    #[must_use]
    pub const fn resident_bytes(&self) -> u64 {
        self.resident_bytes
    }

    fn as_build(&self) -> SegmentBuild<'_> {
        let factors = match &self.factors {
            PreparedFactors::Bit4(values) => SegmentFactors::Bit4(values.as_slice()),
            PreparedFactors::Int8(values) => SegmentFactors::Int8(values.as_slice()),
        };
        SegmentBuild {
            id: self.id,
            scheme: self.scheme,
            dims: self.dims,
            codes: self.codes.as_slice(),
            factors,
            rescore: self.rescore.as_slice(),
            columns: self.columns,
            alive: self.alive,
        }
    }
}

impl Store {
    /// Adopts already-built vector buffers and charges their exact capacities.
    ///
    /// No ingest, graph construction, scheduling, or background work occurs at
    /// this seam. The returned value must be consumed by [`Self::seal_snapshot`].
    pub fn prepare_segment<'a>(
        &self,
        segment: InMemorySegment<'a>,
    ) -> Result<PreparedSegment<'a>, StoreError> {
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
        let InMemorySegment {
            id,
            scheme,
            dims,
            codes,
            factors,
            rescore,
            columns,
            alive,
        } = segment;
        let codes =
            Accounted::try_from_vec(&self.accounting, codes, AllocationComponent::Snapshot)?;
        let factors = match factors {
            InMemorySegmentFactors::Bit4(values) => PreparedFactors::Bit4(Accounted::try_from_vec(
                &self.accounting,
                values,
                AllocationComponent::Snapshot,
            )?),
            InMemorySegmentFactors::Int8(values) => PreparedFactors::Int8(Accounted::try_from_vec(
                &self.accounting,
                values,
                AllocationComponent::Snapshot,
            )?),
        };
        let rescore =
            Accounted::try_from_vec(&self.accounting, rescore, AllocationComponent::Snapshot)?;
        let factor_bytes = match &factors {
            PreparedFactors::Bit4(values) => values.resident_bytes(),
            PreparedFactors::Int8(values) => values.resident_bytes(),
        };
        let resident_bytes = codes
            .resident_bytes()
            .checked_add(factor_bytes)
            .and_then(|bytes| bytes.checked_add(rescore.resident_bytes()))
            .ok_or(StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "snapshot",
            })?;
        drop(writer_lock);
        drop(state);
        Ok(PreparedSegment {
            accounting: Arc::clone(&self.accounting),
            id,
            scheme,
            dims,
            codes,
            factors,
            rescore,
            columns,
            alive,
            resident_bytes,
        })
    }

    /// Writes, commits, read-only remaps, and publishes one complete snapshot.
    ///
    /// The prepared anonymous vector buffers are consumed only after the new
    /// [`SegmentReader`] snapshot is live. The committed manifest contains the
    /// one complete segment supplied here; this is snapshot replacement, not
    /// task 10's future append/ingest behavior.
    pub fn seal_snapshot(&self, segment: PreparedSegment<'_>) -> Result<u64, StoreError> {
        self.seal_snapshot_with_vfs(segment, &StdVfs)
    }

    fn seal_snapshot_with_vfs(
        &self,
        segment: PreparedSegment<'_>,
        vfs: &dyn Vfs,
    ) -> Result<u64, StoreError> {
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
        if !Arc::ptr_eq(&segment.accounting, &self.accounting) {
            return Err(StoreError::ForeignPreparedSegment);
        }
        let generation = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?
            .as_ref()
            .ok_or(StoreError::Closed)?
            .generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let mut meta = write_segment(
            vfs,
            &self.directory,
            segment.as_build(),
            self.durability_policy,
        )
        .map_err(StoreError::Segment)?;
        let epoch_alias = self.epoch_identity();
        meta.epoch_id = epoch_alias.map(|identity| identity.embedding);
        commit_manifest(
            vfs,
            &self.directory,
            &Manifest {
                generation,
                log_seq: 0,
                segments: vec![meta],
                epochs: self.epoch_registry(&[]),
                epoch_alias,
                schema: segment.columns.schema().clone(),
            },
            self.durability_policy,
        )
        .map_err(StoreError::Manifest)?;
        let remapped = PublishedSnapshot::load(&self.directory, &self.accounting)?;
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let previous = published.replace(Arc::new(remapped));
        drop(published);
        drop(previous);
        drop(segment);
        let mut active_guard = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        active_guard.as_mut().ok_or(StoreError::Closed)?.generation = generation;
        drop(active_guard);
        drop(writer_lock);
        drop(state);
        Ok(generation)
    }
}

/// One atomically published generation and its immutable segment readers.
pub struct PublishedSnapshot {
    generation: u64,
    absorbed_through: u64,
    epoch_alias: Option<crate::epoch::EpochIdentity>,
    graph_profile:
        Result<crate::graph::search::EpochGraphProfile, crate::graph::search::GraphProfileError>,
    schema: crate::meta::Schema,
    segments: Accounted<Vec<SegmentReader>>,
    query_segment_count: usize,
    cancelled: AtomicBool,
    reader_changed: Condvar,
    reader_signal: Mutex<()>,
    _mapping_reservation: Option<MappingReservation>,
    #[cfg(test)]
    release_probe: SnapshotReleaseProbe,
}

impl PublishedSnapshot {
    pub(crate) fn empty(generation: u64) -> Self {
        Self {
            generation,
            absorbed_through: 0,
            epoch_alias: None,
            graph_profile: Err(crate::graph::search::GraphProfileError::EpochUnstamped),
            schema: crate::meta::Schema::timestamp_only(),
            segments: Accounted::unaccounted_empty(),
            query_segment_count: 0,
            cancelled: AtomicBool::new(false),
            reader_changed: Condvar::new(),
            reader_signal: Mutex::new(()),
            _mapping_reservation: None,
            #[cfg(test)]
            release_probe: SnapshotReleaseProbe(None),
        }
    }

    pub(crate) fn load(directory: &Path, accounting: &Arc<Accounting>) -> Result<Self, StoreError> {
        let manifest_path = directory.join(MANIFEST_FILE);
        match StdVfs.open(&manifest_path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty(0));
            }
            Err(source) => {
                return Err(StoreError::Io {
                    path: manifest_path,
                    source,
                });
            }
        }
        let wal =
            WalReader::open(&StdVfs, &directory.join(STORE_WAL_FILE)).map_err(StoreError::Wal)?;
        let manifest = load_manifest(&StdVfs, &manifest_path, wal.durable_end())
            .map_err(StoreError::Manifest)?;
        Self::from_manifest(directory, &manifest, accounting)
    }

    pub(crate) fn from_manifest(
        directory: &Path,
        manifest: &Manifest,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, StoreError> {
        let mut segments = Accounted::try_with_capacity(
            accounting,
            manifest.segments.len(),
            AllocationComponent::Snapshot,
        )?;
        let query_segment_count = manifest
            .segments
            .iter()
            .filter(|segment| segment_is_published(manifest.epoch_alias, segment))
            .count();
        let ordered_segments = manifest
            .segments
            .iter()
            .filter(|segment| segment_is_published(manifest.epoch_alias, segment))
            .chain(
                manifest
                    .segments
                    .iter()
                    .filter(|segment| !segment_is_published(manifest.epoch_alias, segment)),
            );
        for expected in ordered_segments {
            let path = directory.join(expected.id.file_name());
            let reader =
                SegmentReader::open_accounted(&path, expected, accounting, |allocation_bytes| {
                    let bytes = u64::try_from(allocation_bytes).map_err(|_| {
                        StoreError::BudgetExceeded {
                            needed: u64::MAX,
                            budget: u64::MAX,
                            component: "snapshot",
                        }
                    })?;
                    segments.reserve_additional_bytes(bytes)
                })?;
            segments.push(reader)?;
        }
        let mapped_bytes = segments.iter().try_fold(0_u64, |total, segment| {
            let bytes =
                u64::try_from(segment.mapped_bytes()).map_err(|_| StoreError::Statistics {
                    component: "mapped bytes",
                    source: std::io::Error::other("mapped byte count exceeds u64"),
                })?;
            total
                .checked_add(bytes)
                .ok_or_else(|| StoreError::Statistics {
                    component: "mapped bytes",
                    source: std::io::Error::other("mapped byte count overflow"),
                })
        })?;
        let mapping_reservation = accounting.track_mapping(mapped_bytes)?;
        let graph_profile = manifest.epoch_alias.map_or(
            Err(crate::graph::search::GraphProfileError::EpochUnstamped),
            |alias| {
                manifest
                    .epochs
                    .iter()
                    .find(|epoch| epoch.id == alias.embedding && epoch.tokenizer == alias.tokenizer)
                    .ok_or(
                        crate::graph::search::GraphProfileError::EpochNotRegistered {
                            epoch: alias,
                        },
                    )
                    .and_then(|epoch| {
                        crate::graph::search::select_epoch_graph_profile(
                            epoch,
                            crate::graph::search::GraphDistanceMetric::SquaredL2,
                        )
                    })
            },
        );
        Ok(Self {
            generation: manifest.generation,
            absorbed_through: manifest.log_seq,
            epoch_alias: manifest.epoch_alias,
            graph_profile,
            schema: manifest.schema.clone(),
            segments,
            query_segment_count,
            cancelled: AtomicBool::new(false),
            reader_changed: Condvar::new(),
            reader_signal: Mutex::new(()),
            _mapping_reservation: Some(mapping_reservation),
            #[cfg(test)]
            release_probe: SnapshotReleaseProbe(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_teardown_probe(&mut self, probe: Arc<TeardownProbe>) {
        self.release_probe.0 = Some(probe);
    }

    /// Returns the monotonic manifest generation published with these segments.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) const fn absorbed_through(&self) -> u64 {
        self.absorbed_through
    }

    pub(crate) const fn epoch_alias(&self) -> Option<crate::epoch::EpochIdentity> {
        self.epoch_alias
    }

    pub(crate) const fn graph_profile(
        &self,
    ) -> Result<crate::graph::search::EpochGraphProfile, crate::graph::search::GraphProfileError>
    {
        self.graph_profile
    }

    pub(crate) const fn schema(&self) -> &crate::meta::Schema {
        &self.schema
    }

    /// Returns immutable segments belonging to the published epoch alias.
    #[must_use]
    pub fn segments(&self) -> &[SegmentReader] {
        match self.segments.get(..self.query_segment_count) {
            Some(segments) => segments,
            None => &[],
        }
    }

    pub(crate) fn all_segments(&self) -> &[SegmentReader] {
        &self.segments
    }

    pub(crate) fn drain_readers(self: &Arc<Self>, grace: Duration) -> Result<(), StoreError> {
        let deadline = Instant::now().checked_add(grace);
        let mut signal = self
            .reader_signal
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "snapshot readers",
            })?;
        while Arc::strong_count(self) > 1 {
            let Some(deadline) = deadline else {
                break;
            };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let waited = self
                .reader_changed
                .wait_timeout(signal, remaining)
                .map_err(|_| StoreError::Synchronization {
                    component: "snapshot readers",
                })?;
            signal = waited.0;
        }
        if Arc::strong_count(self) > 1 {
            self.cancelled.store(true, Ordering::Release);
            self.reader_changed.notify_all();
        }
        while Arc::strong_count(self) > 1 {
            signal = self
                .reader_changed
                .wait(signal)
                .map_err(|_| StoreError::Synchronization {
                    component: "snapshot readers",
                })?;
        }
        drop(signal);
        Ok(())
    }

    pub(crate) fn cancel_readers(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.reader_changed.notify_all();
    }
}

fn segment_is_published(
    alias: Option<crate::epoch::EpochIdentity>,
    segment: &crate::segment::SegmentMeta,
) -> bool {
    alias.is_none_or(|identity| segment.epoch_id == Some(identity.embedding))
}

/// One admitted read's strong ownership of its published snapshot.
pub struct SnapshotLease {
    snapshot: Arc<PublishedSnapshot>,
    generation: u64,
}

impl SnapshotLease {
    pub(crate) fn new(snapshot: Arc<PublishedSnapshot>) -> Self {
        let generation = snapshot.generation();
        Self {
            snapshot,
            generation,
        }
    }

    pub(crate) const fn new_at(snapshot: Arc<PublishedSnapshot>, generation: u64) -> Self {
        Self {
            snapshot,
            generation,
        }
    }

    /// Returns the active-state generation pinned when this lease was admitted.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Rejects work after close has cancelled this admitted read.
    pub fn check_active(&self) -> Result<(), StoreError> {
        if self.is_cancelled() {
            Err(StoreError::ReadCancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.snapshot.cancelled.load(Ordering::Relaxed)
    }

    /// Blocks without polling until close cancels this admitted read.
    pub fn wait_for_close_cancellation(&self) -> Result<(), StoreError> {
        let mut signal =
            self.snapshot
                .reader_signal
                .lock()
                .map_err(|_| StoreError::Synchronization {
                    component: "snapshot readers",
                })?;
        while !self.snapshot.cancelled.load(Ordering::Acquire) {
            signal = self.snapshot.reader_changed.wait(signal).map_err(|_| {
                StoreError::Synchronization {
                    component: "snapshot readers",
                }
            })?;
        }
        drop(signal);
        Ok(())
    }
}

impl Deref for SnapshotLease {
    type Target = PublishedSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        if let Ok(signal) = self.snapshot.reader_signal.lock() {
            self.snapshot.reader_changed.notify_all();
            drop(signal);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use tempfile::tempdir;

    use super::{InMemorySegment, InMemorySegmentFactors, PublishedSnapshot};
    use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
    use crate::lifecycle::{OpenOptions, Store, StoreError};
    use crate::manifest::Manifest;
    use crate::manifest::io::commit_manifest;
    use crate::meta::{AliveSet, ColumnStoreBuilder, Schema};
    use crate::quant::Bit4Factors;
    use crate::segment::SegmentId;
    use crate::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
    use crate::vfs::crash::{CrashOperation, RecordingVfs};
    use crate::vfs::{StdVfs, SyncKind};

    #[test]
    fn publish_swaps_a_whole_snapshot_while_existing_readers_keep_their_generation() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        let old_reader = store.snapshot().expect("old reader");

        store
            .publish_snapshot(PublishedSnapshot::empty(9))
            .expect("publish generation");
        let new_reader = store.snapshot().expect("new reader");

        assert_eq!(old_reader.generation(), 0);
        assert_eq!(new_reader.generation(), 9);
        drop(old_reader);
        drop(new_reader);
        store.close().expect("close");
    }

    #[test]
    fn publish_after_close_returns_the_typed_closed_error() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        store.close().expect("close");

        let result = store.publish_snapshot(PublishedSnapshot::empty(10));

        assert!(matches!(result, Err(StoreError::Closed)));
    }

    #[test]
    fn seal_uses_the_opened_durable_policy_for_segment_and_manifest() {
        const DIMS: usize = 4;

        let directory = tempdir().expect("store directory");
        let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
        let mut builder = ColumnStoreBuilder::new(schema.clone());
        builder.push_row(7, &[]).expect("fixture row");
        let columns = builder.finish().expect("fixture columns");
        let alive = AliveSet::new(1);
        let initial_id = SegmentId::new(0x0102_0304_0506, [0x41; 10]);
        let codes = [0x88_u8; DIMS.div_ceil(2)];
        let factors = [Bit4Factors::from_persisted(1.0, 1.0, 1.0)];
        let rescore = [0.0_f32; DIMS];
        let derived = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
            .expect("derived policy");
        let initial = write_segment(
            &StdVfs,
            directory.path(),
            SegmentBuild {
                id: initial_id,
                scheme: 4,
                dims: DIMS as u32,
                codes: &codes,
                factors: SegmentFactors::Bit4(&factors),
                rescore: &rescore,
                columns: &columns,
                alive: &alive,
            },
            derived,
        )
        .expect("initial segment");
        commit_manifest(
            &StdVfs,
            directory.path(),
            &Manifest {
                generation: 1,
                log_seq: 0,
                segments: vec![initial],
                epochs: Vec::new(),
                epoch_alias: None,
                schema: schema.clone(),
            },
            derived,
        )
        .expect("initial manifest");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable),
        )
        .expect("durable store open");
        let replacement_id = SegmentId::new(0x0102_0304_0507, [0x42; 10]);
        let prepared = store
            .prepare_segment(InMemorySegment {
                id: replacement_id,
                scheme: 4,
                dims: DIMS as u32,
                codes: codes.to_vec(),
                factors: InMemorySegmentFactors::Bit4(factors.to_vec()),
                rescore: rescore.to_vec(),
                columns: &columns,
                alive: &alive,
            })
            .expect("prepare replacement");
        let recorder = RecordingVfs::new(StdVfs);

        store
            .seal_snapshot_with_vfs(prepared, &recorder)
            .expect("durable seal");

        let sync_kinds = recorder
            .operations()
            .expect("recorded seal operations")
            .into_iter()
            .filter_map(|operation| match operation {
                CrashOperation::Sync { kind, .. } => Some(kind),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sync_kinds,
            vec![SyncKind::Full; 4],
            "seal must preserve the opened policy's exact SyncKind sequence"
        );
        store.close().expect("close");
    }
}
