//! Single-writer ownership via a process-local registry and an OS file lock.
//!
//! Two mechanisms are needed, because neither alone is sufficient:
//!
//! * The **process-local registry** provides same-process exclusion. POSIX
//!   record locks do not conflict between descriptors held by one process, and
//!   Windows byte-range locks likewise do not conflict with the process that
//!   already owns them, so without the registry a second `Store` in the same
//!   process would be admitted.
//! * The **persistent `writer.lock`** provides cross-process exclusion and is
//!   released by the operating system when the holder dies, orderly or not.
//!
//! The registry is keyed on the store directory's stable filesystem identity —
//! `(st_dev, st_ino)` on Unix, `(volume serial, file index)` on Windows — never
//! on path text, so case differences, separator differences, relative versus
//! absolute spellings and supported junctions all converge on one store.
//!
//! Nothing else in this process may open [`STORE_LOCK_FILE`].

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Persistent lock filename; only [`StoreLock`] may open this file.
pub const STORE_LOCK_FILE: &str = "writer.lock";

/// Stable filesystem identity of a store directory.
type StoreKey = (u64, u64);

static HELD_STORES: Mutex<BTreeSet<StoreKey>> = Mutex::new(BTreeSet::new());

/// Exclusive writer ownership automatically released when the process dies.
#[derive(Debug)]
pub struct StoreLock {
    file: Option<File>,
    registry_key: StoreKey,
}

impl StoreLock {
    /// Attempts to acquire exclusive non-blocking ownership for one store.
    ///
    /// Registry admission happens first so a second same-process writer is
    /// rejected before any lock file is touched. Every failure after that point
    /// unregisters exactly once, so a failed admission never leaves the store
    /// permanently unopenable.
    pub fn acquire(directory: &Path) -> Result<Self, StoreLockError> {
        let path = directory.join(STORE_LOCK_FILE);
        let registry_key = store_identity(directory).map_err(|source| StoreLockError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        if !held_stores().insert(registry_key) {
            return Err(StoreLockError::Io {
                path,
                source: std::io::Error::from(std::io::ErrorKind::WouldBlock),
            });
        }
        let file = open_lock_file(&path).map_err(|source| {
            held_stores().remove(&registry_key);
            StoreLockError::Io {
                path: path.clone(),
                source,
            }
        })?;
        if let Err(source) = lock_exclusive(&file) {
            drop(file);
            held_stores().remove(&registry_key);
            return Err(StoreLockError::Io { path, source });
        }
        Ok(Self {
            file: Some(file),
            registry_key,
        })
    }
}

fn held_stores() -> MutexGuard<'static, BTreeSet<StoreKey>> {
    match HELD_STORES.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The store directory's stable filesystem identity.
#[cfg(unix)]
fn store_identity(directory: &Path) -> std::io::Result<StoreKey> {
    let metadata = std::fs::metadata(directory)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn store_identity(directory: &Path) -> std::io::Result<StoreKey> {
    crate::sys::windows::directory_identity(directory)
}

#[cfg(not(any(unix, windows)))]
fn store_identity(_directory: &Path) -> std::io::Result<StoreKey> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "store identity requires a supported platform",
    ))
}

/// Opens the persistent lock file with the access the platform lock needs.
#[cfg(not(windows))]
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

/// Opens the persistent lock file, additionally refusing to share deletion.
///
/// Rust opens files with `FILE_SHARE_READ | FILE_SHARE_WRITE |
/// FILE_SHARE_DELETE` by default. Sharing deletion would let any process
/// unlink or rename `writer.lock` out from under a live writer, after which a
/// second writer would create a fresh lock file and be admitted. Narrowing the
/// share mode makes the held lock file undeletable and unrenameable for as long
/// as ownership lasts. Handles are non-inheritable by default, so a child
/// process cannot inherit writer ownership either.
#[cfg(windows)]
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .share_mode(crate::sys::windows::FILE_SHARE_READ | crate::sys::windows::FILE_SHARE_WRITE)
        .open(path)
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    let mut lock = unsafe {
        // SAFETY: all-zero is a valid `flock` value before named fields are set.
        std::mem::zeroed::<libc::flock>()
    };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = 0;
    lock.l_len = 0;
    let result = unsafe {
        // SAFETY: `file` owns a live descriptor and `lock` remains valid for
        // the duration of this non-blocking `F_SETLK` call.
        libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock)
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|errno| errno == libc::EAGAIN || errno == libc::EACCES)
        {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        } else {
            Err(error)
        }
    } else {
        Ok(())
    }
}

/// Takes the cross-process lock with `LockFileEx`, non-blocking and exclusive,
/// over the fixed range documented by `sys::windows::WRITER_LOCK_RANGE`.
///
/// Contention arrives as [`std::io::ErrorKind::WouldBlock`]; a permission
/// failure stays a permission failure and is never reported as contention.
#[cfg(windows)]
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    crate::sys::windows::lock_file_exclusive(file)
}

#[cfg(not(any(unix, windows)))]
fn lock_exclusive(_: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "store locks require a supported file-locking platform",
    ))
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Closing the last handle releases the OS lock on both platforms; the
        // registry entry is then removed exactly once.
        drop(self.file.take());
        held_stores().remove(&self.registry_key);
    }
}

/// Failure to open or acquire the single-writer lock.
#[derive(Debug)]
pub enum StoreLockError {
    /// A named filesystem or lock operation failed.
    Io {
        /// Lock-file path.
        path: PathBuf,
        /// Underlying platform error.
        source: std::io::Error,
    },
}

impl std::fmt::Display for StoreLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "store lock {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for StoreLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
        }
    }
}
