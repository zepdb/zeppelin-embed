//! In-memory mutating-operation recorder and deterministic crash-state materializer.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::{SyncKind, Vfs, VfsFile};

/// Disk-sector boundary used when materializing torn writes.
pub const TORN_WRITE_BLOCK_BYTES: usize = 512;

/// Region alignment frozen by the persisted segment format.
const SEGMENT_REGION_ALIGNMENT: usize = 16 * 1_024;

/// Chunk width covered by one persisted segment chunk checksum.
const SEGMENT_CHECKSUM_CHUNK_BYTES: usize = 64 * 1_024;

/// Maximum semantic tear points selected for one byte-carrying operation.
pub const MAX_TEAR_POINTS_PER_OPERATION: usize = 96;

const COMMON_FILE_HEADER_BYTES: usize = 32;
const SINGLE_BLOCK_PREFIX_BYTES: usize = 40;
const FILE_TRAILER_BYTES: usize = 8;
const SEGMENT_PREFIX_BYTES: usize = 32;
const SEGMENT_DIRECTORY_ENTRY_BYTES: usize = 32;
const SEGMENT_FAMILY_ID: u16 = 2;
const SINGLE_BLOCK_FAMILY_IDS: [u16; 2] = [1, 10];
const MAX_PARSED_SEGMENT_REGIONS: usize = 16;
const SEEDED_INTERIOR_SECTOR_SAMPLES: usize = 8;

/// Hard upper bound on materialized states from one recorded operation stream.
pub const MAX_CRASH_STATES: usize = 4_096;

/// One successful mutating VFS operation, including every supplied byte.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CrashOperation {
    /// Creates or truncates `path` and writes `bytes`.
    Write {
        /// Mutated path.
        path: PathBuf,
        /// Complete submitted payload.
        bytes: Vec<u8>,
    },
    /// Opens or creates one append-only path.
    OpenAppend {
        /// Opened path.
        path: PathBuf,
    },
    /// Appends bytes through an already-open handle.
    Append {
        /// Mutated path.
        path: PathBuf,
        /// Complete submitted payload.
        bytes: Vec<u8>,
    },
    /// Synchronizes one path or open handle.
    Sync {
        /// Synchronized path.
        path: PathBuf,
        /// Requested ordering primitive.
        kind: SyncKind,
    },
    /// Renames one directory entry over another.
    Rename {
        /// Source path.
        from: PathBuf,
        /// Destination path.
        to: PathBuf,
    },
    /// Deletes one file.
    Delete {
        /// Deleted path.
        path: PathBuf,
    },
}

/// Crash-outcome family used to audit enumeration coverage.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CrashStateClass {
    /// Machine stopped after an exact operation prefix.
    Prefix,
    /// One end of a write or append persisted through a semantic tear point.
    TornWrite,
    /// A write reached its declared length with deterministic non-zero tail garbage.
    ExtendedWithGarbage,
    /// A write reached its declared length with a zero-filled tail.
    ExtendedWithZeros,
    /// A bounded interior byte range was replaced while both file ends survived.
    InteriorDamage,
    /// Unsynchronized write effects persisted in a permitted alternate order/subset.
    ReorderedWrites,
    /// A rename persisted while the renamed file's newest content did not.
    RenameWithOldContent,
}

/// Which contiguous end of a byte-carrying operation reached storage.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TornWriteEdge {
    /// The first `persisted_bytes` reached storage.
    Prefix,
    /// The last `persisted_bytes` reached storage.
    Suffix,
}

/// Exact schedule that produced one materialized filesystem state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CrashStateKind {
    /// Exact prefix of the recorded operation stream.
    Prefix {
        /// Number of completed operations.
        completed_operations: usize,
    },
    /// Partial completion of one byte-carrying operation from either end.
    TornWrite {
        /// Zero-based operation index.
        operation_index: usize,
        /// Which end of the submitted bytes persisted.
        edge: TornWriteEdge,
        /// Bytes that reached the filesystem.
        persisted_bytes: usize,
    },
    /// Full declared length with a deterministic non-zero damaged tail.
    ExtendedWithGarbage {
        /// Zero-based operation index.
        operation_index: usize,
        /// Leading submitted bytes that reached storage intact.
        intact_prefix_bytes: usize,
    },
    /// Full declared length with a zero-filled damaged tail.
    ExtendedWithZeros {
        /// Zero-based operation index.
        operation_index: usize,
        /// Leading submitted bytes that reached storage intact.
        intact_prefix_bytes: usize,
    },
    /// Full declared length with one replaced interior range.
    InteriorDamage {
        /// Zero-based operation index.
        operation_index: usize,
        /// First replaced byte within the submitted payload.
        damage_start: usize,
        /// Number of replaced bytes.
        damage_length: usize,
    },
    /// Subset/permutation of writes since the latest completed sync.
    ReorderedWrites {
        /// Exclusive operation-stream end considered by this state.
        through_operation: usize,
        /// Zero-based write operation indexes in persistence order.
        persisted_operations: Vec<usize>,
    },
    /// Directory rename visible with the source's prior content.
    RenameWithOldContent {
        /// Zero-based rename operation index.
        operation_index: usize,
    },
}

impl CrashStateKind {
    /// Returns the broad outcome family.
    #[must_use]
    pub const fn class(&self) -> CrashStateClass {
        match self {
            Self::Prefix { .. } => CrashStateClass::Prefix,
            Self::TornWrite { .. } => CrashStateClass::TornWrite,
            Self::ExtendedWithGarbage { .. } => CrashStateClass::ExtendedWithGarbage,
            Self::ExtendedWithZeros { .. } => CrashStateClass::ExtendedWithZeros,
            Self::InteriorDamage { .. } => CrashStateClass::InteriorDamage,
            Self::ReorderedWrites { .. } => CrashStateClass::ReorderedWrites,
            Self::RenameWithOldContent { .. } => CrashStateClass::RenameWithOldContent,
        }
    }
}

/// Cloneable in-memory VFS used as both the live backing and each crash image.
#[derive(Clone, Debug, Default)]
pub struct MemoryVfs {
    files: Arc<Mutex<BTreeMap<PathBuf, Vec<u8>>>>,
}

impl MemoryVfs {
    /// Creates an empty in-memory filesystem.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds or replaces one file outside a recorder.
    pub fn insert(&self, path: impl Into<PathBuf>, bytes: Vec<u8>) -> std::io::Result<()> {
        self.lock_files()?.insert(path.into(), bytes);
        Ok(())
    }

    /// Returns a deep, independently mutable snapshot.
    pub fn snapshot(&self) -> std::io::Result<Self> {
        Ok(Self {
            files: Arc::new(Mutex::new(self.lock_files()?.clone())),
        })
    }

    /// Returns a deterministic deep copy of every stored file.
    pub fn files(&self) -> std::io::Result<BTreeMap<PathBuf, Vec<u8>>> {
        Ok(self.lock_files()?.clone())
    }

    fn lock_files(&self) -> std::io::Result<MutexGuard<'_, BTreeMap<PathBuf, Vec<u8>>>> {
        self.files
            .lock()
            .map_err(|_| std::io::Error::other("memory VFS mutex poisoned"))
    }
}

