//! File-backed mmap footprint measurements.

#[cfg(target_os = "macos")]
use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

/// Configuration for clean and copy-on-write mmap probes.
#[derive(Clone, Debug)]
pub struct FootprintConfig {
    /// File and mapping size in bytes.
    pub file_size: usize,
    /// Directory in which the probe file is created.
    pub directory: PathBuf,
}

impl FootprintConfig {
    /// The one-gibibyte Task 09 architecture invariant.
    pub fn architecture_invariant() -> Self {
        Self {
            file_size: 1024 * 1024 * 1024,
            directory: PathBuf::from("/private/tmp"),
        }
    }

    /// CI-sized counter and mapping plausibility probe.
    pub fn smoke() -> Self {
        Self {
            file_size: 32 * 1024 * 1024,
            directory: PathBuf::from("/private/tmp"),
        }
    }
}

/// Raw before/after counters for one mmap case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FootprintCase {
    /// `phys_footprint` immediately before touching the mapping.
    pub phys_footprint_before: u64,
    /// `phys_footprint` immediately after touching the mapping.
    pub phys_footprint_after: u64,
    /// Signed after-minus-before physical-footprint delta.
    pub phys_footprint_delta: i64,
    /// Resident size immediately before touching the mapping.
    pub rss_before: u64,
    /// Resident size immediately after touching the mapping.
    pub rss_after: u64,
    /// Signed after-minus-before resident-size delta.
    pub rss_delta: i64,
    /// Pages reported resident before touching the mapping.
    pub resident_pages_before: usize,
    /// Pages reported resident after touching the mapping.
    pub resident_pages_after: usize,
}

/// Complete clean-file and dirty-private mmap report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FootprintReport {
    /// Probe file size in bytes.
    pub file_size: u64,
    /// VM page size used to stride the mapping.
    pub page_size: usize,
    /// Clean read-only shared mapping counters.
    pub clean: FootprintCase,
    /// Dirtied read-write private mapping counters.
    pub private_dirty: FootprintCase,
}

/// Measures clean file-backed and dirtied copy-on-write mappings.
#[cfg(target_os = "macos")]
pub fn measure(config: FootprintConfig) -> io::Result<FootprintReport> {
    use std::os::fd::AsRawFd;
    use zeppelin_embed::sys::darwin::{mincore_resident, task_memory_info};

    if config.file_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "footprint mapping must not be empty",
        ));
    }
    std::fs::create_dir_all(&config.directory)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let file_id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path = config.directory.join(format!(
        "zeppelin-platform-footprint-{}-{nonce}-{file_id}.tmp",
        std::process::id(),
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)?;
    file.set_len(u64::try_from(config.file_size).map_err(io::Error::other)?)?;
    let page_size = vm_page_size()?;

    let clean = {
        let mapping = Mapping::new(
            file.as_raw_fd(),
            config.file_size,
            libc::PROT_READ,
            libc::MAP_SHARED,
        )?;
        let resident_before = mincore_resident(mapping.as_slice()).map_err(io::Error::other)?;
        let before = task_memory_info().map_err(io::Error::other)?;
        touch_read_only(mapping.as_slice(), page_size);
        let resident_after = mincore_resident(mapping.as_slice()).map_err(io::Error::other)?;
        let after = task_memory_info().map_err(io::Error::other)?;
        case_from(before, after, resident_before, resident_after)?
    };

    let private_dirty = {
        let mut mapping = Mapping::new(
            file.as_raw_fd(),
            config.file_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
        )?;
        let resident_before = mincore_resident(mapping.as_slice()).map_err(io::Error::other)?;
        let before = task_memory_info().map_err(io::Error::other)?;
        touch_private(mapping.as_mut_slice(), page_size);
        let resident_after = mincore_resident(mapping.as_slice()).map_err(io::Error::other)?;
        let after = task_memory_info().map_err(io::Error::other)?;
        case_from(before, after, resident_before, resident_after)?
    };

    drop(file);
    std::fs::remove_file(&path)?;
    Ok(FootprintReport {
        file_size: u64::try_from(config.file_size).map_err(io::Error::other)?,
        page_size,
        clean,
        private_dirty,
    })
}

/// Returns an unsupported-platform error outside Darwin.
#[cfg(not(target_os = "macos"))]
pub fn measure(_config: FootprintConfig) -> io::Result<FootprintReport> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "phys_footprint is only available on Darwin",
    ))
}

#[cfg(target_os = "macos")]
fn vm_page_size() -> io::Result<usize> {
    let raw = unsafe {
        // SAFETY: `_SC_PAGESIZE` takes no pointer arguments and has no preconditions.
        libc::sysconf(libc::_SC_PAGESIZE)
    };
    if raw <= 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(raw).map_err(io::Error::other)
}

#[cfg(target_os = "macos")]
fn case_from(
    before: zeppelin_embed::sys::darwin::TaskMemoryInfo,
    after: zeppelin_embed::sys::darwin::TaskMemoryInfo,
    resident_pages_before: usize,
    resident_pages_after: usize,
) -> io::Result<FootprintCase> {
    Ok(FootprintCase {
        phys_footprint_before: before.phys_footprint,
        phys_footprint_after: after.phys_footprint,
        phys_footprint_delta: signed_delta(after.phys_footprint, before.phys_footprint)?,
        rss_before: before.resident_size,
        rss_after: after.resident_size,
        rss_delta: signed_delta(after.resident_size, before.resident_size)?,
        resident_pages_before,
        resident_pages_after,
    })
}

