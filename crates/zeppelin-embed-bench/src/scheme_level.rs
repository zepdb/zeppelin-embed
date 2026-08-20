//! Full coarse-scoring fixtures shared by the scheme-level benchmark and tests.

use zeppelin_embed::quant::{
    Bit4Factors, Bit4Query, Int8Query, Int8Vec, QuantError, QuantScheme, dot_int8_query,
    est_dot_bit4, prepare_bit4_query, prepare_int8_query, quantize_bit4, quantize_int8,
};

/// Deterministic validation failure from the scheme-level benchmark seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemeLevelError {
    /// F32 and F16 are not coarse quantization schemes in this comparison.
    UnsupportedScheme(QuantScheme),
    /// A row matrix must declare a non-zero dimension.
    ZeroDimension,
    /// The flat row matrix did not contain a positive whole number of rows.
    RowShape {
        /// Declared row width.
        dimension: usize,
        /// Supplied scalar count.
        values: usize,
    },
    /// The caller-owned output did not contain one score per row.
    OutputCount {
        /// Encoded corpus row count.
        expected: usize,
        /// Supplied score slots.
        actual: usize,
    },
    /// A prepared query belonged to a different quantization scheme.
    SchemeMismatch,
    /// A required command-line flag was absent.
    MissingFlag(&'static str),
    /// A command-line flag appeared more than once.
    DuplicateFlag(String),
    /// An unknown command-line flag was supplied.
    UnknownFlag(String),
    /// A command-line flag had no following value.
    MissingValue(String),
    /// A command-line value could not be parsed or was zero.
    InvalidValue {
        /// Flag owning the value.
        flag: String,
        /// Rejected literal.
        value: String,
    },
    /// The concrete encoded buffers were not large enough for a cold stream.
    WorkingSetTooSmall {
        /// Measured code-plus-factor slice bytes.
        actual: usize,
        /// Required minimum bytes.
        minimum: usize,
    },
    /// A requested allocation or byte count overflowed `usize`.
    ArithmeticOverflow,
    /// A production quantizer or scorer rejected its input.
    Quant(QuantError),
}

impl std::fmt::Display for SchemeLevelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedScheme(scheme) => {
                write!(
                    formatter,
                    "unsupported scheme-level benchmark scheme {scheme:?}"
                )
            }
            Self::ZeroDimension => formatter.write_str("scheme-level dimension must be positive"),
            Self::RowShape { dimension, values } => write!(
                formatter,
                "scheme-level row data length {values} is not a positive multiple of dimension {dimension}"
            ),
            Self::OutputCount { expected, actual } => write!(
                formatter,
                "scheme-level output count mismatch: expected {expected}, got {actual}"
            ),
            Self::SchemeMismatch => {
                formatter.write_str("prepared query scheme does not match the encoded corpus")
            }
            Self::MissingFlag(flag) => write!(formatter, "missing required flag {flag}"),
            Self::DuplicateFlag(flag) => write!(formatter, "duplicate flag {flag}"),
            Self::UnknownFlag(flag) => write!(formatter, "unknown flag {flag}"),
            Self::MissingValue(flag) => write!(formatter, "missing value for {flag}"),
            Self::InvalidValue { flag, value } => {
                write!(formatter, "invalid value {value:?} for {flag}")
            }
            Self::WorkingSetTooSmall { actual, minimum } => write!(
                formatter,
                "encoded working set {actual} B is below the required cache-cold minimum {minimum} B"
            ),
            Self::ArithmeticOverflow => {
                formatter.write_str("scheme-level size arithmetic overflowed usize")
            }
            Self::Quant(error) => write!(formatter, "scheme-level quantization failed: {error}"),
        }
    }
}

/// Approximate system-level cache size used by Tasks 02 and 03.
pub const SYSTEM_LEVEL_CACHE_BYTES: usize = 48 * 1024 * 1024;
/// Required encoded working set: at least eight complete cache capacities.
pub const MIN_WORKING_SET_BYTES: usize = 8 * SYSTEM_LEVEL_CACHE_BYTES;
/// Adopted Task-02 four-accumulator wide-load single-reader denominator.
pub const WIDE_LOAD_MEMORY_CEILING_GBPS: f64 =
    crate::frontier::roofline::WIDE_LOAD_SINGLE_CORE_GBPS;

