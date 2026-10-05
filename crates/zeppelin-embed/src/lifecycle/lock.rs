//! Store ownership via a process-local registry and an OS file lock.
//!
//! Two mechanisms are needed, because neither alone is sufficient:
//!
//! * The **process-local registry** provides same-process exclusion. POSIX
//!   record locks do not conflict between descriptors held by one process, so
//!   without the registry a second `Store` in the same process would be
//!   admitted. Windows byte-range locks do conflict per handle, even inside
//!   one process, but the registry is still what keeps the two platforms on
//!   one contract: it owns exactly one descriptor per store, counts the local
//!   shared holders, and refuses a conflicting mode before any lock call, so
//!   admission does not depend on which platform is answering.
//! * The **persistent `writer.lock`** provides cross-process exclusion and is
//!   released by the operating system when the holder dies, orderly or not.
//!
//! The registry is keyed on the store directory's stable filesystem identity —
//! `(st_dev, st_ino)` on Unix, `(volume serial, file index)` on Windows — never
//! on path text, so case differences, separator differences, relative versus
//! absolute spellings and supported junctions all converge on one store.
//!
//! Nothing else in this process may open [`STORE_LOCK_FILE`].

#[cfg(all(test, feature = "graph-cypher"))]
mod native_tests;

use std::collections::BTreeMap;
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
type StoreKey = (u64, u64, bool);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LockMode {
    Exclusive,
    Shared,
}

struct RegistryEntry {
    _file: File,
    mode: LockMode,
    shared_count: u32,
}

static HELD_STORES: Mutex<BTreeMap<StoreKey, RegistryEntry>> = Mutex::new(BTreeMap::new());

/// Exclusive writer ownership automatically released when the process dies.
#[derive(Debug)]
pub struct StoreLock {
    registry_key: StoreKey,
    mode: LockMode,
}

impl StoreLock {
    /// Attempts to acquire exclusive non-blocking ownership for one store.
    ///
    /// Registry admission happens first so a second same-process owner is
    /// rejected before any lock file is touched.
    pub fn acquire(directory: &Path) -> Result<Self, StoreLockError> {
        Self::acquire_exclusive(directory, true)
    }

    /// Acquires exclusive ownership only when the persistent lock already exists.
    #[cfg(feature = "graph-cypher")]
    pub(crate) fn acquire_existing(directory: &Path) -> Result<Self, StoreLockError> {
        Self::acquire_exclusive(directory, false)
    }

    fn acquire_exclusive(directory: &Path, create: bool) -> Result<Self, StoreLockError> {
        Self::exclusive_named(directory, create, false)
    }

    pub(crate) fn reclaim_exclusive(directory: &Path) -> Result<Self, StoreLockError> {
        Self::exclusive_named(directory, true, true)
    }

    fn exclusive_named(
        directory: &Path,
        create: bool,
        lease: bool,
    ) -> Result<Self, StoreLockError> {
        let path = directory.join(if lease {
            ".ze-readers.lock"
        } else {
            STORE_LOCK_FILE
        });
        let identity = store_identity(directory).map_err(|source| StoreLockError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let registry_key = (identity.0, identity.1, lease);
        let mut held = held_stores();
        if held.contains_key(&registry_key) {
            return Err(StoreLockError::Io {
                path,
                source: std::io::Error::from(std::io::ErrorKind::WouldBlock),
            });
        }
        let file = open_lock_file(&path, create).map_err(|source| StoreLockError::Io {
            path: path.clone(),
            source,
        })?;
        if let Err(source) = lock_exclusive(&file) {
            drop(file);
            return Err(StoreLockError::Io { path, source });
        }
        held.insert(
            registry_key,
            RegistryEntry {
                _file: file,
                mode: LockMode::Exclusive,
                shared_count: 0,
            },
        );
        Ok(Self {
            registry_key,
            mode: LockMode::Exclusive,
        })
    }

    /// Attempts to acquire non-blocking shared ownership for a graph reader.
    ///
    /// The first reader opens the existing lock file read-only and owns the
    /// process's single descriptor. Further local readers increment the
    /// checked holder count without opening the file or issuing another lock
    /// call. Missing lock files are never created.
    #[cfg(feature = "graph-cypher")]
    #[allow(dead_code)] // Consumed by the ZE-40 native read-only constructor.
    pub(crate) fn acquire_shared(directory: &Path) -> Result<Self, StoreLockError> {
        Self::shared_named(directory, false)
    }

    pub(crate) fn reader_lease(directory: &Path) -> Result<Self, StoreLockError> {
        Self::shared_named(directory, true)
    }

    fn shared_named(directory: &Path, lease: bool) -> Result<Self, StoreLockError> {
        let path = directory.join(if lease {
            ".ze-readers.lock"
        } else {
            STORE_LOCK_FILE
        });
        let identity = store_identity(directory).map_err(|source| StoreLockError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let registry_key = (identity.0, identity.1, lease);
        let mut held = held_stores();
        if let Some(entry) = held.get_mut(&registry_key) {
            if entry.mode == LockMode::Exclusive {
                return Err(StoreLockError::Io {
                    path,
                    source: std::io::Error::from(std::io::ErrorKind::WouldBlock),
                });
            }
            let Some(next_count) = entry.shared_count.checked_add(1) else {
                return Err(StoreLockError::Io {
                    path,
                    source: std::io::Error::other("shared store lock holder count overflow"),
                });
            };
            entry.shared_count = next_count;
            return Ok(Self {
                registry_key,
                mode: LockMode::Shared,
            });
        }

        let file = (if lease {
            open_lock_file(&path, true)
        } else {
            open_shared_lock_file(&path)
        })
        .map_err(|source| StoreLockError::Io {
            path: path.clone(),
            source,
        })?;
        if let Err(source) = lock_shared(&file) {
            drop(file);
            return Err(StoreLockError::Io { path, source });
        }
        held.insert(
            registry_key,
            RegistryEntry {
                _file: file,
                mode: LockMode::Shared,
                shared_count: 1,
            },
        );
        Ok(Self {
            registry_key,
            mode: LockMode::Shared,
        })
    }
}

fn held_stores() -> MutexGuard<'static, BTreeMap<StoreKey, RegistryEntry>> {
    match HELD_STORES.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Opens an already-existing lock file without write or creation authority.
fn open_shared_lock_file(path: &Path) -> std::io::Result<File> {
    let file = open_shared_lock_handle(path)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            "store lock path is not a regular file",
        ))
    }
}

