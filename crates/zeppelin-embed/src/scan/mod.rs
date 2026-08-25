//! Exact flat scanning over row-major vector layouts.

pub mod parallel;
pub(crate) mod topk;

pub use parallel::{ScanOptions, ScanOutcome, ScanStats, physical_thread_capacity};

use crate::kernels;
use crate::lifecycle::QueryCancellation;
use crate::quant::{
    Bit4Factors, Bit4Query, Int8Query, Int8Vec, QuantError, QuantScheme, dot_int8_query,
    est_dot_bit4_batch,
};

use topk::BoundedTopK;

/// Immutable row-major full-precision rows with cached input validation.
#[derive(Clone, Debug)]
pub struct F32Rows {
    values: Box<[f32]>,
    first_non_finite: Option<usize>,
}

impl F32Rows {
    /// Takes ownership of row-major values and caches their first non-finite scalar.
    #[must_use]
    pub fn new(values: Vec<f32>) -> Self {
        let first_non_finite = values.iter().position(|value| !value.is_finite());
        Self {
            values: values.into_boxed_slice(),
            first_non_finite,
        }
    }

    /// Returns the immutable row-major scalar buffer.
    #[must_use]
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    const fn first_non_finite(&self) -> Option<usize> {
        self.first_non_finite
    }
}

/// One exact scan candidate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanCandidate {
    /// Zero-based row id.
    pub row_id: usize,
    /// Larger-is-better exact scan score.
    pub score: f32,
}

/// Per-row affine reconstruction factors for signed-byte scan rows.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Int8Factors {
    scale: f32,
    offset: f32,
}

impl Int8Factors {
    /// Constructs validated affine factors from [`crate::quant::quantize_int8`].
    ///
    /// # Errors
    ///
    /// Returns [`Int8FactorsError`] for a negative or non-finite scale or a
    /// non-finite offset.
    pub fn new(scale: f32, offset: f32) -> Result<Self, Int8FactorsError> {
        if !scale.is_finite() {
            return Err(Int8FactorsError::NonFiniteScale);
        }
        if scale < 0.0 {
            return Err(Int8FactorsError::NegativeScale);
        }
        if !offset.is_finite() {
            return Err(Int8FactorsError::NonFiniteOffset);
        }
        Ok(Self { scale, offset })
    }
}

/// Typed failure while constructing signed-byte row factors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Int8FactorsError {
    /// Scale was NaN or infinite.
    NonFiniteScale,
    /// Scale was finite but negative.
    NegativeScale,
    /// Offset was NaN or infinite.
    NonFiniteOffset,
}

impl std::fmt::Display for Int8FactorsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFiniteScale => formatter.write_str("Int8 row scale must be finite"),
            Self::NegativeScale => formatter.write_str("Int8 row scale must not be negative"),
            Self::NonFiniteOffset => formatter.write_str("Int8 row offset must be finite"),
        }
    }
}

impl std::error::Error for Int8FactorsError {}

/// Strongly typed query representation for an exact scan.
#[derive(Clone, Copy, Debug)]
pub enum ScanQuery<'a> {
    /// Full-precision query coordinates.
    F32(&'a [f32]),
    /// IEEE-f16 query bit patterns.
    F16(&'a [u16]),
    /// Prepared affine signed-byte query.
    Int8(&'a Int8Query),
    /// Prepared four-bit Extended-RaBitQ query.
    Bit4(&'a Bit4Query),
}

/// Strongly typed row representation for an exact scan.
#[derive(Clone, Copy, Debug)]
pub enum ScanRows<'a> {
    /// Contiguous row-major full-precision rows.
    F32RowMajor(&'a F32Rows),
    /// Borrowed contiguous row-major full-precision rows.
    ///
    /// Persisted F32 segments use this variant so a query does not copy the
    /// mapped vector region merely to enter the scan executor.
    F32BorrowedRowMajor(&'a [f32]),
    /// Contiguous row-major IEEE-f16 bit patterns.
    F16RowMajor(&'a [u16]),
    /// Contiguous row-major signed-byte codes and affine row factors.
    Int8RowMajor {
        /// One signed code per coordinate and row.
        codes: &'a [i8],
        /// One validated affine factor pair per row.
        factors: &'a [Int8Factors],
    },
    /// Contiguous row-major packed four-bit codes and estimator factors.
    Bit4RowMajor {
        /// MSB-first packed four-bit codes.
        codes: &'a [u8],
        /// One Extended-RaBitQ factor record per row.
        factors: &'a [Bit4Factors],
    },
}

/// Borrowed inputs for one exact scan.
#[derive(Clone, Copy, Debug)]
pub struct ScanRequest<'a> {
    /// Query representation, whose scheme must match `rows`.
    pub query: ScanQuery<'a>,
    /// Candidate row representation.
    pub rows: ScanRows<'a>,
    /// Optional allow-list of zero-based row ids.
    pub row_mask: Option<&'a roaring::RoaringBitmap>,
}

/// Pull-based stream of exact candidates in permanent parity order.
///
/// Every batch is ranked by descending score and ascending row id on ties.
/// For every valid stream and `k`, concatenating `pull(k)` followed by another
/// `pull(k)` is exactly equal to `pull(2 * k)` from a fresh stream. Future
/// filter and fusion stages may stop pulling without changing candidates that
/// were already yielded.
pub trait CandidateStream {
    /// Pulls at most `maximum` not-yet-yielded candidates.
    ///
    /// # Errors
    ///
    /// Returns [`ScanError`] if the request is invalid or scoring fails.
    fn pull(&mut self, maximum: usize) -> Result<Vec<ScanCandidate>, ScanError>;
}

/// Exact single-threaded candidate stream used by the Part A entry point.
#[derive(Debug)]
pub struct ExactCandidateStream<'a> {
    request: ScanRequest<'a>,
    yielded: usize,
}

impl CandidateStream for ExactCandidateStream<'_> {
    fn pull(&mut self, maximum: usize) -> Result<Vec<ScanCandidate>, ScanError> {
        let requested = self
            .yielded
            .checked_add(maximum)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let ranked = scan_top_k(self.request, requested)?;
        let batch = ranked.into_iter().skip(self.yielded).collect::<Vec<_>>();
        self.yielded = self
            .yielded
            .checked_add(batch.len())
            .ok_or(ScanError::ArithmeticOverflow)?;
        Ok(batch)
    }
}

/// Creates a pull-based exact candidate stream.
#[must_use]
pub const fn candidate_stream(request: ScanRequest<'_>) -> ExactCandidateStream<'_> {
    ExactCandidateStream {
        request,
        yielded: 0,
    }
}

/// Typed failure from exact scan validation or scoring.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanError {
    /// The query dimension was zero.
    ZeroDimension,
    /// Row data was not an exact multiple of the query dimension.
    RowDataLength {
        /// Query dimension.
        dimension: usize,
        /// Supplied scalar count.
        actual: usize,
    },
    /// Query and row representations selected different schemes.
    SchemeMismatch {
        /// Query scheme.
        query: QuantScheme,
        /// Row scheme.
        rows: QuantScheme,
    },
    /// Row-factor count differed from the encoded row count.
    FactorCount {
        /// Encoded row count.
        expected: usize,
        /// Supplied factor count.
        actual: usize,
    },
    /// An input coordinate was NaN or infinite.
    NonFiniteInput {
        /// Zero-based scalar position in the affected input.
        index: usize,
    },
    /// A computed row score was NaN or infinite.
    NonFiniteScore {
        /// Zero-based row id.
        row_id: usize,
    },
    /// A quantization scorer rejected encoded input.
    Quant(QuantError),
    /// Candidate-window arithmetic overflowed `usize`.
    ArithmeticOverflow,
    /// A persistent scan worker panicked before returning a typed result.
    WorkerPanicked,
    /// The operating system could not report a usable CPU count.
    CpuCount(String),
    /// A store query deadline expired. Partial results are never returned.
    Timeout {
        /// Permanently false.
        partial: bool,
    },
    /// A caller-provided token cancelled the store query.
    Cancelled {
        /// Permanently false.
        partial: bool,
    },
    /// Store close cancelled an already admitted query.
    ReadCancelled {
        /// Permanently false.
        partial: bool,
    },
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDimension => formatter.write_str("scan dimension must not be zero"),
            Self::RowDataLength { dimension, actual } => write!(
                formatter,
                "scan row data length {actual} is not divisible by dimension {dimension}"
            ),
            Self::SchemeMismatch { query, rows } => {
                write!(
                    formatter,
                    "scan scheme mismatch: query={query:?}, rows={rows:?}"
                )
            }
            Self::FactorCount { expected, actual } => write!(
                formatter,
                "scan row factor count mismatch: expected {expected}, got {actual}"
            ),
            Self::NonFiniteInput { index } => {
                write!(formatter, "scan input is non-finite at scalar {index}")
            }
            Self::NonFiniteScore { row_id } => {
                write!(formatter, "scan score is non-finite at row {row_id}")
            }
            Self::Quant(error) => write!(formatter, "scan quantization error: {error}"),
            Self::ArithmeticOverflow => {
                formatter.write_str("scan candidate-window arithmetic overflowed")
            }
            Self::WorkerPanicked => formatter.write_str("a persistent scan worker panicked"),
            Self::CpuCount(error) => {
                write!(formatter, "could not determine scan CPU count: {error}")
            }
            Self::Timeout { partial } => {
                write!(formatter, "scan deadline expired (partial={partial})")
            }
            Self::Cancelled { partial } => {
                write!(formatter, "scan was cancelled (partial={partial})")
            }
            Self::ReadCancelled { partial } => {
                write!(formatter, "store close cancelled scan (partial={partial})")
            }
        }
    }
}

