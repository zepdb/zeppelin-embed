//! Synthetic embedding families and standard fvecs/bvecs readers.

use std::path::{Path, PathBuf};

/// One contiguous row-major f32 matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct DenseMatrix {
    /// Number of coordinates in every row.
    pub dimension: usize,
    /// Contiguous row-major coordinate storage.
    pub values: Vec<f32>,
}

impl DenseMatrix {
    /// Returns the number of complete rows.
    #[must_use]
    pub fn rows(&self) -> usize {
        if self.dimension == 0 {
            0
        } else {
            self.values.len() / self.dimension
        }
    }

    /// Returns one row by zero-based position.
    #[must_use]
    pub fn row(&self, index: usize) -> Option<&[f32]> {
        let start = index.checked_mul(self.dimension)?;
        let end = start.checked_add(self.dimension)?;
        self.values.get(start..end)
    }
}

/// Vectors, queries, and optional authoritative neighbor lists.
#[derive(Clone, Debug, PartialEq)]
pub struct Dataset {
    /// Human-readable provenance name printed in reports.
    pub name: String,
    /// Corpus matrix.
    pub vectors: DenseMatrix,
    /// Query matrix with the same dimension.
    pub queries: DenseMatrix,
    /// Optional exact neighbors per query; absent means brute-force locally.
    pub ground_truth: Option<Vec<Vec<usize>>>,
    /// Synthetic-only cluster labels used to prove density variation.
    pub cluster_labels: Option<Vec<usize>>,
}

impl Dataset {
    /// Builds a validated dataset from vector and query matrices.
    ///
    /// # Errors
    ///
    /// Returns [`DatasetError::InvalidShape`] for empty, inconsistent, or
    /// non-row-aligned matrices.
    pub fn new(
        name: impl Into<String>,
        vectors: DenseMatrix,
        queries: DenseMatrix,
        ground_truth: Option<Vec<Vec<usize>>>,
    ) -> Result<Self, DatasetError> {
        validate_matrices(&vectors, &queries)?;
        Ok(Self {
            name: name.into(),
            vectors,
            queries,
            ground_truth,
            cluster_labels: None,
        })
    }
}

/// Training-free synthetic distribution families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntheticKind {
    /// Near-isotropic unit vectors sampled from a spherical Gaussian.
    Uniform,
    /// Unit vectors whose per-dimension variance spans several orders.
    Anisotropic,
    /// Unequal-density, unequal-spread semantic clusters.
    Clustered,
    /// Random directions with log-normal-like vector magnitudes.
    HeavyTailed,
    /// Unit vectors from a strongly autocorrelated latent process.
    Correlated,
}

impl SyntheticKind {
    /// Stable label used in CLI tables and evidence.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Uniform => "uniform",
            Self::Anisotropic => "anisotropic",
            Self::Clustered => "clustered",
            Self::HeavyTailed => "heavy-tailed",
            Self::Correlated => "correlated",
        }
    }

    /// All four owner-required jagged families.
    pub const JAGGED: [Self; 4] = [
        Self::Anisotropic,
        Self::Clustered,
        Self::HeavyTailed,
        Self::Correlated,
    ];
}

/// Dataset creation or binary-format failure.
#[derive(Debug)]
pub enum DatasetError {
    /// A matrix or requested synthetic shape was empty or inconsistent.
    InvalidShape {
        /// Actionable shape explanation.
        reason: String,
    },
    /// Reading a dataset file failed.
    Io {
        /// Path that could not be read.
        path: PathBuf,
        /// Operating-system failure.
        source: std::io::Error,
    },
    /// A record header or payload was truncated.
    TruncatedRecord {
        /// Zero-based record position.
        record: usize,
        /// Byte offset at which the record began.
        offset: usize,
    },
    /// A record declared zero coordinates.
    ZeroDimension {
        /// Zero-based record position.
        record: usize,
    },
    /// Records in one file declared different dimensions.
    InconsistentDimension {
        /// Dimension established by the first record.
        expected: usize,
        /// Dimension declared by the offending record.
        actual: usize,
        /// Zero-based record position.
        record: usize,
    },
    /// Record length arithmetic overflowed the host address space.
    ArithmeticOverflow,
}

