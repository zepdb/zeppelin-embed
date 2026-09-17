//! Windows platform boundary: Win32 declarations, owned handles and truthful
//! virtual-memory probes.
//!
//! Every signature, constant and struct layout here was read out of the
//! installed Windows SDK (`Windows Kits\10\Include`), not recalled, and the
//! numeric values are re-asserted by `constants_match_the_installed_sdk` below
//! so a wrong constant fails a test rather than corrupting a store.
//!
//! The declarations resolve against `kernel32`, which the Rust standard library
//! already links: the SDK itself defines `QueryWorkingSetEx` and
//! `GetProcessMemoryInfo` as aliases of the `K32`-prefixed kernel32 exports, so
//! binding those names directly keeps `psapi` off the native link line.
//!
//! No raw handle escapes this module.

// The Win32 type, struct and field names are kept character-for-character
// identical to the SDK so a reader can diff them against the headers. That is
// the whole point of a hand-written FFI boundary, so the naming lints are
// disabled for this module only.
#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

pub(crate) type BOOL = i32;
pub(crate) type DWORD = u32;
pub(crate) type HANDLE = *mut c_void;

/// `winnt.h`: `((HANDLE)(LONG_PTR)-1)`.
pub(crate) const INVALID_HANDLE_VALUE: HANDLE = -1_isize as HANDLE;

// winnt.h
pub(crate) const GENERIC_WRITE: DWORD = 0x4000_0000;
pub(crate) const FILE_SHARE_READ: DWORD = 0x0000_0001;
pub(crate) const FILE_SHARE_WRITE: DWORD = 0x0000_0002;
pub(crate) const FILE_SHARE_DELETE: DWORD = 0x0000_0004;
pub(crate) const FILE_ATTRIBUTE_NORMAL: DWORD = 0x0000_0080;
pub(crate) const FILE_FLAG_BACKUP_SEMANTICS: DWORD = 0x0200_0000;
#[cfg(any(test, feature = "test-support"))]
pub(crate) const PAGE_READONLY: DWORD = 0x02;
pub(crate) const PAGE_WRITECOPY: DWORD = 0x08;
pub(crate) const SECTION_MAP_READ: DWORD = 0x0004;
pub(crate) const FILE_MAP_READ: DWORD = SECTION_MAP_READ;

// fileapi.h
pub(crate) const OPEN_EXISTING: DWORD = 3;

// minwinbase.h
pub(crate) const LOCKFILE_FAIL_IMMEDIATELY: DWORD = 0x0000_0001;
pub(crate) const LOCKFILE_EXCLUSIVE_LOCK: DWORD = 0x0000_0002;

// winerror.h
#[cfg(test)]
pub(crate) const ERROR_ACCESS_DENIED: DWORD = 5;
pub(crate) const ERROR_LOCK_VIOLATION: DWORD = 33;
pub(crate) const ERROR_IO_PENDING: DWORD = 997;

/// `minwinbase.h`: `OVERLAPPED`. The union is expressed as its `Offset`/
/// `OffsetHigh` arm, which is the arm `LockFileEx` reads.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct OVERLAPPED {
    Internal: usize,
    InternalHigh: usize,
    Offset: DWORD,
    OffsetHigh: DWORD,
    hEvent: HANDLE,
}

/// `fileapi.h`: `BY_HANDLE_FILE_INFORMATION`, field for field.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct BY_HANDLE_FILE_INFORMATION {
    dwFileAttributes: DWORD,
    ftCreationTime: [DWORD; 2],
    ftLastAccessTime: [DWORD; 2],
    ftLastWriteTime: [DWORD; 2],
    dwVolumeSerialNumber: DWORD,
    nFileSizeHigh: DWORD,
    nFileSizeLow: DWORD,
    nNumberOfLinks: DWORD,
    nFileIndexHigh: DWORD,
    nFileIndexLow: DWORD,
}

/// `psapi.h`: `{ PVOID VirtualAddress; PSAPI_WORKING_SET_EX_BLOCK VirtualAttributes; }`.
/// The block is a `ULONG_PTR` union whose bit 0 is `Valid`, so a plain
/// pointer-width integer reproduces the layout exactly.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct PSAPI_WORKING_SET_EX_INFORMATION {
    VirtualAddress: *mut c_void,
    VirtualAttributes: usize,
}

/// Bit 0 of `PSAPI_WORKING_SET_EX_BLOCK`: the page is resident.
const WORKING_SET_EX_VALID: usize = 1;

/// `psapi.h`: `PROCESS_MEMORY_COUNTERS_EX`, field for field.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct PROCESS_MEMORY_COUNTERS_EX {
    cb: DWORD,
    PageFaultCount: DWORD,
    PeakWorkingSetSize: usize,
    WorkingSetSize: usize,
    QuotaPeakPagedPoolUsage: usize,
    QuotaPagedPoolUsage: usize,
    QuotaPeakNonPagedPoolUsage: usize,
    QuotaNonPagedPoolUsage: usize,
    PagefileUsage: usize,
    PeakPagefileUsage: usize,
    PrivateUsage: usize,
}

