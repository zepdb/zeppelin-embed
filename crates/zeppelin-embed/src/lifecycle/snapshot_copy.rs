//! ZE-220: a consistent point-in-time copy of an open store.
//!
//! A snapshot pins one generation for a short time, then copies what that
//! generation references without holding any store lock:
//!
//! - Pin. Under the WAL writer mutex, which every manifest commit and
//!   snapshot publication also holds, it takes a reader lease on the
//!   published segment set, reads the committed manifest bytes, and clones
//!   the retained WAL records after the manifest's absorbed prefix. The cost
//!   is O(manifest + unsealed records); the records share the writer's
//!   encoded buffers.
//! - Copy. Segments are copied from the lease's mappings, so a segment that
//!   seal, maintenance or a sealed-row rewrite unlinks during the copy is
//!   still read in full. The WAL is rebuilt as a header whose first sequence
//!   follows the absorbed prefix, then the pinned records. Close cancels the
//!   lease, and the copy stops at its next chunk.
//! - Publish. Everything is written into a hidden staging directory beside
//!   the target, each file and the directory are fully synced, the staging
//!   directory is renamed to the target, and the parent is synced. A crash
//!   before the rename leaves no target; the staging directory is never a
//!   snapshot.
//!
//! The snapshot is an ordinary store directory. Opening it read-only or
//! read-write restores exactly the pinned state.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::manifest::decode_manifest;
use crate::manifest::io::MANIFEST_FILE;
use crate::vfs::{SyncKind, Vfs};
use crate::wal::header::encode_header;
use crate::wal::{LogSeq, VisibleRecord, WalWriteError};

use super::{PublishedSnapshot, SnapshotLease, Store, StoreError, StoreState};

const WAL_FILE: &str = "wal.ze";
/// Copy granularity. Close cancellation is checked between chunks.
const COPY_CHUNK_BYTES: usize = 8 * 1024 * 1024;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Why a snapshot target was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotTargetReason {
    /// The path has no final name component, such as `/` or `a/..`.
    NoFinalName,
    /// The target's parent directory does not exist.
    ParentMissing,
    /// The target is the store directory or lies inside it.
    InsideStore,
    /// The target exists and is not a directory, such as a file or a link.
    NotADirectory,
    /// The target is a directory that already has entries.
    NotEmpty,
}

impl std::fmt::Display for SnapshotTargetReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NoFinalName => "has no final name",
            Self::ParentMissing => "parent directory does not exist",
            Self::InsideStore => "is inside the store",
            Self::NotADirectory => "exists and is not a directory",
            Self::NotEmpty => "is not empty",
        })
    }
}

/// A validated target: its canonical parent and final path.
struct Target {
    parent: PathBuf,
    path: PathBuf,
    /// An existing empty directory is replaced at publication.
    existed: bool,
}

/// Everything one snapshot copies, captured at one generation.
struct Pinned {
    lease: SnapshotLease,
    /// Committed manifest bytes; `None` for a store that never committed one.
    manifest: Option<Vec<u8>>,
    wal_first_seq: LogSeq,
    wal_records: Vec<VisibleRecord>,
}