struct MemoryVfsFile {
    filesystem: MemoryVfs,
    path: PathBuf,
}

struct RecordedVfsFile {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    operations: Arc<Mutex<Vec<CrashOperation>>>,
}

/// Records successful mutating operations while delegating to any backing VFS.
///
/// This is the operation-observation half of [`CrashVfs`] without the
/// in-memory crash-state materializer. Tests that need real filesystem effects
/// (for example, a subsequent `mmap`) can therefore assert the same ordered
/// [`CrashOperation`] stream used by the crash harness.
#[allow(dead_code)]
pub struct RecordingVfs<V> {
    inner: V,
    operations: Arc<Mutex<Vec<CrashOperation>>>,
}

#[allow(dead_code)]
impl<V> RecordingVfs<V> {
    /// Wraps `inner` with an initially empty operation stream.
    #[must_use]
    pub fn new(inner: V) -> Self {
        Self {
            inner,
            operations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns a snapshot of the complete ordered mutation stream.
    pub fn operations(&self) -> std::io::Result<Vec<CrashOperation>> {
        self.operations
            .lock()
            .map(|operations| operations.clone())
            .map_err(|_| std::io::Error::other("recording VFS mutex poisoned"))
    }

    /// Returns the wrapped filesystem.
    #[must_use]
    pub const fn inner(&self) -> &V {
        &self.inner
    }

    fn lock_operations(&self) -> std::io::Result<MutexGuard<'_, Vec<CrashOperation>>> {
        self.operations
            .lock()
            .map_err(|_| std::io::Error::other("recording VFS mutex poisoned"))
    }
}

impl<V: Vfs> Vfs for RecordingVfs<V> {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        self.inner.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let mut operations = self.lock_operations()?;
        self.inner.write(path, bytes)?;
        operations.push(CrashOperation::Write {
            path: path.to_path_buf(),
            bytes: bytes.to_vec(),
        });
        Ok(())
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        let mut operations = self.lock_operations()?;
        let inner = self.inner.open_append(path)?;
        operations.push(CrashOperation::OpenAppend {
            path: path.to_path_buf(),
        });
        Ok(Box::new(RecordedVfsFile {
            inner,
            path: path.to_path_buf(),
            operations: Arc::clone(&self.operations),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let mut operations = self.lock_operations()?;
        self.inner.rename(from, to)?;
        operations.push(CrashOperation::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        Ok(())
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        let mut operations = self.lock_operations()?;
        self.inner.sync(path, kind)?;
        operations.push(CrashOperation::Sync {
            path: path.to_path_buf(),
            kind,
        });
        Ok(())
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        let mut operations = self.lock_operations()?;
        self.inner.delete(path)?;
        operations.push(CrashOperation::Delete {
            path: path.to_path_buf(),
        });
        Ok(())
    }
}

impl RecordedVfsFile {
    fn lock_operations(
        operations: &Mutex<Vec<CrashOperation>>,
    ) -> std::io::Result<MutexGuard<'_, Vec<CrashOperation>>> {
        operations
            .lock()
            .map_err(|_| std::io::Error::other("CrashVfs recorder mutex poisoned"))
    }
}

impl VfsFile for RecordedVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let recorder = Arc::clone(&self.operations);
        let mut operations = Self::lock_operations(&recorder)?;
        self.inner.append(bytes)?;
        operations.push(CrashOperation::Append {
            path: self.path.clone(),
            bytes: bytes.to_vec(),
        });
        Ok(())
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        let length = buffers
            .iter()
            .fold(0_usize, |total, buffer| total.saturating_add(buffer.len()));
        let mut bytes = Vec::with_capacity(length);
        for buffer in buffers.iter() {
            bytes.extend_from_slice(buffer);
        }
        let recorder = Arc::clone(&self.operations);
        let mut operations = Self::lock_operations(&recorder)?;
        self.inner.append_vectored(buffers)?;
        operations.push(CrashOperation::Append {
            path: self.path.clone(),
            bytes,
        });
        Ok(())
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        let mut operations = Self::lock_operations(&self.operations)?;
        self.inner.sync(kind)?;
        operations.push(CrashOperation::Sync {
            path: self.path.clone(),
            kind,
        });
        Ok(())
    }
}

impl VfsFile for MemoryVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut files = self.filesystem.lock_files()?;
        let file = files.get_mut(&self.path).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "append path is absent")
        })?;
        file.extend_from_slice(bytes);
        Ok(())
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        let mut files = self.filesystem.lock_files()?;
        let file = files.get_mut(&self.path).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "append path is absent")
        })?;
        for buffer in buffers {
            file.extend_from_slice(buffer);
        }
        Ok(())
    }

    fn sync(&self, _: SyncKind) -> std::io::Result<()> {
        Ok(())
    }
}

impl Vfs for MemoryVfs {
    fn ensure_directory(&self, _: &Path, _: bool) -> std::io::Result<bool> {
        Ok(true)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.lock_files()?
            .get(path)
            .map(|bytes| bytes.len() as u64)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "path is absent"))
    }

    fn open_for_map(&self, _: &Path) -> std::io::Result<File> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "memory VFS does not support file-backed mappings",
        ))
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.lock_files()?
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "path is absent"))
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        let start = usize::try_from(offset).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset exceeds usize")
        })?;
        let files = self.lock_files()?;
        let bytes = files
            .get(path)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "path is absent"))?;
        if start >= bytes.len() {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(length).min(bytes.len());
        bytes
            .get(start..end)
            .map(ToOwned::to_owned)
            .ok_or_else(|| std::io::Error::other("invalid in-memory read range"))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.lock_files()?
            .insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.lock_files()?.entry(path.to_path_buf()).or_default();
        Ok(Box::new(MemoryVfsFile {
            filesystem: self.clone(),
            path: path.to_path_buf(),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let mut files = self.lock_files()?;
        let bytes = files.remove(from).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "rename source is absent")
        })?;
        files.insert(to.to_path_buf(), bytes);
        Ok(())
    }

    fn sync(&self, _: &Path, _: SyncKind) -> std::io::Result<()> {
        Ok(())
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        Ok(self
            .lock_files()?
            .keys()
            .filter(|path| path.parent() == Some(directory))
            .cloned()
            .collect())
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.lock_files()?.remove(path).map(|_| ()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "delete path is absent")
        })
    }
}

/// Filesystem state at one enumerated power-cut point.
#[derive(Clone, Debug)]
pub struct CrashState {
    kind: CrashStateKind,
    filesystem: MemoryVfs,
}

impl CrashState {
    /// Returns the schedule that produced this state.
    #[must_use]
    pub const fn kind(&self) -> &CrashStateKind {
        &self.kind
    }

    /// Returns the independently mutable in-memory filesystem image.
    #[must_use]
    pub const fn vfs(&self) -> &MemoryVfs {
        &self.filesystem
    }

    /// Returns whether this state includes the complete successful operation
    /// sequence without truncation, tearing, or reordering.
    #[must_use]
    pub fn includes_complete_operation_sequence(&self, operation_count: usize) -> bool {
        matches!(
            self.kind,
            CrashStateKind::Prefix {
                completed_operations
            } if completed_operations == operation_count
        )
    }
}

