//! Hand-written Win32 declarations and owned wrappers.
//!
//! Every signature, constant and struct layout below was read out of the
//! installed SDK (`Windows Kits\10\Include\10.0.19041.0`) rather than recalled;
//! `tests/sdk_constants.rs` re-asserts the numeric values so a wrong constant
//! fails a test instead of corrupting a store.

#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

pub type BOOL = i32;
pub type DWORD = u32;
pub type HANDLE = *mut core::ffi::c_void;
pub type LPCWSTR = *const u16;

/// `winnt.h`: `((HANDLE)(LONG_PTR)-1)`.
pub const INVALID_HANDLE_VALUE: HANDLE = -1_isize as HANDLE;

// winnt.h
pub const GENERIC_READ: DWORD = 0x8000_0000;
pub const GENERIC_WRITE: DWORD = 0x4000_0000;
pub const FILE_SHARE_READ: DWORD = 0x0000_0001;
pub const FILE_SHARE_WRITE: DWORD = 0x0000_0002;
pub const FILE_SHARE_DELETE: DWORD = 0x0000_0004;
pub const FILE_ATTRIBUTE_NORMAL: DWORD = 0x0000_0080;
pub const FILE_FLAG_BACKUP_SEMANTICS: DWORD = 0x0200_0000;
pub const FILE_FLAG_WRITE_THROUGH: DWORD = 0x8000_0000;
pub const PAGE_READONLY: DWORD = 0x02;
pub const SECTION_MAP_READ: DWORD = 0x0004;
pub const FILE_MAP_READ: DWORD = SECTION_MAP_READ;

// fileapi.h
pub const CREATE_NEW: DWORD = 1;
pub const CREATE_ALWAYS: DWORD = 2;
pub const OPEN_EXISTING: DWORD = 3;
pub const OPEN_ALWAYS: DWORD = 4;
pub const TRUNCATE_EXISTING: DWORD = 5;

// winbase.h
pub const MOVEFILE_REPLACE_EXISTING: DWORD = 0x0000_0001;
pub const MOVEFILE_COPY_ALLOWED: DWORD = 0x0000_0002;
pub const MOVEFILE_WRITE_THROUGH: DWORD = 0x0000_0008;

// winerror.h
pub const ERROR_INVALID_FUNCTION: DWORD = 1;
pub const ERROR_FILE_NOT_FOUND: DWORD = 2;
pub const ERROR_PATH_NOT_FOUND: DWORD = 3;
pub const ERROR_ACCESS_DENIED: DWORD = 5;
pub const ERROR_NOT_SAME_DEVICE: DWORD = 17;
pub const ERROR_SHARING_VIOLATION: DWORD = 32;
pub const ERROR_LOCK_VIOLATION: DWORD = 33;
pub const ERROR_FILE_EXISTS: DWORD = 80;
pub const ERROR_INVALID_PARAMETER: DWORD = 87;
pub const ERROR_DISK_FULL: DWORD = 112;
pub const ERROR_INVALID_NAME: DWORD = 123;
pub const ERROR_ALREADY_EXISTS: DWORD = 183;
pub const ERROR_USER_MAPPED_FILE: DWORD = 1224;

/// `fileapi.h`: field order and widths copied from the SDK declaration.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BY_HANDLE_FILE_INFORMATION {
    pub dwFileAttributes: DWORD,
    pub ftCreationTime: [DWORD; 2],
    pub ftLastAccessTime: [DWORD; 2],
    pub ftLastWriteTime: [DWORD; 2],
    pub dwVolumeSerialNumber: DWORD,
    pub nFileSizeHigh: DWORD,
    pub nFileSizeLow: DWORD,
    pub nNumberOfLinks: DWORD,
    pub nFileIndexHigh: DWORD,
    pub nFileIndexLow: DWORD,
}