/// Read-only handle on the existing lock file; never creates and never writes.
#[cfg(not(windows))]
fn open_shared_lock_handle(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

/// Read-only handle that additionally refuses to share deletion.
///
/// This is the same reasoning `open_lock_file` already documents, applied to
/// readers. Rust's default share mode includes `FILE_SHARE_DELETE`, and a
/// reader-held lock file that anyone may rename or unlink is no protection at
/// all: a writer would then create a fresh `writer.lock`, take an uncontended
/// exclusive range on it, and run beside live readers. Sharing read and write
/// but not deletion keeps the file in place for as long as any reader holds
/// it, while still admitting the further readers and the eventual writer that
/// the lock range itself arbitrates. No write access is requested, because
/// `LockFileEx` needs only `GENERIC_READ`.
#[cfg(windows)]
fn open_shared_lock_handle(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .read(true)
        .share_mode(crate::sys::windows::FILE_SHARE_READ | crate::sys::windows::FILE_SHARE_WRITE)
        .open(path)
}

/// The store directory's stable filesystem identity.
#[cfg(unix)]
pub(super) fn store_identity(directory: &Path) -> std::io::Result<(u64, u64)> {
    let metadata = std::fs::metadata(directory)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
pub(super) fn store_identity(directory: &Path) -> std::io::Result<(u64, u64)> {
    crate::sys::windows::directory_identity(directory)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn store_identity(_directory: &Path) -> std::io::Result<(u64, u64)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "store identity requires a supported platform",
    ))
}

/// Opens the persistent lock file with the access the platform lock needs.
#[cfg(not(windows))]
fn open_lock_file(path: &Path, create: bool) -> std::io::Result<File> {
    OpenOptions::new()
        .create(create)
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
fn open_lock_file(path: &Path, create: bool) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .create(create)
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

#[cfg(unix)]
#[allow(dead_code)] // Called by the ZE-40 producer seam above.
fn lock_shared(file: &File) -> std::io::Result<()> {
    let mut lock = unsafe {
        // SAFETY: all-zero is a valid `flock` value before named fields are set.
        std::mem::zeroed::<libc::flock>()
    };
    lock.l_type = libc::F_RDLCK as libc::c_short;
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

/// Takes the cross-process lock with `LockFileEx`, non-blocking and shared,
/// over the same fixed range the exclusive arm uses.
///
/// Contention with a live writer arrives as [`std::io::ErrorKind::WouldBlock`];
/// a permission failure stays a permission failure and is never reported as
/// contention.
#[cfg(windows)]
#[allow(dead_code)] // Called by the ZE-40 producer seam above.
fn lock_shared(file: &File) -> std::io::Result<()> {
    crate::sys::windows::lock_file_shared(file)
}

#[cfg(not(any(unix, windows)))]
#[allow(dead_code)] // Called by the ZE-40 producer seam above.
fn lock_shared(_: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "shared store locks require a supported file-locking platform",
    ))
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
        let mut held = held_stores();
        let remove = match held.get_mut(&self.registry_key) {
            Some(entry) if entry.mode == LockMode::Shared && self.mode == LockMode::Shared => {
                if entry.shared_count > 1 {
                    entry.shared_count -= 1;
                    false
                } else {
                    true
                }
            }
            Some(entry)
                if entry.mode == LockMode::Exclusive && self.mode == LockMode::Exclusive =>
            {
                true
            }
            _ => false,
        };
        if remove {
            // POSIX closes of any descriptor for the file release this
            // process's record locks. Remove and close the registry-owned sole
            // descriptor while local admission remains excluded.
            drop(held.remove(&self.registry_key));
        }
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