/// Bounded deterministic crash-state enumeration plus explicit cap status.
#[derive(Clone, Debug)]
pub struct CrashStates {
    states: Vec<CrashState>,
    capped: bool,
}

struct StateBuilder {
    states: Vec<CrashState>,
    capped: bool,
}

impl StateBuilder {
    fn new() -> Self {
        Self {
            states: Vec::new(),
            capped: false,
        }
    }

    fn push(&mut self, kind: CrashStateKind, filesystem: MemoryVfs) -> bool {
        if self.states.len() == MAX_CRASH_STATES {
            self.capped = true;
            return false;
        }
        self.states.push(CrashState { kind, filesystem });
        true
    }

    fn finish(self) -> CrashStates {
        if self.capped {
            eprintln!(
                "CrashVfs: crash-state enumeration capped at {MAX_CRASH_STATES}; coverage is truncated"
            );
        }
        CrashStates {
            states: self.states,
            capped: self.capped,
        }
    }
}

impl CrashStates {
    /// Returns the materialized states.
    pub fn iter(&self) -> std::slice::Iter<'_, CrashState> {
        self.states.iter()
    }

    /// Returns the exact materialized state count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Returns whether candidate generation hit [`MAX_CRASH_STATES`].
    #[must_use]
    pub const fn was_capped(&self) -> bool {
        self.capped
    }

    /// Returns whether no state was materialized.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }
}

/// Records successful mutations over an in-memory VFS and enumerates crash images.
pub struct CrashVfs {
    initial: MemoryVfs,
    inner: MemoryVfs,
    operations: Arc<Mutex<Vec<CrashOperation>>>,
}