unsafe extern "system" {
    pub fn CreateFileW(
        lpFileName: LPCWSTR,
        dwDesiredAccess: DWORD,
        dwShareMode: DWORD,
        lpSecurityAttributes: *mut core::ffi::c_void,
        dwCreationDisposition: DWORD,
        dwFlagsAndAttributes: DWORD,
        hTemplateFile: HANDLE,
    ) -> HANDLE;
    pub fn WriteFile(
        hFile: HANDLE,
        lpBuffer: *const u8,
        nNumberOfBytesToWrite: DWORD,
        lpNumberOfBytesWritten: *mut DWORD,
        lpOverlapped: *mut core::ffi::c_void,
    ) -> BOOL;
    pub fn FlushFileBuffers(hFile: HANDLE) -> BOOL;
    pub fn CloseHandle(hObject: HANDLE) -> BOOL;
    pub fn MoveFileExW(
        lpExistingFileName: LPCWSTR,
        lpNewFileName: LPCWSTR,
        dwFlags: DWORD,
    ) -> BOOL;
    pub fn DeleteFileW(lpFileName: LPCWSTR) -> BOOL;
    pub fn GetFileInformationByHandle(
        hFile: HANDLE,
        lpFileInformation: *mut BY_HANDLE_FILE_INFORMATION,
    ) -> BOOL;
    pub fn GetFileSizeEx(hFile: HANDLE, lpFileSize: *mut i64) -> BOOL;
    pub fn CreateFileMappingW(
        hFile: HANDLE,
        lpFileMappingAttributes: *mut core::ffi::c_void,
        flProtect: DWORD,
        dwMaximumSizeHigh: DWORD,
        dwMaximumSizeLow: DWORD,
        lpName: LPCWSTR,
    ) -> HANDLE;
    pub fn MapViewOfFile(
        hFileMappingObject: HANDLE,
        dwDesiredAccess: DWORD,
        dwFileOffsetHigh: DWORD,
        dwFileOffsetLow: DWORD,
        dwNumberOfBytesToMap: usize,
    ) -> *mut core::ffi::c_void;
    pub fn UnmapViewOfFile(lpBaseAddress: *const core::ffi::c_void) -> BOOL;
    pub fn GetDiskFreeSpaceExW(
        lpDirectoryName: LPCWSTR,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> BOOL;
}

/// Encodes an OS path as a NUL-terminated UTF-16 string without lossy
/// conversion, rejecting an interior NUL rather than silently truncating.
pub fn wide(path: &Path) -> io::Result<Vec<u16>> {
    wide_os(path.as_os_str())
}

/// Encodes any `OsStr` the same way.
pub fn wide_os(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut encoded: Vec<u16> = value.encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }
    encoded.push(0);
    Ok(encoded)
}

/// The last Win32 error as a raw code, for tests that assert an exact outcome.
#[must_use]
pub fn last_error_code() -> DWORD {
    // `io::Error::last_os_error` reads the same thread-local `GetLastError`.
    let raw = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    DWORD::try_from(raw).unwrap_or(0)
}

/// An owned kernel handle closed exactly once.
#[derive(Debug)]
pub struct Handle(HANDLE);

// SAFETY: a Win32 file handle is not thread-affine; ownership is exclusive.
unsafe impl Send for Handle {}
// SAFETY: every shared method below only performs kernel calls that are
// documented as safe to issue concurrently on one handle.
unsafe impl Sync for Handle {}

