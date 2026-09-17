//! Windows thread census shared by the tests that assert no engine thread
//! survives a close.
//!
//! The macOS and Linux arms live beside their tests because each is a handful
//! of lines; the Windows arm is long enough that duplicating it would be a
//! liability, so it is shared from here instead.
//!
//! This is the counterpart of the Linux arm's `/proc/self/task/*/comm` filter:
//! `std::thread::Builder::name` reaches Windows through `SetThreadDescription`,
//! so the same `ze-lifecycle-` / `ze-query-` prefixes are observable, and
//! unrelated threads in the test process cannot pollute the census.

// Each `#[path]` includer uses one of the two censuses, so the other is
// dead in that target. The SDK names are kept verbatim so they can be
// diffed against `tlhelp32.h`.
#![allow(dead_code, clippy::upper_case_acronyms)]

#[allow(non_snake_case, non_camel_case_types)]
mod win {
    pub type DWORD = u32;
    pub type HANDLE = *mut core::ffi::c_void;
    pub const INVALID_HANDLE_VALUE: HANDLE = -1_isize as HANDLE;
    pub const TH32CS_SNAPTHREAD: DWORD = 0x0000_0004;
    pub const THREAD_QUERY_LIMITED_INFORMATION: DWORD = 0x0800;

    /// `tlhelp32.h`: `THREADENTRY32`, field for field.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct THREADENTRY32 {
        pub dwSize: DWORD,
        pub cntUsage: DWORD,
        pub th32ThreadID: DWORD,
        pub th32OwnerProcessID: DWORD,
        pub tpBasePri: i32,
        pub tpDeltaPri: i32,
        pub dwFlags: DWORD,
    }

    unsafe extern "system" {
        pub fn CreateToolhelp32Snapshot(dwFlags: DWORD, th32ProcessID: DWORD) -> HANDLE;
        pub fn Thread32First(hSnapshot: HANDLE, lpte: *mut THREADENTRY32) -> i32;
        pub fn Thread32Next(hSnapshot: HANDLE, lpte: *mut THREADENTRY32) -> i32;
        pub fn CloseHandle(hObject: HANDLE) -> i32;
        pub fn GetCurrentProcessId() -> DWORD;
        pub fn OpenThread(dwDesiredAccess: DWORD, bInheritHandle: i32, dwThreadId: DWORD)
        -> HANDLE;
        pub fn GetThreadDescription(hThread: HANDLE, ppszThreadDescription: *mut *mut u16) -> i32;
        pub fn LocalFree(hMem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }
}

/// Closes a snapshot or thread handle exactly once.
struct Handle(win::HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: only ever constructed from a validated non-failure handle.
        unsafe {
            win::CloseHandle(self.0);
        }
    }
}

/// The thread's description, or `None` when it has none or cannot be opened.
fn thread_description(id: win::DWORD) -> Option<String> {
    // SAFETY: no pointer arguments; a null return means the open was refused.
    let raw = unsafe { win::OpenThread(win::THREAD_QUERY_LIMITED_INFORMATION, 0, id) };
    if raw.is_null() || raw == win::INVALID_HANDLE_VALUE {
        return None;
    }
    let thread = Handle(raw);
    let mut text: *mut u16 = core::ptr::null_mut();
    // SAFETY: `text` is one writable out-pointer that the call fills with a
    // `LocalAlloc`-owned string on success.
    let result = unsafe { win::GetThreadDescription(thread.0, &raw mut text) };
    if result < 0 || text.is_null() {
        return None;
    }
    let mut length = 0_usize;
    // SAFETY: the returned string is NUL-terminated.
    while unsafe { *text.add(length) } != 0 {
        length = length.saturating_add(1);
    }
    // SAFETY: `text` is valid for `length` UTF-16 code units.
    let slice = unsafe { core::slice::from_raw_parts(text, length) };
    let name = String::from_utf16_lossy(slice);
    // SAFETY: `GetThreadDescription` documents `LocalFree` as the release.
    unsafe {
        win::LocalFree(text.cast());
    }
    Some(name)
}

/// Every live thread in this process, unfiltered.
///
/// The counterpart of an unfiltered `/proc/self/task` listing, for callers that
/// isolate themselves in a child process instead of filtering by name.
pub fn all_thread_ids() -> std::io::Result<std::collections::BTreeSet<u64>> {
    thread_ids(false)
}

/// Every live engine-named thread in this process.
///
/// The counterpart of the Linux arm that filters `/proc/self/task/*/comm` by
/// the `ze-lifecycle-` / `ze-query-` prefixes.
pub fn named_thread_ids() -> std::io::Result<std::collections::BTreeSet<u64>> {
    thread_ids(true)
}

fn thread_ids(named_only: bool) -> std::io::Result<std::collections::BTreeSet<u64>> {
    // SAFETY: a thread snapshot takes no pointer arguments.
    let raw = unsafe { win::CreateToolhelp32Snapshot(win::TH32CS_SNAPTHREAD, 0) };
    if raw == win::INVALID_HANDLE_VALUE || raw.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let snapshot = Handle(raw);
    // SAFETY: no pointer arguments.
    let process = unsafe { win::GetCurrentProcessId() };
    let entry_size = u32::try_from(size_of::<win::THREADENTRY32>())
        .map_err(|_| std::io::Error::other("THREADENTRY32 exceeds DWORD"))?;

    let mut entry = win::THREADENTRY32 {
        dwSize: entry_size,
        ..Default::default()
    };
    let mut ids = std::collections::BTreeSet::new();
    // SAFETY: `entry` is one writable struct whose `dwSize` states its size.
    let mut more = unsafe { win::Thread32First(snapshot.0, &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == process {
            let keep = !named_only
                || thread_description(entry.th32ThreadID).is_some_and(|name| {
                    name.starts_with("ze-lifecycle-") || name.starts_with("ze-query-")
                });
            if keep {
                ids.insert(u64::from(entry.th32ThreadID));
            }
        }
        entry.dwSize = entry_size;
        // SAFETY: same contract as `Thread32First`.
        more = unsafe { win::Thread32Next(snapshot.0, &raw mut entry) } != 0;
    }
    Ok(ids)
}