/// User-selected subset of coarse quantization schemes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemeSelection {
    /// Run Bit4 and Int8 in that order.
    All,
    /// Run only Bit4.
    Bit4,
    /// Run only Int8.
    Int8,
}

impl SchemeSelection {
    /// Returns selected production scheme identifiers in stable display order.
    #[must_use]
    pub fn schemes(self) -> Vec<QuantScheme> {
        match self {
            Self::All => vec![QuantScheme::Bit4, QuantScheme::Int8],
            Self::Bit4 => vec![QuantScheme::Bit4],
            Self::Int8 => vec![QuantScheme::Int8],
        }
    }
}

/// Fully specified command-line controls for a scheme-level run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BenchmarkConfig {
    /// Encoded corpus rows per scheme.
    pub rows: usize,
    /// Coordinates per row and query.
    pub dimension: usize,
    /// Prepared queries scored per repeat.
    pub queries: usize,
    /// Deterministic corpus/query seed.
    pub seed: u64,
    /// Scheme subset.
    pub scheme: SchemeSelection,
    /// Complete query-sweep timing repeats.
    pub repeats: usize,
}

/// Parses the six required `--flag value` controls.
///
/// # Errors
///
/// Returns a typed error for missing, duplicate, unknown, zero, or malformed
/// values. Seeds accept decimal or a `0x` hexadecimal prefix.
pub fn parse_arguments<I, S>(arguments: I) -> Result<BenchmarkConfig, SchemeLevelError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut rows = None;
    let mut dimension = None;
    let mut queries = None;
    let mut seed = None;
    let mut scheme = None;
    let mut repeats = None;
    let mut arguments = arguments.into_iter();
    while let Some(flag) = arguments.next() {
        let flag = flag.as_ref().to_owned();
        let value = arguments
            .next()
            .ok_or_else(|| SchemeLevelError::MissingValue(flag.clone()))?;
        let value = value.as_ref();
        match flag.as_str() {
            "--rows" => set_once(&mut rows, parse_nonzero_usize(&flag, value)?, &flag)?,
            "--dimension" => set_once(&mut dimension, parse_nonzero_usize(&flag, value)?, &flag)?,
            "--queries" => set_once(&mut queries, parse_nonzero_usize(&flag, value)?, &flag)?,
            "--seed" => set_once(&mut seed, parse_seed(&flag, value)?, &flag)?,
            "--scheme" => set_once(&mut scheme, parse_scheme(&flag, value)?, &flag)?,
            "--repeats" => set_once(&mut repeats, parse_nonzero_usize(&flag, value)?, &flag)?,
            _ => return Err(SchemeLevelError::UnknownFlag(flag)),
        }
    }
    Ok(BenchmarkConfig {
        rows: rows.ok_or(SchemeLevelError::MissingFlag("--rows"))?,
        dimension: dimension.ok_or(SchemeLevelError::MissingFlag("--dimension"))?,
        queries: queries.ok_or(SchemeLevelError::MissingFlag("--queries"))?,
        seed: seed.ok_or(SchemeLevelError::MissingFlag("--seed"))?,
        scheme: scheme.ok_or(SchemeLevelError::MissingFlag("--scheme"))?,
        repeats: repeats.ok_or(SchemeLevelError::MissingFlag("--repeats"))?,
    })
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), SchemeLevelError> {
    if slot.replace(value).is_some() {
        return Err(SchemeLevelError::DuplicateFlag(flag.to_owned()));
    }
    Ok(())
}

fn parse_nonzero_usize(flag: &str, value: &str) -> Result<usize, SchemeLevelError> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| SchemeLevelError::InvalidValue {
            flag: flag.to_owned(),
            value: value.to_owned(),
        })?;
    if parsed == 0 {
        return Err(SchemeLevelError::InvalidValue {
            flag: flag.to_owned(),
            value: value.to_owned(),
        });
    }
    Ok(parsed)
}

fn parse_seed(flag: &str, value: &str) -> Result<u64, SchemeLevelError> {
    let parsed = if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        value.parse::<u64>()
    };
    parsed.map_err(|_| SchemeLevelError::InvalidValue {
        flag: flag.to_owned(),
        value: value.to_owned(),
    })
}

