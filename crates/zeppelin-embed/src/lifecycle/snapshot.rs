//! Generation-stamped immutable segment snapshots.

use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::manifest::io::{DurableLog, MANIFEST_FILE, load_manifest};
use crate::segment::SegmentError;
use crate::segment::layout::RegionEntry;
use crate::segment::reader::SegmentReader;
use crate::vfs::{StdVfs, Vfs};
use crate::wal::WalReader;

use super::StoreError;
#[cfg(test)]
use super::close::TeardownProbe;
use super::stats::{Accounted, Accounting, AllocationComponent, MappingReservation};

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

/// One atomically published generation and its immutable segment readers.
pub struct PublishedSnapshot {
    generation: u64,
    segments: Accounted<Vec<SegmentReader>>,
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
            segments: Accounted::unaccounted_empty(),
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
        let mut segments = Accounted::try_with_capacity(
            accounting,
            manifest.segments.len(),
            AllocationComponent::Snapshot,
        )?;
        for expected in &manifest.segments {
            let path = directory.join(expected.id.file_name());
            let reader = SegmentReader::open_accounted(&path, expected.id, |region_count| {
                let bytes = region_count
                    .checked_mul(std::mem::size_of::<RegionEntry>())
                    .and_then(|bytes| u64::try_from(bytes).ok())
                    .ok_or(StoreError::BudgetExceeded {
                        needed: u64::MAX,
                        budget: u64::MAX,
                        component: "snapshot",
                    })?;
                segments.reserve_additional_bytes(bytes)
            })?;
            if reader.meta() != expected {
                return Err(StoreError::Segment(SegmentError::Geometry(format!(
                    "manifest metadata {expected:?}, mapped header metadata {:?}",
                    reader.meta()
                ))));
            }
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
        Ok(Self {
            generation: manifest.generation,
            segments,
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

    /// Returns the complete immutable segment-reader set for this generation.
    #[must_use]
    pub fn segments(&self) -> &[SegmentReader] {
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

/// One admitted read's strong ownership of its published snapshot.
pub struct SnapshotLease {
    snapshot: Arc<PublishedSnapshot>,
}

impl SnapshotLease {
    pub(crate) const fn new(snapshot: Arc<PublishedSnapshot>) -> Self {
        Self { snapshot }
    }

    /// Rejects work after close has cancelled this admitted read.
    pub fn check_active(&self) -> Result<(), StoreError> {
        if self.snapshot.cancelled.load(Ordering::Acquire) {
            Err(StoreError::ReadCancelled)
        } else {
            Ok(())
        }
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

    use super::PublishedSnapshot;
    use crate::lifecycle::{OpenOptions, Store, StoreError};

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
}
