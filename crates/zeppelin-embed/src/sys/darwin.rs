//! Darwin durability and virtual-memory probes.

use std::fmt;
use std::io;
use std::os::fd::RawFd;

const TASK_VM_INFO: libc::task_flavor_t = 22;

unsafe extern "C" {
    static mach_task_self_: libc::mach_port_t;
}

/// A typed failure from a Darwin durability or virtual-memory operation.
#[derive(Debug)]
pub enum SysError {
    /// `F_BARRIERFSYNC` rejected the supplied descriptor.
    BarrierFsync(io::Error),
    /// `F_FULLFSYNC` rejected the supplied descriptor.
    FullFsync(io::Error),
    /// `task_info(TASK_VM_INFO)` failed with a Mach error code.
    TaskVmInfo(libc::kern_return_t),
    /// `sysconf(_SC_PAGESIZE)` did not return a valid page size.
    PageSize(io::Error),
    /// The supplied memory range could not be represented safely.
    RangeOverflow,
    /// `mincore` rejected the supplied memory range.
    Mincore(io::Error),
    /// `sysctlbyname(hw.perflevel0.physicalcpu)` failed.
    PerformanceCoreCount(io::Error),
    /// Darwin returned a zero or malformed performance-core count.
    InvalidPerformanceCoreCount,
}

impl fmt::Display for SysError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BarrierFsync(error) => write!(formatter, "F_BARRIERFSYNC failed: {error}"),
            Self::FullFsync(error) => write!(formatter, "F_FULLFSYNC failed: {error}"),
            Self::TaskVmInfo(code) => {
                write!(
                    formatter,
                    "task_info(TASK_VM_INFO) failed with Mach code {code}"
                )
            }
            Self::PageSize(error) => write!(formatter, "could not determine VM page size: {error}"),
            Self::RangeOverflow => formatter.write_str("memory range overflows the address space"),
            Self::Mincore(error) => write!(formatter, "mincore failed: {error}"),
            Self::PerformanceCoreCount(error) => {
                write!(
                    formatter,
                    "could not read physical performance-core count: {error}"
                )
            }
            Self::InvalidPerformanceCoreCount => {
                formatter.write_str("Darwin returned an invalid physical performance-core count")
            }
        }
    }
}

impl std::error::Error for SysError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BarrierFsync(error)
            | Self::FullFsync(error)
            | Self::PageSize(error)
            | Self::Mincore(error)
            | Self::PerformanceCoreCount(error) => Some(error),
            Self::TaskVmInfo(_) | Self::RangeOverflow | Self::InvalidPerformanceCoreCount => None,
        }
    }
}

/// Current resident and physical-footprint counters for this process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskMemoryInfo {
    /// Resident bytes reported by `TASK_VM_INFO`.
    pub resident_size: u64,
    /// Ledger-backed physical footprint bytes reported by `TASK_VM_INFO`.
    pub phys_footprint: u64,
}

#[repr(C)]
#[derive(Default)]
struct TaskVmInfoRev1 {
    virtual_size: libc::mach_vm_size_t,
    region_count: libc::integer_t,
    page_size: libc::integer_t,
    resident_size: libc::mach_vm_size_t,
    resident_size_peak: libc::mach_vm_size_t,
    device: libc::mach_vm_size_t,
    device_peak: libc::mach_vm_size_t,
    internal: libc::mach_vm_size_t,
    internal_peak: libc::mach_vm_size_t,
    external: libc::mach_vm_size_t,
    external_peak: libc::mach_vm_size_t,
    reusable: libc::mach_vm_size_t,
    reusable_peak: libc::mach_vm_size_t,
    purgeable_volatile_pmap: libc::mach_vm_size_t,
    purgeable_volatile_resident: libc::mach_vm_size_t,
    purgeable_volatile_virtual: libc::mach_vm_size_t,
    compressed: libc::mach_vm_size_t,
    compressed_peak: libc::mach_vm_size_t,
    compressed_lifetime: libc::mach_vm_size_t,
    phys_footprint: libc::mach_vm_size_t,
}