/// `sysinfoapi.h`: `SYSTEM_INFO`, truncated to the fields this module reads.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SYSTEM_INFO {
    wProcessorArchitecture: u16,
    wReserved: u16,
    dwPageSize: DWORD,
    lpMinimumApplicationAddress: *mut c_void,
    lpMaximumApplicationAddress: *mut c_void,
    dwActiveProcessorMask: usize,
    dwNumberOfProcessors: DWORD,
    dwProcessorType: DWORD,
    dwAllocationGranularity: DWORD,
    wProcessorLevel: u16,
    wProcessorRevision: u16,
}

unsafe extern "system" {
    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: DWORD,
        dwShareMode: DWORD,
        lpSecurityAttributes: *mut c_void,
        dwCreationDisposition: DWORD,
        dwFlagsAndAttributes: DWORD,
        hTemplateFile: HANDLE,
    ) -> HANDLE;
    fn FlushFileBuffers(hFile: HANDLE) -> BOOL;
    fn CloseHandle(hObject: HANDLE) -> BOOL;
    fn CreateFileMappingW(
        hFile: HANDLE,
        lpFileMappingAttributes: *mut c_void,
        flProtect: DWORD,
        dwMaximumSizeHigh: DWORD,
        dwMaximumSizeLow: DWORD,
        lpName: *const u16,
    ) -> HANDLE;
    fn MapViewOfFile(
        hFileMappingObject: HANDLE,
        dwDesiredAccess: DWORD,
        dwFileOffsetHigh: DWORD,
        dwFileOffsetLow: DWORD,
        dwNumberOfBytesToMap: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(lpBaseAddress: *const c_void) -> BOOL;
    fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> BOOL;
    fn GetCurrentProcess() -> HANDLE;
    fn GetFileInformationByHandle(
        hFile: HANDLE,
        lpFileInformation: *mut BY_HANDLE_FILE_INFORMATION,
    ) -> BOOL;
    fn LockFileEx(
        hFile: HANDLE,
        dwFlags: DWORD,
        dwReserved: DWORD,
        nNumberOfBytesToLockLow: DWORD,
        nNumberOfBytesToLockHigh: DWORD,
        lpOverlapped: *mut OVERLAPPED,
    ) -> BOOL;
    fn GetSystemInfo(lpSystemInfo: *mut SYSTEM_INFO);
    fn K32QueryWorkingSetEx(hProcess: HANDLE, pv: *mut c_void, cb: DWORD) -> BOOL;
    fn K32GetProcessMemoryInfo(
        hProcess: HANDLE,
        ppsmemCounters: *mut PROCESS_MEMORY_COUNTERS_EX,
        cb: DWORD,
    ) -> BOOL;
    #[cfg(any(test, feature = "test-support"))]
    fn VirtualProtect(
        lpAddress: *mut c_void,
        dwSize: usize,
        flNewProtect: DWORD,
        lpflOldProtect: *mut DWORD,
    ) -> BOOL;
}

/// Encodes an OS path as NUL-terminated UTF-16 without lossy conversion,
/// rejecting an interior NUL instead of silently truncating the path.
pub(crate) fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "store path contains an interior NUL",
        ));
    }
    encoded.push(0);
    Ok(encoded)
}

/// An owned kernel handle, closed exactly once.
#[derive(Debug)]
pub(crate) struct OwnedHandle(HANDLE);

