//! Synchronous virtual-filesystem seam for crash and fault injection.

#[cfg(any(test, feature = "test-support"))]
pub mod crash;

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Storage-ordering primitive requested by a persisted commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncKind {
    /// Order prior writes before later writes without requesting durable media.
    ///
    /// Linux has no barrier-only primitive, so `ordered` durability degrades
    /// to `fdatasync` on Linux.
    Barrier,
    /// Flush prior writes through the platform's durable-media primitive.
    Full,
}

/// One open append-only file owned by a WAL writer.
pub trait VfsFile: Send {
    /// Appends every supplied byte without reopening the path.
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    /// Synchronizes this already-open handle using the named primitive.
    fn sync(&self, kind: SyncKind) -> std::io::Result<()>;
}

/// Minimal synchronous filesystem operations used by persisted commits.
pub trait Vfs: Send + Sync {
    /// Opens an existing path and returns its byte length.
    fn open(&self, path: &Path) -> std::io::Result<u64>;
    /// Reads an entire file.
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// Reads at most `length` bytes beginning at `offset`.
    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>>;
    /// Creates or truncates a file and writes all bytes.
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()>;
    /// Opens or creates one file for handle-oriented append-only writes.
    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>>;
    /// Atomically renames one path over another according to platform semantics.
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    /// Synchronizes a file or directory using an explicit platform primitive.
    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()>;
    /// Lists direct children of a directory.
    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>>;
    /// Deletes one file.
    fn delete(&self, path: &Path) -> std::io::Result<()>;
}

/// Standard-library-backed production filesystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdVfs;

struct StdVfsFile(File);

impl VfsFile for StdVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_all(bytes)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        sync_file(&self.0, kind)
    }
}

