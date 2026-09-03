//! Persistent-pool scan controls, outcomes, and CPU-capacity detection.

use super::ScanError;

/// Runtime controls for the extended exact scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanOptions {
    /// Requested worker count. Zero explicitly selects all detected physical
    /// performance cores; a nonzero request is capped at that count.
    pub thread_budget: usize,
}

/// Deterministic companion counters for one completed scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanStats {
    /// Logical row-coordinate multiply-accumulates scored by the scan. Packed
    /// Bit4 batches score every row in a four-row window before applying masks,
    /// so their masked lanes are included.
    pub dims_touched: u64,
    /// Row-major payload bytes read by scoring. Row-factor records are excluded.
    /// This counter is a pure function of the request and scan options.
    pub bytes_read: u64,
    /// Persistent workers actually used after CPU and work-unit caps.
    pub threads_used: usize,
    /// Actual worker thread ids that scored the partitions.
    pub worker_thread_ids: Vec<std::thread::ThreadId>,
}

/// Ranked candidates and deterministic counters from an extended exact scan.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanOutcome {
    /// Candidates in descending-score, ascending-row-id parity order.
    pub candidates: Vec<super::ScanCandidate>,
    /// Deterministic work counters.
    pub stats: ScanStats,
    /// Worst score over every scored row, before the top-k cut. Present only
    /// where the scan visited every allowed row and scored it exactly, which
    /// is what makes it usable as a normalization anchor.
    pub worst_score: Option<f32>,
}

/// Returns the physical performance-core capacity used to cap scan workers.
///
/// On Darwin this is `hw.perflevel0.physicalcpu`. Other targets use
/// [`std::thread::available_parallelism`] as the documented explicit default
/// because those targets do not expose Darwin performance-level topology.
///
/// # Errors
///
/// Returns [`ScanError::CpuCount`] when the operating system cannot report a
/// positive count.
pub fn physical_thread_capacity() -> Result<usize, ScanError> {
    static CAPACITY: std::sync::OnceLock<Result<usize, String>> = std::sync::OnceLock::new();
    match CAPACITY.get_or_init(detect_physical_thread_capacity) {
        Ok(capacity) => Ok(*capacity),
        Err(error) => Err(ScanError::CpuCount(error.clone())),
    }
}

fn detect_physical_thread_capacity() -> Result<usize, String> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        crate::sys::darwin::physical_performance_core_count().map_err(|error| error.to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .map_err(|error| error.to_string())
    }
}