impl std::fmt::Display for DatasetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidShape { reason } => write!(formatter, "invalid dataset shape: {reason}"),
            Self::Io { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::TruncatedRecord { record, offset } => write!(
                formatter,
                "dataset record {record} is truncated at byte offset {offset}"
            ),
            Self::ZeroDimension { record } => {
                write!(
                    formatter,
                    "dataset record {record} declares zero dimensions"
                )
            }
            Self::InconsistentDimension {
                expected,
                actual,
                record,
            } => write!(
                formatter,
                "dataset record {record} dimension mismatch: expected {expected}, got {actual}"
            ),
            Self::ArithmeticOverflow => formatter.write_str("dataset record size overflowed usize"),
        }
    }
}

impl std::error::Error for DatasetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Generates one deterministic uniform or jagged synthetic dataset.
///
/// Queries are held-out perturbations of corpus directions or cluster centers,
/// never literal corpus copies. Ground truth is deliberately absent so the
/// recall harness exercises its independent brute-force path.
///
/// # Errors
///
/// Returns [`DatasetError::InvalidShape`] unless row/query counts are nonzero
/// and the dimension is at least two.
pub fn synthetic(
    kind: SyntheticKind,
    row_count: usize,
    query_count: usize,
    dimension: usize,
    seed: u64,
) -> Result<Dataset, DatasetError> {
    if row_count == 0 || query_count == 0 || dimension < 2 {
        return Err(DatasetError::InvalidShape {
            reason: format!(
                "rows={row_count}, queries={query_count}, dimension={dimension}; require nonzero counts and dimension >= 2"
            ),
        });
    }
    let value_count = row_count
        .checked_mul(dimension)
        .ok_or(DatasetError::ArithmeticOverflow)?;
    let mut random = SplitMix64::new(seed);
    let (values, labels, centers) = match kind {
        SyntheticKind::Uniform => (
            generate_unit_rows(&mut random, row_count, dimension),
            None,
            None,
        ),
        SyntheticKind::Anisotropic => (
            generate_anisotropic_rows(&mut random, row_count, dimension),
            None,
            None,
        ),
        SyntheticKind::Clustered => {
            let (rows, row_labels, cluster_centers) =
                generate_clustered_rows(&mut random, row_count, dimension);
            (rows, Some(row_labels), Some(cluster_centers))
        }
        SyntheticKind::HeavyTailed => (
            generate_heavy_tailed_rows(&mut random, row_count, dimension),
            None,
            None,
        ),
        SyntheticKind::Correlated => (
            generate_correlated_rows(&mut random, row_count, dimension),
            None,
            None,
        ),
    };
    if values.len() != value_count {
        return Err(DatasetError::InvalidShape {
            reason: String::from("synthetic generator produced a partial matrix"),
        });
    }
    let queries = generate_queries(
        kind,
        &values,
        centers.as_deref(),
        row_count,
        query_count,
        dimension,
        &mut random,
    );
    let mut dataset = Dataset::new(
        format!("synthetic-{}", kind.label()),
        DenseMatrix { dimension, values },
        DenseMatrix {
            dimension,
            values: queries,
        },
        None,
    )?;
    dataset.cluster_labels = labels;
    Ok(dataset)
}

/// Loads the standard little-endian fvecs record format.
///
/// Each record is `[dimension: i32][dimension * f32]`. All records must have
/// one positive, consistent dimension and the file must end on a record
/// boundary.
pub fn load_fvecs(path: impl AsRef<Path>) -> Result<DenseMatrix, DatasetError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|source| DatasetError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_records(&bytes, 4, |payload, values| {
        for chunk in payload.chunks_exact(4) {
            values.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
    })
}

/// Loads the standard little-endian bvecs record format as f32 coordinates.
///
/// Each record is `[dimension: i32][dimension * u8]`; byte coordinates are
/// converted exactly to f32 so the common recall path can consume them.
pub fn load_bvecs(path: impl AsRef<Path>) -> Result<DenseMatrix, DatasetError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|source| DatasetError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_records(&bytes, 1, |payload, values| {
        values.extend(payload.iter().map(|value| f32::from(*value)));
    })
}

