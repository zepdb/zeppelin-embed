//! Read-only private mapping for native graph object files.

use super::TreeError;
use crate::lifecycle::native_graph::{NativeMappingOwnership, NativeReadLease};
use crate::property_graph::storage::artifact::MAX_ARTIFACT_BYTES;
use std::fs::File;
use std::path::Path;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::ptr::NonNull;

#[cfg(unix)]
pub(crate) struct NativeReadonlyMapping {
    _ownership: NativeMappingOwnership,
    inner: UnixReadonlyMapping,
}

#[cfg(unix)]
struct UnixReadonlyMapping {
    _file: File,
    pointer: NonNull<u8>,
    length: usize,
}

#[cfg(unix)]
impl NativeReadonlyMapping {
    pub(super) fn open(
        file: File,
        path: &Path,
        lease: &NativeReadLease,
    ) -> Result<Self, TreeError> {
        let inner = UnixReadonlyMapping::open(file, path, MAX_ARTIFACT_BYTES)?;
        let ownership = lease
            .track_mapping(inner.as_bytes())
            .map_err(|_| TreeError::Memory)?;
        Ok(Self {
            _ownership: ownership,
            inner,
        })
    }

    pub(crate) fn open_recovery(
        file: File,
        path: &Path,
        publication: &std::sync::Arc<crate::lifecycle::native_graph::NativeGraphPublication>,
        maximum: usize,
    ) -> Result<Self, crate::lifecycle::native_graph::NativeGraphError> {
        let inner = UnixReadonlyMapping::open(file, path, maximum)?;
        let ownership = publication.register_mapping(inner.as_bytes())?;
        Ok(Self {
            _ownership: ownership,
            inner,
        })
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }
}

#[cfg(unix)]
impl UnixReadonlyMapping {
    fn open(file: File, path: &Path, maximum: usize) -> Result<Self, TreeError> {
        let metadata = file.metadata().map_err(TreeError::Io)?;
        if !metadata.file_type().is_file() {
            return Err(TreeError::Invalid(
                "native graph artifact is not a regular file",
            ));
        }
        let length = usize::try_from(metadata.len())
            .map_err(|_| TreeError::Invalid("native graph artifact length exceeds usize"))?;
        if length == 0 || length > maximum {
            return Err(TreeError::Invalid(
                "native graph artifact length is outside bounds",
            ));
        }
        // SAFETY: `file` is a live regular-file descriptor and `length` is its
        // checked nonzero extent. The private mapping is kernel read-only, its
        // sole owner retains the descriptor, and Drop unmaps this exact pair.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(TreeError::Io(std::io::Error::last_os_error()));
        }
        let pointer = NonNull::new(mapped.cast::<u8>()).ok_or(TreeError::Invalid(
            "mmap returned a null non-failure pointer",
        ))?;
        let _ = path;
        Ok(UnixReadonlyMapping {
            _file: file,
            pointer,
            length,
        })
    }
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: the read-only mapping remains valid for `length` bytes for
        // the entire returned borrow through `&self`.
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }
}

#[cfg(unix)]
impl Drop for UnixReadonlyMapping {
    fn drop(&mut self) {
        // SAFETY: this is the exact successful mmap pointer/length pair and the
        // sole mapping owner drops once.
        let _ = unsafe { libc::munmap(self.pointer.as_ptr().cast(), self.length) };
    }
}

#[cfg(windows)]
pub(crate) struct NativeReadonlyMapping {
    _ownership: NativeMappingOwnership,
    inner: crate::sys::windows::FileMapping,
}

#[cfg(windows)]
impl NativeReadonlyMapping {
    pub(super) fn open(
        file: File,
        _path: &Path,
        lease: &NativeReadLease,
    ) -> Result<Self, TreeError> {
        let inner = windows_mapping(file, MAX_ARTIFACT_BYTES)?;
        let ownership = lease
            .track_mapping(inner.as_bytes())
            .map_err(|_| TreeError::Memory)?;
        Ok(Self {
            _ownership: ownership,
            inner,
        })
    }

    pub(crate) fn open_recovery(
        file: File,
        _path: &Path,
        publication: &std::sync::Arc<crate::lifecycle::native_graph::NativeGraphPublication>,
        maximum: usize,
    ) -> Result<Self, crate::lifecycle::native_graph::NativeGraphError> {
        let inner = windows_mapping(file, maximum)?;
        let ownership = publication.register_mapping(inner.as_bytes())?;
        Ok(Self {
            _ownership: ownership,
            inner,
        })
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }
}

#[cfg(windows)]
fn windows_mapping(
    file: File,
    maximum: usize,
) -> Result<crate::sys::windows::FileMapping, TreeError> {
    let metadata = file.metadata().map_err(TreeError::Io)?;
    if !metadata.file_type().is_file() {
        return Err(TreeError::Invalid(
            "native graph artifact is not a regular file",
        ));
    }
    let length = usize::try_from(metadata.len())
        .map_err(|_| TreeError::Invalid("native graph artifact length exceeds usize"))?;
    if length == 0 || length > maximum {
        return Err(TreeError::Invalid(
            "native graph artifact length is outside bounds",
        ));
    }
    crate::sys::windows::FileMapping::from_file(file, metadata.len()).map_err(TreeError::Io)
}