impl CrashVfs {
    /// Wraps an in-memory backing and freezes its initial crash-replay state.
    pub fn new(inner: MemoryVfs) -> std::io::Result<Self> {
        Ok(Self {
            initial: inner.snapshot()?,
            inner,
            operations: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Returns a snapshot of the complete ordered mutation stream.
    pub fn operations(&self) -> std::io::Result<Vec<CrashOperation>> {
        Ok(self.lock_operations()?.clone())
    }

    /// Materializes every bounded deterministic crash image.
    pub fn crash_states(&self) -> std::io::Result<CrashStates> {
        let operations = self.operations()?;
        let mut states = StateBuilder::new();

        for completed_operations in 0..=operations.len() {
            let filesystem = replay_prefix(&self.initial, &operations, completed_operations)?;
            if !states.push(
                CrashStateKind::Prefix {
                    completed_operations,
                },
                filesystem,
            ) {
                return Ok(states.finish());
            }
        }

        for (operation_index, operation) in operations.iter().enumerate() {
            let Some(bytes) = byte_payload(operation) else {
                continue;
            };
            for persisted_bytes in semantic_tear_points(bytes) {
                let filesystem = replay_prefix(&self.initial, &operations, operation_index)?;
                apply_partial(
                    &filesystem,
                    operation,
                    TornWriteEdge::Prefix,
                    persisted_bytes,
                )?;
                if !states.push(
                    CrashStateKind::TornWrite {
                        operation_index,
                        edge: TornWriteEdge::Prefix,
                        persisted_bytes,
                    },
                    filesystem,
                ) {
                    return Ok(states.finish());
                }

                let filesystem = replay_prefix(&self.initial, &operations, operation_index)?;
                apply_partial(
                    &filesystem,
                    operation,
                    TornWriteEdge::Suffix,
                    persisted_bytes,
                )?;
                if !states.push(
                    CrashStateKind::TornWrite {
                        operation_index,
                        edge: TornWriteEdge::Suffix,
                        persisted_bytes,
                    },
                    filesystem,
                ) {
                    return Ok(states.finish());
                }

                let filesystem = replay_prefix(&self.initial, &operations, operation_index)?;
                apply_extended_damage(
                    &filesystem,
                    operation,
                    operation_index,
                    persisted_bytes,
                    TailFill::Garbage,
                )?;
                if !states.push(
                    CrashStateKind::ExtendedWithGarbage {
                        operation_index,
                        intact_prefix_bytes: persisted_bytes,
                    },
                    filesystem,
                ) {
                    return Ok(states.finish());
                }

                let filesystem = replay_prefix(&self.initial, &operations, operation_index)?;
                apply_extended_damage(
                    &filesystem,
                    operation,
                    operation_index,
                    persisted_bytes,
                    TailFill::Zeros,
                )?;
                if !states.push(
                    CrashStateKind::ExtendedWithZeros {
                        operation_index,
                        intact_prefix_bytes: persisted_bytes,
                    },
                    filesystem,
                ) {
                    return Ok(states.finish());
                }

                if let Some((damage_start, damage_length)) =
                    interior_damage_range(bytes.len(), persisted_bytes)
                {
                    let filesystem = replay_prefix(&self.initial, &operations, operation_index)?;
                    apply_interior_damage(
                        &filesystem,
                        operation,
                        operation_index,
                        damage_start,
                        damage_length,
                    )?;
                    if !states.push(
                        CrashStateKind::InteriorDamage {
                            operation_index,
                            damage_start,
                            damage_length,
                        },
                        filesystem,
                    ) {
                        return Ok(states.finish());
                    }
                }
            }
        }

        for through_operation in 1..=operations.len() {
            let issued = operations.get(..through_operation).ok_or_else(|| {
                std::io::Error::other("invalid operation prefix during reorder enumeration")
            })?;
            let durable_start = issued
                .iter()
                .rposition(|operation| matches!(operation, CrashOperation::Sync { .. }))
                .map_or(0, |index| index.saturating_add(1));
            let write_indexes = issued
                .iter()
                .enumerate()
                .skip(durable_start)
                .filter_map(|(index, operation)| byte_payload(operation).map(|_| index))
                .collect::<Vec<_>>();
            if write_indexes.len() < 2 {
                continue;
            }
            let mut schedule = Vec::new();
            let mut visit = |persisted_operations: &[usize]| -> std::io::Result<bool> {
                let Some(filesystem) = materialize_reordered(
                    &self.initial,
                    &operations,
                    durable_start,
                    through_operation,
                    persisted_operations,
                )?
                else {
                    return Ok(true);
                };
                Ok(states.push(
                    CrashStateKind::ReorderedWrites {
                        through_operation,
                        persisted_operations: persisted_operations.to_vec(),
                    },
                    filesystem,
                ))
            };
            if !visit_ordered_subsets(&write_indexes, &mut schedule, &mut visit)? {
                return Ok(states.finish());
            }
        }

        for (operation_index, operation) in operations.iter().enumerate() {
            let CrashOperation::Rename { from, to } = operation else {
                continue;
            };
            let prior = operations.get(..operation_index).ok_or_else(|| {
                std::io::Error::other("invalid rename prefix during crash enumeration")
            })?;
            let Some(content_operation) = prior
                .iter()
                .rposition(|candidate| byte_operation_path(candidate) == Some(from.as_path()))
            else {
                continue;
            };
            let after_content = operations
                .get(content_operation.saturating_add(1)..operation_index)
                .ok_or_else(|| {
                    std::io::Error::other("invalid content/sync range during crash enumeration")
                })?;
            let content_was_synced = after_content
                .iter()
                .any(|candidate| matches!(candidate, CrashOperation::Sync { .. }));
            if content_was_synced {
                continue;
            }
            let before_content = replay_prefix(&self.initial, &operations, content_operation)?;
            let old_content = match before_content.read(from) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let renamed = replay_prefix(
                &self.initial,
                &operations,
                operation_index.saturating_add(1),
            )?;
            renamed.insert(to.clone(), old_content.unwrap_or_default())?;
            if !states.push(
                CrashStateKind::RenameWithOldContent { operation_index },
                renamed,
            ) {
                return Ok(states.finish());
            }
        }

        Ok(states.finish())
    }

    fn lock_operations(&self) -> std::io::Result<MutexGuard<'_, Vec<CrashOperation>>> {
        self.operations
            .lock()
            .map_err(|_| std::io::Error::other("CrashVfs recorder mutex poisoned"))
    }
}

impl Vfs for CrashVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        self.inner.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let mut operations = self.lock_operations()?;
        self.inner.write(path, bytes)?;
        operations.push(CrashOperation::Write {
            path: path.to_path_buf(),
            bytes: bytes.to_vec(),
        });
        Ok(())
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        let mut operations = self.lock_operations()?;
        let inner = self.inner.open_append(path)?;
        operations.push(CrashOperation::OpenAppend {
            path: path.to_path_buf(),
        });
        Ok(Box::new(RecordedVfsFile {
            inner,
            path: path.to_path_buf(),
            operations: Arc::clone(&self.operations),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let mut operations = self.lock_operations()?;
        self.inner.rename(from, to)?;
        operations.push(CrashOperation::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        Ok(())
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        let mut operations = self.lock_operations()?;
        self.inner.sync(path, kind)?;
        operations.push(CrashOperation::Sync {
            path: path.to_path_buf(),
            kind,
        });
        Ok(())
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        let mut operations = self.lock_operations()?;
        self.inner.delete(path)?;
        operations.push(CrashOperation::Delete {
            path: path.to_path_buf(),
        });
        Ok(())
    }
}

fn byte_payload(operation: &CrashOperation) -> Option<&[u8]> {
    match operation {
        CrashOperation::Write { bytes, .. } | CrashOperation::Append { bytes, .. } => Some(bytes),
        CrashOperation::OpenAppend { .. }
        | CrashOperation::Sync { .. }
        | CrashOperation::Rename { .. }
        | CrashOperation::Delete { .. } => None,
    }
}

fn byte_operation_path(operation: &CrashOperation) -> Option<&Path> {
    match operation {
        CrashOperation::Write { path, .. } | CrashOperation::Append { path, .. } => Some(path),
        CrashOperation::OpenAppend { .. }
        | CrashOperation::Sync { .. }
        | CrashOperation::Rename { .. }
        | CrashOperation::Delete { .. } => None,
    }
}

struct TearPointBuilder {
    length: usize,
    points: BTreeSet<usize>,
}

impl TearPointBuilder {
    fn new(length: usize) -> Self {
        Self {
            length,
            points: BTreeSet::new(),
        }
    }

    fn add(&mut self, point: usize) -> bool {
        if point == 0
            || point >= self.length
            || self.points.len() == MAX_TEAR_POINTS_PER_OPERATION
            || self.points.contains(&point)
        {
            return false;
        }
        self.points.insert(point)
    }

    fn finish(self) -> Vec<usize> {
        self.points.into_iter().collect()
    }
}

fn semantic_tear_points(bytes: &[u8]) -> Vec<usize> {
    let length = bytes.len();
    if length < 2 {
        return Vec::new();
    }
    let mut points = TearPointBuilder::new(length);

    points.add(1);
    points.add(length / 2);
    points.add(length.saturating_sub(1));

    add_sector_edge_points(&mut points);
    add_file_structure_points(bytes, &mut points);
    add_seeded_interior_sector_points(&mut points);
    points.finish()
}

fn add_sector_edge_points(points: &mut TearPointBuilder) {
    points.add(TORN_WRITE_BLOCK_BYTES);
    let last_sector = points
        .length
        .saturating_sub(1)
        .div_euclid(TORN_WRITE_BLOCK_BYTES)
        .saturating_mul(TORN_WRITE_BLOCK_BYTES);
    points.add(last_sector);
    let midpoint = points.length / 2;
    let before_midpoint = midpoint
        .div_euclid(TORN_WRITE_BLOCK_BYTES)
        .saturating_mul(TORN_WRITE_BLOCK_BYTES);
    points.add(before_midpoint);
    points.add(before_midpoint.saturating_add(TORN_WRITE_BLOCK_BYTES));
}

fn add_file_structure_points(bytes: &[u8], points: &mut TearPointBuilder) {
    if bytes.get(..8) != Some(b"ZEPEMBED".as_slice()) {
        return;
    }
    for boundary in [8, 10, 12, 16, 24, COMMON_FILE_HEADER_BYTES] {
        points.add(boundary);
    }
    points.add(bytes.len().saturating_sub(FILE_TRAILER_BYTES));

    let Some(family) = read_u16_at(bytes, 8) else {
        return;
    };
    let Some(header_length) = read_u64_at(bytes, 16).and_then(|value| usize::try_from(value).ok())
    else {
        return;
    };
    let declared_length = read_u64_at(bytes, 24).and_then(|value| usize::try_from(value).ok());
    if declared_length != Some(bytes.len()) {
        return;
    }
    points.add(header_length);

    if SINGLE_BLOCK_FAMILY_IDS.contains(&family) {
        points.add(SINGLE_BLOCK_PREFIX_BYTES);
        points.add(bytes.len().saturating_sub(2 * FILE_TRAILER_BYTES));
        return;
    }
    if family != SEGMENT_FAMILY_ID {
        return;
    }
    add_segment_structure_points(bytes, header_length, points);
}

fn add_segment_structure_points(bytes: &[u8], header_length: usize, points: &mut TearPointBuilder) {
    let directory_start = COMMON_FILE_HEADER_BYTES.saturating_add(SEGMENT_PREFIX_BYTES);
    let directory_end = header_length.saturating_sub(FILE_TRAILER_BYTES);
    points.add(directory_start);
    points.add(directory_end);
    let Some(region_count) = read_u16_at(bytes, COMMON_FILE_HEADER_BYTES.saturating_add(20)) else {
        return;
    };
    let parsed_regions = usize::from(region_count).min(MAX_PARSED_SEGMENT_REGIONS);
    for position in 0..parsed_regions {
        let entry =
            directory_start.saturating_add(position.saturating_mul(SEGMENT_DIRECTORY_ENTRY_BYTES));
        let entry_end = entry.saturating_add(SEGMENT_DIRECTORY_ENTRY_BYTES);
        if entry_end > directory_end {
            break;
        }
        points.add(entry);
        points.add(entry_end);
        let Some(region_start) = read_u64_at(bytes, entry.saturating_add(8))
            .and_then(|value| usize::try_from(value).ok())
        else {
            continue;
        };
        let Some(region_length) = read_u64_at(bytes, entry.saturating_add(16))
            .and_then(|value| usize::try_from(value).ok())
        else {
            continue;
        };
        let Some(region_end) = region_start.checked_add(region_length) else {
            continue;
        };
        if region_start % SEGMENT_REGION_ALIGNMENT != 0 || region_end > bytes.len() {
            continue;
        }
        add_region_points(points, region_start, region_end, position);
    }
}

fn add_region_points(
    points: &mut TearPointBuilder,
    region_start: usize,
    region_end: usize,
    region_index: usize,
) {
    points.add(region_start);
    points.add(region_end);

    let region_length = region_end.saturating_sub(region_start);
    if region_length > TORN_WRITE_BLOCK_BYTES {
        points.add(region_start.saturating_add(TORN_WRITE_BLOCK_BYTES));
        points.add(region_end.saturating_sub(TORN_WRITE_BLOCK_BYTES));
    }
    if region_length <= SEGMENT_CHECKSUM_CHUNK_BYTES {
        return;
    }
    points.add(region_start.saturating_add(SEGMENT_CHECKSUM_CHUNK_BYTES));
    let final_chunk = region_length
        .saturating_sub(1)
        .div_euclid(SEGMENT_CHECKSUM_CHUNK_BYTES)
        .saturating_mul(SEGMENT_CHECKSUM_CHUNK_BYTES);
    points.add(region_start.saturating_add(final_chunk));
    let interior_chunks = region_length.div_euclid(SEGMENT_CHECKSUM_CHUNK_BYTES);
    if interior_chunks > 1 {
        let selected = deterministic_index(
            region_start as u64 ^ region_end as u64 ^ region_index as u64,
            interior_chunks.saturating_sub(1),
        )
        .saturating_add(1);
        points.add(
            region_start.saturating_add(selected.saturating_mul(SEGMENT_CHECKSUM_CHUNK_BYTES)),
        );
    }
}

fn add_seeded_interior_sector_points(points: &mut TearPointBuilder) {
    let interior_sectors = points
        .length
        .saturating_sub(1)
        .div_euclid(TORN_WRITE_BLOCK_BYTES);
    if interior_sectors == 0 {
        return;
    }
    let mut seed = 0xD1B5_4A32_D192_ED03_u64 ^ points.length as u64;
    let mut added = 0;
    let mut attempts = 0;
    while added < SEEDED_INTERIOR_SECTOR_SAMPLES
        && attempts < SEEDED_INTERIOR_SECTOR_SAMPLES.saturating_mul(8)
    {
        seed = xorshift64(seed);
        let sector = deterministic_index(seed, interior_sectors).saturating_add(1);
        if points.add(sector.saturating_mul(TORN_WRITE_BLOCK_BYTES)) {
            added += 1;
        }
        attempts += 1;
    }
}

fn deterministic_index(seed: u64, bound: usize) -> usize {
    if bound == 0 {
        return 0;
    }
    usize::try_from(seed % bound as u64).unwrap_or(0)
}

fn xorshift64(mut value: u64) -> u64 {
    value ^= value << 13;
    value ^= value >> 7;
    value ^ (value << 17)
}

fn read_u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(raw))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(raw))
}

fn replay_prefix(
    initial: &MemoryVfs,
    operations: &[CrashOperation],
    completed_operations: usize,
) -> std::io::Result<MemoryVfs> {
    let filesystem = initial.snapshot()?;
    let prefix = operations.get(..completed_operations).ok_or_else(|| {
        std::io::Error::other("invalid operation prefix during crash materialization")
    })?;
    for operation in prefix {
        apply_operation(&filesystem, operation)?;
    }
    Ok(filesystem)
}

fn apply_operation(filesystem: &MemoryVfs, operation: &CrashOperation) -> std::io::Result<()> {
    match operation {
        CrashOperation::Write { path, bytes } => filesystem.write(path, bytes),
        CrashOperation::OpenAppend { path } => filesystem.open_append(path).map(drop),
        CrashOperation::Append { path, bytes } => {
            let mut file = filesystem.open_append(path)?;
            file.append(bytes)
        }
        CrashOperation::Sync { path, kind } => filesystem.sync(path, *kind),
        CrashOperation::Rename { from, to } => filesystem.rename(from, to),
        CrashOperation::Delete { path } => filesystem.delete(path),
    }
}

fn apply_partial(
    filesystem: &MemoryVfs,
    operation: &CrashOperation,
    edge: TornWriteEdge,
    persisted_bytes: usize,
) -> std::io::Result<()> {
    match operation {
        CrashOperation::Write { path, bytes } => {
            let partial = partial_bytes(bytes, edge, persisted_bytes)?;
            filesystem.write(path, partial)
        }
        CrashOperation::Append { path, bytes } => {
            let partial = partial_bytes(bytes, edge, persisted_bytes)?;
            let mut file = filesystem.open_append(path)?;
            file.append(partial)
        }
        CrashOperation::OpenAppend { .. }
        | CrashOperation::Sync { .. }
        | CrashOperation::Rename { .. }
        | CrashOperation::Delete { .. } => Err(std::io::Error::other(
            "partial completion requested for a non-byte operation",
        )),
    }
}

fn partial_bytes(
    bytes: &[u8],
    edge: TornWriteEdge,
    persisted_bytes: usize,
) -> std::io::Result<&[u8]> {
    match edge {
        TornWriteEdge::Prefix => bytes.get(..persisted_bytes),
        TornWriteEdge::Suffix => bytes.get(bytes.len().saturating_sub(persisted_bytes)..),
    }
    .ok_or_else(|| std::io::Error::other("torn boundary exceeds submitted payload"))
}

#[derive(Clone, Copy)]
enum TailFill {
    Garbage,
    Zeros,
}

fn apply_extended_damage(
    filesystem: &MemoryVfs,
    operation: &CrashOperation,
    operation_index: usize,
    intact_prefix_bytes: usize,
    fill: TailFill,
) -> std::io::Result<()> {
    let bytes = byte_payload(operation)
        .ok_or_else(|| std::io::Error::other("tail damage requested for a non-byte operation"))?;
    let mut damaged = bytes.to_vec();
    let tail = damaged
        .get_mut(intact_prefix_bytes..)
        .ok_or_else(|| std::io::Error::other("tail damage boundary exceeds submitted payload"))?;
    for (relative_offset, byte) in tail.iter_mut().enumerate() {
        *byte = match fill {
            TailFill::Garbage => deterministic_garbage_byte(
                operation_index,
                intact_prefix_bytes.saturating_add(relative_offset),
                *byte,
            ),
            TailFill::Zeros => 0,
        };
    }
    apply_damaged_bytes(filesystem, operation, &damaged)
}

fn interior_damage_range(length: usize, semantic_point: usize) -> Option<(usize, usize)> {
    if length < 3 {
        return None;
    }
    let damage_start = semantic_point.clamp(1, length.saturating_sub(2));
    let damage_end = damage_start
        .saturating_add(TORN_WRITE_BLOCK_BYTES)
        .min(length.saturating_sub(1));
    (damage_end > damage_start).then_some((damage_start, damage_end - damage_start))
}

fn apply_interior_damage(
    filesystem: &MemoryVfs,
    operation: &CrashOperation,
    operation_index: usize,
    damage_start: usize,
    damage_length: usize,
) -> std::io::Result<()> {
    let bytes = byte_payload(operation).ok_or_else(|| {
        std::io::Error::other("interior damage requested for a non-byte operation")
    })?;
    let mut damaged = bytes.to_vec();
    let damage_end = damage_start
        .checked_add(damage_length)
        .ok_or_else(|| std::io::Error::other("interior damage range overflow"))?;
    let interior = damaged
        .get_mut(damage_start..damage_end)
        .ok_or_else(|| std::io::Error::other("interior damage range exceeds submitted payload"))?;
    for (relative_offset, byte) in interior.iter_mut().enumerate() {
        *byte = deterministic_garbage_byte(
            operation_index,
            damage_start.saturating_add(relative_offset),
            *byte,
        );
    }
    apply_damaged_bytes(filesystem, operation, &damaged)
}

fn deterministic_garbage_byte(operation_index: usize, byte_offset: usize, original: u8) -> u8 {
    let seed = 0xA076_1D64_78BD_642F_u64
        ^ (operation_index as u64).wrapping_mul(0xE703_7ED1_A0B4_28DB)
        ^ (byte_offset as u64).wrapping_mul(0x8EBC_6AF0_9C88_C6E3);
    let mixed = xorshift64(seed);
    let mut byte = u8::try_from(mixed % 255).unwrap_or(0).saturating_add(1);
    if byte == original {
        byte = if byte == u8::MAX {
            1
        } else {
            byte.saturating_add(1)
        };
    }
    byte
}

fn apply_damaged_bytes(
    filesystem: &MemoryVfs,
    operation: &CrashOperation,
    damaged: &[u8],
) -> std::io::Result<()> {
    match operation {
        CrashOperation::Write { path, .. } => filesystem.write(path, damaged),
        CrashOperation::Append { path, .. } => {
            let mut file = filesystem.open_append(path)?;
            file.append(damaged)
        }
        CrashOperation::OpenAppend { .. }
        | CrashOperation::Sync { .. }
        | CrashOperation::Rename { .. }
        | CrashOperation::Delete { .. } => Err(std::io::Error::other(
            "damaged bytes requested for a non-byte operation",
        )),
    }
}

fn materialize_reordered(
    initial: &MemoryVfs,
    operations: &[CrashOperation],
    durable_start: usize,
    through_operation: usize,
    persisted_operations: &[usize],
) -> std::io::Result<Option<MemoryVfs>> {
    let filesystem = replay_prefix(initial, operations, durable_start)?;
    let pending = operations
        .get(durable_start..through_operation)
        .ok_or_else(|| std::io::Error::other("invalid pending reorder range"))?;
    for operation in pending {
        if matches!(operation, CrashOperation::OpenAppend { .. }) {
            apply_operation(&filesystem, operation)?;
        }
    }
    for operation_index in persisted_operations {
        let operation = operations
            .get(*operation_index)
            .ok_or_else(|| std::io::Error::other("invalid reordered operation index"))?;
        apply_operation(&filesystem, operation)?;
    }
    for operation in pending {
        if matches!(
            operation,
            CrashOperation::Rename { .. } | CrashOperation::Delete { .. }
        ) {
            match apply_operation(&filesystem, operation) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
    }
    Ok(Some(filesystem))
}

fn visit_ordered_subsets<F>(
    remaining: &[usize],
    schedule: &mut Vec<usize>,
    visitor: &mut F,
) -> std::io::Result<bool>
where
    F: FnMut(&[usize]) -> std::io::Result<bool>,
{
    if !visitor(schedule)? {
        return Ok(false);
    }
    for position in 0..remaining.len() {
        let operation = *remaining
            .get(position)
            .ok_or_else(|| std::io::Error::other("invalid permutation position"))?;
        schedule.push(operation);
        let next = remaining
            .iter()
            .enumerate()
            .filter_map(|(index, value)| (index != position).then_some(*value))
            .collect::<Vec<_>>();
        if !visit_ordered_subsets(&next, schedule, visitor)? {
            return Ok(false);
        }
        let _ = schedule.pop();
    }
    Ok(true)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        Path::new("/store").join(name)
    }

    fn pointer_violations(states: &CrashStates) -> Vec<String> {
        states
            .iter()
            .filter_map(|state| {
                let pointer = match state.vfs().read(&path("current")) {
                    Ok(pointer) => pointer,
                    Err(error) => {
                        return Some(format!("{:?}: pointer read failed: {error}", state.kind()));
                    }
                };
                let target = match std::str::from_utf8(&pointer) {
                    Ok(target) => target,
                    Err(error) => {
                        return Some(format!("{:?}: pointer is not UTF-8: {error}", state.kind()));
                    }
                };
                let expected = match target {
                    "data-old" => b"old".as_slice(),
                    "data-new" => b"new durable bytes".as_slice(),
                    _ => return Some(format!("{:?}: invalid pointer {pointer:?}", state.kind())),
                };
                let actual = state.vfs().read(&path(target)).ok();
                (actual.as_deref() != Some(expected)).then(|| {
                    format!(
                        "{:?}: pointer={target}, data={actual:?}, expected={expected:?}",
                        state.kind()
                    )
                })
            })
            .collect()
    }

    fn seeded_pointer_store() -> MemoryVfs {
        let backing = MemoryVfs::new();
        backing
            .insert(path("current"), b"data-old".to_vec())
            .expect("pointer");
        backing
            .insert(path("data-old"), b"old".to_vec())
            .expect("old data");
        backing
    }

    #[test]
    fn records_every_mutation_and_payload_in_order() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        recorder.write(&path("a"), b"first").expect("write");
        let mut append = recorder.open_append(&path("a")).expect("open append");
        append.append(b"-second").expect("append");
        append.sync(SyncKind::Full).expect("handle sync");
        recorder
            .sync(&path("a"), SyncKind::Barrier)
            .expect("path sync");
        recorder.rename(&path("a"), &path("b")).expect("rename");
        recorder.delete(&path("b")).expect("delete");

        assert_eq!(
            recorder.operations().expect("operations"),
            vec![
                CrashOperation::Write {
                    path: path("a"),
                    bytes: b"first".to_vec(),
                },
                CrashOperation::OpenAppend { path: path("a") },
                CrashOperation::Append {
                    path: path("a"),
                    bytes: b"-second".to_vec(),
                },
                CrashOperation::Sync {
                    path: path("a"),
                    kind: SyncKind::Full,
                },
                CrashOperation::Sync {
                    path: path("a"),
                    kind: SyncKind::Barrier,
                },
                CrashOperation::Rename {
                    from: path("a"),
                    to: path("b"),
                },
                CrashOperation::Delete { path: path("b") },
            ]
        );
    }

