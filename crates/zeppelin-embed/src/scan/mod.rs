//! Exact flat scanning over row-major and PDX vector layouts.

pub mod pdx;
pub(crate) mod topk;

use crate::kernels;
use crate::quant::{
    Bit4Factors, Bit4Query, Int8Query, Int8Vec, QuantError, QuantScheme, dot_int8_query,
    est_dot_bit4_batch,
};

use pdx::{PdxError, PdxMatrix};
use topk::BoundedTopK;

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
    F32RowMajor(&'a [f32]),
    /// Dimension-major full-precision PDX rows.
    F32Pdx(&'a PdxMatrix),
    /// Contiguous row-major IEEE-f16 bit patterns.
    F16RowMajor(&'a [u16]),
    /// Dimension-major IEEE-f16 PDX rows.
    F16Pdx(&'a PdxMatrix),
    /// Contiguous row-major signed-byte codes and affine row factors.
    Int8RowMajor {
        /// One signed code per coordinate and row.
        codes: &'a [i8],
        /// One validated affine factor pair per row.
        factors: &'a [Int8Factors],
    },
    /// Dimension-major signed-byte PDX codes and affine row factors.
    Int8Pdx {
        /// PDX-encoded signed-byte codes.
        codes: &'a PdxMatrix,
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
    /// Dimension-major packed four-bit PDX codes and estimator factors.
    Bit4Pdx {
        /// PDX-encoded packed four-bit codes.
        codes: &'a PdxMatrix,
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
    /// Query and PDX row dimensions differed.
    DimensionMismatch {
        /// Query dimension.
        query: usize,
        /// Row dimension.
        rows: usize,
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
    /// PDX geometry or decoding was invalid.
    Pdx(PdxError),
    /// A quantization scorer rejected encoded input.
    Quant(QuantError),
    /// Candidate-window arithmetic overflowed `usize`.
    ArithmeticOverflow,
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
            Self::DimensionMismatch { query, rows } => write!(
                formatter,
                "scan dimension mismatch: query={query}, rows={rows}"
            ),
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
            Self::Pdx(error) => write!(formatter, "scan PDX error: {error}"),
            Self::Quant(error) => write!(formatter, "scan quantization error: {error}"),
            Self::ArithmeticOverflow => {
                formatter.write_str("scan candidate-window arithmetic overflowed")
            }
        }
    }
}

impl std::error::Error for ScanError {}

impl From<PdxError> for ScanError {
    fn from(error: PdxError) -> Self {
        Self::Pdx(error)
    }
}

impl From<QuantError> for ScanError {
    fn from(error: QuantError) -> Self {
        Self::Quant(error)
    }
}

/// Returns the exact best `k` candidates in permanent parity order.
///
/// Scores are ordered descending. Equal scores are always ordered by ascending
/// row id. This is a permanent public contract shared by row-major, PDX,
/// streaming, early-abandon, and parallel implementations.
///
/// # Errors
///
/// Returns [`ScanError`] for invalid shapes, non-finite inputs, or non-finite
/// scores.
pub fn top_k(request: ScanRequest<'_>, k: usize) -> Result<Vec<ScanCandidate>, ScanError> {
    candidate_stream(request).pull(k)
}

fn scan_top_k(request: ScanRequest<'_>, k: usize) -> Result<Vec<ScanCandidate>, ScanError> {
    match (request.query, request.rows) {
        (ScanQuery::F32(query), ScanRows::F32RowMajor(rows)) => {
            scan_f32(query, rows, request.row_mask, k)
        }
        (ScanQuery::F32(query), ScanRows::F32Pdx(matrix)) => {
            if matrix.dimension() != query.len() {
                return Err(ScanError::DimensionMismatch {
                    query: query.len(),
                    rows: matrix.dimension(),
                });
            }
            let rows = matrix.decode_f32()?;
            scan_f32(query, &rows, request.row_mask, k)
        }
        (ScanQuery::F16(query), ScanRows::F16RowMajor(rows)) => {
            scan_f16(query, rows, request.row_mask, k)
        }
        (ScanQuery::F16(query), ScanRows::F16Pdx(matrix)) => {
            if matrix.dimension() != query.len() {
                return Err(ScanError::DimensionMismatch {
                    query: query.len(),
                    rows: matrix.dimension(),
                });
            }
            let rows = matrix.decode_f16()?;
            scan_f16(query, &rows, request.row_mask, k)
        }
        (ScanQuery::Int8(query), ScanRows::Int8RowMajor { codes, factors }) => {
            scan_int8(query, codes, factors, request.row_mask, k)
        }
        (ScanQuery::Int8(query), ScanRows::Int8Pdx { codes, factors }) => {
            if codes.dimension() != query.len() {
                return Err(ScanError::DimensionMismatch {
                    query: query.len(),
                    rows: codes.dimension(),
                });
            }
            let rows = codes.decode_int8()?;
            scan_int8(query, &rows, factors, request.row_mask, k)
        }
        (ScanQuery::Bit4(query), ScanRows::Bit4RowMajor { codes, factors }) => {
            scan_bit4(query, codes, factors, request.row_mask, k)
        }
        (ScanQuery::Bit4(query), ScanRows::Bit4Pdx { codes, factors }) => {
            if codes.dimension() != query.len() {
                return Err(ScanError::DimensionMismatch {
                    query: query.len(),
                    rows: codes.dimension(),
                });
            }
            let rows = codes.decode_bit4()?;
            scan_bit4(query, &rows, factors, request.row_mask, k)
        }
        (query, rows) => Err(ScanError::SchemeMismatch {
            query: query.scheme(),
            rows: rows.scheme(),
        }),
    }
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
            Self::F32RowMajor(_) | Self::F32Pdx(_) => QuantScheme::F32,
            Self::F16RowMajor(_) | Self::F16Pdx(_) => QuantScheme::F16,
            Self::Int8RowMajor { .. } | Self::Int8Pdx { .. } => QuantScheme::Int8,
            Self::Bit4RowMajor { .. } | Self::Bit4Pdx { .. } => QuantScheme::Bit4,
        }
    }
}

fn scan_f32(
    query: &[f32],
    rows: &[f32],
    row_mask: Option<&roaring::RoaringBitmap>,
    k: usize,
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
    if let Some((index, _)) = query
        .iter()
        .chain(rows)
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(ScanError::NonFiniteInput { index });
    }

    let row_count = rows.len() / query.len();
    let mut selected = BoundedTopK::new(k.min(row_count));
    for (row_id, row) in rows.chunks_exact(query.len()).enumerate() {
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
    for (row_id, row) in rows.chunks_exact(query.len()).enumerate() {
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
    for (row_id, (row, factor)) in codes.chunks_exact(dimension).zip(factors).enumerate() {
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
    let mut scores = vec![0.0_f32; row_count];
    est_dot_bit4_batch(query, codes, factors, &mut scores)?;
    let mut selected = BoundedTopK::new(k.min(row_count));
    for (row_id, score) in scores.into_iter().enumerate() {
        if row_mask.is_some_and(|mask| u32::try_from(row_id).map_or(true, |id| !mask.contains(id)))
        {
            continue;
        }
        if !score.is_finite() {
            return Err(ScanError::NonFiniteScore { row_id });
        }
        selected.push(ScanCandidate { row_id, score });
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

    use super::pdx::PdxMatrix;
    use super::{
        CandidateStream, Int8Factors, ScanQuery, ScanRequest, ScanRows, candidate_stream, top_k,
    };
    use crate::quant::{prepare_bit4_query, prepare_int8_query, quantize_bit4};

    #[test]
    fn all_identical_vectors_tie_by_ascending_row_id() {
        let query = [1.0_f32, -2.0];
        let rows = [1.0_f32, -2.0, 1.0, -2.0, 1.0, -2.0];
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
    fn empty_row_set_returns_no_candidates() {
        let query = [1.0_f32, -2.0];
        let rows = PdxMatrix::encode_f32(&[], query.len()).expect("empty PDX matrix");
        let request = ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32Pdx(&rows),
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
        let rows = (0..127 * query.len())
            .map(|_| random.random_range(-2.0_f32..=2.0_f32))
            .collect::<Vec<_>>();
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
    fn prop_pdx_scan_equals_naive_topk() {
        let mut random = crate::test_support::seeded_rng("scan::prop_pdx_scan_equals_naive_topk");
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

            let (row_major, pdx) = match case % 3 {
                0 => {
                    let query = (0..dimension)
                        .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                        .collect::<Vec<_>>();
                    let rows = (0..row_count * dimension)
                        .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                        .collect::<Vec<_>>();
                    let pdx = PdxMatrix::encode_f32(&rows, dimension).expect("valid f32 PDX");
                    (
                        top_k(
                            ScanRequest {
                                query: ScanQuery::F32(&query),
                                rows: ScanRows::F32RowMajor(&rows),
                                row_mask: None,
                            },
                            k,
                        )
                        .expect("valid row-major f32 scan"),
                        top_k(
                            ScanRequest {
                                query: ScanQuery::F32(&query),
                                rows: ScanRows::F32Pdx(&pdx),
                                row_mask: None,
                            },
                            k,
                        )
                        .expect("valid PDX f32 scan"),
                    )
                }
                1 => {
                    let query_values = (0..dimension)
                        .map(|_| random.random_range(-2.0_f32..=2.0_f32))
                        .collect::<Vec<_>>();
                    let query = prepare_int8_query(&query_values).expect("finite int8 query");
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
                    let pdx = PdxMatrix::encode_int8(&codes, dimension).expect("valid int8 PDX");
                    (
                        top_k(
                            ScanRequest {
                                query: ScanQuery::Int8(&query),
                                rows: ScanRows::Int8RowMajor {
                                    codes: &codes,
                                    factors: &factors,
                                },
                                row_mask: None,
                            },
                            k,
                        )
                        .expect("valid row-major int8 scan"),
                        top_k(
                            ScanRequest {
                                query: ScanQuery::Int8(&query),
                                rows: ScanRows::Int8Pdx {
                                    codes: &pdx,
                                    factors: &factors,
                                },
                                row_mask: None,
                            },
                            k,
                        )
                        .expect("valid PDX int8 scan"),
                    )
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
                    let pdx = PdxMatrix::encode_bit4(&codes, dimension).expect("valid Bit4 PDX");
                    (
                        top_k(
                            ScanRequest {
                                query: ScanQuery::Bit4(&query),
                                rows: ScanRows::Bit4RowMajor {
                                    codes: &codes,
                                    factors: &factors,
                                },
                                row_mask: None,
                            },
                            k,
                        )
                        .expect("valid row-major Bit4 scan"),
                        top_k(
                            ScanRequest {
                                query: ScanQuery::Bit4(&query),
                                rows: ScanRows::Bit4Pdx {
                                    codes: &pdx,
                                    factors: &factors,
                                },
                                row_mask: None,
                            },
                            k,
                        )
                        .expect("valid PDX Bit4 scan"),
                    )
                }
            };

            assert_eq!(pdx, row_major, "case {case}");
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
        let pdx = PdxMatrix::encode_f16(&rows, 1).expect("one-dimensional PDX");
        let request = ScanRequest {
            query: ScanQuery::F16(&query),
            rows: ScanRows::F16Pdx(&pdx),
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
        let rows = [1.0_f32, 0.0, 0.0, 1.0, -1.0, 0.0];
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
        let rows = [2.0_f32, 1.0, -1.0];
        let pdx = PdxMatrix::encode_f32(&rows, query.len()).expect("single-row PDX");
        let request = ScanRequest {
            query: ScanQuery::F32(&query),
            rows: ScanRows::F32Pdx(&pdx),
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
        rows: &'a [f32],
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
        let rows = [1.0_f32, 0.0, 2.0, 0.0, 3.0, 0.0];
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
        let rows = [1.0_f32, 0.0, 2.0, 0.0, 3.0, 0.0];
        let mask = RoaringBitmap::from_iter(0_u32..3);

        assert_eq!(
            top_k(mask_fixture(&query, &rows, Some(&mask)), 3).expect("valid full mask"),
            top_k(mask_fixture(&query, &rows, None), 3).expect("valid unmasked scan")
        );
    }

    #[test]
    fn alternating_row_mask_returns_exactly_allowed_ids() {
        let query = [1.0_f32, 0.0];
        let rows = [
            0.0_f32, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0, 4.0, 0.0, 5.0, 0.0,
        ];
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
                    rows: ScanRows::F32RowMajor(&[1.0]),
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
                    rows: ScanRows::F32RowMajor(&[1.0]),
                    row_mask: None,
                },
                1,
            )
            .expect_err("non-finite input"),
            top_k(
                ScanRequest {
                    query: ScanQuery::F32(&[f32::MAX]),
                    rows: ScanRows::F32RowMajor(&[2.0]),
                    row_mask: None,
                },
                1,
            )
            .expect_err("non-finite score"),
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }

        let pdx = PdxMatrix::encode_f32(&[1.0, 2.0, 3.0], 3).expect("valid PDX");
        assert!(matches!(
            top_k(
                ScanRequest {
                    query: ScanQuery::F32(&f32_query),
                    rows: ScanRows::F32Pdx(&pdx),
                    row_mask: None,
                },
                1,
            ),
            Err(super::ScanError::DimensionMismatch { .. })
        ));
        let mut stream = super::ExactCandidateStream {
            request: mask_fixture(&f32_query, &[], None),
            yielded: usize::MAX,
        };
        assert_eq!(stream.pull(1), Err(super::ScanError::ArithmeticOverflow));
        assert!(
            !super::ScanError::from(crate::quant::QuantError::EmptyVector)
                .to_string()
                .is_empty()
        );
        assert!(
            !super::ScanError::from(super::pdx::PdxError::ZeroDimension)
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