// SAFETY: a Win32 file or section handle is not thread-affine and ownership
// here is exclusive.
unsafe impl Send for OwnedHandle {}
// SAFETY: every shared operation this module performs on a handle is a kernel
// call documented as safe to issue concurrently.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    /// Adopts a raw handle, rejecting both documented failure values.
    ///
    /// `CreateFileW` reports failure as `INVALID_HANDLE_VALUE` while
    /// `CreateFileMappingW` reports it as null, so both are refused.
    fn adopt(raw: HANDLE) -> io::Result<Self> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(raw))
    }

    const fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` was validated on construction and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Opens a file or directory with fully explicit access, sharing and flags.
fn open(
    path: &Path,
    access: DWORD,
    share: DWORD,
    disposition: DWORD,
    flags: DWORD,
) -> io::Result<OwnedHandle> {
    let encoded = wide(path)?;
    // SAFETY: `encoded` is NUL-terminated and outlives the call; a null
    // security-attributes pointer and null template handle select the defaults.
    let raw = unsafe {
        CreateFileW(
            encoded.as_ptr(),
            access,
            share,
            std::ptr::null_mut(),
            disposition,
            flags,
            std::ptr::null_mut(),
        )
    };
    OwnedHandle::adopt(raw)
}

/// Flushes a file's data through a handle that carries write access.
///
/// `FlushFileBuffers` requires write access: a `GENERIC_READ` handle returns
/// `ERROR_ACCESS_DENIED`. Callers must therefore never reopen a path read-only
/// and call this, which is why [`sync_path`] opens for writing itself.
fn flush(handle: &OwnedHandle) -> io::Result<()> {
    // SAFETY: `handle` owns a live handle for the duration of the call.
    if unsafe { FlushFileBuffers(handle.raw()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Flushes an already-open file through the handle the caller still owns.
///
/// This is the handle-oriented path used by the WAL: the same file object that
/// received the appends is the one flushed, with no reopen in between, so a
/// concurrent replacement of the path cannot redirect the flush to a different
/// file.
///
/// The handle must carry write access. `OpenOptions::append` yields
/// `FILE_APPEND_DATA` rather than `GENERIC_WRITE`; that this is sufficient for
/// `FlushFileBuffers` is asserted by `an_append_handle_can_be_flushed` rather
/// than assumed.
pub(crate) fn flush_file(file: &std::fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;

    // SAFETY: `file` owns a live handle for the duration of the call.
    if unsafe { FlushFileBuffers(file.as_raw_handle().cast()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Makes a path's contents durable, opening it with the access the kernel
/// requires and adding backup semantics when the path is a directory.
///
/// This is the Windows counterpart of `fsync`, and for a directory it is the
/// counterpart of `fsync` on a directory descriptor: it is what makes a
/// published or retired name itself survive. Measured on local NTFS as a
/// standard user, a writable directory handle is obtainable without elevation
/// and its flush succeeds, while read-only and metadata-only directory handles
/// return `ERROR_ACCESS_DENIED`.
///
/// Windows exposes no ordering-only barrier, so a `Barrier` request is served
/// by the same full flush as `Full`. That is stronger and more expensive than a
/// barrier, never cheaper, and it is documented as such.
pub(crate) fn sync_path(path: &Path) -> io::Result<()> {
    let is_directory = std::fs::metadata(path)?.is_dir();
    let flags = if is_directory {
        // A directory handle cannot be obtained without backup semantics.
        FILE_FLAG_BACKUP_SEMANTICS
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let handle = open(
        path,
        GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        flags,
    )?;
    flush(&handle)
}

/// Stable identity of a store directory: `(volume serial, file index)`.
///
/// This is the Windows counterpart of `(st_dev, st_ino)` and is deliberately
/// **not** derived from the path text. Case differences, `/` versus `\`,
/// relative versus absolute spellings, short 8.3 names and supported junctions
/// all resolve to the same directory object and therefore to the same identity,
/// so the process-local writer registry cannot be defeated by an alias.
pub(crate) fn directory_identity(path: &Path) -> io::Result<(u64, u64)> {
    // A directory handle needs backup semantics; metadata-only access is
    // enough to read its identity, and it shares freely so this probe never
    // interferes with a concurrent writer.
    let handle = open(
        path,
        0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS,
    )?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `info` is one writable struct of the exact declared layout.
    if unsafe { GetFileInformationByHandle(handle.raw(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}

/// Offset of the byte the writer lock covers.
///
/// Deliberately far beyond the end of a zero-length lock file. Windows
/// byte-range locks are **mandatory**, not advisory like POSIX record locks: a
/// locked range cannot be read by anyone, including another handle in the same
/// process. Locking byte zero would therefore make `writer.lock` unreadable
/// while a writer is open, which is a behaviour difference from Unix that
/// nothing in the engine's contract asks for — a directory sweep that reads
/// every file in the store would fail with `ERROR_LOCK_VIOLATION`.
///
/// Locking a range past end-of-file is explicitly permitted and gives exactly
/// the exclusion that is wanted: every `StoreLock` contends for this one byte,
/// and no ordinary read of the file touches it.
pub(crate) const WRITER_LOCK_OFFSET: u64 = 0x4000_0000;

/// Length of the writer-lock range. Fixed and documented so that every
/// participant locks the same range; the lock file's contents are irrelevant,
/// only the byte-range lock on it matters.
pub(crate) const WRITER_LOCK_RANGE: u64 = 1;

/// Takes an exclusive, non-blocking byte-range lock on an open lock file.
///
/// Contention is reported as [`io::ErrorKind::WouldBlock`] so the caller can
/// map it to the typed busy error. Only `ERROR_LOCK_VIOLATION` — the code
/// `LOCKFILE_FAIL_IMMEDIATELY` produces when another owner holds the range —
/// is treated as contention. Every other failure, `ERROR_ACCESS_DENIED`
/// included, is surfaced unchanged: a permission problem must never be
/// reported as "another writer holds this store".
///
/// The lock is released when the last handle to the file closes, which the OS
/// guarantees on process death as well as on an orderly drop.
pub(crate) fn lock_file_exclusive(file: &std::fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;

    let mut overlapped = OVERLAPPED {
        Internal: 0,
        InternalHigh: 0,
        // `OVERLAPPED` carries the lock's starting offset as a 64-bit value
        // split across two `DWORD`s.
        Offset: (WRITER_LOCK_OFFSET & 0xFFFF_FFFF) as DWORD,
        OffsetHigh: (WRITER_LOCK_OFFSET >> 32) as DWORD,
        hEvent: std::ptr::null_mut(),
    };
    let low = DWORD::try_from(WRITER_LOCK_RANGE)
        .map_err(|_| io::Error::other("writer lock range exceeds DWORD"))?;
    // SAFETY: `file` owns a live handle and `overlapped` is a writable,
    // fully initialized `OVERLAPPED` that outlives this synchronous call.
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle().cast(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            low,
            0,
            &raw mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    let code = error
        .raw_os_error()
        .and_then(|raw| DWORD::try_from(raw).ok());
    match code {
        Some(ERROR_LOCK_VIOLATION) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        // `LOCKFILE_FAIL_IMMEDIATELY` is documented never to return this, so
        // seeing it means the flags were wrong; fail loudly rather than block.
        Some(ERROR_IO_PENDING) => Err(io::Error::other(
            "LockFileEx returned ERROR_IO_PENDING despite LOCKFILE_FAIL_IMMEDIATELY",
        )),
        _ => Err(error),
    }
}

/// Bytes available to this caller on the volume that actually holds `path`.
pub(crate) fn available_disk_bytes(path: &Path) -> io::Result<u64> {
    let encoded = wide(path)?;
    let mut available: u64 = 0;
    // SAFETY: `encoded` is NUL-terminated; the trailing out-parameters are
    // documented as optional and are skipped with null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            encoded.as_ptr(),
            &raw mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
}

/// The system page size.
fn page_size() -> io::Result<usize> {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: `info` is one writable, fully initialized `SYSTEM_INFO`.
    unsafe {
        GetSystemInfo(&raw mut info);
    }
    let size = usize::try_from(info.dwPageSize)
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(|| io::Error::other("GetSystemInfo reported an invalid page size"))?;
    Ok(size)
}

/// Counts bytes of `range` whose intersecting pages are actually resident.
///
/// This is the Windows counterpart of the Unix `mincore` probe. It reports
/// residency, never the mapped length: an unbacked or paged-out mapping counts
/// zero resident bytes while still having a non-zero mapped length, and the two
/// are reported through separate accessors so neither can stand in for the
/// other. A failed query is an error, never a zero-valued success.
pub fn resident_bytes(range: &[u8]) -> io::Result<u64> {
    if range.is_empty() {
        return Ok(0);
    }
    let page_size = page_size()?;
    let start = range.as_ptr() as usize;
    let end = start
        .checked_add(range.len())
        .ok_or_else(|| io::Error::other("mapped range end overflow"))?;
    let aligned_start = start / page_size * page_size;
    let aligned_end = end
        .checked_add(page_size.saturating_sub(1))
        .ok_or_else(|| io::Error::other("mapped range alignment overflow"))?
        / page_size
        * page_size;
    let page_count = aligned_end
        .checked_sub(aligned_start)
        .ok_or_else(|| io::Error::other("mapped range underflow"))?
        / page_size;

    // Bounded scratch: the probe never allocates proportionally to the mapping.
    const QUERY_CAPACITY: usize = 1_024;
    let mut block = [PSAPI_WORKING_SET_EX_INFORMATION {
        VirtualAddress: std::ptr::null_mut(),
        VirtualAttributes: 0,
    }; QUERY_CAPACITY];
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no close.
    let process = unsafe { GetCurrentProcess() };

    let mut first_page = 0_usize;
    let mut resident = 0_u64;
    while first_page < page_count {
        let pages = QUERY_CAPACITY.min(page_count.saturating_sub(first_page));
        let byte_offset = first_page
            .checked_mul(page_size)
            .ok_or_else(|| io::Error::other("working-set byte offset overflow"))?;
        let chunk_start = aligned_start
            .checked_add(byte_offset)
            .ok_or_else(|| io::Error::other("working-set address overflow"))?;
        for index in 0..pages {
            let offset = index
                .checked_mul(page_size)
                .ok_or_else(|| io::Error::other("working-set page offset overflow"))?;
            let address = chunk_start
                .checked_add(offset)
                .ok_or_else(|| io::Error::other("working-set page address overflow"))?;
            let entry = block
                .get_mut(index)
                .ok_or_else(|| io::Error::other("working-set scratch index out of range"))?;
            entry.VirtualAddress = address as *mut c_void;
            entry.VirtualAttributes = 0;
        }
        let bytes = pages
            .checked_mul(size_of::<PSAPI_WORKING_SET_EX_INFORMATION>())
            .and_then(|value| DWORD::try_from(value).ok())
            .ok_or_else(|| io::Error::other("working-set query size overflow"))?;
        // SAFETY: `block` holds `pages` initialized entries of the exact layout
        // the SDK declares, and `bytes` is that span's size.
        let ok = unsafe { K32QueryWorkingSetEx(process, block.as_mut_ptr().cast(), bytes) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        for entry in block.iter().take(pages) {
            if entry.VirtualAttributes & WORKING_SET_EX_VALID != WORKING_SET_EX_VALID {
                continue;
            }
            let page_start = entry.VirtualAddress as usize;
            let page_end = page_start
                .checked_add(page_size)
                .ok_or_else(|| io::Error::other("resident page end overflow"))?;
            // Count only the intersection with the caller's exact range, so a
            // partially covered first or last page is not over-counted.
            let overlap_start = page_start.max(start);
            let overlap_end = page_end.min(end);
            if overlap_end > overlap_start {
                let overlap = u64::try_from(overlap_end.saturating_sub(overlap_start))
                    .map_err(|_| io::Error::other("resident overlap exceeds u64"))?;
                resident = resident.saturating_add(overlap);
            }
        }
        first_page = first_page.saturating_add(pages);
    }
    Ok(resident)
}

/// Process-wide memory figures, each reported under its own Windows name.
///
/// These are Windows measurements and are never relabelled as Darwin's
/// `phys_footprint`: `working_set` is the resident set including shared and
/// mapped pages, while `private_bytes` is the committed private charge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessMemory {
    /// `PROCESS_MEMORY_COUNTERS_EX::WorkingSetSize`.
    pub working_set: u64,
    /// `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage`, the commit charge.
    pub private_bytes: u64,
}

/// Reads this process's working-set and private-byte counters.
pub fn process_memory() -> io::Result<ProcessMemory> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = DWORD::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>())
        .map_err(|_| io::Error::other("memory counter struct size exceeds DWORD"))?;
    counters.cb = size;
    // SAFETY: `GetCurrentProcess` is a pseudo-handle, and `counters` is one
    // writable struct of the exact declared layout whose `cb` states its size.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, size) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ProcessMemory {
        working_set: u64::try_from(counters.WorkingSetSize)
            .map_err(|_| io::Error::other("working set exceeds u64"))?,
        private_bytes: u64::try_from(counters.PrivateUsage)
            .map_err(|_| io::Error::other("private bytes exceed u64"))?,
    })
}

/// A read-only mapping of a whole file, owning its view, section and file
/// handles so they are released in that exact order.
///
/// The section is created `PAGE_WRITECOPY` and the view is mapped `FILE_MAP_READ`.
/// That pair is the exact counterpart of the Unix `MAP_PRIVATE` + `PROT_READ`
/// mapping this replaces: the view is read-only, writes are impossible without
/// an explicit protection change, and no modification can ever reach the file.
/// It also lets the test-support corruption hook obtain one copy-on-write page,
/// which a `PAGE_READONLY` section's maximum protection would forbid.
///
/// The view is taken from offset zero, so the file format's own 16 KiB internal
/// alignment is never confused with Windows' mapping-offset allocation
/// granularity.
#[derive(Debug)]
pub(crate) struct FileMapping {
    address: *mut c_void,
    length: usize,
    /// Dropped after the view is unmapped; see [`Drop`].
    section: Option<OwnedHandle>,
    /// The file the view is backed by, retained so the mapping outlives every
    /// other reference to it and dropped last of the three.
    file: Option<std::fs::File>,
}

// SAFETY: the mapping is immutable for its entire lifetime.
unsafe impl Send for FileMapping {}
// SAFETY: all shared access exposes read-only slices.
unsafe impl Sync for FileMapping {}

impl FileMapping {
    /// Maps an entire non-empty file read-only, taking ownership of the open
    /// file the VFS supplied rather than reopening its path.
    ///
    /// Reopening would be wrong as well as wasteful: by the time a reader maps
    /// a sealed artifact, the path may already name a different file. Taking
    /// the handle preserves the VFS seam, so crash and fault filesystems keep
    /// controlling what gets mapped.
    ///
    /// The file must have been opened sharing deletion, which is what lets
    /// compaction unlink a retired artifact while this reader is still alive;
    /// `std::fs::File` does so by default. Without it `DeleteFileW` fails with
    /// `ERROR_SHARING_VIOLATION`.
    ///
    /// Every failure path releases each resource acquired so far, so a
    /// partially constructed mapping never leaks a handle.
    pub(crate) fn from_file(file: std::fs::File, expected_length: u64) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle as _;

        if expected_length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "refusing to map an empty segment file",
            ));
        }
        let length = usize::try_from(expected_length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "mapped file length exceeds usize",
            )
        })?;
        // SAFETY: `file` owns a live handle for the whole call; a zero maximum
        // size maps the whole current file and a null name creates an unnamed
        // section. The section takes its own reference to the file object, so
        // the borrow does not outlive this statement.
        let section_raw = unsafe {
            CreateFileMappingW(
                file.as_raw_handle().cast(),
                std::ptr::null_mut(),
                PAGE_WRITECOPY,
                0,
                0,
                std::ptr::null(),
            )
        };
        // `file` drops here if the section could not be created.
        let section = OwnedHandle::adopt(section_raw)?;
        // SAFETY: the section is live and `length` is the validated file length
        // taken from offset zero.
        let address = unsafe { MapViewOfFile(section.raw(), FILE_MAP_READ, 0, 0, length) };
        if address.is_null() {
            let error = io::Error::last_os_error();
            // Both handles drop here, releasing everything this call acquired.
            drop(section);
            drop(file);
            return Err(error);
        }
        Ok(Self {
            address,
            length,
            section: Some(section),
            file: Some(file),
        })
    }

    /// The mapped bytes.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        // SAFETY: the view is valid for `length` bytes and outlives the borrow.
        unsafe { std::slice::from_raw_parts(self.address.cast::<u8>(), self.length) }
    }

    /// Exact virtual byte length of this mapping. Never a residency figure.
    pub(crate) const fn length(&self) -> usize {
        self.length
    }

    /// Flips `mask` into one byte of this private view through copy-on-write,
    /// leaving the file on disk untouched, so a test can corrupt what an
    /// already-validated reader sees.
    ///
    /// `&mut self` proves no `as_bytes` borrow is live during the write.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn corrupt_byte(&mut self, offset: usize, mask: u8) -> io::Result<()> {
        if offset >= self.length {
            return Err(io::Error::other("corruption offset is outside the mapping"));
        }
        let page_size = page_size()?;
        let page_offset = offset - offset % page_size;
        let page = self.address.cast::<u8>().wrapping_add(page_offset).cast();
        let mut previous: DWORD = 0;
        // SAFETY: `page` is the page-aligned start of a page inside this live
        // view, and the section's `PAGE_WRITECOPY` maximum protection permits
        // this transition.
        if unsafe { VirtualProtect(page, page_size, PAGE_WRITECOPY, &raw mut previous) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `offset < length` keeps the byte inside the view, the page is
        // now copy-on-write writable, and `&mut self` excludes shared views.
        unsafe {
            let byte = self.address.cast::<u8>().add(offset);
            byte.write(byte.read() ^ mask);
        }
        // SAFETY: the same page range, restored to read-only.
        let mut restored: DWORD = 0;
        if unsafe { VirtualProtect(page, page_size, PAGE_READONLY, &raw mut restored) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Resident bytes of this mapping, per the working-set query.
    pub(crate) fn resident_bytes(&self) -> io::Result<u64> {
        resident_bytes(self.as_bytes())
    }
}

impl Drop for FileMapping {
    fn drop(&mut self) {
        // The view must be unmapped before its section handle is closed, and
        // the file handle is released last.
        // SAFETY: `address` is the exact base returned by `MapViewOfFile`.
        unsafe {
            UnmapViewOfFile(self.address);
        }
        drop(self.section.take());
        drop(self.file.take());
    }
}

/// A read-only file mapping exposed to the crate's integration tests.
///
/// [`FileMapping`] itself stays crate-private so no raw handle escapes the
/// platform boundary. This is the seam the Windows storage-protocol tests use
/// to hold one live mapping while the artifact behind it is replaced or
/// unlinked, which is the lifetime property compaction depends on.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub struct TestMapping(FileMapping);

#[cfg(any(test, feature = "test-support"))]
impl TestMapping {
    /// Maps an entire non-empty file read-only, taking ownership of the file.
    pub fn open(file: std::fs::File, length: u64) -> io::Result<Self> {
        FileMapping::from_file(file, length).map(Self)
    }

    /// The mapped bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Exact virtual byte length of this mapping. Never a residency figure.
    #[must_use]
    pub const fn length(&self) -> usize {
        self.0.length()
    }

    /// Bytes of this mapping whose intersecting pages are actually resident.
    pub fn resident_bytes(&self) -> io::Result<u64> {
        self.0.resident_bytes()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{
        FileMapping, PROCESS_MEMORY_COUNTERS_EX, PSAPI_WORKING_SET_EX_INFORMATION,
        available_disk_bytes, page_size, process_memory, resident_bytes, sync_path, wide,
    };
    use std::path::Path;

    #[test]
    fn constants_match_the_installed_sdk() {
        assert_eq!(super::GENERIC_WRITE, 0x4000_0000);
        assert_eq!(super::FILE_SHARE_READ, 1);
        assert_eq!(super::FILE_SHARE_WRITE, 2);
        assert_eq!(super::FILE_SHARE_DELETE, 4);
        assert_eq!(super::FILE_ATTRIBUTE_NORMAL, 0x80);
        assert_eq!(super::FILE_FLAG_BACKUP_SEMANTICS, 0x0200_0000);
        assert_eq!(super::PAGE_READONLY, 0x02);
        assert_eq!(super::PAGE_WRITECOPY, 0x08);
        assert_eq!(super::FILE_MAP_READ, 0x0004);
        assert_eq!(super::OPEN_EXISTING, 3);
        assert_eq!(super::ERROR_ACCESS_DENIED, 5);
        assert_eq!(super::ERROR_LOCK_VIOLATION, 33);
        assert_eq!(super::INVALID_HANDLE_VALUE as isize, -1);
    }

    #[test]
    fn declared_struct_layouts_match_the_sdk() {
        // Two pointer-width fields.
        assert_eq!(
            size_of::<PSAPI_WORKING_SET_EX_INFORMATION>(),
            2 * size_of::<usize>()
        );
        // Two DWORDs then nine SIZE_Ts, with the pair padded to alignment.
        assert_eq!(
            size_of::<PROCESS_MEMORY_COUNTERS_EX>(),
            size_of::<usize>() + 9 * size_of::<usize>()
        );
    }

    #[test]
    fn an_interior_nul_is_rejected_before_any_kernel_call() {
        let error = wide(Path::new("bad\u{0}path")).expect_err("interior NUL");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn the_page_size_is_a_non_zero_power_of_two() {
        let size = page_size().expect("page size");
        assert!(size.is_power_of_two());
        assert!(size >= 4_096);
    }

    #[test]
    fn process_memory_reports_non_zero_windows_figures() {
        let memory = process_memory().expect("process memory");
        assert!(memory.working_set > 0, "a live process has a working set");
        assert!(memory.private_bytes > 0, "a live process has private bytes");
    }

    #[test]
    fn free_space_is_reported_for_the_temp_volume() {
        let available = available_disk_bytes(&std::env::temp_dir()).expect("free space");
        assert!(available > 0);
    }

    #[test]
    fn residency_of_an_empty_range_is_zero_without_a_kernel_call() {
        assert_eq!(resident_bytes(&[]).expect("empty range"), 0);
    }

    #[test]
    fn a_directory_is_synchronised_through_a_writable_backup_semantics_handle() {
        let directory = std::env::temp_dir().join(format!(
            "ze-sys-windows-sync-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&directory).expect("create");
        std::fs::write(directory.join("artifact"), b"bytes").expect("write");
        sync_path(&directory.join("artifact")).expect("file sync");
        sync_path(&directory).expect("directory sync");
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "ze-sys-windows-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("create scratch");
        directory
    }

    /// The WAL opens its log with `OpenOptions::append`, which yields
    /// `FILE_APPEND_DATA` rather than `GENERIC_WRITE`. An append-only access
    /// mask is not automatically sufficient for `FlushFileBuffers`, so this
    /// asserts the combination the WAL actually uses instead of assuming it.
    #[test]
    fn an_append_handle_can_be_flushed() {
        let directory = scratch("append-flush");
        let path = directory.join("wal.zwal");

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open append");
        std::io::Write::write_all(&mut file, b"record-one\n").expect("append");
        super::flush_file(&file).expect("an append handle must be flushable");
        std::io::Write::write_all(&mut file, b"record-two\n").expect("append again");
        super::flush_file(&file).expect("second flush");
        drop(file);

        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"record-one\nrecord-two\n"
        );
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }

    /// A read-only handle must be refused rather than silently reporting
    /// success, which is what makes reopening a path read-only before flushing
    /// an unusable strategy on Windows.
    #[test]
    fn a_read_only_handle_is_refused_by_the_flush() {
        let directory = scratch("readonly-flush");
        let path = directory.join("artifact");
        std::fs::write(&path, b"bytes").expect("seed");

        let file = std::fs::File::open(&path).expect("read-only open");
        let error = super::flush_file(&file).expect_err("a read-only handle must not flush");
        assert_eq!(
            error.raw_os_error().and_then(|raw| u32::try_from(raw).ok()),
            Some(super::ERROR_ACCESS_DENIED)
        );
        drop(file);
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }

    /// Path aliases that name one directory must produce one identity, so the
    /// writer registry cannot be defeated by respelling the store path.
    #[test]
    fn path_aliases_resolve_to_one_store_identity() {
        let directory = scratch("identity");
        let nested = directory.join("store");
        std::fs::create_dir_all(&nested).expect("nested");

        let canonical = super::directory_identity(&nested).expect("identity");

        // Forward slashes.
        let slashed = std::path::PathBuf::from(nested.to_string_lossy().replace('\\', "/"));
        assert_eq!(
            super::directory_identity(&slashed).expect("slashed"),
            canonical
        );

        // A `.` round trip and an upper-cased spelling.
        let dotted = nested.join(".");
        assert_eq!(
            super::directory_identity(&dotted).expect("dotted"),
            canonical
        );
        let uppercased = std::path::PathBuf::from(nested.to_string_lossy().to_uppercase());
        assert_eq!(
            super::directory_identity(&uppercased).expect("uppercased"),
            canonical
        );

        // A different directory must not collide.
        let sibling = directory.join("other");
        std::fs::create_dir_all(&sibling).expect("sibling");
        assert_ne!(
            super::directory_identity(&sibling).expect("sibling identity"),
            canonical
        );
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }

    #[test]
    fn mapping_reports_length_and_residency_separately() {
        let directory = scratch("map");
        let path = directory.join("segment.zseg");
        let payload: Vec<u8> = (0..65_536_u32).map(|value| (value % 251) as u8).collect();
        std::fs::write(&path, &payload).expect("seed");

        let file = std::fs::File::open(&path).expect("open");
        let mapping = FileMapping::from_file(file, payload.len() as u64).expect("map");
        assert_eq!(mapping.length(), payload.len());
        assert_eq!(mapping.as_bytes(), payload.as_slice());
        let resident = mapping.resident_bytes().expect("residency");
        assert!(
            resident <= mapping.length() as u64,
            "residency {resident} must never exceed the mapped length {}",
            mapping.length()
        );
        drop(mapping);
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }

    /// The reclamation property the purge path depends on: a retired artifact
    /// can be unlinked while a reader still holds its mapping, and that reader
    /// keeps serving the bytes it validated.
    #[test]
    fn a_mapped_artifact_can_be_unlinked_while_its_reader_stays_valid() {
        let directory = scratch("unlink");
        let path = directory.join("segment.zseg");
        let payload: Vec<u8> = (0..8_192_u32).map(|value| (value % 251) as u8).collect();
        std::fs::write(&path, &payload).expect("seed");

        let file = std::fs::File::open(&path).expect("open");
        let mapping = FileMapping::from_file(file, payload.len() as u64).expect("map");

        std::fs::remove_file(&path).expect("a std::fs::File shares deletion by default");
        assert!(
            !path.try_exists().expect("exists probe"),
            "unlinked at once"
        );
        assert_eq!(
            mapping.as_bytes(),
            payload.as_slice(),
            "a retained reader keeps its validated bytes after the unlink"
        );
        drop(mapping);
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }

    /// Copy-on-write corruption must change what the reader sees and leave the
    /// file on disk untouched.
    #[test]
    fn corrupting_a_mapped_byte_does_not_reach_the_file() {
        let directory = scratch("corrupt");
        let path = directory.join("segment.zseg");
        let payload = vec![0xAA_u8; 8_192];
        std::fs::write(&path, &payload).expect("seed");

        let file = std::fs::File::open(&path).expect("open");
        let mut mapping = FileMapping::from_file(file, payload.len() as u64).expect("map");
        mapping.corrupt_byte(4_100, 0xFF).expect("corrupt");

        assert_eq!(mapping.as_bytes().get(4_100).copied(), Some(0x55));
        assert_eq!(mapping.as_bytes().get(4_099).copied(), Some(0xAA));
        assert_eq!(
            std::fs::read(&path).expect("read").get(4_100).copied(),
            Some(0xAA),
            "copy-on-write must leave the file untouched"
        );
        assert!(mapping.corrupt_byte(payload.len(), 0x01).is_err());
        drop(mapping);
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }

    #[test]
    fn an_empty_file_is_refused_with_a_typed_error() {
        let directory = scratch("empty");
        let path = directory.join("empty.zseg");
        std::fs::write(&path, b"").expect("seed");
        let file = std::fs::File::open(&path).expect("open");
        let error = FileMapping::from_file(file, 0).expect_err("empty");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }

    /// A length longer than the file must be refused by the kernel rather than
    /// producing a slice over bytes that are not there.
    #[test]
    fn a_length_beyond_the_file_is_refused() {
        let directory = scratch("truncated");
        let path = directory.join("short.zseg");
        std::fs::write(&path, b"only-sixteen-b!!").expect("seed");
        let file = std::fs::File::open(&path).expect("open");
        let error = FileMapping::from_file(file, 1 << 20).expect_err("over-long mapping");
        assert!(
            error.raw_os_error().is_some(),
            "a real Win32 error: {error}"
        );
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }
}