    #[test]
    fn real_filesystem_recorder_delegates_reads_and_records_mutations() {
        let backing = MemoryVfs::new();
        backing
            .insert(path("seed"), b"abcdef".to_vec())
            .expect("seed");
        let recorder = RecordingVfs::new(backing);

        assert_eq!(recorder.inner().open(&path("seed")).expect("inner"), 6);
        assert_eq!(recorder.open(&path("seed")).expect("open"), 6);
        assert_eq!(recorder.read(&path("seed")).expect("read"), b"abcdef");
        assert_eq!(
            recorder
                .read_range(&path("seed"), 2, 3)
                .expect("read range"),
            b"cde"
        );
        assert_eq!(recorder.list(Path::new("/store")).expect("list").len(), 1);

        recorder.write(&path("a"), b"first").expect("write");
        let mut append = recorder.open_append(&path("a")).expect("open append");
        append.append(b"-second").expect("append");
        append.sync(SyncKind::Full).expect("handle sync");
        recorder
            .sync(&path("a"), SyncKind::Barrier)
            .expect("path sync");
        recorder.rename(&path("a"), &path("b")).expect("rename");
        recorder.delete(&path("b")).expect("delete");

        assert_eq!(recorder.operations().expect("operations").len(), 7);
    }

    #[test]
    fn reads_pass_through_without_entering_the_mutation_stream() {
        let backing = MemoryVfs::new();
        backing.insert(path("a"), b"abcdef".to_vec()).expect("seed");
        let recorder = CrashVfs::new(backing).expect("recorder");
        assert_eq!(recorder.open(&path("a")).expect("open"), 6);
        assert_eq!(recorder.read(&path("a")).expect("read"), b"abcdef");
        assert_eq!(
            recorder.read_range(&path("a"), 2, 3).expect("read range"),
            b"cde"
        );
        assert_eq!(recorder.list(Path::new("/store")).expect("list").len(), 1);
        assert_eq!(recorder.operations().expect("operations").len(), 0);
    }

