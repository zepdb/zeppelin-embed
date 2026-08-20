//! Recall-retention sweeps with exact rescore byte accounting.

use zeppelin_embed::quant::{
    Bit4Factors, Int8Vec, QuantError, QuantScheme, RescoreError, SearchByteCounts, est_dot_bit4,
    prepare_bit4_query, prepare_int8_query, quantize_bit4, quantize_int8, rescore_top_k,
};

/// Dataset generators and standard binary-vector loaders.
pub mod datasets;

use datasets::Dataset;

/// Recall target used for the default oversample recommendation.
pub const DEFAULT_RECALL_TARGET: f64 = 0.95;

/// One scheme/oversample point in a retention curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecallPoint {
    /// Persisted quantization scheme.
    pub scheme: QuantScheme,
    /// Coarse frontier size divided by exact top-k.
    pub oversample: usize,
    /// Mean exact-neighbor retention at the requested `k`.
    pub recall_at_10: f64,
    /// Mean stored-data bytes touched per query.
    pub bytes_per_query: SearchByteCounts,
}

/// Smallest measured oversample reaching one recall target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecallRecommendation {
    /// Quantization scheme.
    pub scheme: QuantScheme,
    /// Recall threshold used for selection.
    pub target: f64,
    /// Smallest passing oversample, or `None` when the sweep never passed.
    pub oversample: Option<usize>,
    /// Recall at the selected point.
    pub measured_recall: Option<f64>,
}

/// Complete retention sweep for one dataset.
#[derive(Clone, Debug, PartialEq)]
pub struct RecallReport {
    /// Dataset provenance label.
    pub dataset_name: String,
    /// Coordinate count.
    pub dimension: usize,
    /// Corpus row count.
    pub row_count: usize,
    /// Query count.
    pub query_count: usize,
    /// Exact result count.
    pub k: usize,
    /// Scheme-by-oversample retention points.
    pub points: Vec<RecallPoint>,
    /// Smallest measured oversample reaching 95% recall, per scheme.
    pub recommendations: Vec<RecallRecommendation>,
}

/// Recall harness validation or quantization failure.
#[derive(Debug)]
pub enum RecallError {
    /// Exact top-k was zero or larger than the corpus.
    InvalidK {
        /// Requested result count.
        k: usize,
        /// Available corpus rows.
        rows: usize,
    },
    /// The sweep was empty or contained zero.
    InvalidOversamples,
    /// Supplied ground truth did not match the query shape or corpus.
    InvalidGroundTruth {
        /// Actionable mismatch explanation.
        reason: String,
    },
    /// A quantizer rejected the dataset.
    Quant(QuantError),
    /// The exact rescore helper rejected a shape or counter.
    Rescore(RescoreError),
    /// Internal stored-byte counts differed between equal-shape queries.
    InconsistentByteCounts,
}

impl std::fmt::Display for RecallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidK { k, rows } => {
                write!(formatter, "recall k={k} is invalid for {rows} corpus rows")
            }
            Self::InvalidOversamples => {
                formatter.write_str("recall oversample sweep must contain only positive values")
            }
            Self::InvalidGroundTruth { reason } => {
                write!(formatter, "invalid recall ground truth: {reason}")
            }
            Self::Quant(error) => write!(formatter, "quantization failed: {error}"),
            Self::Rescore(error) => write!(formatter, "rescore failed: {error}"),
            Self::InconsistentByteCounts => {
                formatter.write_str("equal-shape recall queries reported different byte counts")
            }
        }
    }
}

impl std::error::Error for RecallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Quant(error) => Some(error),
            Self::Rescore(error) => Some(error),
            _ => None,
        }
    }
}

impl From<QuantError> for RecallError {
    fn from(error: QuantError) -> Self {
        Self::Quant(error)
    }
}

impl From<RescoreError> for RecallError {
    fn from(error: RescoreError) -> Self {
        Self::Rescore(error)
    }
}