fn parse_records(
    bytes: &[u8],
    coordinate_width: usize,
    mut append: impl FnMut(&[u8], &mut Vec<f32>),
) -> Result<DenseMatrix, DatasetError> {
    let mut offset = 0_usize;
    let mut record = 0_usize;
    let mut dimension = None;
    let mut values = Vec::new();
    while offset < bytes.len() {
        let header_end = offset
            .checked_add(4)
            .ok_or(DatasetError::ArithmeticOverflow)?;
        let header = bytes
            .get(offset..header_end)
            .ok_or(DatasetError::TruncatedRecord { record, offset })?;
        let actual_dimension =
            u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if actual_dimension == 0 {
            return Err(DatasetError::ZeroDimension { record });
        }
        if let Some(expected) = dimension {
            if actual_dimension != expected {
                return Err(DatasetError::InconsistentDimension {
                    expected,
                    actual: actual_dimension,
                    record,
                });
            }
        } else {
            dimension = Some(actual_dimension);
        }
        let payload_len = actual_dimension
            .checked_mul(coordinate_width)
            .ok_or(DatasetError::ArithmeticOverflow)?;
        let end = header_end
            .checked_add(payload_len)
            .ok_or(DatasetError::ArithmeticOverflow)?;
        let payload = bytes
            .get(header_end..end)
            .ok_or(DatasetError::TruncatedRecord { record, offset })?;
        append(payload, &mut values);
        offset = end;
        record += 1;
    }
    let dimension = dimension.ok_or_else(|| DatasetError::InvalidShape {
        reason: String::from("dataset file is empty"),
    })?;
    Ok(DenseMatrix { dimension, values })
}