#[cfg(target_os = "macos")]
fn signed_delta(after: u64, before: u64) -> io::Result<i64> {
    let delta = i128::from(after) - i128::from(before);
    i64::try_from(delta).map_err(io::Error::other)
}

#[cfg(target_os = "macos")]
fn touch_read_only(bytes: &[u8], page_size: usize) {
    let mut checksum = 0_u8;
    for offset in (0..bytes.len()).step_by(page_size) {
        let value = unsafe {
            // SAFETY: every stepped offset is within the live readable mapping.
            std::ptr::read_volatile(bytes.as_ptr().add(offset))
        };
        checksum ^= value;
    }
    std::hint::black_box(checksum);
}

#[cfg(target_os = "macos")]
fn touch_private(bytes: &mut [u8], page_size: usize) {
    for offset in (0..bytes.len()).step_by(page_size) {
        let pointer = unsafe {
            // SAFETY: every stepped offset is within the live writable private mapping.
            bytes.as_mut_ptr().add(offset)
        };
        let old = unsafe {
            // SAFETY: `pointer` names one initialized byte in the writable mapping.
            std::ptr::read_volatile(pointer)
        };
        unsafe {
            // SAFETY: `pointer` remains valid and writable for the duration of this call.
            std::ptr::write_volatile(pointer, old.wrapping_add(1));
        }
    }
}

#[cfg(target_os = "macos")]
struct Mapping {
    pointer: std::ptr::NonNull<u8>,
    len: usize,
}

#[cfg(target_os = "macos")]
impl Mapping {
    fn new(
        fd: libc::c_int,
        len: usize,
        protection: libc::c_int,
        flags: libc::c_int,
    ) -> io::Result<Self> {
        let raw = unsafe {
            // SAFETY: the descriptor remains live in the caller, the mapped file is at least
            // `len` bytes, and the return value is validated before constructing `NonNull`.
            libc::mmap(std::ptr::null_mut(), len, protection, flags, fd, 0)
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let pointer = std::ptr::NonNull::new(raw.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap returned a null address"))?;
        Ok(Self { pointer, len })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe {
            // SAFETY: the mapping remains live for `self`, and `len` is its exact byte length.
            std::slice::from_raw_parts(self.pointer.as_ptr(), self.len)
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe {
            // SAFETY: the caller constructed this mapping writable and holds exclusive `&mut self`.
            std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.len)
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for Mapping {
    fn drop(&mut self) {
        let _result = unsafe {
            // SAFETY: this is the exact live address and length returned by `mmap`.
            libc::munmap(self.pointer.as_ptr().cast(), self.len)
        };
    }
}

/// Prints a human-readable table followed by a machine-readable JSON block.
pub fn print_report(report: &FootprintReport) {
    println!("file_size: {}", report.file_size);
    println!("page_size: {}", report.page_size);
    println!(
        "case\tphys_before\tphys_after\tphys_delta\trss_before\trss_after\trss_delta\tresident_pages_before\tresident_pages_after"
    );
    for (label, case) in [
        ("clean", report.clean),
        ("private_dirty", report.private_dirty),
    ] {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            label,
            case.phys_footprint_before,
            case.phys_footprint_after,
            case.phys_footprint_delta,
            case.rss_before,
            case.rss_after,
            case.rss_delta,
            case.resident_pages_before,
            case.resident_pages_after
        );
    }
    println!(
        "JSON {}",
        serde_json::json!({
            "kind": "footprint",
            "file_size": report.file_size,
            "page_size": report.page_size,
            "clean": case_json(report.clean),
            "private_dirty": case_json(report.private_dirty)
        })
    );
}

fn case_json(case: FootprintCase) -> serde_json::Value {
    serde_json::json!({
        "phys_footprint_before": case.phys_footprint_before,
        "phys_footprint_after": case.phys_footprint_after,
        "phys_footprint_delta": case.phys_footprint_delta,
        "rss_before": case.rss_before,
        "rss_after": case.rss_after,
        "rss_delta": case.rss_delta,
        "resident_pages_before": case.resident_pages_before,
        "resident_pages_after": case.resident_pages_after
    })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{FootprintConfig, measure};

    #[test]
    fn footprint_clean_mmap_costs_zero_phys_footprint() -> Result<(), Box<dyn std::error::Error>> {
        let report = measure(FootprintConfig::architecture_invariant())?;
        assert!(report.clean.phys_footprint_delta.unsigned_abs() < report.file_size / 20);
        Ok(())
    }

    #[test]
    fn footprint_smoke_has_working_counter_case() -> Result<(), Box<dyn std::error::Error>> {
        let report = measure(FootprintConfig::smoke())?;
        assert!(report.clean.resident_pages_after > 0);
        assert!(report.private_dirty.phys_footprint_delta > 0);
        Ok(())
    }
}