/// Runs every required quantization scheme across an oversample sweep.
///
/// Ground truth is consumed when supplied; otherwise exact f64 brute force is
/// computed locally. Coarse rows are encoded once per scheme. Every query then
/// scores all encoded rows, selects `k * oversample`, and calls the core exact
/// rescore helper. No wall-clock timing is performed.
///
/// # Errors
///
/// Returns [`RecallError`] for invalid controls, malformed ground truth, a
/// rejected vector, or rescore shape/counter failure.
pub fn run_recall(
    dataset: &Dataset,
    oversamples: &[usize],
    k: usize,
) -> Result<RecallReport, RecallError> {
    let row_count = dataset.vectors.rows();
    if k == 0 || k > row_count {
        return Err(RecallError::InvalidK { k, rows: row_count });
    }
    if oversamples.is_empty() || oversamples.contains(&0) {
        return Err(RecallError::InvalidOversamples);
    }
    let truth = ground_truth(dataset, k)?;
    let schemes = [QuantScheme::Bit4, QuantScheme::Int8];
    let mut points = Vec::with_capacity(schemes.len() * oversamples.len());
    for scheme in schemes {
        let encoded = EncodedRows::new(scheme, &dataset.vectors.values, dataset.vectors.dimension)?;
        for &oversample in oversamples {
            let mut retained = 0_usize;
            let mut expected_bytes = None;
            for (query_index, query) in dataset
                .queries
                .values
                .chunks_exact(dataset.queries.dimension)
                .enumerate()
            {
                let coarse_scores = encoded.score(query, query_index as u64)?;
                let result = rescore_top_k(
                    query,
                    &dataset.vectors.values,
                    dataset.vectors.dimension,
                    &coarse_scores,
                    k,
                    oversample,
                    encoded.stored_bytes_per_row(),
                )?;
                if let Some(bytes) = expected_bytes {
                    if bytes != result.bytes {
                        return Err(RecallError::InconsistentByteCounts);
                    }
                } else {
                    expected_bytes = Some(result.bytes);
                }
                let expected =
                    truth
                        .get(query_index)
                        .ok_or_else(|| RecallError::InvalidGroundTruth {
                            reason: format!("missing query {query_index}"),
                        })?;
                retained += result
                    .hits
                    .iter()
                    .filter(|hit| expected.contains(&hit.row_index))
                    .count();
            }
            let denominator = dataset.queries.rows() * k;
            let bytes_per_query =
                expected_bytes.ok_or_else(|| RecallError::InvalidGroundTruth {
                    reason: String::from("dataset contains no queries"),
                })?;
            points.push(RecallPoint {
                scheme,
                oversample,
                recall_at_10: retained as f64 / denominator as f64,
                bytes_per_query,
            });
        }
    }
    let recommendations = schemes
        .into_iter()
        .map(|scheme| recommendation(&points, scheme, DEFAULT_RECALL_TARGET))
        .collect();
    Ok(RecallReport {
        dataset_name: dataset.name.clone(),
        dimension: dataset.vectors.dimension,
        row_count,
        query_count: dataset.queries.rows(),
        k,
        points,
        recommendations,
    })
}

fn recommendation(
    points: &[RecallPoint],
    scheme: QuantScheme,
    target: f64,
) -> RecallRecommendation {
    let selected = points
        .iter()
        .filter(|point| point.scheme == scheme && point.recall_at_10 >= target)
        .min_by_key(|point| point.oversample);
    RecallRecommendation {
        scheme,
        target,
        oversample: selected.map(|point| point.oversample),
        measured_recall: selected.map(|point| point.recall_at_10),
    }
}