fn parse_scheme(flag: &str, value: &str) -> Result<SchemeSelection, SchemeLevelError> {
    match value {
        "all" => Ok(SchemeSelection::All),
        "bit4" => Ok(SchemeSelection::Bit4),
        "int8" => Ok(SchemeSelection::Int8),
        _ => Err(SchemeLevelError::InvalidValue {
            flag: flag.to_owned(),
            value: value.to_owned(),
        }),
    }
}

/// Human- and machine-reportable facts about the actual scoring implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScoringPath {
    /// Stable descriptive label.
    pub label: &'static str,
    /// Whether the selected scorer itself uses native runtime-dispatched SIMD.
    pub native_runtime_dispatched_simd: bool,
    /// Whether query-time scoring allocates or writes an expanded row.
    pub expanded_row_materialized: bool,
    /// Expanded row write traffic, zero for all current production scorers.
    pub unpack_bytes_per_row: usize,
}

/// Describes the production scorer reached by one scheme and dispatch family.
#[must_use]
pub const fn scoring_path(
    scheme: QuantScheme,
    arm: zeppelin_embed::kernels::KernelArm,
) -> ScoringPath {
    use zeppelin_embed::kernels::KernelArm;
    match scheme {
        QuantScheme::Int8 => match arm {
            KernelArm::Scalar => scalar_path("scalar Int8 dot plus affine scale/offset correction"),
            KernelArm::Neon => {
                native_path("runtime-dispatched NEON Int8 dot plus affine scale/offset correction")
            }
            KernelArm::Avx2 => {
                native_path("runtime-dispatched AVX2 Int8 dot plus affine scale/offset correction")
            }
        },
        QuantScheme::Bit4 => match arm {
            KernelArm::Neon => {
                native_path("runtime-dispatched NEON packed Bit4 dot plus RaBitQ correction")
            }
            KernelArm::Scalar => {
                scalar_path("scalar packed Bit4 extraction plus RaBitQ correction")
            }
            KernelArm::Avx2 => scalar_path(
                "scalar packed Bit4 extraction in the AVX2 table plus RaBitQ correction",
            ),
        },
        QuantScheme::F32 | QuantScheme::F16 => {
            scalar_path("unsupported scheme-level coarse scorer")
        }
    }
}

const fn native_path(label: &'static str) -> ScoringPath {
    ScoringPath {
        label,
        native_runtime_dispatched_simd: true,
        expanded_row_materialized: false,
        unpack_bytes_per_row: 0,
    }
}

const fn scalar_path(label: &'static str) -> ScoringPath {
    ScoringPath {
        label,
        native_runtime_dispatched_simd: false,
        expanded_row_materialized: false,
        unpack_bytes_per_row: 0,
    }
}

/// Rejects encoded buffers smaller than eight approximate SLC capacities.
///
/// The comparison uses [`EncodedCorpus::encoded_buffer_bytes`], so the actual
/// code and factor slices—not a bits-per-coordinate formula—are authoritative.
///
/// # Errors
///
/// Returns [`SchemeLevelError::WorkingSetTooSmall`] below the fixed floor.
pub fn enforce_cache_floor(corpus: &EncodedCorpus) -> Result<(), SchemeLevelError> {
    let actual = corpus.encoded_buffer_bytes().total;
    if actual < MIN_WORKING_SET_BYTES {
        return Err(SchemeLevelError::WorkingSetTooSmall {
            actual,
            minimum: MIN_WORKING_SET_BYTES,
        });
    }
    Ok(())
}

/// Timing and traffic values from one complete per-scheme measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasurementMetrics {
    /// Measured concrete code-plus-factor bytes divided by row count.
    pub bytes_per_row: usize,
    /// Measured concrete code-plus-factor bytes in the corpus.
    pub working_set_bytes: usize,
    /// Median nanoseconds per scored row across complete query sweeps.
    pub ns_per_row: f64,
    /// Encoded code-plus-factor bytes per median elapsed second.
    pub effective_gbps: f64,
    /// Effective GB/s divided by the adopted wide-load denominator.
    pub percent_of_wide_load_ceiling: f64,
    /// Observable checksum over every produced score buffer.
    pub checksum: u64,
}

