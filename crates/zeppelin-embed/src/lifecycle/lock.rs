//! Single-writer ownership via a process-local registry and `fcntl` lock.
//!
//! POSIX record locks are not inherited across `fork`, but same-process locks
//! do not conflict and closing any descriptor for the lock file releases them.
//! The registry preserves same-process exclusion. Nothing else in this process
//! may open [`STORE_LOCK_FILE`].

#[cfg(unix)]
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Mutex, MutexGuard};

/// Persistent lock filename; only [`StoreLock`] may open this file.
pub const STORE_LOCK_FILE: &str = "writer.lock";

#[cfg(unix)]
type StoreKey = (u64, u64);

#[cfg(unix)]
static HELD_STORES: Mutex<BTreeSet<StoreKey>> = Mutex::new(BTreeSet::new());

/// Exclusive writer ownership automatically released when the process dies.
#[derive(Debug)]
pub struct StoreLock {
    file: Option<File>,
    #[cfg(unix)]
    registry_key: StoreKey,
}

impl StoreLock {
    /// Attempts to acquire exclusive non-blocking ownership for one store.
    pub fn acquire(directory: &Path) -> Result<Self, StoreLockError> {
        let path = directory.join(STORE_LOCK_FILE);
        #[cfg(unix)]
        let registry_key = {
            let metadata = std::fs::metadata(directory).map_err(|source| StoreLockError::Io {
                path: directory.to_path_buf(),
                source,
            })?;
            let key = (metadata.dev(), metadata.ino());
            if !held_stores().insert(key) {
                return Err(StoreLockError::Io {
                    path,
                    source: std::io::Error::from(std::io::ErrorKind::WouldBlock),
                });
            }
            key
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| {
                #[cfg(unix)]
                held_stores().remove(&registry_key);
                StoreLockError::Io {
                    path: path.clone(),
                    source,
                }
            })?;
        if let Err(source) = lock_exclusive(&file) {
            drop(file);
            #[cfg(unix)]
            held_stores().remove(&registry_key);
            return Err(StoreLockError::Io { path, source });
        }
        Ok(Self {
            file: Some(file),
            #[cfg(unix)]
            registry_key,
        })
    }
}

#[cfg(unix)]
fn held_stores() -> MutexGuard<'static, BTreeSet<StoreKey>> {
    match HELD_STORES.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    }
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

#[cfg(not(unix))]
fn lock_exclusive(_: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "store locks require Unix fcntl record locks",
    ))
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        drop(self.file.take());
        #[cfg(unix)]
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