impl std::error::Error for ScanError {}

impl From<QuantError> for ScanError {
    fn from(error: QuantError) -> Self {
        Self::Quant(error)
    }
}

/// Returns the exact best `k` candidates in permanent parity order.
///
/// Scores are ordered descending. Equal scores are always ordered by ascending
/// row id. This is a permanent public contract shared by row-major, streaming,
/// and parallel implementations.
///
/// # Errors
///
/// Returns [`ScanError`] for invalid shapes, non-finite inputs, or non-finite
/// scores.
pub fn top_k(request: ScanRequest<'_>, k: usize) -> Result<Vec<ScanCandidate>, ScanError> {
    candidate_stream(request).pull(k)
}

/// Forces the planner's gather executor for threshold calibration.
///
/// This seam is available only to repository test and benchmark tooling. It
/// keeps calibration on the shipping gather implementation without making a
/// forced execution branch part of the product API.
#[cfg(any(test, feature = "test-support"))]
pub fn calibration_gather_top_k(
    query: ScanQuery<'_>,
    rows: ScanRows<'_>,
    allow_list: &crate::meta::DocBitmap,
    k: usize,
) -> Result<Vec<ScanCandidate>, ScanError> {
    Ok(gather_top_k(
        ScanRequest {
            query,
            rows,
            row_mask: Some(allow_list.as_roaring()),
        },
        k,
        None,
    )?
    .candidates)
}

/// Forces the planner's masked full-sweep executor for threshold calibration.
///
/// This seam is available only to repository test and benchmark tooling.
#[cfg(any(test, feature = "test-support"))]
pub fn calibration_masked_top_k(
    query: ScanQuery<'_>,
    rows: ScanRows<'_>,
    allow_list: &crate::meta::DocBitmap,
    k: usize,
) -> Result<Vec<ScanCandidate>, ScanError> {
    scan_top_k(
        ScanRequest {
            query,
            rows,
            row_mask: Some(allow_list.as_roaring()),
        },
        k,
    )
}

fn scan_top_k(request: ScanRequest<'_>, k: usize) -> Result<Vec<ScanCandidate>, ScanError> {
    match (request.query, request.rows) {
        (ScanQuery::F32(query), ScanRows::F32RowMajor(rows)) => {
            scan_f32(query, rows, request.row_mask, k)
        }
        (ScanQuery::F32(query), ScanRows::F32BorrowedRowMajor(rows)) => {
            validate_f32_slice(query, rows)?;
            scan_f32_rows(query, rows, request.row_mask, k, 0, None)
        }
        (ScanQuery::F16(query), ScanRows::F16RowMajor(rows)) => {
            scan_f16(query, rows, request.row_mask, k)
        }
        (ScanQuery::Int8(query), ScanRows::Int8RowMajor { codes, factors }) => {
            scan_int8(query, codes, factors, request.row_mask, k)
        }
        (ScanQuery::Bit4(query), ScanRows::Bit4RowMajor { codes, factors }) => {
            scan_bit4(query, codes, factors, request.row_mask, k)
        }
        (query, rows) => Err(ScanError::SchemeMismatch {
            query: query.scheme(),
            rows: rows.scheme(),
        }),
    }
}

#[derive(Debug)]
pub(crate) struct PartitionScan {
    pub(crate) candidates: Vec<ScanCandidate>,
    pub(crate) dims_touched: u64,
    pub(crate) bytes_read: u64,
    pub(crate) worker_thread_id: std::thread::ThreadId,
}

impl ScanQuery<'_> {
    const fn scheme(self) -> QuantScheme {
        match self {
            Self::F32(_) => QuantScheme::F32,
            Self::F16(_) => QuantScheme::F16,
            Self::Int8(_) => QuantScheme::Int8,
            Self::Bit4(_) => QuantScheme::Bit4,
        }
    }
}