/// One per-scheme report emitted by the benchmark binary.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemeReport {
    scheme: QuantScheme,
    status: &'static str,
    measurement_authority: &'static str,
    config: BenchmarkConfig,
    path: ScoringPath,
    bytes_per_row: Option<usize>,
    working_set_bytes: Option<usize>,
    ns_per_row: Option<f64>,
    effective_gbps: Option<f64>,
    percent_of_wide_load_ceiling: Option<f64>,
    checksum: Option<u64>,
    provenance: Option<String>,
    note: String,
}

impl SchemeReport {
    /// Builds a fail-closed record containing no timing claims.
    #[must_use]
    pub fn not_measured(
        scheme: QuantScheme,
        config: &BenchmarkConfig,
        path: ScoringPath,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            scheme,
            status: "NOT_MEASURED",
            measurement_authority: "NOT_AUTHORITATIVE",
            config: *config,
            path,
            bytes_per_row: None,
            working_set_bytes: None,
            ns_per_row: None,
            effective_gbps: None,
            percent_of_wide_load_ceiling: None,
            checksum: None,
            provenance: None,
            note: reason.into(),
        }
    }

    /// Builds a fail-closed record after buffers exist but timing is refused.
    #[must_use]
    pub fn not_measured_with_buffers(
        scheme: QuantScheme,
        config: &BenchmarkConfig,
        path: ScoringPath,
        buffers: EncodedBufferBytes,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            scheme,
            status: "NOT_MEASURED",
            measurement_authority: "NOT_AUTHORITATIVE",
            config: *config,
            path,
            bytes_per_row: Some(buffers.total / config.rows),
            working_set_bytes: Some(buffers.total),
            ns_per_row: None,
            effective_gbps: None,
            percent_of_wide_load_ceiling: None,
            checksum: None,
            provenance: None,
            note: reason.into(),
        }
    }

    /// Builds a timing candidate, loudly classifying values above the ceiling.
    #[must_use]
    pub fn measured_candidate(
        scheme: QuantScheme,
        config: &BenchmarkConfig,
        path: ScoringPath,
        metrics: MeasurementMetrics,
        provenance: impl Into<String>,
    ) -> Self {
        let above_ceiling = metrics.percent_of_wide_load_ceiling > 100.0;
        let (status, note) = if above_ceiling {
            (
                "MEASUREMENT_DEFECT",
                format!(
                    "observed {:.6}% is above 100% of the {:.6} GB/s denominator; explain as a measurement defect, never a success",
                    metrics.percent_of_wide_load_ceiling, WIDE_LOAD_MEMORY_CEILING_GBPS
                ),
            )
        } else {
            (
                "MEASURED_CANDIDATE",
                String::from(
                    "timing requires orchestrator confirmation of a single-tenant machine before it is authoritative",
                ),
            )
        };
        Self {
            scheme,
            status,
            measurement_authority: "SINGLE_TENANT_REQUIRED",
            config: *config,
            path,
            bytes_per_row: Some(metrics.bytes_per_row),
            working_set_bytes: Some(metrics.working_set_bytes),
            ns_per_row: Some(metrics.ns_per_row),
            effective_gbps: Some(metrics.effective_gbps),
            percent_of_wide_load_ceiling: Some(metrics.percent_of_wide_load_ceiling),
            checksum: Some(metrics.checksum),
            provenance: Some(provenance.into()),
            note,
        }
    }

    /// Serializes one stable line-oriented JSON record.
    ///
    /// # Errors
    ///
    /// Returns the benchmark crate's JSON serializer error.
    pub fn machine_summary_line(&self) -> Result<String, serde_json::Error> {
        let value = serde_json::json!({
            "schema": 1,
            "scheme": scheme_label(self.scheme),
            "status": self.status,
            "measurement_authority": self.measurement_authority,
            "rows": self.config.rows,
            "dimension": self.config.dimension,
            "queries": self.config.queries,
            "seed": self.config.seed,
            "repeats": self.config.repeats,
            "bytes_per_row": self.bytes_per_row,
            "working_set_bytes": self.working_set_bytes,
            "ns_per_row": self.ns_per_row,
            "effective_gbps": self.effective_gbps,
            "wide_load_memory_ceiling_gbps": WIDE_LOAD_MEMORY_CEILING_GBPS,
            "percent_of_wide_load_ceiling": self.percent_of_wide_load_ceiling,
            "scoring_path": self.path.label,
            "native_runtime_dispatched_simd": self.path.native_runtime_dispatched_simd,
            "expanded_row_materialized": self.path.expanded_row_materialized,
            "unpack_bytes_per_row": self.path.unpack_bytes_per_row,
            "checksum": self.checksum,
            "machine_state_provenance": self.provenance,
            "note": self.note,
        });
        serde_json::to_string(&value).map(|json| format!("SCHEME_SUMMARY {json}"))
    }
}