fn ground_truth(dataset: &Dataset, k: usize) -> Result<Vec<Vec<usize>>, RecallError> {
    if let Some(truth) = &dataset.ground_truth {
        if truth.len() != dataset.queries.rows() {
            return Err(RecallError::InvalidGroundTruth {
                reason: format!(
                    "{} query lists for {} queries",
                    truth.len(),
                    dataset.queries.rows()
                ),
            });
        }
        for (query_index, neighbors) in truth.iter().enumerate() {
            if neighbors.len() < k
                || neighbors
                    .iter()
                    .any(|index| *index >= dataset.vectors.rows())
            {
                return Err(RecallError::InvalidGroundTruth {
                    reason: format!("query {query_index} has short or out-of-range neighbors"),
                });
            }
        }
        return Ok(truth
            .iter()
            .map(|neighbors| neighbors[..k].to_vec())
            .collect());
    }

    Ok(dataset
        .queries
        .values
        .chunks_exact(dataset.queries.dimension)
        .map(|query| {
            let scores = dataset
                .vectors
                .values
                .chunks_exact(dataset.vectors.dimension)
                .map(|row| dot_f64(query, row))
                .collect::<Vec<_>>();
            top_indices(&scores, k)
        })
        .collect())
}

fn dot_f64(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum()
}

fn top_indices(scores: &[f64], k: usize) -> Vec<usize> {
    let mut ranked = scores.iter().copied().enumerate().collect::<Vec<_>>();
    ranked.sort_unstable_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.into_iter().take(k).map(|(index, _)| index).collect()
}

enum EncodedRows {
    Int8 {
        dimension: usize,
        codes: Vec<i8>,
        factors: Vec<(f32, f32)>,
    },
    Bit4 {
        dimension: usize,
        codes: Vec<u8>,
        factors: Vec<Bit4Factors>,
    },
}

impl EncodedRows {
    fn new(scheme: QuantScheme, values: &[f32], dimension: usize) -> Result<Self, QuantError> {
        match scheme {
            QuantScheme::Int8 => {
                let mut codes = Vec::with_capacity(values.len());
                let mut factors = Vec::with_capacity(values.len() / dimension);
                for row in values.chunks_exact(dimension) {
                    let start = codes.len();
                    codes.resize(start + dimension, 0_i8);
                    let (scale, offset) = quantize_int8(row, &mut codes[start..])?;
                    factors.push((scale, offset));
                }
                Ok(Self::Int8 {
                    dimension,
                    codes,
                    factors,
                })
            }
            QuantScheme::Bit4 => {
                let stride = dimension.div_ceil(2);
                let mut codes = Vec::with_capacity(values.len() / dimension * stride);
                let mut factors = Vec::with_capacity(values.len() / dimension);
                for row in values.chunks_exact(dimension) {
                    let start = codes.len();
                    codes.resize(start + stride, 0_u8);
                    factors.push(quantize_bit4(row, &mut codes[start..])?);
                }
                Ok(Self::Bit4 {
                    dimension,
                    codes,
                    factors,
                })
            }
            QuantScheme::F32 | QuantScheme::F16 => Err(QuantError::EmptyVector),
        }
    }

    fn score(&self, query: &[f32], seed: u64) -> Result<Vec<f32>, QuantError> {
        match self {
            Self::Int8 {
                dimension,
                codes,
                factors,
            } => {
                let prepared = prepare_int8_query(query)?;
                codes
                    .chunks_exact(*dimension)
                    .zip(factors)
                    .map(|(codes, &(scale, offset))| {
                        zeppelin_embed::quant::dot_int8_query(
                            &prepared,
                            Int8Vec {
                                codes,
                                scale,
                                offset,
                            },
                        )
                    })
                    .collect()
            }
            Self::Bit4 {
                dimension,
                codes,
                factors,
            } => {
                let prepared = prepare_bit4_query(query, seed ^ 0x04b4_7004)?;
                codes
                    .chunks_exact(dimension.div_ceil(2))
                    .zip(factors)
                    .map(|(codes, &factors)| est_dot_bit4(&prepared, codes, factors))
                    .collect()
            }
        }
    }

    fn stored_bytes_per_row(&self) -> usize {
        match self {
            Self::Int8 { dimension, .. } => dimension + 2 * std::mem::size_of::<f32>(),
            Self::Bit4 { dimension, .. } => {
                dimension.div_ceil(2) + std::mem::size_of::<Bit4Factors>()
            }
        }
    }
}