impl Store {
    /// Writes a consistent snapshot of this store into `target` and returns
    /// the generation it captured.
    ///
    /// `target` must not exist, or must be an empty directory; its parent
    /// must exist; and it must not be the store directory or lie inside it.
    /// A rejected target is [`StoreError::SnapshotTarget`] and nothing is
    /// written. The handle must be writable ([`StoreError::ReadOnly`]
    /// otherwise), because only the writer owns the WAL tail. While a
    /// physical purge is pending the call is refused with
    /// [`StoreError::SnapshotPurgePending`].
    ///
    /// Writers are blocked only while the generation is pinned, which is
    /// O(manifest + unsealed records) and copies no document bytes. Writes,
    /// seal and maintenance run during the copy and are absent from the
    /// snapshot. Close cancels an in-flight snapshot
    /// ([`StoreError::ReadCancelled`]); a failed snapshot removes its
    /// staging directory and never creates `target`.
    ///
    /// The snapshot directory is an ordinary store: opening it read-only or
    /// read-write restores exactly the state at the returned generation.
    pub fn write_snapshot(&self, target: impl AsRef<Path>) -> Result<u64, StoreError> {
        let target = resolve_target(&self.directory, target.as_ref())?;
        let pinned = self.pin_snapshot()?;
        let vfs = self.vfs.as_ref();
        let staging = target.staging_path();
        vfs.create_directory(&staging)
            .map_err(|source| io(&staging, source))?;
        if let Err(error) =
            stage(vfs, &staging, &pinned).and_then(|()| target.publish(vfs, &staging))
        {
            // The original error is the one reported. After a completed
            // rename the staging path is gone and this finds nothing.
            remove_staging(vfs, &staging);
            return Err(error);
        }
        Ok(pinned.lease.generation())
    }

    fn pin_snapshot(&self) -> Result<Pinned, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        // Every manifest commit and snapshot publication holds this mutex,
        // so the manifest file, the published snapshot, the active
        // generation and the WAL tail agree while it is held.
        let wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_ref().ok_or(StoreError::ReadOnly)?;
        let active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let generation = active.as_ref().ok_or(StoreError::Closed)?.generation;
        // `purge` returns with its intent on disk and the bytes it will
        // remove still in the store; copying them would defeat the purge.
        let intent = self
            .directory
            .join(crate::ingest::purge_support::PURGE_INTENT_FILE);
        match self.vfs.open(&intent) {
            Ok(_) => return Err(StoreError::SnapshotPurgePending),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io(&intent, source)),
        }
        let published = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        let manifest = self.pinned_manifest(&published)?;
        let absorbed_through = published.absorbed_through();
        let wal_records = writer.unabsorbed_records(absorbed_through)?;
        drop(active);
        drop(wal);
        drop(state);
        Ok(Pinned {
            lease: SnapshotLease::new_at(published, generation),
            manifest,
            wal_first_seq: LogSeq::new(absorbed_through.saturating_add(1)),
            wal_records,
        })
    }

    /// Reads the committed manifest and proves it describes `published`.
    fn pinned_manifest(
        &self,
        published: &PublishedSnapshot,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.directory.join(MANIFEST_FILE);
        let bytes = match self.vfs.read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                if published.all_segments().is_empty() && published.absorbed_through() == 0 {
                    return Ok(None);
                }
                return Err(StoreError::SnapshotPin {
                    detail: "the published snapshot has no committed manifest",
                });
            }
            Err(source) => return Err(io(&path, source)),
        };
        let manifest =
            decode_manifest(&path.display().to_string(), &bytes).map_err(StoreError::Manifest)?;
        let mut committed = manifest
            .segments
            .iter()
            .map(|segment| segment.id)
            .collect::<Vec<_>>();
        let mut pinned = published
            .all_segments()
            .iter()
            .map(|segment| segment.meta().id)
            .collect::<Vec<_>>();
        committed.sort_unstable();
        pinned.sort_unstable();
        if manifest.generation != published.generation()
            || manifest.log_seq != published.absorbed_through()
            || committed != pinned
        {
            return Err(StoreError::SnapshotPin {
                detail: "the committed manifest does not describe the published snapshot",
            });
        }
        Ok(Some(bytes))
    }
}

fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn resolve_target(store_directory: &Path, target: &Path) -> Result<Target, StoreError> {
    let reject = |reason| StoreError::SnapshotTarget {
        path: target.to_path_buf(),
        reason,
    };
    let name = target
        .file_name()
        .ok_or_else(|| reject(SnapshotTargetReason::NoFinalName))?;
    let parent = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let parent = match std::fs::canonicalize(parent) {
        Ok(parent) => parent,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(reject(SnapshotTargetReason::ParentMissing));
        }
        Err(source) => return Err(io(parent, source)),
    };
    let store =
        std::fs::canonicalize(store_directory).map_err(|source| io(store_directory, source))?;
    let path = parent.join(name);
    if path.starts_with(&store) {
        return Err(reject(SnapshotTargetReason::InsideStore));
    }
    let existed = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => {
            let mut entries = std::fs::read_dir(&path).map_err(|source| io(&path, source))?;
            if entries.next().is_some() {
                return Err(reject(SnapshotTargetReason::NotEmpty));
            }
            true
        }
        Ok(_) => return Err(reject(SnapshotTargetReason::NotADirectory)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => return Err(io(&path, source)),
    };
    Ok(Target {
        parent,
        path,
        existed,
    })
}

impl Target {
    /// A hidden sibling that no other snapshot, process or crash leftover
    /// shares; it is created exclusively.
    fn staging_path(&self) -> PathBuf {
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.parent.join(format!(
            ".{name}.snapshot-{}-{nanos}-{sequence}.tmp",
            std::process::id()
        ))
    }

    fn publish(&self, vfs: &dyn Vfs, staging: &Path) -> Result<(), StoreError> {
        if self.existed {
            vfs.remove_directory(&self.path)
                .map_err(|source| io(&self.path, source))?;
        }
        vfs.rename(staging, &self.path)
            .map_err(|source| io(&self.path, source))?;
        vfs.sync(&self.parent, SyncKind::Full)
            .map_err(|source| io(&self.parent, source))
    }
}

fn stage(vfs: &dyn Vfs, staging: &Path, pinned: &Pinned) -> Result<(), StoreError> {
    let lease = &pinned.lease;
    for segment in lease.all_segments() {
        let bytes = segment.file_bytes();
        if u64::try_from(bytes.len()).ok() != Some(segment.meta().file_size) {
            return Err(StoreError::SnapshotPin {
                detail: "a pinned segment mapping differs from its manifest length",
            });
        }
        write_file(
            vfs,
            &staging.join(segment.meta().id.file_name()),
            lease,
            bytes.chunks(COPY_CHUNK_BYTES).map(Ok),
        )?;
    }
    write_wal(vfs, &staging.join(WAL_FILE), pinned)?;
    if let Some(manifest) = &pinned.manifest {
        write_file(
            vfs,
            &staging.join(MANIFEST_FILE),
            lease,
            std::iter::once(Ok(manifest.as_slice())),
        )?;
    }
    vfs.sync(staging, SyncKind::Full)
        .map_err(|source| io(staging, source))
}

/// The snapshot's WAL: a header naming the first unabsorbed sequence, then
/// every pinned record exactly as the writer encoded it.
fn write_wal(vfs: &dyn Vfs, path: &Path, pinned: &Pinned) -> Result<(), StoreError> {
    let header = encode_header(pinned.wal_first_seq)
        .map_err(|error| StoreError::WalWrite(WalWriteError::Header(error)))?;
    let records = pinned.wal_records.iter().map(|record| {
        record.encoded().map_err(|source| StoreError::WalRecord {
            seq: record.seq,
            source,
        })
    });
    write_file(
        vfs,
        path,
        &pinned.lease,
        std::iter::once(Ok(header.as_slice())).chain(records),
    )
}

fn write_file<'a>(
    vfs: &dyn Vfs,
    path: &Path,
    lease: &SnapshotLease,
    chunks: impl IntoIterator<Item = Result<&'a [u8], StoreError>>,
) -> Result<(), StoreError> {
    let mut file = vfs.open_append(path).map_err(|source| io(path, source))?;
    for chunk in chunks {
        lease.check_active()?;
        file.append(chunk?).map_err(|source| io(path, source))?;
    }
    file.sync(SyncKind::Full).map_err(|source| io(path, source))
}

fn remove_staging(vfs: &dyn Vfs, staging: &Path) {
    if let Ok(paths) = vfs.list(staging) {
        for path in paths {
            let _ = vfs.delete(&path);
        }
    }
    let _ = vfs.remove_directory(staging);
}