/// Stable lowercase scheme label used by text and JSON output.
#[must_use]
pub const fn scheme_label(scheme: QuantScheme) -> &'static str {
    match scheme {
        QuantScheme::Int8 => "int8",
        QuantScheme::Bit4 => "bit4",
        QuantScheme::F32 => "f32",
        QuantScheme::F16 => "f16",
    }
}

impl std::error::Error for SchemeLevelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Quant(error) => Some(error),
            _ => None,
        }
    }
}

impl From<QuantError> for SchemeLevelError {
    fn from(error: QuantError) -> Self {
        Self::Quant(error)
    }
}

/// Bytes occupied by the concrete code and factor slices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodedBufferBytes {
    /// Bytes in the encoded coordinate buffer.
    pub code_bytes: usize,
    /// Bytes in the per-row factor buffer.
    pub factor_bytes: usize,
    /// Code plus factor bytes.
    pub total: usize,
}

/// One query prepared through the selected production scheme entry point.
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedQuery {
    /// Prepared affine Int8 query.
    Int8(Int8Query),
    /// Prepared four-bit Extended-RaBitQ query.
    Bit4(Bit4Query),
}

impl PreparedQuery {
    /// Prepares a query once for repeated row scoring.
    ///
    /// # Errors
    ///
    /// Returns [`SchemeLevelError::UnsupportedScheme`] for F32/F16, or the
    /// production query preparer's typed validation error.
    pub fn new(scheme: QuantScheme, query: &[f32], seed: u64) -> Result<Self, SchemeLevelError> {
        match scheme {
            QuantScheme::Int8 => Ok(Self::Int8(prepare_int8_query(query)?)),
            QuantScheme::Bit4 => Ok(Self::Bit4(prepare_bit4_query(query, seed)?)),
            QuantScheme::F32 | QuantScheme::F16 => Err(SchemeLevelError::UnsupportedScheme(scheme)),
        }
    }
}

/// Contiguous encoded rows and every per-row factor consumed by coarse scoring.
#[derive(Clone, Debug, PartialEq)]
pub enum EncodedCorpus {
    /// Affine signed-byte rows and `(scale, offset)` factors.
    Int8 {
        dimension: usize,
        codes: Vec<i8>,
        factors: Vec<(f32, f32)>,
    },
    /// Packed four-bit rows and Extended-RaBitQ factors.
    Bit4 {
        dimension: usize,
        codes: Vec<u8>,
        factors: Vec<Bit4Factors>,
    },
}