impl Handle {
    /// Adopts a raw handle, rejecting the two documented failure values.
    pub fn adopt(raw: HANDLE) -> io::Result<Self> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(raw))
    }

    /// Borrows the raw handle for one call; it stays owned by `self`.
    #[must_use]
    pub const fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` was validated on construction and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Opens or creates a file with fully explicit access, sharing and disposition.
pub fn create_file(
    path: &Path,
    access: DWORD,
    share: DWORD,
    disposition: DWORD,
    flags: DWORD,
) -> io::Result<Handle> {
    let encoded = wide(path)?;
    // SAFETY: `encoded` is NUL-terminated and outlives the call; a null
    // security-attributes pointer and null template handle are documented
    // as "use the defaults".
    let raw = unsafe {
        CreateFileW(
            encoded.as_ptr(),
            access,
            share,
            core::ptr::null_mut(),
            disposition,
            flags,
            core::ptr::null_mut(),
        )
    };
    Handle::adopt(raw)
}

/// Opens a directory handle. `FILE_FLAG_BACKUP_SEMANTICS` is mandatory: without
/// it `CreateFileW` refuses every directory.
pub fn open_directory(path: &Path, access: DWORD) -> io::Result<Handle> {
    create_file(
        path,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS,
    )
}

/// Writes every byte, looping over short writes and rejecting a zero-progress
/// write instead of reporting a silent partial success.
pub fn write_all(handle: &Handle, bytes: &[u8]) -> io::Result<()> {
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let remaining = bytes.get(offset..).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "write offset past buffer")
        })?;
        let chunk = DWORD::try_from(remaining.len()).unwrap_or(DWORD::MAX);
        let mut written: DWORD = 0;
        // SAFETY: `remaining` is valid for `chunk` bytes and `written` points
        // to one initialized `DWORD` the kernel overwrites on success.
        let ok = unsafe {
            WriteFile(
                handle.raw(),
                remaining.as_ptr(),
                chunk,
                &raw mut written,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "WriteFile reported success with zero bytes written",
            ));
        }
        offset = offset.saturating_add(written as usize);
    }
    Ok(())
}

/// Flushes one already-open handle. The caller must have opened it with write
/// access; a read-only handle is refused by the kernel, not by this wrapper.
pub fn flush(handle: &Handle) -> io::Result<()> {
    // SAFETY: `handle` owns a live handle for the duration of the call.
    if unsafe { FlushFileBuffers(handle.raw()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Renames or replaces one path with fully explicit flags.
pub fn move_file_ex(from: &Path, to: &Path, flags: DWORD) -> io::Result<()> {
    let source = wide(from)?;
    let target = wide(to)?;
    // SAFETY: both encodings are NUL-terminated and outlive the call.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Unlinks one path.
pub fn delete_file(path: &Path) -> io::Result<()> {
    let encoded = wide(path)?;
    // SAFETY: `encoded` is NUL-terminated and outlives the call.
    if unsafe { DeleteFileW(encoded.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Stable identity of whatever a handle names: `(volume serial, file index)`.
pub fn file_identity(handle: &Handle) -> io::Result<(u32, u64)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `info` is one writable, fully initialized struct of the exact
    // layout declared by the SDK.
    if unsafe { GetFileInformationByHandle(handle.raw(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((info.dwVolumeSerialNumber, index))
}

/// Byte length of the file behind a handle.
pub fn file_size(handle: &Handle) -> io::Result<u64> {
    let mut size: i64 = 0;
    // SAFETY: `size` is one writable `LARGE_INTEGER`.
    if unsafe { GetFileSizeEx(handle.raw(), &raw mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    u64::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative file size"))
}

/// Bytes available to this caller on the volume holding `path`.
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    let encoded = wide(path)?;
    let mut available: u64 = 0;
    // SAFETY: `encoded` is NUL-terminated; the two optional out-pointers are
    // documented as skippable when null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            encoded.as_ptr(),
            &raw mut available,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
}

/// A read-only view of a file, owning its view, section and file handles so
/// they are released in that exact order.
#[derive(Debug)]
pub struct ReadOnlyMap {
    address: *const core::ffi::c_void,
    length: usize,
    section: Option<Handle>,
    file: Option<Handle>,
}

// SAFETY: the mapping is immutable for its whole lifetime.
unsafe impl Send for ReadOnlyMap {}
// SAFETY: every shared accessor yields a read-only slice.
unsafe impl Sync for ReadOnlyMap {}

impl ReadOnlyMap {
    /// Maps an entire non-empty file read-only from offset zero.
    ///
    /// `share` is explicit so a caller can prove what a concurrent replacement
    /// or deletion is allowed to do while this view is alive.
    pub fn open(path: &Path, share: DWORD) -> io::Result<Self> {
        let file = create_file(
            path,
            GENERIC_READ,
            share,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
        )?;
        let length_u64 = file_size(&file)?;
        if length_u64 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "refusing to map an empty file",
            ));
        }
        let length = usize::try_from(length_u64).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "file length exceeds usize")
        })?;
        // SAFETY: `file` is a live read handle; a zero maximum size means "the
        // whole current file", and a null name creates an unnamed section.
        let section_raw = unsafe {
            CreateFileMappingW(
                file.raw(),
                core::ptr::null_mut(),
                PAGE_READONLY,
                0,
                0,
                core::ptr::null(),
            )
        };
        // `CreateFileMappingW` reports failure as NULL, not INVALID_HANDLE_VALUE.
        let section = Handle::adopt(section_raw)?;
        // SAFETY: the section is live and `length` is the validated file length
        // starting at offset zero, so no allocation-granularity adjustment applies.
        let address = unsafe { MapViewOfFile(section.raw(), FILE_MAP_READ, 0, 0, length) };
        if address.is_null() {
            // `section` and `file` drop here, releasing both handles.
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            address: address.cast_const(),
            length,
            section: Some(section),
            file: Some(file),
        })
    }

    /// The mapped bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: the view is valid for `length` bytes and outlives the borrow.
        unsafe { core::slice::from_raw_parts(self.address.cast::<u8>(), self.length) }
    }
}

impl Drop for ReadOnlyMap {
    fn drop(&mut self) {
        // The view must be unmapped before its section handle closes.
        // SAFETY: `address` is the exact base returned by `MapViewOfFile`.
        unsafe {
            UnmapViewOfFile(self.address);
        }
        drop(self.section.take());
        drop(self.file.take());
    }
}
