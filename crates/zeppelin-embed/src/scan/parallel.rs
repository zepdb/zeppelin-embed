//! Deterministic scoped-thread partitioning and bounded top-k merge.

use std::ops::Range;

use super::topk::BoundedTopK;
use super::{ScanError, ScanRequest, ScanRows, scan_geometry, scan_partition};

/// Runtime controls for the extended exact scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanOptions {
    /// Enables conservative early abandonment where the scheme and layout
    /// have a proven bound. Unsupported combinations remain exhaustive.
    pub early_abandon: bool,
    /// Requested worker count. Zero explicitly selects all detected physical
    /// performance cores; a nonzero request is capped at that count.
    pub thread_budget: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            early_abandon: true,
            thread_budget: 0,
        }
    }
}

/// Deterministic companion counters for one completed scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanStats {
    /// Logical score dimensions evaluated, including the final exact score of
    /// every row that survives partial evaluation.
    pub dims_touched: u64,
    /// Rows rejected by a proven partial-score upper bound.
    pub rows_abandoned: u64,
    /// Scoped workers actually used after CPU and work-unit caps.
    pub threads_used: usize,
}

/// Ranked candidates and deterministic counters from an extended exact scan.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanOutcome {
    /// Candidates in descending-score, ascending-row-id parity order.
    pub candidates: Vec<super::ScanCandidate>,
    /// Deterministic work counters.
    pub stats: ScanStats,
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
    #[cfg(target_os = "macos")]
    {
        crate::sys::darwin::physical_performance_core_count().map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .map_err(|error| error.to_string())
    }
}

/// Runs an exact partitioned scan without changing Part A's `top_k` API.
///
/// Local partitions retain their own best `k`; a deterministic bounded merge
/// then applies the permanent descending-score, ascending-row-id ordering.
///
/// # Errors
///
/// Returns [`ScanError`] for invalid scan inputs, CPU detection failure,
/// scoped worker creation failure, or a worker panic.
pub fn top_k_with_options(
    request: ScanRequest<'_>,
    k: usize,
    options: ScanOptions,
) -> Result<ScanOutcome, ScanError> {
    let geometry = scan_geometry(request)?;
    let capacity = physical_thread_capacity()?;
    let requested = if options.thread_budget == 0 {
        capacity
    } else {
        options.thread_budget.min(capacity)
    };
    let workers = requested.min(geometry.work_units.max(1));
    let ranges = partition_ranges(request.rows, geometry.row_count, workers)?;
    let partitions = if workers == 1 {
        let range = ranges
            .first()
            .cloned()
            .ok_or(ScanError::ArithmeticOverflow)?;
        vec![scan_partition(request, k, range, options.early_abandon)?]
    } else {
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for range in ranges {
                let builder = std::thread::Builder::new();
                let handle = builder
                    .spawn_scoped(scope, move || {
                        scan_partition(request, k, range, options.early_abandon)
                    })
                    .map_err(|error| ScanError::ThreadSpawn(error.to_string()))?;
                handles.push(handle);
            }
            let mut completed = Vec::with_capacity(workers);
            for handle in handles {
                let partition = handle.join().map_err(|_| ScanError::WorkerPanicked)??;
                completed.push(partition);
            }
            Ok::<_, ScanError>(completed)
        })?
    };

    let mut merged = BoundedTopK::new(k.min(geometry.row_count));
    let mut dims_touched = 0_u64;
    let mut rows_abandoned = 0_u64;
    for partition in partitions {
        dims_touched = dims_touched
            .checked_add(partition.dims_touched)
            .ok_or(ScanError::ArithmeticOverflow)?;
        rows_abandoned = rows_abandoned
            .checked_add(partition.rows_abandoned)
            .ok_or(ScanError::ArithmeticOverflow)?;
        for candidate in partition.candidates {
            merged.push(candidate);
        }
    }
    Ok(ScanOutcome {
        candidates: merged.into_sorted(),
        stats: ScanStats {
            dims_touched,
            rows_abandoned,
            threads_used: workers,
        },
    })
}

fn partition_ranges(
    rows: ScanRows<'_>,
    row_count: usize,
    workers: usize,
) -> Result<Vec<Range<usize>>, ScanError> {
    match rows {
        ScanRows::F32Pdx(matrix)
        | ScanRows::F16Pdx(matrix)
        | ScanRows::Int8Pdx { codes: matrix, .. }
        | ScanRows::Bit4Pdx { codes: matrix, .. } => {
            partition_pdx_blocks(matrix, row_count, workers)
        }
        _ => partition_row_count(row_count, workers),
    }
}

fn partition_row_count(row_count: usize, workers: usize) -> Result<Vec<Range<usize>>, ScanError> {
    let mut ranges = Vec::with_capacity(workers);
    for worker in 0..workers {
        let start = worker
            .checked_mul(row_count)
            .ok_or(ScanError::ArithmeticOverflow)?
            / workers;
        let end = worker
            .checked_add(1)
            .and_then(|value| value.checked_mul(row_count))
            .ok_or(ScanError::ArithmeticOverflow)?
            / workers;
        ranges.push(start..end);
    }
    Ok(ranges)
}

fn partition_pdx_blocks(
    matrix: &super::pdx::PdxMatrix,
    row_count: usize,
    workers: usize,
) -> Result<Vec<Range<usize>>, ScanError> {
    if matrix.blocks().is_empty() {
        return Ok(std::iter::once(0..0).collect());
    }
    let block_count = matrix.blocks().len();
    let mut ranges = Vec::with_capacity(workers);
    for worker in 0..workers {
        let first_block = worker
            .checked_mul(block_count)
            .ok_or(ScanError::ArithmeticOverflow)?
            / workers;
        let block_end = worker
            .checked_add(1)
            .and_then(|value| value.checked_mul(block_count))
            .ok_or(ScanError::ArithmeticOverflow)?
            / workers;
        let first = matrix
            .blocks()
            .get(first_block)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let start =
            usize::try_from(first.first_row()).map_err(|_| ScanError::ArithmeticOverflow)?;
        let end = if block_end == block_count {
            row_count
        } else {
            matrix
                .blocks()
                .get(block_end)
                .and_then(|block| usize::try_from(block.first_row()).ok())
                .ok_or(ScanError::ArithmeticOverflow)?
        };
        ranges.push(start..end);
    }
    Ok(ranges)
}