impl EncodedCorpus {
    /// Encodes a flat row-major f32 matrix through a production quantizer.
    ///
    /// # Errors
    ///
    /// Returns a shape error for zero or partial rows, an unsupported-scheme
    /// error for F32/F16, or the production quantizer's typed error.
    pub fn encode(
        scheme: QuantScheme,
        values: &[f32],
        dimension: usize,
    ) -> Result<Self, SchemeLevelError> {
        if dimension == 0 {
            return Err(SchemeLevelError::ZeroDimension);
        }
        if values.is_empty() || !values.len().is_multiple_of(dimension) {
            return Err(SchemeLevelError::RowShape {
                dimension,
                values: values.len(),
            });
        }
        match scheme {
            QuantScheme::Int8 => {
                let mut codes = Vec::with_capacity(values.len());
                let mut factors = Vec::with_capacity(values.len() / dimension);
                for row in values.chunks_exact(dimension) {
                    let start = codes.len();
                    codes.resize(start + dimension, 0_i8);
                    factors.push(quantize_int8(row, &mut codes[start..])?);
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
            QuantScheme::F32 | QuantScheme::F16 => Err(SchemeLevelError::UnsupportedScheme(scheme)),
        }
    }

    /// Builds a fully materialized deterministic corpus from production-coded
    /// templates.
    ///
    /// At most 2,048 source rows pay encoding cost. Additional packed rows use
    /// exact sign symmetries of those encoded vectors; Int8 rows use coordinate
    /// permutations. Those transformations preserve the associated factors,
    /// keep every row valid for the production scorer, and fill every byte of
    /// the final code and factor buffers before timing.
    ///
    /// # Errors
    ///
    /// Returns the same scheme/dimension errors as [`Self::encode`], a row
    /// shape error for zero rows, or an arithmetic-overflow error.
    pub fn synthetic(
        scheme: QuantScheme,
        rows: usize,
        dimension: usize,
        seed: u64,
    ) -> Result<Self, SchemeLevelError> {
        const TEMPLATE_ROWS: usize = 2_048;
        if rows == 0 {
            return Err(SchemeLevelError::RowShape {
                dimension,
                values: 0,
            });
        }
        let template_rows = rows.min(TEMPLATE_ROWS);
        let template_values_len = template_rows
            .checked_mul(dimension)
            .ok_or(SchemeLevelError::ArithmeticOverflow)?;
        let mut random = SplitMix64::new(seed ^ 0x7363_6865_6d65_5f6c);
        let mut template_values = Vec::with_capacity(template_values_len);
        for _ in 0..template_values_len {
            template_values.push(random.next_signed_f32());
        }
        let templates = Self::encode(scheme, &template_values, dimension)?;
        if rows == template_rows {
            return Ok(templates);
        }
        expand_templates(templates, rows, &mut random)
    }

    /// Returns the number of encoded rows.
    #[must_use]
    pub fn rows(&self) -> usize {
        match self {
            Self::Int8 { factors, .. } => factors.len(),
            Self::Bit4 { factors, .. } => factors.len(),
        }
    }

    /// Measures code and factor bytes from the concrete allocated slices.
    #[must_use]
    pub fn encoded_buffer_bytes(&self) -> EncodedBufferBytes {
        let (code_bytes, factor_bytes) = match self {
            Self::Int8 { codes, factors, .. } => (
                std::mem::size_of_val(codes.as_slice()),
                std::mem::size_of_val(factors.as_slice()),
            ),
            Self::Bit4 { codes, factors, .. } => (
                std::mem::size_of_val(codes.as_slice()),
                std::mem::size_of_val(factors.as_slice()),
            ),
        };
        EncodedBufferBytes {
            code_bytes,
            factor_bytes,
            total: code_bytes + factor_bytes,
        }
    }

    /// Returns measured encoded code-plus-factor bytes per row.
    #[must_use]
    pub fn bytes_per_row(&self) -> usize {
        self.encoded_buffer_bytes().total / self.rows()
    }

    /// Scores every row through the matching production coarse scorer.
    ///
    /// # Errors
    ///
    /// Returns a typed error for scheme/output mismatches or from the
    /// production scorer's row validation.
    pub fn score_prepared(
        &self,
        query: &PreparedQuery,
        output: &mut [f32],
    ) -> Result<(), SchemeLevelError> {
        if output.len() != self.rows() {
            return Err(SchemeLevelError::OutputCount {
                expected: self.rows(),
                actual: output.len(),
            });
        }
        match (self, query) {
            (
                Self::Int8 {
                    dimension,
                    codes,
                    factors,
                },
                PreparedQuery::Int8(query),
            ) => {
                for ((codes, &(scale, offset)), score) in codes
                    .chunks_exact(*dimension)
                    .zip(factors)
                    .zip(output.iter_mut())
                {
                    *score = dot_int8_query(
                        query,
                        Int8Vec {
                            codes,
                            scale,
                            offset,
                        },
                    )?;
                }
            }
            (
                Self::Bit4 {
                    dimension,
                    codes,
                    factors,
                },
                PreparedQuery::Bit4(query),
            ) => {
                for ((codes, &factors), score) in codes
                    .chunks_exact(dimension.div_ceil(2))
                    .zip(factors)
                    .zip(output.iter_mut())
                {
                    *score = est_dot_bit4(query, codes, factors)?;
                }
            }
            _ => return Err(SchemeLevelError::SchemeMismatch),
        }
        Ok(())
    }
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

    fn next_signed_f32(&mut self) -> f32 {
        let fraction = (self.next_u64() >> 40) as f32 * (1.0 / 16_777_216.0);
        fraction * 2.0 - 1.0
    }

    fn index(&mut self, length: usize) -> usize {
        (self.next_u64() % length as u64) as usize
    }
}

fn expand_templates(
    templates: EncodedCorpus,
    rows: usize,
    random: &mut SplitMix64,
) -> Result<EncodedCorpus, SchemeLevelError> {
    match templates {
        EncodedCorpus::Int8 {
            dimension,
            codes: templates,
            factors: template_factors,
        } => {
            let template_rows = template_factors.len();
            let code_capacity = rows
                .checked_mul(dimension)
                .ok_or(SchemeLevelError::ArithmeticOverflow)?;
            let mut codes = Vec::with_capacity(code_capacity);
            let mut factors = Vec::with_capacity(rows);
            for _ in 0..rows {
                let template_index = random.index(template_rows);
                let row = template_slice(&templates, template_index, dimension)?;
                let rotation = random.index(dimension);
                let (left, right) = row.split_at(rotation);
                codes.extend_from_slice(right);
                codes.extend_from_slice(left);
                factors.push(copy_factor(&template_factors, template_index)?);
            }
            Ok(EncodedCorpus::Int8 {
                dimension,
                codes,
                factors,
            })
        }
        EncodedCorpus::Bit4 {
            dimension,
            codes: templates,
            factors: template_factors,
        } => {
            let stride = dimension.div_ceil(2);
            let (codes, factors) = expand_packed_templates(
                &templates,
                &template_factors,
                rows,
                stride,
                trailing_mask(dimension, 2),
                4,
                random,
            )?;
            Ok(EncodedCorpus::Bit4 {
                dimension,
                codes,
                factors,
            })
        }
    }
}

fn expand_packed_templates<F: Copy>(
    templates: &[u8],
    template_factors: &[F],
    rows: usize,
    stride: usize,
    final_byte_mask: u8,
    field_bits: u32,
    random: &mut SplitMix64,
) -> Result<(Vec<u8>, Vec<F>), SchemeLevelError> {
    let capacity = rows
        .checked_mul(stride)
        .ok_or(SchemeLevelError::ArithmeticOverflow)?;
    let template_rows = template_factors.len();
    let mut codes = Vec::with_capacity(capacity);
    let mut factors = Vec::with_capacity(rows);
    for _ in 0..rows {
        let template_index = random.index(template_rows);
        let row = template_slice(templates, template_index, stride)?;
        for (byte_index, &byte) in row.iter().enumerate() {
            let allowed = if byte_index + 1 == stride {
                final_byte_mask
            } else {
                u8::MAX
            };
            let sign_mask = sign_symmetry_mask(random.next_u64() as u8, field_bits) & allowed;
            codes.push(byte ^ sign_mask);
        }
        factors.push(copy_factor(template_factors, template_index)?);
    }
    Ok((codes, factors))
}

fn template_slice<T>(
    values: &[T],
    template_index: usize,
    stride: usize,
) -> Result<&[T], SchemeLevelError> {
    let start = template_index
        .checked_mul(stride)
        .ok_or(SchemeLevelError::ArithmeticOverflow)?;
    let end = start
        .checked_add(stride)
        .ok_or(SchemeLevelError::ArithmeticOverflow)?;
    values
        .get(start..end)
        .ok_or(SchemeLevelError::ArithmeticOverflow)
}

fn copy_factor<F: Copy>(values: &[F], index: usize) -> Result<F, SchemeLevelError> {
    values
        .get(index)
        .copied()
        .ok_or(SchemeLevelError::ArithmeticOverflow)
}

fn trailing_mask(dimension: usize, fields_per_byte: usize) -> u8 {
    let used = dimension % fields_per_byte;
    if used == 0 {
        u8::MAX
    } else {
        let field_bits = 8 / fields_per_byte;
        u8::MAX << ((fields_per_byte - used) * field_bits)
    }
}

fn sign_symmetry_mask(random: u8, field_bits: u32) -> u8 {
    let fields = 8_u32 / field_bits;
    let field_mask = ((1_u16 << field_bits) - 1) as u8;
    let mut mask = 0_u8;
    for field in 0..fields {
        if random & (1_u8 << field) != 0 {
            mask |= field_mask << (8 - field_bits * (field + 1));
        }
    }
    mask
}
