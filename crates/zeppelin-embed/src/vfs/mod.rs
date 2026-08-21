//! Synchronous virtual-filesystem seam for crash and fault injection.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
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

/// Part-A placeholder for the `ordered` default; Part B must re-derive this
/// synchronization choice from the real durability-tier policy.
pub(crate) const PART_A_ORDERED_SYNC: SyncKind = SyncKind::Barrier;

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
    open_calls: AtomicU64,
    read_calls: AtomicU64,
    read_bytes: AtomicU64,
}

impl<V> CountingVfs<V> {
    /// Wraps a filesystem with zeroed counters.
    #[must_use]
    pub const fn new(inner: V) -> Self {
        Self {
            inner,
            open_calls: AtomicU64::new(0),
            read_calls: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
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
        self.open_calls.load(Ordering::Relaxed)
    }

    /// Returns observed read calls.
    #[must_use]
    pub fn read_calls(&self) -> u64 {
        self.read_calls.load(Ordering::Relaxed)
    }

    /// Returns exact bytes returned by reads.
    #[must_use]
    pub fn read_bytes(&self) -> u64 {
        self.read_bytes.load(Ordering::Relaxed)
    }

    /// Resets all counters without changing the wrapped filesystem.
    pub fn reset(&self) {
        self.open_calls.store(0, Ordering::Relaxed);
        self.read_calls.store(0, Ordering::Relaxed);
        self.read_bytes.store(0, Ordering::Relaxed);
    }
}

impl<V: Vfs> Vfs for CountingVfs<V> {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.open_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.open(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let bytes = self.inner.read(path)?;
        self.read_calls.fetch_add(1, Ordering::Relaxed);
        self.read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        let bytes = self.inner.read_range(path, offset, length)?;
        self.read_calls.fetch_add(1, Ordering::Relaxed);
        self.read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.inner.open_append(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
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