    #[test]
    fn enumerates_every_prefix_including_before_first_and_after_last() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        recorder.write(&path("a"), b"a").expect("a");
        recorder.write(&path("b"), b"b").expect("b");
        recorder
            .sync(Path::new("/store"), SyncKind::Barrier)
            .expect("sync");
        let completed = recorder
            .crash_states()
            .expect("states")
            .iter()
            .filter_map(|state| match state.kind() {
                CrashStateKind::Prefix {
                    completed_operations,
                } => Some(*completed_operations),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(completed, vec![0, 1, 2, 3]);
    }

    #[test]
    fn selects_edge_midpoint_and_sector_tear_points_without_a_uniform_grid() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        recorder
            .write(&path("large"), &vec![7_u8; 1_537])
            .expect("large write");
        let torn = recorder
            .crash_states()
            .expect("states")
            .iter()
            .filter_map(|state| match state.kind() {
                CrashStateKind::TornWrite {
                    edge: TornWriteEdge::Prefix,
                    persisted_bytes,
                    ..
                } => Some(*persisted_bytes),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(torn, vec![1, 512, 768, 1_024, 1_536]);
    }

    #[test]
    fn torn_append_keeps_prior_bytes_and_appends_only_the_boundary_prefix() {
        let backing = MemoryVfs::new();
        backing
            .insert(path("wal"), b"header".to_vec())
            .expect("seed WAL");
        let recorder = CrashVfs::new(backing).expect("recorder");
        let mut file = recorder.open_append(&path("wal")).expect("open append");
        file.append(&vec![9_u8; 1_025]).expect("append");
        let states = recorder.crash_states().expect("states");
        let state = states
            .iter()
            .find(|state| {
                matches!(
                    state.kind(),
                    CrashStateKind::TornWrite {
                        operation_index: 1,
                        edge: TornWriteEdge::Prefix,
                        persisted_bytes: 512
                    }
                )
            })
            .expect("512-byte torn append");
        let bytes = state.vfs().read(&path("wal")).expect("torn WAL");
        assert_eq!(bytes.len(), 6 + 512);
        assert_eq!(bytes.get(..6), Some(b"header".as_slice()));
        assert!(
            bytes
                .get(6..)
                .expect("append bytes")
                .iter()
                .all(|byte| *byte == 9)
        );
    }

    #[test]
    fn full_length_corruption_classes_are_distinct_and_deterministic() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        let bytes = (1_u8..=97).collect::<Vec<_>>();
        recorder.write(&path("small"), &bytes).expect("small write");
        let states = recorder.crash_states().expect("states");
        let counts = [
            CrashStateClass::ExtendedWithGarbage,
            CrashStateClass::ExtendedWithZeros,
            CrashStateClass::InteriorDamage,
        ]
        .map(|class| {
            states
                .iter()
                .filter(|state| state.kind().class() == class)
                .count()
        });
        assert_eq!(counts, [3, 3, 3], "97-byte write corruption-class counts");

        let garbage = states
            .iter()
            .find(|state| {
                matches!(
                    state.kind(),
                    CrashStateKind::ExtendedWithGarbage {
                        operation_index: 0,
                        intact_prefix_bytes: 48
                    }
                )
            })
            .expect("48-byte garbage extension")
            .vfs()
            .read(&path("small"))
            .expect("garbage image");
        let zeros = states
            .iter()
            .find(|state| {
                matches!(
                    state.kind(),
                    CrashStateKind::ExtendedWithZeros {
                        operation_index: 0,
                        intact_prefix_bytes: 48
                    }
                )
            })
            .expect("48-byte zero extension")
            .vfs()
            .read(&path("small"))
            .expect("zero image");
        let interior = states
            .iter()
            .find(|state| {
                matches!(
                    state.kind(),
                    CrashStateKind::InteriorDamage {
                        operation_index: 0,
                        damage_start: 48,
                        damage_length: 48
                    }
                )
            })
            .expect("48-byte interior damage")
            .vfs()
            .read(&path("small"))
            .expect("interior image");
        assert_eq!(garbage.len(), bytes.len(), "garbage extension length");
        assert_eq!(zeros.len(), bytes.len(), "zero extension length");
        assert_eq!(interior.len(), bytes.len(), "interior damage length");
        assert_eq!(&garbage[..48], &bytes[..48], "garbage intact prefix");
        assert_eq!(&zeros[..48], &bytes[..48], "zero intact prefix");
        assert!(
            garbage[48..].iter().all(|byte| *byte != 0),
            "garbage tail contains zero: {:?}",
            &garbage[48..]
        );
        assert!(
            zeros[48..].iter().all(|byte| *byte == 0),
            "zero tail is not zero-filled: {:?}",
            &zeros[48..]
        );
        assert_ne!(garbage, zeros, "garbage and zero classes collapsed");
        assert_eq!(interior[0], bytes[0], "interior damage changed first byte");
        assert_eq!(interior[96], bytes[96], "interior damage changed last byte");
        assert_ne!(
            &interior[48..96],
            &bytes[48..96],
            "interior range was unchanged"
        );
    }

    #[test]
    fn sub_sector_writes_tear_from_both_edges() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        recorder
            .write(&path("small"), b"abcdefghijklmnopq")
            .expect("small write");
        let states = recorder.crash_states().expect("states");
        let torn = states
            .iter()
            .filter_map(|state| match state.kind() {
                CrashStateKind::TornWrite {
                    edge,
                    persisted_bytes,
                    ..
                } => Some((*edge, *persisted_bytes)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            torn,
            vec![
                (TornWriteEdge::Prefix, 1),
                (TornWriteEdge::Suffix, 1),
                (TornWriteEdge::Prefix, 8),
                (TornWriteEdge::Suffix, 8),
                (TornWriteEdge::Prefix, 16),
                (TornWriteEdge::Suffix, 16),
            ],
            "17-byte sub-sector tear schedules"
        );
        let prefix = states
            .iter()
            .find(|state| {
                matches!(
                    state.kind(),
                    CrashStateKind::TornWrite {
                        edge: TornWriteEdge::Prefix,
                        persisted_bytes: 1,
                        ..
                    }
                )
            })
            .expect("one-byte prefix")
            .vfs()
            .read(&path("small"))
            .expect("prefix image");
        let suffix = states
            .iter()
            .find(|state| {
                matches!(
                    state.kind(),
                    CrashStateKind::TornWrite {
                        edge: TornWriteEdge::Suffix,
                        persisted_bytes: 1,
                        ..
                    }
                )
            })
            .expect("one-byte suffix")
            .vfs()
            .read(&path("small"))
            .expect("suffix image");
        assert_eq!(prefix, b"a", "one-byte prefix materialization");
        assert_eq!(suffix, b"q", "one-byte suffix materialization");
    }

    #[test]
    fn unsynchronized_writes_may_persist_as_either_singleton() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        recorder.write(&path("same"), b"first").expect("first");
        recorder.write(&path("same"), b"second").expect("second");
        let states = recorder.crash_states().expect("states");
        let singleton_schedules = states
            .iter()
            .filter_map(|state| match state.kind() {
                CrashStateKind::ReorderedWrites {
                    persisted_operations,
                    ..
                } if persisted_operations.len() == 1 => Some(persisted_operations.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            singleton_schedules.contains(&vec![0]) && singleton_schedules.contains(&vec![1]),
            "singleton schedules: {singleton_schedules:?}"
        );
        let full_schedules = states
            .iter()
            .filter_map(|state| match state.kind() {
                CrashStateKind::ReorderedWrites {
                    persisted_operations,
                    ..
                } if persisted_operations.len() == 2 => Some(persisted_operations.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(full_schedules, vec![vec![0, 1], vec![1, 0]]);
        let reverse = states
            .iter()
            .find(|state| {
                matches!(
                    state.kind(),
                    CrashStateKind::ReorderedWrites {
                        persisted_operations,
                        ..
                    } if persisted_operations == &[1, 0]
                )
            })
            .expect("reverse persistence order");
        assert_eq!(
            reverse.vfs().read(&path("same")).expect("same path"),
            b"first"
        );
    }

    #[test]
    fn a_sync_barrier_prevents_cross_barrier_reordering() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        recorder
            .write(&path("data-new"), b"new durable bytes")
            .expect("data");
        recorder
            .sync(&path("data-new"), SyncKind::Barrier)
            .expect("barrier");
        recorder
            .write(&path("current"), b"data-new")
            .expect("pointer");
        let invalid = recorder
            .crash_states()
            .expect("states")
            .iter()
            .filter(|state| {
                state.vfs().read(&path("current")).is_ok()
                    && state.vfs().read(&path("data-new")).is_err()
            })
            .count();
        assert_eq!(invalid, 0, "pointer survived without pre-barrier data");
    }

    #[test]
    fn unsynced_rename_can_expose_the_sources_old_content() {
        let backing = MemoryVfs::new();
        backing
            .insert(path("pointer.tmp"), b"old-pointer".to_vec())
            .expect("seed temp");
        let recorder = CrashVfs::new(backing).expect("recorder");
        recorder
            .write(&path("pointer.tmp"), b"new-pointer")
            .expect("new temp");
        recorder
            .rename(&path("pointer.tmp"), &path("current"))
            .expect("rename");
        let old_content = recorder
            .crash_states()
            .expect("states")
            .iter()
            .filter(|state| {
                matches!(
                    state.kind(),
                    CrashStateKind::RenameWithOldContent { operation_index: 1 }
                ) && state.vfs().read(&path("current")).ok().as_deref()
                    == Some(b"old-pointer".as_slice())
            })
            .count();
        assert_eq!(old_content, 1);
    }

    #[test]
    fn planted_pointer_before_data_bug_is_caught() {
        let recorder = CrashVfs::new(seeded_pointer_store()).expect("recorder");
        recorder
            .write(&path("current"), b"data-new")
            .expect("pointer first");
        recorder
            .write(&path("data-new"), b"new durable bytes")
            .expect("data second");
        let states = recorder.crash_states().expect("states");
        let violations = pointer_violations(&states);
        assert!(
            !violations.is_empty(),
            "planted bug escaped {} crash states",
            states.len()
        );
    }

    #[test]
    fn correct_data_sync_pointer_rename_directory_sync_protocol_passes() {
        let recorder = CrashVfs::new(seeded_pointer_store()).expect("recorder");
        recorder
            .write(&path("data-new"), b"new durable bytes")
            .expect("data");
        recorder
            .sync(&path("data-new"), SyncKind::Barrier)
            .expect("data sync");
        recorder
            .write(&path("pointer.tmp"), b"data-new")
            .expect("pointer temp");
        recorder
            .sync(&path("pointer.tmp"), SyncKind::Barrier)
            .expect("pointer sync");
        recorder
            .rename(&path("pointer.tmp"), &path("current"))
            .expect("pointer rename");
        recorder
            .sync(Path::new("/store"), SyncKind::Barrier)
            .expect("directory sync");
        let states = recorder.crash_states().expect("states");
        assert!(
            states.len() > 1,
            "correct protocol enumerated only {} state",
            states.len()
        );
        assert_eq!(pointer_violations(&states), Vec::<String>::new());
    }

    #[test]
    fn repeated_enumeration_is_byte_for_byte_deterministic() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        recorder.write(&path("a"), &vec![1_u8; 1_025]).expect("a");
        recorder.write(&path("b"), b"b").expect("b");
        let first = recorder.crash_states().expect("first");
        let second = recorder.crash_states().expect("second");
        let first_images = first
            .iter()
            .map(|state| (state.kind().clone(), state.vfs().files().expect("files")))
            .collect::<Vec<_>>();
        let second_images = second
            .iter()
            .map(|state| (state.kind().clone(), state.vfs().files().expect("files")))
            .collect::<Vec<_>>();
        assert_eq!(first_images, second_images);
    }

    #[test]
    fn enumeration_cap_is_loud_and_never_exceeded() {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
        for index in 0..8 {
            recorder
                .write(&path(&format!("file-{index}")), &[index as u8])
                .expect("write");
        }
        let states = recorder.crash_states().expect("states");
        assert_eq!(states.len(), MAX_CRASH_STATES);
        assert!(
            states.was_capped(),
            "4096-state enumeration did not report its cap"
        );
    }
}
