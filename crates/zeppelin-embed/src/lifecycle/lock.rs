//! Kernel-owned single-writer lock whose lifetime is one open descriptor.

use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// Persistent lock filename; ownership is the open descriptor, not this entry.
pub const STORE_LOCK_FILE: &str = "writer.lock";

/// Exclusive writer ownership automatically released when the process dies.
#[derive(Debug)]
pub struct StoreLock {
    _file: File,
}

impl StoreLock {
    /// Attempts to acquire exclusive non-blocking ownership for one store.
    pub fn acquire(directory: &Path) -> Result<Self, StoreLockError> {
        let path = directory.join(STORE_LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| StoreLockError::Io {
                path: path.clone(),
                source,
            })?;
        lock_exclusive(&file).map_err(|source| StoreLockError::Io { path, source })?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    let result = unsafe {
        // SAFETY: `file` owns a live descriptor for the duration of the call.
        libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "descriptor-scoped store locks require Unix flock",
    ))
}

/// Failure to open or acquire the single-writer descriptor lock.
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