impl Vfs for StdVfs {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        Ok(File::open(path)?.metadata()?.len())
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let length = usize::try_from(file.metadata()?.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "file length exceeds usize")
        })?;
        let mut bytes = Vec::with_capacity(length);
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0_u8; length];
        let mut filled = 0_usize;
        while filled < length {
            let remaining = bytes.get_mut(filled..).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid read range")
            })?;
            let read = file.read(remaining)?;
            if read == 0 {
                break;
            }
            filled = filled.saturating_add(read);
        }
        bytes.truncate(filled);
        Ok(bytes)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        file.write_all(bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Box::new(StdVfsFile(file)))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        sync_file(&File::open(path)?, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        std::fs::read_dir(directory)?
            .map(|entry| entry.map(|value| value.path()))
            .collect()
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// Byte/call-counting decorator used by deterministic open-cost gates.
pub struct CountingVfs<V> {
    inner: V,
    counters: Arc<CountingVfsCounters>,
}

#[derive(Default)]
struct CountingVfsCounters {
    open_calls: AtomicU64,
    read_calls: AtomicU64,
    read_bytes: AtomicU64,
    write_calls: AtomicU64,
    bytes_written: AtomicU64,
    append_calls: AtomicU64,
    bytes_appended: AtomicU64,
    rename_calls: AtomicU64,
    delete_calls: AtomicU64,
    barrier_sync_calls: AtomicU64,
    full_sync_calls: AtomicU64,
    handle_barrier_sync_calls: AtomicU64,
    handle_full_sync_calls: AtomicU64,
}

struct CountingVfsFile {
    inner: Box<dyn VfsFile>,
    counters: Arc<CountingVfsCounters>,
}

impl VfsFile for CountingVfsFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.append(bytes)?;
        self.counters.append_calls.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_appended
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(kind)?;
        match kind {
            SyncKind::Barrier => &self.counters.handle_barrier_sync_calls,
            SyncKind::Full => &self.counters.handle_full_sync_calls,
        }
        .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl<V> CountingVfs<V> {
    /// Wraps a filesystem with zeroed counters.
    #[must_use]
    pub fn new(inner: V) -> Self {
        Self {
            inner,
            counters: Arc::new(CountingVfsCounters::default()),
        }
    }

    /// Returns the wrapped filesystem.
    #[must_use]
    pub const fn inner(&self) -> &V {
        &self.inner
    }

    /// Returns observed open calls.
    #[must_use]
    pub fn open_calls(&self) -> u64 {
        self.counters.open_calls.load(Ordering::Relaxed)
    }

    /// Returns observed read calls.
    #[must_use]
    pub fn read_calls(&self) -> u64 {
        self.counters.read_calls.load(Ordering::Relaxed)
    }

    /// Returns exact bytes returned by reads.
    #[must_use]
    pub fn read_bytes(&self) -> u64 {
        self.counters.read_bytes.load(Ordering::Relaxed)
    }

    /// Returns observed whole-file write calls.
    #[must_use]
    pub fn write_calls(&self) -> u64 {
        self.counters.write_calls.load(Ordering::Relaxed)
    }

    /// Returns exact bytes supplied to successful whole-file writes.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.counters.bytes_written.load(Ordering::Relaxed)
    }

    /// Returns observed successful handle append calls.
    #[must_use]
    pub fn append_calls(&self) -> u64 {
        self.counters.append_calls.load(Ordering::Relaxed)
    }

    /// Returns exact bytes supplied to successful handle appends.
    #[must_use]
    pub fn bytes_appended(&self) -> u64 {
        self.counters.bytes_appended.load(Ordering::Relaxed)
    }

    /// Returns observed successful rename calls.
    #[must_use]
    pub fn rename_calls(&self) -> u64 {
        self.counters.rename_calls.load(Ordering::Relaxed)
    }

    /// Returns observed successful delete calls.
    #[must_use]
    pub fn delete_calls(&self) -> u64 {
        self.counters.delete_calls.load(Ordering::Relaxed)
    }

    /// Returns path-based barrier synchronization calls.
    #[must_use]
    pub fn barrier_sync_calls(&self) -> u64 {
        self.counters.barrier_sync_calls.load(Ordering::Relaxed)
    }

    /// Returns path-based full synchronization calls.
    #[must_use]
    pub fn full_sync_calls(&self) -> u64 {
        self.counters.full_sync_calls.load(Ordering::Relaxed)
    }

    /// Returns open-handle barrier synchronization calls.
    #[must_use]
    pub fn handle_barrier_sync_calls(&self) -> u64 {
        self.counters
            .handle_barrier_sync_calls
            .load(Ordering::Relaxed)
    }

    /// Returns open-handle full synchronization calls.
    #[must_use]
    pub fn handle_full_sync_calls(&self) -> u64 {
        self.counters.handle_full_sync_calls.load(Ordering::Relaxed)
    }

    /// Resets all counters without changing the wrapped filesystem.
    pub fn reset(&self) {
        self.counters.open_calls.store(0, Ordering::Relaxed);
        self.counters.read_calls.store(0, Ordering::Relaxed);
        self.counters.read_bytes.store(0, Ordering::Relaxed);
        self.counters.write_calls.store(0, Ordering::Relaxed);
        self.counters.bytes_written.store(0, Ordering::Relaxed);
        self.counters.append_calls.store(0, Ordering::Relaxed);
        self.counters.bytes_appended.store(0, Ordering::Relaxed);
        self.counters.rename_calls.store(0, Ordering::Relaxed);
        self.counters.delete_calls.store(0, Ordering::Relaxed);
        self.counters.barrier_sync_calls.store(0, Ordering::Relaxed);
        self.counters.full_sync_calls.store(0, Ordering::Relaxed);
        self.counters
            .handle_barrier_sync_calls
            .store(0, Ordering::Relaxed);
        self.counters
            .handle_full_sync_calls
            .store(0, Ordering::Relaxed);
    }
}