/// Issues an APFS ordering barrier for a file descriptor.
///
/// The installed macOS SDK defines `F_BARRIERFSYNC` as `85` in
/// `MacOSX.sdk/usr/include/sys/fcntl.h:305`; libc exposes that verified value.
pub fn barrier_fsync(fd: RawFd) -> Result<(), SysError> {
    let result = unsafe {
        // SAFETY: `fcntl` accepts any integer descriptor; invalid descriptors are reported via
        // `-1` and `errno`, which is converted to the typed error below.
        libc::fcntl(fd, libc::F_BARRIERFSYNC)
    };
    if result == -1 {
        Err(SysError::BarrierFsync(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

/// Flushes a file descriptor through the storage device's durable-media path.
///
/// The installed macOS SDK defines `F_FULLFSYNC` as `51` in
/// `MacOSX.sdk/usr/include/sys/fcntl.h:258`.
pub fn full_fsync(fd: RawFd) -> Result<(), SysError> {
    let result = unsafe {
        // SAFETY: `fcntl` accepts any integer descriptor; invalid descriptors are reported via
        // `-1` and `errno`, which is converted to the typed error below.
        libc::fcntl(fd, libc::F_FULLFSYNC)
    };
    if result == -1 {
        Err(SysError::FullFsync(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

/// Reads the current process's resident-size and physical-footprint counters.
pub fn task_memory_info() -> Result<TaskMemoryInfo, SysError> {
    let mut info = TaskVmInfoRev1::default();
    let count_value = std::mem::size_of::<TaskVmInfoRev1>()
        .checked_div(std::mem::size_of::<libc::natural_t>())
        .ok_or(SysError::RangeOverflow)?;
    let mut count =
        libc::mach_msg_type_number_t::try_from(count_value).map_err(|_| SysError::RangeOverflow)?;
    let task = unsafe {
        // SAFETY: libSystem initializes the current-task Mach port before Rust `main` executes.
        mach_task_self_
    };
    let result = unsafe {
        // SAFETY: `info` is a writable C-compatible prefix of `task_vm_info`, and `count`
        // advertises exactly that prefix in `natural_t` units as required by the Mach API.
        libc::task_info(
            task,
            TASK_VM_INFO,
            (&raw mut info).cast::<libc::integer_t>(),
            &raw mut count,
        )
    };
    if result != libc::KERN_SUCCESS {
        return Err(SysError::TaskVmInfo(result));
    }
    Ok(TaskMemoryInfo {
        resident_size: info.resident_size,
        phys_footprint: info.phys_footprint,
    })
}

/// Reads the current process's `phys_footprint` ledger counter in bytes.
pub fn phys_footprint() -> Result<u64, SysError> {
    task_memory_info().map(|info| info.phys_footprint)
}

/// Reads the physical performance-core count from Darwin's performance level
/// zero topology.
///
/// # Errors
///
/// Returns [`SysError::PerformanceCoreCount`] when `sysctlbyname` fails and
/// [`SysError::InvalidPerformanceCoreCount`] for zero or malformed output.
pub fn physical_performance_core_count() -> Result<usize, SysError> {
    let mut count = 0_u32;
    let mut length = std::mem::size_of::<u32>();
    let result = unsafe {
        // SAFETY: the name is a static nul-terminated C string, `count` is a
        // writable integer, and `length` advertises its exact byte size.
        libc::sysctlbyname(
            c"hw.perflevel0.physicalcpu".as_ptr(),
            (&raw mut count).cast::<libc::c_void>(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result == -1 {
        return Err(SysError::PerformanceCoreCount(io::Error::last_os_error()));
    }
    if count == 0 || length != std::mem::size_of::<u32>() {
        return Err(SysError::InvalidPerformanceCoreCount);
    }
    usize::try_from(count).map_err(|_| SysError::InvalidPerformanceCoreCount)
}

/// Counts resident virtual-memory pages intersecting a byte slice.
///
/// The supplied slice may begin or end between page boundaries. The wrapper rounds the queried
/// address range outward while retaining the slice's validity guarantee.
pub fn mincore_resident(range: &[u8]) -> Result<usize, SysError> {
    if range.is_empty() {
        return Ok(0);
    }
    let page_size_raw = unsafe {
        // SAFETY: `_SC_PAGESIZE` takes no pointer arguments and has no preconditions.
        libc::sysconf(libc::_SC_PAGESIZE)
    };
    if page_size_raw <= 0 {
        return Err(SysError::PageSize(io::Error::last_os_error()));
    }
    let page_size = usize::try_from(page_size_raw).map_err(|_| SysError::RangeOverflow)?;
    let start = range.as_ptr() as usize;
    let end = start
        .checked_add(range.len())
        .ok_or(SysError::RangeOverflow)?;
    let aligned_start = start / page_size * page_size;
    let aligned_end = end
        .checked_add(page_size - 1)
        .ok_or(SysError::RangeOverflow)?
        / page_size
        * page_size;
    let aligned_len = aligned_end
        .checked_sub(aligned_start)
        .ok_or(SysError::RangeOverflow)?;
    let page_count = aligned_len / page_size;
    let mut residency = vec![0 as libc::c_char; page_count];
    let result = unsafe {
        // SAFETY: rounding outward remains within the mapped pages that contain the valid slice;
        // `residency` has one status byte for every page in the rounded range.
        libc::mincore(
            aligned_start as *const libc::c_void,
            aligned_len,
            residency.as_mut_ptr(),
        )
    };
    if result == -1 {
        return Err(SysError::Mincore(io::Error::last_os_error()));
    }
    Ok(residency
        .into_iter()
        .filter(|status| *status & 1 == 1)
        .count())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::{
        SysError, barrier_fsync, full_fsync, mincore_resident, phys_footprint,
        physical_performance_core_count,
    };
    use std::os::fd::AsRawFd;
    use std::slice;

    #[test]
    fn barrier_fsync_succeeds_on_regular_file() -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::tempfile()?;
        barrier_fsync(file.as_raw_fd())?;
        Ok(())
    }

    #[test]
    fn full_fsync_succeeds_on_regular_file() -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::tempfile()?;
        full_fsync(file.as_raw_fd())?;
        Ok(())
    }

    #[test]
    fn barrier_fsync_returns_typed_error_on_closed_fd() -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::tempfile()?;
        let fd = file.as_raw_fd();
        drop(file);

        let error = barrier_fsync(fd).expect_err("a closed descriptor must be rejected");
        assert!(matches!(error, SysError::BarrierFsync(_)));
        Ok(())
    }

    #[test]
    fn phys_footprint_is_nonzero_for_current_task() -> Result<(), Box<dyn std::error::Error>> {
        assert!(phys_footprint()? > 0);
        Ok(())
    }

    #[test]
    fn physical_p_core_count_is_within_logical_cpu_count() -> Result<(), Box<dyn std::error::Error>>
    {
        let physical = physical_performance_core_count()?;
        let logical = std::thread::available_parallelism()?.get();
        assert!(
            physical >= 1,
            "assertion failed: physical P-core count was {physical}"
        );
        assert!(
            physical <= logical,
            "physical P-core count {physical} exceeded logical CPU count {logical}"
        );
        Ok(())
    }

    #[test]
    fn mincore_resident_reports_touched_pages() -> Result<(), Box<dyn std::error::Error>> {
        let page_size_raw = unsafe {
            // SAFETY: `_SC_PAGESIZE` takes no pointer arguments and has no preconditions.
            libc::sysconf(libc::_SC_PAGESIZE)
        };
        let page_size = usize::try_from(page_size_raw)?;
        let page_count = 256_usize;
        let touched_pages = page_count / 2;
        let map_len = page_size * page_count;
        let touched_len = page_size * touched_pages;
        let file = tempfile::tempfile()?;
        file.set_len(u64::try_from(map_len)?)?;
        let mapping = unsafe {
            // SAFETY: `file` remains open, the length is within the file, and the result is
            // checked against `MAP_FAILED` before it is dereferenced.
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapping == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        let bytes = unsafe {
            // SAFETY: `mapping` names a live readable mapping of exactly `map_len` bytes.
            slice::from_raw_parts(mapping.cast::<u8>(), map_len)
        };
        for offset in (0..touched_len).step_by(page_size) {
            unsafe {
                // SAFETY: every stepped offset is within the live readable mapping.
                std::ptr::read_volatile(bytes.as_ptr().add(offset));
            }
        }

        let resident = mincore_resident(&bytes[..touched_len])?;
        assert!(resident >= touched_pages * 40 / 100);
        assert!(resident <= touched_pages);

        let unmap_result = unsafe {
            // SAFETY: `mapping` is the original address returned by `mmap`, with its exact length.
            libc::munmap(mapping, map_len)
        };
        if unmap_result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[test]
    fn mincore_resident_empty_range_is_zero() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(mincore_resident(&[])?, 0);
        Ok(())
    }

    #[test]
    fn sys_error_preserves_typed_operation() {
        let error = SysError::BarrierFsync(std::io::Error::from_raw_os_error(libc::EBADF));
        assert!(error.to_string().contains("F_BARRIERFSYNC"));
        assert!(std::error::Error::source(&error).is_some());
    }
}