impl ScanRows<'_> {
    const fn scheme(self) -> QuantScheme {
        match self {
            Self::F32RowMajor(_) | Self::F32BorrowedRowMajor(_) => QuantScheme::F32,
            Self::F16RowMajor(_) => QuantScheme::F16,
            Self::Int8RowMajor { .. } => QuantScheme::Int8,
            Self::Bit4RowMajor { .. } => QuantScheme::Bit4,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanGeometry {
    pub(crate) row_count: usize,
    pub(crate) work_units: usize,
}

pub(crate) fn scan_geometry(request: ScanRequest<'_>) -> Result<ScanGeometry, ScanError> {
    let query_scheme = request.query.scheme();
    let row_scheme = request.rows.scheme();
    if query_scheme != row_scheme {
        return Err(ScanError::SchemeMismatch {
            query: query_scheme,
            rows: row_scheme,
        });
    }
    let query_dimension = match request.query {
        ScanQuery::F32(query) => query.len(),
        ScanQuery::F16(query) => query.len(),
        ScanQuery::Int8(query) => query.len(),
        ScanQuery::Bit4(query) => query.len(),
    };
    if query_dimension == 0 {
        return Err(ScanError::ZeroDimension);
    }
    let (row_count, work_units) = match request.rows {
        ScanRows::F32RowMajor(rows) => {
            let geometry = row_major_geometry(rows.values().len(), query_dimension)?;
            let ScanQuery::F32(query) = request.query else {
                return Err(ScanError::ArithmeticOverflow);
            };
            validate_f32(query, rows)?;
            geometry
        }
        ScanRows::F32BorrowedRowMajor(rows) => {
            let geometry = row_major_geometry(rows.len(), query_dimension)?;
            let ScanQuery::F32(query) = request.query else {
                return Err(ScanError::ArithmeticOverflow);
            };
            validate_f32_slice(query, rows)?;
            geometry
        }
        ScanRows::F16RowMajor(rows) => row_major_geometry(rows.len(), query_dimension)?,
        ScanRows::Int8RowMajor { codes, factors } => {
            let geometry = row_major_geometry(codes.len(), query_dimension)?;
            require_factor_count(geometry.0, factors.len())?;
            geometry
        }
        ScanRows::Bit4RowMajor { codes, factors } => {
            let row_width = query_dimension.div_ceil(2);
            let geometry = row_major_geometry(codes.len(), row_width)?;
            require_factor_count(geometry.0, factors.len())?;
            geometry
        }
    };
    Ok(ScanGeometry {
        row_count,
        work_units,
    })
}

fn row_major_geometry(scalar_count: usize, row_width: usize) -> Result<(usize, usize), ScanError> {
    if !scalar_count.is_multiple_of(row_width) {
        return Err(ScanError::RowDataLength {
            dimension: row_width,
            actual: scalar_count,
        });
    }
    let rows = scalar_count / row_width;
    Ok((rows, rows))
}

fn require_factor_count(expected: usize, actual: usize) -> Result<(), ScanError> {
    if expected != actual {
        return Err(ScanError::FactorCount { expected, actual });
    }
    Ok(())
}

pub(crate) fn scan_partition(
    request: ScanRequest<'_>,
    k: usize,
    range: std::ops::Range<usize>,
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<PartitionScan, ScanError> {
    check_cancellation(cancellation)?;
    let geometry = scan_geometry(request)?;
    if range.start > range.end || range.end > geometry.row_count {
        return Err(ScanError::ArithmeticOverflow);
    }
    let first_row = range.start;
    let candidates = match (request.query, request.rows) {
        (ScanQuery::F32(query), ScanRows::F32RowMajor(rows)) => {
            let rows = scalar_row_range(rows.values(), query.len(), range.clone())?;
            scan_f32_rows(query, rows, request.row_mask, k, first_row, cancellation)?
        }
        (ScanQuery::F32(query), ScanRows::F32BorrowedRowMajor(rows)) => {
            let rows = scalar_row_range(rows, query.len(), range.clone())?;
            scan_f32_rows(query, rows, request.row_mask, k, first_row, cancellation)?
        }
        (ScanQuery::F16(query), ScanRows::F16RowMajor(rows)) => {
            let rows = scalar_row_range(rows, query.len(), range.clone())?;
            scan_f16_rows(query, rows, request.row_mask, k, first_row, cancellation)?
        }
        (ScanQuery::Int8(query), ScanRows::Int8RowMajor { codes, factors }) => {
            let codes = scalar_row_range(codes, query.len(), range.clone())?;
            let factors = factors
                .get(range.clone())
                .ok_or(ScanError::ArithmeticOverflow)?;
            scan_int8_rows(
                query,
                codes,
                factors,
                request.row_mask,
                k,
                first_row,
                cancellation,
            )?
        }
        (ScanQuery::Bit4(query), ScanRows::Bit4RowMajor { codes, factors }) => {
            let codes = scalar_row_range(codes, query.len().div_ceil(2), range.clone())?;
            let factors = factors
                .get(range.clone())
                .ok_or(ScanError::ArithmeticOverflow)?;
            scan_bit4_rows(
                query,
                codes,
                factors,
                request.row_mask,
                k,
                first_row,
                cancellation,
            )?
        }
        (query, rows) => {
            return Err(ScanError::SchemeMismatch {
                query: query.scheme(),
                rows: rows.scheme(),
            });
        }
    };
    let scored_rows = match request.rows {
        ScanRows::Bit4RowMajor { .. } => range.end - range.start,
        _ => allowed_row_count(request.row_mask, range.clone(), cancellation)?,
    };
    let dimensions = u64::try_from(match request.query {
        ScanQuery::F32(query) => query.len(),
        ScanQuery::F16(query) => query.len(),
        ScanQuery::Int8(query) => query.len(),
        ScanQuery::Bit4(query) => query.len(),
    })
    .map_err(|_| ScanError::ArithmeticOverflow)?;
    let dims_touched = u64::try_from(scored_rows)
        .map_err(|_| ScanError::ArithmeticOverflow)?
        .checked_mul(dimensions)
        .ok_or(ScanError::ArithmeticOverflow)?;
    let bytes_per_row = match request.query {
        ScanQuery::F32(query) => query
            .len()
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or(ScanError::ArithmeticOverflow)?,
        ScanQuery::F16(query) => query
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or(ScanError::ArithmeticOverflow)?,
        ScanQuery::Int8(query) => query.len(),
        ScanQuery::Bit4(query) => query.len().div_ceil(2),
    };
    let bytes_read = scored_rows
        .checked_mul(bytes_per_row)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(ScanError::ArithmeticOverflow)?;
    Ok(PartitionScan {
        candidates,
        dims_touched,
        bytes_read,
        worker_thread_id: std::thread::current().id(),
    })
}

/// Scores only rows enumerated by the request's allow-list.
///
/// This is the gather counterpart to [`scan_partition`]. It deliberately
/// shares the same scheme validation and scoring kernels so planner branch
/// selection can change work without changing score semantics.
pub(crate) fn gather_top_k(
    request: ScanRequest<'_>,
    k: usize,
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<ScanOutcome, ScanError> {
    check_cancellation(cancellation)?;
    let geometry = scan_geometry(request)?;
    let mut selected = BoundedTopK::new(k.min(geometry.row_count));
    let mut scored_rows = 0_usize;
    if let Some(allow_list) = request.row_mask {
        for (ordinal, row) in allow_list.iter().enumerate() {
            check_cancellation_at_row(cancellation, ordinal)?;
            let row = usize::try_from(row).map_err(|_| ScanError::ArithmeticOverflow)?;
            if row >= geometry.row_count {
                continue;
            }
            selected.push(score_gather_row(request, row)?);
            scored_rows = scored_rows
                .checked_add(1)
                .ok_or(ScanError::ArithmeticOverflow)?;
        }
    } else {
        for row in 0..geometry.row_count {
            check_cancellation_at_row(cancellation, row)?;
            selected.push(score_gather_row(request, row)?);
            scored_rows = scored_rows
                .checked_add(1)
                .ok_or(ScanError::ArithmeticOverflow)?;
        }
    }
    let dimensions = u64::try_from(match request.query {
        ScanQuery::F32(query) => query.len(),
        ScanQuery::F16(query) => query.len(),
        ScanQuery::Int8(query) => query.len(),
        ScanQuery::Bit4(query) => query.len(),
    })
    .map_err(|_| ScanError::ArithmeticOverflow)?;
    let dims_touched = u64::try_from(scored_rows)
        .map_err(|_| ScanError::ArithmeticOverflow)?
        .checked_mul(dimensions)
        .ok_or(ScanError::ArithmeticOverflow)?;
    let bytes_per_row = match request.query {
        ScanQuery::F32(query) => query
            .len()
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or(ScanError::ArithmeticOverflow)?,
        ScanQuery::F16(query) => query
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or(ScanError::ArithmeticOverflow)?,
        ScanQuery::Int8(query) => query.len(),
        ScanQuery::Bit4(query) => query.len().div_ceil(2),
    };
    let bytes_read = u64::try_from(
        scored_rows
            .checked_mul(bytes_per_row)
            .ok_or(ScanError::ArithmeticOverflow)?,
    )
    .map_err(|_| ScanError::ArithmeticOverflow)?;
    let worker_thread_ids = (scored_rows != 0)
        .then(|| std::thread::current().id())
        .into_iter()
        .collect::<Vec<_>>();
    Ok(ScanOutcome {
        candidates: selected.into_sorted(),
        stats: ScanStats {
            dims_touched,
            bytes_read,
            threads_used: worker_thread_ids.len(),
            worker_thread_ids,
        },
    })
}

fn score_gather_row(request: ScanRequest<'_>, row: usize) -> Result<ScanCandidate, ScanError> {
    let score = match (request.query, request.rows) {
        (ScanQuery::F32(query), ScanRows::F32RowMajor(rows)) => {
            let values = scalar_row_range(rows.values(), query.len(), row..row + 1)?;
            kernels::dot_f32(query, values)
        }
        (ScanQuery::F32(query), ScanRows::F32BorrowedRowMajor(rows)) => {
            let values = scalar_row_range(rows, query.len(), row..row + 1)?;
            kernels::dot_f32(query, values)
        }
        (ScanQuery::F16(query), ScanRows::F16RowMajor(rows)) => {
            let values = scalar_row_range(rows, query.len(), row..row + 1)?;
            kernels::dot_f16(query, values)
        }
        (ScanQuery::Int8(query), ScanRows::Int8RowMajor { codes, factors }) => {
            let values = scalar_row_range(codes, query.len(), row..row + 1)?;
            let factor = factors.get(row).ok_or(ScanError::ArithmeticOverflow)?;
            dot_int8_query(
                query,
                Int8Vec {
                    codes: values,
                    scale: factor.scale,
                    offset: factor.offset,
                },
            )?
        }
        (ScanQuery::Bit4(query), ScanRows::Bit4RowMajor { codes, factors }) => {
            let values = scalar_row_range(codes, query.len().div_ceil(2), row..row + 1)?;
            let factor = factors.get(row).ok_or(ScanError::ArithmeticOverflow)?;
            let mut score = [0.0_f32; 1];
            est_dot_bit4_batch(query, values, std::slice::from_ref(factor), &mut score)?;
            score[0]
        }
        (query, rows) => {
            return Err(ScanError::SchemeMismatch {
                query: query.scheme(),
                rows: rows.scheme(),
            });
        }
    };
    if !score.is_finite() {
        return Err(ScanError::NonFiniteScore { row_id: row });
    }
    Ok(ScanCandidate { row_id: row, score })
}

const CANCELLATION_CHECK_ROWS: usize = 64;

fn check_cancellation(cancellation: Option<&QueryCancellation<'_>>) -> Result<(), ScanError> {
    cancellation.map_or(Ok(()), QueryCancellation::check)
}

fn check_cancellation_at_row(
    cancellation: Option<&QueryCancellation<'_>>,
    local_row: usize,
) -> Result<(), ScanError> {
    if local_row.is_multiple_of(CANCELLATION_CHECK_ROWS) {
        check_cancellation(cancellation)?;
    }
    Ok(())
}

fn row_is_allowed(row_mask: Option<&roaring::RoaringBitmap>, row_id: usize) -> bool {
    !row_mask.is_some_and(|mask| u32::try_from(row_id).map_or(true, |id| !mask.contains(id)))
}

fn scalar_row_range<T>(
    values: &[T],
    row_width: usize,
    rows: std::ops::Range<usize>,
) -> Result<&[T], ScanError> {
    let start = rows
        .start
        .checked_mul(row_width)
        .ok_or(ScanError::ArithmeticOverflow)?;
    let end = rows
        .end
        .checked_mul(row_width)
        .ok_or(ScanError::ArithmeticOverflow)?;
    values.get(start..end).ok_or(ScanError::ArithmeticOverflow)
}

fn allowed_row_count(
    row_mask: Option<&roaring::RoaringBitmap>,
    rows: std::ops::Range<usize>,
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<usize, ScanError> {
    let mut allowed = 0_usize;
    for (local_row, row_id) in rows.enumerate() {
        check_cancellation_at_row(cancellation, local_row)?;
        if !row_mask.is_some_and(|mask| u32::try_from(row_id).map_or(true, |id| !mask.contains(id)))
        {
            allowed = allowed
                .checked_add(1)
                .ok_or(ScanError::ArithmeticOverflow)?;
        }
    }
    Ok(allowed)
}

fn scan_f32(
    query: &[f32],
    rows: &F32Rows,
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
) -> Result<Vec<ScanCandidate>, ScanError> {
    if query.is_empty() {
        return Err(ScanError::ZeroDimension);
    }
    if !rows.values().len().is_multiple_of(query.len()) {
        return Err(ScanError::RowDataLength {
            dimension: query.len(),
            actual: rows.values().len(),
        });
    }
    validate_f32(query, rows)?;
    scan_f32_rows(query, rows.values(), row_mask, k, 0, None)
}

fn validate_f32(query: &[f32], rows: &F32Rows) -> Result<(), ScanError> {
    if let Some(index) = query.iter().position(|value| !value.is_finite()) {
        return Err(ScanError::NonFiniteInput { index });
    }
    if let Some(local_index) = rows.first_non_finite() {
        let index = query
            .len()
            .checked_add(local_index)
            .ok_or(ScanError::ArithmeticOverflow)?;
        return Err(ScanError::NonFiniteInput { index });
    }
    Ok(())
}

fn validate_f32_slice(query: &[f32], rows: &[f32]) -> Result<(), ScanError> {
    if let Some(index) = query.iter().position(|value| !value.is_finite()) {
        return Err(ScanError::NonFiniteInput { index });
    }
    if let Some(local_index) = rows.iter().position(|value| !value.is_finite()) {
        let index = query
            .len()
            .checked_add(local_index)
            .ok_or(ScanError::ArithmeticOverflow)?;
        return Err(ScanError::NonFiniteInput { index });
    }
    Ok(())
}

fn scan_f32_rows(
    query: &[f32],
    rows: &[f32],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
    first_row: usize,
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<Vec<ScanCandidate>, ScanError> {
    let row_count = rows.len() / query.len();
    let mut selected = BoundedTopK::new(k.min(row_count));
    for (local_row, row) in rows.chunks_exact(query.len()).enumerate() {
        check_cancellation_at_row(cancellation, local_row)?;
        let row_id = first_row
            .checked_add(local_row)
            .ok_or(ScanError::ArithmeticOverflow)?;
        if row_mask.is_some_and(|mask| u32::try_from(row_id).map_or(true, |id| !mask.contains(id)))
        {
            continue;
        }
        let score = kernels::dot_f32(query, row);
        if !score.is_finite() {
            return Err(ScanError::NonFiniteScore { row_id });
        }
        selected.push(ScanCandidate { row_id, score });
    }
    Ok(selected.into_sorted())
}

fn scan_f16(
    query: &[u16],
    rows: &[u16],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
) -> Result<Vec<ScanCandidate>, ScanError> {
    scan_f16_rows(query, rows, row_mask, k, 0, None)
}

fn scan_f16_rows(
    query: &[u16],
    rows: &[u16],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
    first_row: usize,
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<Vec<ScanCandidate>, ScanError> {
    if query.is_empty() {
        return Err(ScanError::ZeroDimension);
    }
    if !rows.len().is_multiple_of(query.len()) {
        return Err(ScanError::RowDataLength {
            dimension: query.len(),
            actual: rows.len(),
        });
    }
    let row_count = rows.len() / query.len();
    let mut selected = BoundedTopK::new(k.min(row_count));
    for (local_row, row) in rows.chunks_exact(query.len()).enumerate() {
        check_cancellation_at_row(cancellation, local_row)?;
        let row_id = first_row
            .checked_add(local_row)
            .ok_or(ScanError::ArithmeticOverflow)?;
        if row_mask.is_some_and(|mask| u32::try_from(row_id).map_or(true, |id| !mask.contains(id)))
        {
            continue;
        }
        let score = kernels::dot_f16(query, row);
        if !score.is_finite() {
            return Err(ScanError::NonFiniteScore { row_id });
        }
        selected.push(ScanCandidate { row_id, score });
    }
    Ok(selected.into_sorted())
}

fn scan_int8(
    query: &Int8Query,
    codes: &[i8],
    factors: &[Int8Factors],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
) -> Result<Vec<ScanCandidate>, ScanError> {
    scan_int8_rows(query, codes, factors, row_mask, k, 0, None)
}

fn scan_int8_rows(
    query: &Int8Query,
    codes: &[i8],
    factors: &[Int8Factors],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
    first_row: usize,
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<Vec<ScanCandidate>, ScanError> {
    let dimension = query.len();
    if dimension == 0 {
        return Err(ScanError::ZeroDimension);
    }
    if !codes.len().is_multiple_of(dimension) {
        return Err(ScanError::RowDataLength {
            dimension,
            actual: codes.len(),
        });
    }
    let row_count = codes.len() / dimension;
    if factors.len() != row_count {
        return Err(ScanError::FactorCount {
            expected: row_count,
            actual: factors.len(),
        });
    }
    let mut selected = BoundedTopK::new(k.min(row_count));
    for (local_row, (row, factor)) in codes.chunks_exact(dimension).zip(factors).enumerate() {
        check_cancellation_at_row(cancellation, local_row)?;
        let row_id = first_row
            .checked_add(local_row)
            .ok_or(ScanError::ArithmeticOverflow)?;
        if row_mask.is_some_and(|mask| u32::try_from(row_id).map_or(true, |id| !mask.contains(id)))
        {
            continue;
        }
        let score = dot_int8_query(
            query,
            Int8Vec {
                codes: row,
                scale: factor.scale,
                offset: factor.offset,
            },
        )?;
        if !score.is_finite() {
            return Err(ScanError::NonFiniteScore { row_id });
        }
        selected.push(ScanCandidate { row_id, score });
    }
    Ok(selected.into_sorted())
}

fn scan_bit4(
    query: &Bit4Query,
    codes: &[u8],
    factors: &[Bit4Factors],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
) -> Result<Vec<ScanCandidate>, ScanError> {
    scan_bit4_rows(query, codes, factors, row_mask, k, 0, None)
}

fn scan_bit4_rows(
    query: &Bit4Query,
    codes: &[u8],
    factors: &[Bit4Factors],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
    first_row: usize,
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<Vec<ScanCandidate>, ScanError> {
    let dimension = query.len();
    if dimension == 0 {
        return Err(ScanError::ZeroDimension);
    }
    let row_width = dimension.div_ceil(2);
    if !codes.len().is_multiple_of(row_width) {
        return Err(ScanError::RowDataLength {
            dimension: row_width,
            actual: codes.len(),
        });
    }
    let row_count = codes.len() / row_width;
    if factors.len() != row_count {
        return Err(ScanError::FactorCount {
            expected: row_count,
            actual: factors.len(),
        });
    }
    let mut selected = BoundedTopK::new(k.min(row_count));
    let mut scores = [0.0_f32; 4];
    let mut batch_start = 0_usize;
    while batch_start < row_count {
        check_cancellation_at_row(cancellation, batch_start)?;
        let batch_rows = (row_count - batch_start).min(scores.len());
        let batch_end = batch_start
            .checked_add(batch_rows)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let code_start = batch_start
            .checked_mul(row_width)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let code_end = batch_end
            .checked_mul(row_width)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let batch_codes = codes
            .get(code_start..code_end)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let batch_factors = factors
            .get(batch_start..batch_end)
            .ok_or(ScanError::ArithmeticOverflow)?;
        let batch_scores = scores
            .get_mut(..batch_rows)
            .ok_or(ScanError::ArithmeticOverflow)?;
        est_dot_bit4_batch(query, batch_codes, batch_factors, batch_scores)?;
        for (batch_row, &score) in batch_scores.iter().enumerate() {
            let local_row = batch_start
                .checked_add(batch_row)
                .ok_or(ScanError::ArithmeticOverflow)?;
            let row_id = first_row
                .checked_add(local_row)
                .ok_or(ScanError::ArithmeticOverflow)?;
            if !row_is_allowed(row_mask, row_id) {
                continue;
            }
            if !score.is_finite() {
                return Err(ScanError::NonFiniteScore { row_id });
            }
            selected.push(ScanCandidate { row_id, score });
        }
        batch_start = batch_end;
    }
    Ok(selected.into_sorted())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use rand::Rng;
    use roaring::RoaringBitmap;
    use tempfile::{TempDir, tempdir};

    use super::{
        CandidateStream, F32Rows, Int8Factors, ScanOptions, ScanQuery, ScanRequest, ScanRows,
        candidate_stream, top_k,
    };
    use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, QueryError, Store};
    use crate::quant::{prepare_bit4_query, prepare_int8_query, quantize_bit4};

    fn query_store() -> (TempDir, Store) {
        let directory = tempdir().expect("query store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("query store");
        (directory, store)
    }

    fn pooled_top_k(
        store: &Store,
        request: ScanRequest<'_>,
        k: usize,
        options: ScanOptions,
    ) -> Result<super::ScanOutcome, QueryError> {
        store.top_k_with_options(
            request,
            k,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
    }

    #[test]
    fn all_identical_vectors_tie_by_ascending_row_id() {
        let query = [1.0_f32, -2.0];
        let rows = F32Rows::new(vec![1.0_f32, -2.0, 1.0, -2.0, 1.0, -2.0]);
        let request = ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&rows),
            row_mask: None,
        };

        let hits = top_k(request, 3).expect("valid scan");

        assert_eq!(
            hits.iter().map(|hit| hit.row_id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn f32_rows_cache_first_non_finite_input() {
        let rows = super::F32Rows::new(vec![1.0, f32::NAN, f32::INFINITY]);

        assert_eq!(rows.first_non_finite(), Some(1));
        assert_eq!(
            top_k(
                ScanRequest {
                    query: ScanQuery::F32(&[1.0, 1.0, 1.0]),
                    rows: ScanRows::F32RowMajor(&rows),
                    row_mask: None,
                },
                1,
            ),
            Err(super::ScanError::NonFiniteInput { index: 4 })
        );
    }

    #[test]
    fn bit4_row_major_scans_partial_final_batch() {
        let query = prepare_bit4_query(&[1.0, -1.0], 0x05).expect("valid Bit4 query");
        let mut codes = Vec::new();
        let mut factors = Vec::new();
        for magnitude in 1..=5 {
            let mut row = [0_u8; 1];
            factors.push(
                quantize_bit4(&[magnitude as f32, -(magnitude as f32)], &mut row)
                    .expect("valid Bit4 row"),
            );
            codes.extend(row);
        }
        let request = ScanRequest {
            query: ScanQuery::Bit4(&query),
            rows: ScanRows::Bit4RowMajor {
                codes: &codes,
                factors: &factors,
            },
            row_mask: None,
        };

        let actual = top_k(request, 5)
            .expect("valid Bit4 scan")
            .into_iter()
            .map(|candidate| candidate.row_id)
            .collect::<Vec<_>>();

        assert_eq!(actual, [4, 3, 2, 1, 0]);
    }

    #[test]
    fn empty_row_set_returns_no_candidates() {
        let query = [1.0_f32, -2.0];
        let rows = F32Rows::new(Vec::new());
        let request = ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&rows),
            row_mask: None,
        };

        assert!(top_k(request, 4).expect("valid empty scan").is_empty());
    }

    #[test]
    fn prop_candidate_stream_prefix_consistent() {
        let mut random =
            crate::test_support::seeded_rng("scan::prop_candidate_stream_prefix_consistent");
        let query = (0..17)
            .map(|_| random.random_range(-2.0_f32..=2.0_f32))
            .collect::<Vec<_>>();
        let rows = F32Rows::new(
            (0..127 * query.len())
                .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                .collect::<Vec<_>>(),
        );
        let request = ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&rows),
            row_mask: None,
        };

        let mut incremental = candidate_stream(request);
        let mut two_pulls = incremental.pull(19).expect("first pull");
        two_pulls.extend(incremental.pull(19).expect("second pull"));
        let mut fresh = candidate_stream(request);
        let one_pull = fresh.pull(38).expect("fresh pull");

        assert_eq!(two_pulls, one_pull);
    }

    #[test]
    fn prop_row_major_scan_equals_naive_topk() {
        let (_directory, store) = query_store();
        let mut random =
            crate::test_support::seeded_rng("scan::prop_row_major_scan_equals_naive_topk");
        let cases = std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(256);

        for case in 0..cases {
            let dimension = if case.is_multiple_of(17) {
                1_024
            } else {
                random.random_range(8..=1_024)
            };
            let maximum_rows = (32_768 / dimension).clamp(1, 10_000);
            let row_count = if case.is_multiple_of(31) {
                10_000.min(maximum_rows)
            } else {
                random.random_range(0..=maximum_rows)
            };
            let k = random.random_range(0..=row_count);

            match case % 4 {
                0 => {
                    let query = (0..dimension)
                        .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                        .collect::<Vec<_>>();
                    let values = (0..row_count * dimension)
                        .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                        .collect::<Vec<_>>();
                    let rows = F32Rows::new(values);
                    let request = ScanRequest {
                        query: ScanQuery::F32(&query),
                        rows: ScanRows::F32RowMajor(&rows),
                        row_mask: None,
                    };
                    assert_row_major_scan(
                        &store,
                        request,
                        k,
                        case,
                        ScanComparison::F32 {
                            query: &query,
                            rows: rows.values(),
                        },
                    );
                }
                1 => {
                    let query = (0..dimension)
                        .map(|_| random_finite_f16(&mut random))
                        .collect::<Vec<_>>();
                    let rows = (0..row_count * dimension)
                        .map(|_| random_finite_f16(&mut random))
                        .collect::<Vec<_>>();
                    let request = ScanRequest {
                        query: ScanQuery::F16(&query),
                        rows: ScanRows::F16RowMajor(&rows),
                        row_mask: None,
                    };
                    assert_row_major_scan(
                        &store,
                        request,
                        k,
                        case,
                        ScanComparison::F16 {
                            query: &query,
                            rows: &rows,
                        },
                    );
                }
                2 => {
                    let query_values = (0..dimension)
                        .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                        .collect::<Vec<_>>();
                    let query = prepare_int8_query(&query_values).expect("finite Int8 query");
                    let codes = (0..row_count * dimension)
                        .map(|_| random.random::<i8>())
                        .collect::<Vec<_>>();
                    let factors = (0..row_count)
                        .map(|_| {
                            Int8Factors::new(
                                random.random_range(0.001_f32..=0.25_f32),
                                random.random_range(-1.0_f32..=1.0_f32),
                            )
                            .expect("finite factors")
                        })
                        .collect::<Vec<_>>();
                    let request = ScanRequest {
                        query: ScanQuery::Int8(&query),
                        rows: ScanRows::Int8RowMajor {
                            codes: &codes,
                            factors: &factors,
                        },
                        row_mask: None,
                    };
                    assert_row_major_scan(&store, request, k, case, ScanComparison::Exact);
                }
                _ => {
                    let query_values = (0..dimension)
                        .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                        .collect::<Vec<_>>();
                    let query = prepare_bit4_query(&query_values, random.random())
                        .expect("finite Bit4 query");
                    let row_bytes = dimension.div_ceil(2);
                    let mut codes = (0..row_count * row_bytes)
                        .map(|_| random.random::<u8>())
                        .collect::<Vec<_>>();
                    if !dimension.is_multiple_of(2) {
                        for row in codes.chunks_exact_mut(row_bytes) {
                            if let Some(last) = row.last_mut() {
                                *last &= 0xf0;
                            }
                        }
                    }
                    let factor_source = (0..dimension)
                        .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                        .collect::<Vec<_>>();
                    let mut factor_codes = vec![0_u8; row_bytes];
                    let factor = quantize_bit4(&factor_source, &mut factor_codes)
                        .expect("finite factor source");
                    let factors = vec![factor; row_count];
                    let request = ScanRequest {
                        query: ScanQuery::Bit4(&query),
                        rows: ScanRows::Bit4RowMajor {
                            codes: &codes,
                            factors: &factors,
                        },
                        row_mask: None,
                    };
                    assert_row_major_scan(&store, request, k, case, ScanComparison::Exact);
                }
            }
        }
    }

    fn assert_row_major_scan(
        store: &Store,
        request: ScanRequest<'_>,
        k: usize,
        case: usize,
        comparison: ScanComparison<'_>,
    ) {
        let expected = naive_top_k(request, k);
        let single = pooled_top_k(store, request, k, ScanOptions { thread_budget: 1 })
            .expect("valid single-thread row-major scan")
            .candidates;
        let parallel = pooled_top_k(
            store,
            request,
            k,
            ScanOptions {
                thread_budget: case % 12 + 1,
            },
        )
        .expect("valid parallel row-major scan")
        .candidates;
        assert_eq!(
            parallel, single,
            "case {case} thread count changed candidates"
        );
        assert_scan_matches(
            request,
            &expected,
            &parallel,
            comparison,
            &format!("row-major case {case}"),
        );
    }

    fn naive_top_k(request: ScanRequest<'_>, k: usize) -> Vec<super::ScanCandidate> {
        let scalar = crate::kernels::KernelVariant::scalar();
        let mut candidates = Vec::new();
        match (request.query, request.rows) {
            (ScanQuery::F32(query), ScanRows::F32RowMajor(rows)) => {
                for (row_id, row) in rows.values().chunks_exact(query.len()).enumerate() {
                    candidates.push(super::ScanCandidate {
                        row_id,
                        score: scalar.dot_f32(query, row),
                    });
                }
            }
            (ScanQuery::F16(query), ScanRows::F16RowMajor(rows)) => {
                for (row_id, row) in rows.chunks_exact(query.len()).enumerate() {
                    candidates.push(super::ScanCandidate {
                        row_id,
                        score: scalar.dot_f16(query, row),
                    });
                }
            }
            (ScanQuery::Int8(query), ScanRows::Int8RowMajor { codes, factors }) => {
                for (row_id, (row, factor)) in
                    codes.chunks_exact(query.len()).zip(factors).enumerate()
                {
                    let integer_dot = scalar.dot_i8(query.codes(), row);
                    candidates.push(super::ScanCandidate {
                        row_id,
                        score: query.score_integer_dot(integer_dot, factor.scale, factor.offset),
                    });
                }
            }
            (ScanQuery::Bit4(query), ScanRows::Bit4RowMajor { codes, factors }) => {
                let row_width = query.len().div_ceil(2);
                for (row_id, (row, factor)) in
                    codes.chunks_exact(row_width).zip(factors).enumerate()
                {
                    let mut score = [0.0_f32; 1];
                    scalar.score_bit4_prepared_batch(
                        query.kernel_parts(),
                        row,
                        query.len(),
                        std::slice::from_ref(factor),
                        &mut score,
                    );
                    candidates.push(super::ScanCandidate {
                        row_id,
                        score: score[0],
                    });
                }
            }
            _ => unreachable!("test requests are scheme-matched"),
        }
        candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.row_id.cmp(&right.row_id))
        });
        candidates.truncate(k.min(candidates.len()));
        candidates
    }

    #[derive(Clone, Copy)]
    enum ScanComparison<'a> {
        Exact,
        F32 { query: &'a [f32], rows: &'a [f32] },
        F16 { query: &'a [u16], rows: &'a [u16] },
    }

    fn assert_scan_matches(
        reference_request: ScanRequest<'_>,
        expected: &[super::ScanCandidate],
        actual: &[super::ScanCandidate],
        comparison: ScanComparison<'_>,
        context: &str,
    ) {
        assert_eq!(actual.len(), expected.len(), "{context} length");
        assert_own_order(actual, context);
        if matches!(comparison, ScanComparison::Exact) {
            assert_eq!(actual, expected, "{context}");
            return;
        }
        let row_count = match comparison {
            ScanComparison::Exact => 0,
            ScanComparison::F32 { query, rows } => rows.len() / query.len(),
            ScanComparison::F16 { query, rows } => rows.len() / query.len(),
        };
        let reference = naive_top_k(reference_request, row_count);
        let mut rank_by_id = vec![usize::MAX; row_count];
        for (rank, candidate) in reference.iter().enumerate() {
            rank_by_id[candidate.row_id] = rank;
        }
        for (position, (expected_candidate, actual_candidate)) in
            expected.iter().zip(actual).enumerate()
        {
            let tolerance = score_bound(comparison, expected_candidate.row_id)
                .max(score_bound(comparison, actual_candidate.row_id));
            let error =
                (f64::from(actual_candidate.score) - f64::from(expected_candidate.score)).abs();
            assert!(
                error <= tolerance,
                "{context} score position {position}: expected={expected_candidate:?} actual={actual_candidate:?} error={error:?} tolerance={tolerance:?}"
            );
            let actual_rank = rank_by_id[actual_candidate.row_id];
            let (cluster_start, cluster_end) = reference_cluster(&reference, position, comparison);
            assert!(
                actual_rank >= cluster_start && actual_rank <= cluster_end,
                "{context} id escaped near-tie cluster at position {position}: expected={expected_candidate:?} actual={actual_candidate:?} cluster={cluster_start}..={cluster_end} actual_rank={actual_rank}"
            );
            if actual_rank >= expected.len() && !expected.is_empty() {
                let boundary = &reference[expected.len() - 1];
                let exchanged = &reference[actual_rank];
                let boundary_tolerance = score_bound(comparison, boundary.row_id);
                assert!(
                    (f64::from(exchanged.score) - f64::from(boundary.score)).abs()
                        <= boundary_tolerance,
                    "{context} k-boundary exchange exceeded its bound: kth={boundary:?} exchanged={exchanged:?} tolerance={boundary_tolerance:?}"
                );
            }
        }
    }

    fn reference_cluster(
        reference: &[super::ScanCandidate],
        position: usize,
        comparison: ScanComparison<'_>,
    ) -> (usize, usize) {
        let mut start = position;
        while start > 0
            && reference_pair_is_near(&reference[start - 1], &reference[start], comparison)
        {
            start -= 1;
        }
        let mut end = position;
        while end + 1 < reference.len()
            && reference_pair_is_near(&reference[end], &reference[end + 1], comparison)
        {
            end += 1;
        }
        (start, end)
    }

    fn reference_pair_is_near(
        left: &super::ScanCandidate,
        right: &super::ScanCandidate,
        comparison: ScanComparison<'_>,
    ) -> bool {
        let bound = score_bound(comparison, left.row_id).max(score_bound(comparison, right.row_id));
        (f64::from(left.score) - f64::from(right.score)).abs() <= 2.0 * bound
    }

    fn score_bound(comparison: ScanComparison<'_>, row_id: usize) -> f64 {
        match comparison {
            ScanComparison::Exact => 0.0,
            ScanComparison::F32 { query, rows } => {
                rows.get(row_id * query.len()..(row_id + 1) * query.len())
                    .unwrap_or_default()
                    .iter()
                    .zip(query)
                    .map(|(&row, &query)| f64::from(row).abs() * f64::from(query).abs())
                    .sum::<f64>()
                    * 1.0e-5
            }
            ScanComparison::F16 { query, rows } => {
                rows.get(row_id * query.len()..(row_id + 1) * query.len())
                    .unwrap_or_default()
                    .iter()
                    .zip(query)
                    .map(|(&row, &query)| f16_to_f64(row).abs() * f16_to_f64(query).abs())
                    .sum::<f64>()
                    * 1.0e-5
            }
        }
    }

    fn assert_own_order(candidates: &[super::ScanCandidate], context: &str) {
        for pair in candidates.windows(2) {
            let left = pair[0];
            let right = pair[1];
            assert!(
                left.score.total_cmp(&right.score).is_ge(),
                "{context} output is not descending: left={left:?} right={right:?}"
            );
            if left.score.to_bits() == right.score.to_bits() {
                assert!(
                    left.row_id < right.row_id,
                    "{context} bitwise tie is not ascending by row id: left={left:?} right={right:?}"
                );
            }
        }
    }

    fn random_finite_f16(random: &mut impl Rng) -> u16 {
        let sign = if random.random::<bool>() { 0x8000 } else { 0 };
        let exponent = random.random_range(0_u16..=30) << 10;
        let fraction = random.random_range(0_u16..=0x03ff);
        sign | exponent | fraction
    }

    fn f16_to_f64(bits: u16) -> f64 {
        let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
        let exponent = (bits >> 10) & 0x1f;
        let fraction = bits & 0x03ff;
        match exponent {
            0 => sign * f64::from(fraction) * 2.0_f64.powi(-24),
            _ => {
                sign * (1.0 + f64::from(fraction) / 1_024.0)
                    * 2.0_f64.powi(i32::from(exponent) - 15)
            }
        }
    }

    #[test]
    fn prop_parallel_equals_single_thread() {
        let (_directory, store) = query_store();
        let mut random =
            crate::test_support::seeded_rng("scan::prop_parallel_equals_single_thread");
        let cases = std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(256);
        for case in 0..cases {
            let dimension = random.random_range(1..=65);
            let row_count = random.random_range(1..=576);
            let k = random.random_range(1..=row_count);
            let query = (0..dimension)
                .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                .collect::<Vec<_>>();
            let rows = F32Rows::new(if case.is_multiple_of(9) {
                vec![1.0_f32; row_count * dimension]
            } else {
                (0..row_count * dimension)
                    .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                    .collect::<Vec<_>>()
            });
            let request = ScanRequest {
                query: ScanQuery::F32(&query),
                rows: ScanRows::F32RowMajor(&rows),
                row_mask: None,
            };
            let single = pooled_top_k(&store, request, k, ScanOptions { thread_budget: 1 })
                .expect("single-thread scan");
            let requested = case % 12 + 1;
            let parallel = pooled_top_k(
                &store,
                request,
                k,
                ScanOptions {
                    thread_budget: requested,
                },
            )
            .expect("parallel scan");
            assert_eq!(
                parallel.candidates.first(),
                single.candidates.first(),
                "case {case} thread {requested} first candidate"
            );
            assert_eq!(
                parallel.candidates, single.candidates,
                "case {case} thread {requested}"
            );
            assert_eq!(
                (parallel.stats.dims_touched, parallel.stats.bytes_read),
                (single.stats.dims_touched, single.stats.bytes_read),
                "case {case} thread {requested} counters"
            );
        }
    }

    #[test]
    fn row_major_scan_reports_one_exhaustive_payload_pass() {
        let (_directory, store) = query_store();
        let dimension = 37;
        let row_count = 137;
        let query = vec![1.0_f32; dimension];
        let rows = F32Rows::new(vec![0.5_f32; row_count * dimension]);
        let outcome = pooled_top_k(
            &store,
            ScanRequest {
                query: ScanQuery::F32(&query),
                rows: ScanRows::F32RowMajor(&rows),
                row_mask: None,
            },
            10,
            ScanOptions { thread_budget: 1 },
        )
        .expect("exhaustive scan");
        assert_eq!(outcome.stats.dims_touched, (row_count * dimension) as u64);
        assert_eq!(
            outcome.stats.bytes_read,
            (row_count * dimension * std::mem::size_of::<f32>()) as u64,
            "row-major exhaustive scan must report every loaded payload byte"
        );
    }

    #[test]
    fn explicit_thread_budget_is_honoured_up_to_physical_cap() {
        let (_directory, store) = query_store();
        let capacity = super::physical_thread_capacity().expect("physical thread capacity");
        let row_count = capacity.max(8) * 64;
        let query = [1.0_f32];
        let rows = F32Rows::new(vec![1.0_f32; row_count]);
        for requested in 1..=capacity.saturating_add(2) {
            let outcome = pooled_top_k(
                &store,
                ScanRequest {
                    query: ScanQuery::F32(&query),
                    rows: ScanRows::F32RowMajor(&rows),
                    row_mask: None,
                },
                1,
                ScanOptions {
                    thread_budget: requested,
                },
            )
            .expect("parallel scan");
            assert_eq!(outcome.stats.threads_used, requested.min(capacity));
        }
    }

    #[test]
    fn zero_k_returns_no_candidates() {
        let query = [0x3c00_u16, 0xbc00];
        let rows = [0x3c00_u16, 0xbc00, 0x4000, 0x3800];
        let request = ScanRequest {
            query: ScanQuery::F16(&query),
            rows: ScanRows::F16RowMajor(&rows),
            row_mask: None,
        };

        assert!(top_k(request, 0).expect("valid zero-k scan").is_empty());
    }

    #[test]
    fn dimension_one_scans_exactly() {
        let query = [0x3c00_u16];
        let rows = [0x4000_u16, 0x3c00, 0xbc00];
        let request = ScanRequest {
            query: ScanQuery::F16(&query),
            rows: ScanRows::F16RowMajor(&rows),
            row_mask: None,
        };

        let hits = top_k(request, 3).expect("valid one-dimensional scan");

        assert_eq!(
            hits.iter().map(|hit| hit.row_id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(
            hits.iter().map(|hit| hit.score).collect::<Vec<_>>(),
            [2.0, 1.0, -1.0]
        );
    }

    #[test]
    fn k_larger_than_row_count_returns_all_rows() {
        let query = [1.0_f32, 0.5];
        let rows = F32Rows::new(vec![1.0_f32, 0.0, 0.0, 1.0, -1.0, 0.0]);
        let request = ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&rows),
            row_mask: None,
        };

        let hits = top_k(request, usize::MAX).expect("valid oversized-k scan");

        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits.iter().map(|hit| hit.row_id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn single_row_returns_that_row() {
        let query = [0.25_f32, -0.5, 1.0];
        let rows = F32Rows::new(vec![2.0_f32, 1.0, -1.0]);
        let request = ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32RowMajor(&rows),
            row_mask: None,
        };

        assert_eq!(
            top_k(request, 1).expect("valid single-row scan"),
            [super::ScanCandidate {
                row_id: 0,
                score: -1.0,
            }]
        );
    }

    fn mask_fixture<'a>(
        query: &'a [f32],
        rows: &'a F32Rows,
        row_mask: Option<&'a RoaringBitmap>,
    ) -> ScanRequest<'a> {
        ScanRequest {
            query: ScanQuery::F32(query),
            rows: ScanRows::F32RowMajor(rows),
            row_mask,
        }
    }

    #[test]
    fn empty_row_mask_returns_nothing() {
        let query = [1.0_f32, 0.0];
        let rows = F32Rows::new(vec![1.0_f32, 0.0, 2.0, 0.0, 3.0, 0.0]);
        let mask = RoaringBitmap::new();

        assert!(
            top_k(mask_fixture(&query, &rows, Some(&mask)), 3)
                .expect("valid empty mask")
                .is_empty()
        );
    }

    #[test]
    fn full_row_mask_equals_unmasked() {
        let query = [1.0_f32, 0.0];
        let rows = F32Rows::new(vec![1.0_f32, 0.0, 2.0, 0.0, 3.0, 0.0]);
        let mask = RoaringBitmap::from_iter(0_u32..3);

        assert_eq!(
            top_k(mask_fixture(&query, &rows, Some(&mask)), 3).expect("valid full mask"),
            top_k(mask_fixture(&query, &rows, None), 3).expect("valid unmasked scan")
        );
    }

    #[test]
    fn alternating_row_mask_returns_exactly_allowed_ids() {
        let query = [1.0_f32, 0.0];
        let rows = F32Rows::new(vec![
            0.0_f32, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0, 4.0, 0.0, 5.0, 0.0,
        ]);
        let mask = RoaringBitmap::from_iter([0_u32, 2, 4]);

        let hits =
            top_k(mask_fixture(&query, &rows, Some(&mask)), 6).expect("valid alternating mask");
        let mut ids = hits.iter().map(|hit| hit.row_id).collect::<Vec<_>>();
        ids.sort_unstable();

        assert_eq!(ids, [0, 2, 4]);
    }

    #[test]
    fn scan_validation_errors_are_typed_and_actionable() {
        assert_eq!(
            Int8Factors::new(f32::NAN, 0.0),
            Err(super::Int8FactorsError::NonFiniteScale)
        );
        assert_eq!(
            Int8Factors::new(-1.0, 0.0),
            Err(super::Int8FactorsError::NegativeScale)
        );
        assert_eq!(
            Int8Factors::new(1.0, f32::INFINITY),
            Err(super::Int8FactorsError::NonFiniteOffset)
        );
        for error in [
            super::Int8FactorsError::NonFiniteScale,
            super::Int8FactorsError::NegativeScale,
            super::Int8FactorsError::NonFiniteOffset,
        ] {
            assert!(!error.to_string().is_empty());
        }

        let f32_query = [1.0_f32, 2.0];
        let f16_query = [0x3c00_u16, 0x4000];
        let int8_query = prepare_int8_query(&f32_query).expect("finite Int8 query");
        let bit4_query = prepare_bit4_query(&f32_query, 7).expect("finite Bit4 query");
        let int8_factor = Int8Factors::new(1.0, 0.0).expect("valid factor");
        let mut bit4_code = [0_u8; 1];
        let bit4_factor = quantize_bit4(&f32_query, &mut bit4_code).expect("valid factor");
        let partial_f32_rows = F32Rows::new(vec![1.0]);
        let finite_f32_rows = F32Rows::new(vec![1.0]);
        let overflowing_f32_rows = F32Rows::new(vec![2.0]);

        let errors = [
            top_k(
                ScanRequest {
                    query: ScanQuery::F16(&[]),
                    rows: ScanRows::F16RowMajor(&[]),
                    row_mask: None,
                },
                1,
            )
            .expect_err("zero dimension"),
            top_k(
                ScanRequest {
                    query: ScanQuery::F32(&f32_query),
                    rows: ScanRows::F32RowMajor(&partial_f32_rows),
                    row_mask: None,
                },
                1,
            )
            .expect_err("partial f32 row"),
            top_k(
                ScanRequest {
                    query: ScanQuery::F16(&f16_query),
                    rows: ScanRows::F16RowMajor(&[0x3c00]),
                    row_mask: None,
                },
                1,
            )
            .expect_err("partial f16 row"),
            top_k(
                ScanRequest {
                    query: ScanQuery::Int8(&int8_query),
                    rows: ScanRows::Int8RowMajor {
                        codes: &[1],
                        factors: &[],
                    },
                    row_mask: None,
                },
                1,
            )
            .expect_err("partial Int8 row"),
            top_k(
                ScanRequest {
                    query: ScanQuery::Int8(&int8_query),
                    rows: ScanRows::Int8RowMajor {
                        codes: &[1, 2],
                        factors: &[],
                    },
                    row_mask: None,
                },
                1,
            )
            .expect_err("missing Int8 factor"),
            top_k(
                ScanRequest {
                    query: ScanQuery::Bit4(&bit4_query),
                    rows: ScanRows::Bit4RowMajor {
                        codes: &[0xff, 0xff],
                        factors: &[bit4_factor],
                    },
                    row_mask: None,
                },
                1,
            )
            .expect_err("partial Bit4 row"),
            top_k(
                ScanRequest {
                    query: ScanQuery::Bit4(&bit4_query),
                    rows: ScanRows::Bit4RowMajor {
                        codes: &bit4_code,
                        factors: &[],
                    },
                    row_mask: None,
                },
                1,
            )
            .expect_err("missing Bit4 factor"),
            top_k(
                ScanRequest {
                    query: ScanQuery::F32(&f32_query),
                    rows: ScanRows::Int8RowMajor {
                        codes: &[1, 2],
                        factors: &[int8_factor],
                    },
                    row_mask: None,
                },
                1,
            )
            .expect_err("scheme mismatch"),
            top_k(
                ScanRequest {
                    query: ScanQuery::F32(&[f32::NAN]),
                    rows: ScanRows::F32RowMajor(&finite_f32_rows),
                    row_mask: None,
                },
                1,
            )
            .expect_err("non-finite input"),
            top_k(
                ScanRequest {
                    query: ScanQuery::F32(&[f32::MAX]),
                    rows: ScanRows::F32RowMajor(&overflowing_f32_rows),
                    row_mask: None,
                },
                1,
            )
            .expect_err("non-finite score"),
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
        let empty_rows = F32Rows::new(Vec::new());
        let mut stream = super::ExactCandidateStream {
            request: mask_fixture(&f32_query, &empty_rows, None),
            yielded: usize::MAX,
        };
        assert_eq!(stream.pull(1), Err(super::ScanError::ArithmeticOverflow));
        assert!(
            !super::ScanError::from(crate::quant::QuantError::EmptyVector)
                .to_string()
                .is_empty()
        );
    }

    #[test]
    fn row_masks_filter_every_encoded_scheme() {
        let mask = RoaringBitmap::from_iter([1_u32]);

        let f16_query = [0x3c00_u16];
        let f16_rows = [0x3c00_u16, 0x4000];
        assert_eq!(
            top_k(
                ScanRequest {
                    query: ScanQuery::F16(&f16_query),
                    rows: ScanRows::F16RowMajor(&f16_rows),
                    row_mask: Some(&mask),
                },
                2,
            )
            .expect("masked f16 scan")[0]
                .row_id,
            1
        );

        let query_values = [1.0_f32];
        let int8_query = prepare_int8_query(&query_values).expect("finite Int8 query");
        let int8_factors = [
            Int8Factors::new(1.0, 0.0).expect("valid factor"),
            Int8Factors::new(1.0, 0.0).expect("valid factor"),
        ];
        assert_eq!(
            top_k(
                ScanRequest {
                    query: ScanQuery::Int8(&int8_query),
                    rows: ScanRows::Int8RowMajor {
                        codes: &[1, 2],
                        factors: &int8_factors,
                    },
                    row_mask: Some(&mask),
                },
                2,
            )
            .expect("masked Int8 scan")[0]
                .row_id,
            1
        );

        let bit4_query = prepare_bit4_query(&query_values, 9).expect("finite Bit4 query");
        let mut bit4_code = [0_u8; 1];
        let factor = quantize_bit4(&query_values, &mut bit4_code).expect("valid factor");
        let bit4_codes = [bit4_code[0], bit4_code[0]];
        assert_eq!(
            top_k(
                ScanRequest {
                    query: ScanQuery::Bit4(&bit4_query),
                    rows: ScanRows::Bit4RowMajor {
                        codes: &bit4_codes,
                        factors: &[factor, factor],
                    },
                    row_mask: Some(&mask),
                },
                2,
            )
            .expect("masked Bit4 scan")[0]
                .row_id,
            1
        );
    }
}