fn validate_matrices(vectors: &DenseMatrix, queries: &DenseMatrix) -> Result<(), DatasetError> {
    if vectors.dimension == 0 || queries.dimension == 0 {
        return Err(DatasetError::InvalidShape {
            reason: String::from("matrix dimension must not be zero"),
        });
    }
    if vectors.dimension != queries.dimension {
        return Err(DatasetError::InvalidShape {
            reason: format!(
                "vector dimension {} differs from query dimension {}",
                vectors.dimension, queries.dimension
            ),
        });
    }
    if vectors.values.is_empty() || !vectors.values.len().is_multiple_of(vectors.dimension) {
        return Err(DatasetError::InvalidShape {
            reason: String::from("vector matrix is empty or contains a partial row"),
        });
    }
    if queries.values.is_empty() || !queries.values.len().is_multiple_of(queries.dimension) {
        return Err(DatasetError::InvalidShape {
            reason: String::from("query matrix is empty or contains a partial row"),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn open_unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) * (1.0 / 9_007_199_254_740_992.0)
    }

    fn gaussian(&mut self) -> f32 {
        let radius = (-2.0 * self.open_unit().ln()).sqrt();
        let angle = std::f64::consts::TAU * self.open_unit();
        (radius * angle.cos()) as f32
    }
}

fn generate_unit_rows(random: &mut SplitMix64, rows: usize, dimension: usize) -> Vec<f32> {
    let mut values = Vec::with_capacity(rows * dimension);
    for _ in 0..rows {
        let mut row = (0..dimension)
            .map(|_| random.gaussian())
            .collect::<Vec<_>>();
        normalize(&mut row);
        values.extend(row);
    }
    values
}

fn dimension_scale(coordinate: usize, dimension: usize) -> f32 {
    let position = coordinate as f64 / (dimension.saturating_sub(1).max(1)) as f64;
    10.0_f64.powf(-1.5 + 3.0 * position) as f32
}

fn generate_anisotropic_rows(random: &mut SplitMix64, rows: usize, dimension: usize) -> Vec<f32> {
    let mut values = Vec::with_capacity(rows * dimension);
    for _ in 0..rows {
        let mut row = (0..dimension)
            .map(|coordinate| random.gaussian() * dimension_scale(coordinate, dimension))
            .collect::<Vec<_>>();
        normalize(&mut row);
        values.extend(row);
    }
    values
}

fn generate_clustered_rows(
    random: &mut SplitMix64,
    rows: usize,
    dimension: usize,
) -> (Vec<f32>, Vec<usize>, Vec<Vec<f32>>) {
    let cluster_count = (rows / 8).clamp(2, 12);
    let centers = (0..cluster_count)
        .map(|_| {
            let mut center = (0..dimension)
                .map(|_| random.gaussian())
                .collect::<Vec<_>>();
            normalize(&mut center);
            center
        })
        .collect::<Vec<_>>();
    let total_weight = cluster_count * (cluster_count + 1) / 2;
    let mut values = Vec::with_capacity(rows * dimension);
    let mut labels = Vec::with_capacity(rows);
    for row_index in 0..rows {
        let ticket = row_index % total_weight;
        let mut cumulative = 0_usize;
        let mut label = 0_usize;
        for candidate in 0..cluster_count {
            cumulative += candidate + 1;
            if ticket < cumulative {
                label = candidate;
                break;
            }
        }
        let spread = 0.015 + 0.012 * (label % 5) as f32;
        let mut row = centers[label]
            .iter()
            .map(|value| *value + spread * random.gaussian())
            .collect::<Vec<_>>();
        normalize(&mut row);
        values.extend(row);
        labels.push(label);
    }
    (values, labels, centers)
}

fn generate_heavy_tailed_rows(random: &mut SplitMix64, rows: usize, dimension: usize) -> Vec<f32> {
    let mut values = Vec::with_capacity(rows * dimension);
    for _ in 0..rows {
        let mut direction = (0..dimension)
            .map(|_| random.gaussian())
            .collect::<Vec<_>>();
        normalize(&mut direction);
        let magnitude = (1.25 * f64::from(random.gaussian()))
            .exp()
            .clamp(0.05, 30.0) as f32;
        values.extend(direction.into_iter().map(|value| value * magnitude));
    }
    values
}

fn correlated_row(random: &mut SplitMix64, dimension: usize) -> Vec<f32> {
    let rho = 0.93_f32;
    let innovation = (1.0 - rho * rho).sqrt();
    let mut previous = random.gaussian();
    let mut row = Vec::with_capacity(dimension);
    row.push(previous);
    for _ in 1..dimension {
        previous = rho * previous + innovation * random.gaussian();
        row.push(previous);
    }
    normalize(&mut row);
    row
}

fn generate_correlated_rows(random: &mut SplitMix64, rows: usize, dimension: usize) -> Vec<f32> {
    let mut values = Vec::with_capacity(rows * dimension);
    for _ in 0..rows {
        values.extend(correlated_row(random, dimension));
    }
    values
}

fn generate_queries(
    kind: SyntheticKind,
    values: &[f32],
    centers: Option<&[Vec<f32>]>,
    row_count: usize,
    query_count: usize,
    dimension: usize,
    random: &mut SplitMix64,
) -> Vec<f32> {
    let mut queries = Vec::with_capacity(query_count * dimension);
    for query_index in 0..query_count {
        let source_index = query_index.wrapping_mul(37).wrapping_add(11) % row_count;
        let source = values
            .get(source_index * dimension..(source_index + 1) * dimension)
            .unwrap_or(&[]);
        let mut query = match kind {
            SyntheticKind::Uniform => source
                .iter()
                .map(|value| *value + 0.22 * random.gaussian() / (dimension as f32).sqrt())
                .collect::<Vec<_>>(),
            SyntheticKind::Anisotropic => source
                .iter()
                .enumerate()
                .map(|(coordinate, value)| {
                    *value
                        + 0.15 * random.gaussian() * dimension_scale(coordinate, dimension)
                            / (dimension as f32).sqrt()
                })
                .collect::<Vec<_>>(),
            SyntheticKind::Clustered => {
                let available = centers.unwrap_or(&[]);
                let center = available
                    .get(query_index.wrapping_mul(7) % available.len().max(1))
                    .map(Vec::as_slice)
                    .unwrap_or(source);
                center
                    .iter()
                    .map(|value| *value + 0.035 * random.gaussian())
                    .collect::<Vec<_>>()
            }
            SyntheticKind::HeavyTailed => {
                let source_norm = norm(source).max(f64::from(f32::MIN_POSITIVE)) as f32;
                source
                    .iter()
                    .map(|value| {
                        *value / source_norm + 0.18 * random.gaussian() / (dimension as f32).sqrt()
                    })
                    .collect::<Vec<_>>()
            }
            SyntheticKind::Correlated => {
                let noise = correlated_row(random, dimension);
                source
                    .iter()
                    .zip(noise)
                    .map(|(value, noise)| 0.85 * *value + 0.15 * noise)
                    .collect::<Vec<_>>()
            }
        };
        normalize(&mut query);
        queries.extend(query);
    }
    queries
}

fn normalize(values: &mut [f32]) {
    let length = norm(values);
    if length == 0.0 {
        return;
    }
    for value in values {
        *value = (f64::from(*value) / length) as f32;
    }
}

fn norm(values: &[f32]) -> f64 {
    values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt()
}