impl<V: Vfs> Vfs for CountingVfs<V> {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.counters.open_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.open(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let bytes = self.inner.read(path)?;
        self.counters.read_calls.fetch_add(1, Ordering::Relaxed);
        self.counters
            .read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        let bytes = self.inner.read_range(path, offset, length)?;
        self.counters.read_calls.fetch_add(1, Ordering::Relaxed);
        self.counters
            .read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(path, bytes)?;
        self.counters.write_calls.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_written
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        Ok(Box::new(CountingVfsFile {
            inner: self.inner.open_append(path)?,
            counters: Arc::clone(&self.counters),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)?;
        self.counters.rename_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)?;
        match kind {
            SyncKind::Barrier => &self.counters.barrier_sync_calls,
            SyncKind::Full => &self.counters.full_sync_calls,
        }
        .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)?;
        self.counters.delete_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn sync_file(file: &File, kind: SyncKind) -> std::io::Result<()> {
    let result = match kind {
        SyncKind::Barrier => crate::sys::darwin::barrier_fsync(file.as_raw_fd()),
        SyncKind::Full => crate::sys::darwin::full_fsync(file.as_raw_fd()),
    };
    result.map_err(std::io::Error::other)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn sync_file(file: &File, kind: SyncKind) -> std::io::Result<()> {
    let result = unsafe {
        // SAFETY: both calls accept an owned live descriptor and report failures through errno.
        match kind {
            SyncKind::Barrier => libc::fdatasync(file.as_raw_fd()),
            SyncKind::Full => libc::fsync(file.as_raw_fd()),
        }
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn sync_file(_: &File, kind: SyncKind) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("{kind:?} synchronization requires a supported file-descriptor platform"),
    ))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::crash::MemoryVfs;
    use super::{CountingVfs, SyncKind, Vfs};

    #[test]
    fn crash_support_api_gate_is_identical_in_debug_and_release() {
        let source = include_str!("mod.rs");
        let gate = source
            .lines()
            .zip(source.lines().skip(1))
            .find_map(|(line, next)| (next == "pub mod crash;").then_some(line));
        assert_eq!(gate, Some("#[cfg(any(test, feature = \"test-support\"))]"));
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn counting_vfs_counts_every_mutation_and_sync_kind_exactly() {
        let inner = MemoryVfs::new();
        inner
            .insert("/source", b"abc".to_vec())
            .expect("seed source");
        let counting = CountingVfs::new(inner);

        assert_eq!(counting.open(Path::new("/source")).expect("open"), 3);
        assert_eq!(counting.read(Path::new("/source")).expect("read"), b"abc");
        assert_eq!(
            counting
                .read_range(Path::new("/source"), 1, 2)
                .expect("range"),
            b"bc"
        );
        counting.write(Path::new("/write"), b"1234").expect("write");
        let mut handle = counting
            .open_append(Path::new("/append"))
            .expect("open append");
        handle.append(b"12").expect("first append");
        handle.append(b"345").expect("second append");
        handle.sync(SyncKind::Barrier).expect("handle barrier");
        handle.sync(SyncKind::Full).expect("handle full");
        counting
            .sync(Path::new("/write"), SyncKind::Barrier)
            .expect("path barrier");
        counting
            .sync(Path::new("/write"), SyncKind::Full)
            .expect("path full");
        counting
            .rename(Path::new("/write"), Path::new("/renamed"))
            .expect("rename");
        counting.delete(Path::new("/renamed")).expect("delete");

        assert_eq!(
            [
                counting.open_calls(),
                counting.read_calls(),
                counting.read_bytes(),
                counting.write_calls(),
                counting.bytes_written(),
                counting.append_calls(),
                counting.bytes_appended(),
                counting.rename_calls(),
                counting.delete_calls(),
                counting.barrier_sync_calls(),
                counting.full_sync_calls(),
                counting.handle_barrier_sync_calls(),
                counting.handle_full_sync_calls(),
            ],
            [1, 2, 5, 1, 4, 2, 5, 1, 1, 1, 1, 1, 1],
            "CountingVfs must expose every exact VFS and open-handle cost"
        );
        counting.reset();
        assert_eq!(
            [
                counting.open_calls(),
                counting.read_calls(),
                counting.read_bytes(),
                counting.write_calls(),
                counting.bytes_written(),
                counting.append_calls(),
                counting.bytes_appended(),
                counting.rename_calls(),
                counting.delete_calls(),
                counting.barrier_sync_calls(),
                counting.full_sync_calls(),
                counting.handle_barrier_sync_calls(),
                counting.handle_full_sync_calls(),
            ],
            [0; 13],
            "reset must zero decorator and shared-handle counters"
        );
    }
}
