//! Graph construction and recall support for the frozen ANN datasets.

use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use zeppelin_embed::graph::GraphParams;
use zeppelin_embed::graph::build::{
    CheckpointedGraphBuild, GraphBuildArtifact, GraphBuildPasses, build_graph_checkpointed,
};
use zeppelin_embed::graph::search::{GraphSearchRequest, GraphSearchScratch, GraphSearcher};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::quantize_bit4;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::segment::{SegmentId, SegmentMeta};
use zeppelin_embed::vfs::StdVfs;

const DIMS: usize = 128;
const ROWS: usize = 1_000_000;
const QUERIES: usize = 10_000;
const TOP_K: usize = 100;
const CROSS_GRAPH_MARKER_VERSION: &str = "m4b-v1";
const CROSS_INPUT_MARKER_VERSION: &str = "m4b-input-v1";
fn input_id() -> SegmentId {
    SegmentId::new(19, [0x31; 10])
}

fn graph_id() -> SegmentId {
    SegmentId::new(19, [0x32; 10])
}

/// Locations of the three raw SIFT-1M arrays.
#[derive(Clone, Debug)]
pub struct Sift1mPaths {
    /// Row-major raw little-endian f32 base vectors.
    pub base: PathBuf,
    /// Row-major raw little-endian f32 queries.
    pub queries: PathBuf,
    /// Row-major raw little-endian i32 ground-truth ids.
    pub ground_truth: PathBuf,
}

impl Sift1mPaths {
    /// Resolves the frozen cross-benchmark filenames below one data directory.
    #[must_use]
    pub fn in_directory(directory: &Path) -> Self {
        Self {
            base: directory.join("sift-128-euclidean-base.f32bin"),
            queries: directory.join("sift-128-euclidean-query.f32bin"),
            ground_truth: directory.join("sift-128-euclidean-gt.i32bin"),
        }
    }
}

/// Distance semantics used to prepare one cross-dataset graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphDistanceMetric {
    /// Native squared Euclidean distance.
    SquaredL2,
    /// Cosine similarity represented as squared L2 over unit-normalized rows.
    Cosine,
}

impl GraphDistanceMetric {
    /// Returns the stable machine-readable metric label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SquaredL2 => "l2",
            Self::Cosine => "cosine",
        }
    }
}

/// Frozen geometry and file paths for one Task 19-M4b dataset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossGraphDataset {
    name: &'static str,
    rows: usize,
    dimensions: usize,
    query_count: usize,
    metric: GraphDistanceMetric,
    base: PathBuf,
    queries: PathBuf,
    ground_truth: PathBuf,
}

impl CrossGraphDataset {
    /// Resolves one of the three explicitly authorized M4b datasets.
    pub fn named(name: &str, directory: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let (name, rows, dimensions, query_count, metric) = match name {
            "glove-100-angular" => (
                "glove-100-angular",
                1_183_514,
                100,
                10_000,
                GraphDistanceMetric::Cosine,
            ),
            "nytimes-256-angular" => (
                "nytimes-256-angular",
                290_000,
                256,
                10_000,
                GraphDistanceMetric::Cosine,
            ),
            "mnist-784-euclidean" => (
                "mnist-784-euclidean",
                60_000,
                784,
                10_000,
                GraphDistanceMetric::SquaredL2,
            ),
            _ => {
                return Err(io::Error::other(format!(
                    "dataset {name:?} is not one of glove-100-angular, nytimes-256-angular, or mnist-784-euclidean"
                ))
                .into());
            }
        };
        Ok(Self {
            name,
            rows,
            dimensions,
            query_count,
            metric,
            base: directory.join(format!("{name}-base.f32bin")),
            queries: directory.join(format!("{name}-query.f32bin")),
            ground_truth: directory.join(format!("{name}-gt.i32bin")),
        })
    }

    /// Returns the frozen dataset name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the frozen base-row count.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// Returns the logical dimensions per row.
    #[must_use]
    pub const fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Returns the frozen query count.
    #[must_use]
    pub const fn query_count(&self) -> usize {
        self.query_count
    }

    /// Returns the dataset's requested distance semantics.
    #[must_use]
    pub const fn metric(&self) -> GraphDistanceMetric {
        self.metric
    }

    /// Returns the raw base-vector path.
    #[must_use]
    pub fn base_path(&self) -> &Path {
        &self.base
    }

    /// Returns the raw query-vector path.
    #[must_use]
    pub fn query_path(&self) -> &Path {
        &self.queries
    }

    /// Returns the raw ground-truth path.
    #[must_use]
    pub fn ground_truth_path(&self) -> &Path {
        &self.ground_truth
    }
}

/// Persisted measurements and semantics for one cached M4b graph build.
#[derive(Clone, Debug, PartialEq)]
pub struct CrossGraphBuildMetadata {
    build_wall_seconds: f64,
    peak_rss_bytes: u64,
    zero_norm_rows: Vec<u32>,
    cache_hit: bool,
}

impl CrossGraphBuildMetadata {
    /// Returns wall time for the complete input-preparation and graph-build pipeline.
    #[must_use]
    pub const fn build_wall_seconds(&self) -> f64 {
        self.build_wall_seconds
    }

    /// Returns the build process's maximum resident-set size.
    #[must_use]
    pub const fn peak_rss_bytes(&self) -> u64 {
        self.peak_rss_bytes
    }

    /// Returns base rows excluded from cosine results because they have no direction.
    #[must_use]
    pub fn zero_norm_rows(&self) -> &[u32] {
        &self.zero_norm_rows
    }

    /// Returns whether this invocation opened an already complete artifact.
    #[must_use]
    pub const fn cache_hit(&self) -> bool {
        self.cache_hit
    }
}

/// One validated cached graph plus the measurements from the process that built it.
pub struct CrossGraphArtifact {
    reader: SegmentReader,
    metadata: CrossGraphBuildMetadata,
}

impl CrossGraphArtifact {
    /// Returns the production segment reader over the graph artifact.
    #[must_use]
    pub const fn reader(&self) -> &SegmentReader {
        &self.reader
    }

    /// Returns persisted build measurements and zero-vector semantics.
    #[must_use]
    pub const fn metadata(&self) -> &CrossGraphBuildMetadata {
        &self.metadata
    }
}

/// Recall measured after f32 rescoring of the complete ef-sized graph pool.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecallPoint {
    /// Search breadth used by the measurement-only best-first traversal.
    pub ef: usize,
    /// Mean set recall against the first 100 exact neighbours per query.
    pub recall_at_100: f64,
}

/// Builds one requested pass count and atomically publishes it through M2.
pub fn build_sift1m_graph(
    paths: &Sift1mPaths,
    cache_directory: &Path,
    passes: GraphBuildPasses,
    seed: u64,
) -> Result<SegmentReader, Box<dyn std::error::Error>> {
    fs::create_dir_all(cache_directory)?;
    let output_id = graph_id();
    let output_path = cache_directory.join(output_id.file_name());
    let marker_path = cache_directory.join("sift1m-graph-build.meta");
    let marker = graph_marker(passes, seed);
    if fs::read_to_string(&marker_path).ok().as_deref() == Some(marker.as_str())
        && let Ok(reader) = SegmentReader::open(&output_path, output_id)
    {
        return Ok(reader);
    }
    let input = prepare_input_segment(paths, cache_directory)?;
    let control = QueryControl::Cancel(CancelToken::new());
    let store_directory = cache_directory.join("cancellation-store");
    let store = Store::open(
        &store_directory,
        OpenOptions::default()
            .with_max_resident_bytes(u64::MAX)
            .with_max_temp_bytes(u64::MAX),
    )?;
    let lease = store.snapshot()?;
    let checkpoint = match passes {
        GraphBuildPasses::One => cache_directory.join("sift1m-alpha-1.checkpoint"),
        GraphBuildPasses::Two => cache_directory.join("sift1m-alpha-1-then-1_2.checkpoint"),
    };
    let artifact = build_graph_checkpointed(
        &store,
        &input,
        CheckpointedGraphBuild::new(GraphParams::sift_1m(), seed, passes, &checkpoint, &control),
        &lease,
    )?;
    publish_graph(&artifact, &input, cache_directory)?;
    let temporary_marker = cache_directory.join("sift1m-graph-build.meta.tmp");
    fs::write(&temporary_marker, marker.as_bytes())?;
    fs::rename(&temporary_marker, &marker_path)?;
    drop(lease);
    drop(store);
    SegmentReader::open(&output_path, output_id).map_err(Into::into)
}

/// Builds one M4b graph with the unchanged SIFT construction parameters.
///
/// A complete artifact and its build measurements are cached together. A
/// matching cache is opened without rebuilding; a partial checkpoint resumes
/// through the production M3 builder.
pub fn build_cross_dataset_graph(
    dataset: &CrossGraphDataset,
    cache_directory: &Path,
    passes: GraphBuildPasses,
    seed: u64,
) -> Result<CrossGraphArtifact, Box<dyn std::error::Error>> {
    fs::create_dir_all(cache_directory)?;
    if let Ok(cached) = open_cross_dataset_graph(dataset, cache_directory, passes, seed) {
        return Ok(cached);
    }

    let started = Instant::now();
    let (input, zero_norm_rows) = prepare_cross_input_segment(dataset, cache_directory)?;
    let control = QueryControl::Cancel(CancelToken::new());
    let store_directory = cache_directory.join("cancellation-store");
    let store = Store::open(
        &store_directory,
        OpenOptions::default()
            .with_max_resident_bytes(u64::MAX)
            .with_max_temp_bytes(u64::MAX),
    )?;
    let lease = store.snapshot()?;
    let checkpoint = cache_directory.join(match passes {
        GraphBuildPasses::One => format!("{}-alpha-1.checkpoint", dataset.name()),
        GraphBuildPasses::Two => {
            format!("{}-alpha-1-then-1_2.checkpoint", dataset.name())
        }
    });
    let artifact = build_graph_checkpointed(
        &store,
        &input,
        CheckpointedGraphBuild::new(GraphParams::sift_1m(), seed, passes, &checkpoint, &control),
        &lease,
    )?;
    publish_graph(&artifact, &input, cache_directory)?;
    let build_wall_seconds = started.elapsed().as_secs_f64();
    let peak_rss_bytes = peak_rss_bytes()?;
    let metadata = CrossGraphBuildMetadata {
        build_wall_seconds,
        peak_rss_bytes,
        zero_norm_rows,
        cache_hit: false,
    };
    write_cross_graph_marker(dataset, cache_directory, passes, seed, &metadata)?;
    drop(lease);
    drop(store);
    let reader = SegmentReader::open(&cache_directory.join(graph_id().file_name()), graph_id())?;
    Ok(CrossGraphArtifact { reader, metadata })
}

/// Opens an exact cached M4b build without ever starting a rebuild.
pub fn open_cross_dataset_graph(
    dataset: &CrossGraphDataset,
    cache_directory: &Path,
    passes: GraphBuildPasses,
    seed: u64,
) -> Result<CrossGraphArtifact, Box<dyn std::error::Error>> {
    let marker_path = cross_graph_marker_path(dataset, cache_directory);
    let marker = fs::read_to_string(&marker_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cached graph marker {} is unavailable: {error}",
                marker_path.display()
            ),
        )
    })?;
    let mut lines = marker.lines();
    let identity = lines
        .next()
        .ok_or_else(|| io::Error::other("cached graph marker has no identity line"))?;
    let expected_identity = cross_graph_identity(dataset, passes, seed);
    if identity != expected_identity {
        return Err(io::Error::other(format!(
            "cached graph identity mismatch: got {identity:?}, expected {expected_identity:?}"
        ))
        .into());
    }
    let measurements = lines
        .next()
        .ok_or_else(|| io::Error::other("cached graph marker has no measurement line"))?;
    if lines.next().is_some() {
        return Err(io::Error::other("cached graph marker has trailing lines").into());
    }
    let metadata = CrossGraphBuildMetadata {
        build_wall_seconds: parse_marker_value(measurements, "build_wall_s")?,
        peak_rss_bytes: parse_marker_value(measurements, "peak_rss_bytes")?,
        zero_norm_rows: parse_zero_rows(measurements)?,
        cache_hit: true,
    };
    let reader = SegmentReader::open(&cache_directory.join(graph_id().file_name()), graph_id())?;
    if reader.meta().row_count as usize != dataset.rows()
        || reader.meta().dims as usize != dataset.dimensions()
        || reader.meta().scheme != 4
    {
        return Err(io::Error::other(format!(
            "cached graph geometry is {}/{}/scheme {}, expected {}/{}/scheme 4",
            reader.meta().row_count,
            reader.meta().dims,
            reader.meta().scheme,
            dataset.rows(),
            dataset.dimensions()
        ))
        .into());
    }
    Ok(CrossGraphArtifact { reader, metadata })
}

/// Loads query rows and applies the same cosine transform used for the graph.
pub fn read_cross_dataset_queries(
    dataset: &CrossGraphDataset,
) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let expected = dataset
        .query_count()
        .checked_mul(dataset.dimensions())
        .ok_or_else(|| io::Error::other("cross-dataset query geometry overflow"))?;
    let mut queries = read_f32_raw(dataset.query_path(), expected)?;
    if dataset.metric() == GraphDistanceMetric::Cosine {
        let _ = normalize_cosine_rows(&mut queries, dataset.dimensions())?;
    }
    Ok(queries)
}

/// Loads the frozen top-100 ground truth for one M4b dataset.
pub fn read_cross_dataset_truth(
    dataset: &CrossGraphDataset,
) -> Result<Vec<i32>, Box<dyn std::error::Error>> {
    let expected = dataset
        .query_count()
        .checked_mul(TOP_K)
        .ok_or_else(|| io::Error::other("cross-dataset truth geometry overflow"))?;
    read_i32_raw(dataset.ground_truth_path(), expected)
}

fn graph_marker(passes: GraphBuildPasses, seed: u64) -> String {
    let pass = match passes {
        GraphBuildPasses::One => "one",
        GraphBuildPasses::Two => "two",
    };
    format!("m3-v1 pass={pass} seed={seed} rows={ROWS} dims={DIMS} r=32/44 l=100\n")
}

fn publish_graph(
    artifact: &GraphBuildArtifact,
    input: &SegmentReader,
    directory: &Path,
) -> Result<SegmentMeta, Box<dyn std::error::Error>> {
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)?;
    artifact
        .write_segment_with_graph(&StdVfs, directory, input, graph_id(), policy)
        .map_err(Into::into)
}

fn prepare_input_segment(
    paths: &Sift1mPaths,
    cache_directory: &Path,
) -> Result<SegmentReader, Box<dyn std::error::Error>> {
    prepare_input_segment_for_shape(paths, cache_directory, ROWS, DIMS)
}

fn prepare_input_segment_for_shape(
    paths: &Sift1mPaths,
    cache_directory: &Path,
    rows: usize,
    dimensions: usize,
) -> Result<SegmentReader, Box<dyn std::error::Error>> {
    let id = input_id();
    let segment_path = cache_directory.join(id.file_name());
    if let Ok(reader) = SegmentReader::open(&segment_path, id)
        && reader.meta().row_count as usize == rows
        && reader.meta().dims as usize == dimensions
        && reader.meta().scheme == 4
    {
        return Ok(reader);
    }
    let value_count = rows
        .checked_mul(dimensions)
        .ok_or_else(|| io::Error::other("SIFT input geometry overflow"))?;
    let rescore = read_f32_raw(&paths.base, value_count)?;
    let row_bytes = dimensions.div_ceil(2);
    let code_count = rows
        .checked_mul(row_bytes)
        .ok_or_else(|| io::Error::other("SIFT code geometry overflow"))?;
    let mut codes = vec![0_u8; code_count];
    let mut factors = Vec::with_capacity(rows);
    for (row, destination) in rescore
        .chunks_exact(dimensions)
        .zip(codes.chunks_exact_mut(row_bytes))
    {
        factors.push(quantize_bit4(row, destination)?);
    }
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new())?);
    for row in 0..rows {
        columns.push_row(i64::try_from(row)?, &[])?;
    }
    let columns = columns.finish()?;
    let row_count = u32::try_from(rows)?;
    let dims = u32::try_from(dimensions)?;
    let alive = AliveSet::new(row_count);
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)?;
    write_segment(
        &StdVfs,
        cache_directory,
        SegmentBuild {
            id,
            scheme: 4,
            dims,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        policy,
    )?;
    SegmentReader::open(&segment_path, id).map_err(Into::into)
}

fn prepare_cross_input_segment(
    dataset: &CrossGraphDataset,
    cache_directory: &Path,
) -> Result<(SegmentReader, Vec<u32>), Box<dyn std::error::Error>> {
    let marker_path = cross_input_marker_path(dataset, cache_directory);
    let identity = cross_input_identity(dataset);
    if let Ok(marker) = fs::read_to_string(&marker_path) {
        let mut lines = marker.lines();
        if lines.next() == Some(identity.as_str())
            && let Some(values) = lines.next()
            && lines.next().is_none()
            && let Ok(zero_norm_rows) = parse_zero_rows(values)
        {
            let id = input_id();
            let segment_path = cache_directory.join(id.file_name());
            if let Ok(reader) = SegmentReader::open(&segment_path, id)
                && reader.meta().row_count as usize == dataset.rows()
                && reader.meta().dims as usize == dataset.dimensions()
                && reader.meta().scheme == 4
            {
                return Ok((reader, zero_norm_rows));
            }
        }
    }

    let value_count = dataset
        .rows()
        .checked_mul(dataset.dimensions())
        .ok_or_else(|| io::Error::other("cross-dataset base geometry overflow"))?;
    let mut rescore = read_f32_raw(dataset.base_path(), value_count)?;
    let zero_norm_rows = if dataset.metric() == GraphDistanceMetric::Cosine {
        normalize_cosine_rows(&mut rescore, dataset.dimensions())?
    } else {
        Vec::new()
    };
    let row_bytes = dataset.dimensions().div_ceil(2);
    let code_bytes = dataset
        .rows()
        .checked_mul(row_bytes)
        .ok_or_else(|| io::Error::other("cross-dataset code geometry overflow"))?;
    let mut codes = vec![0_u8; code_bytes];
    let mut factors = Vec::with_capacity(dataset.rows());
    for (row, destination) in rescore
        .chunks_exact(dataset.dimensions())
        .zip(codes.chunks_exact_mut(row_bytes))
    {
        factors.push(quantize_bit4(row, destination)?);
    }
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new())?);
    for row in 0..dataset.rows() {
        let timestamp =
            i64::try_from(row).map_err(|_| io::Error::other("cross-dataset row id exceeds i64"))?;
        columns.push_row(timestamp, &[])?;
    }
    let columns = columns.finish()?;
    let row_count = u32::try_from(dataset.rows())
        .map_err(|_| io::Error::other("cross-dataset row count exceeds u32"))?;
    let dimensions = u32::try_from(dataset.dimensions())
        .map_err(|_| io::Error::other("cross-dataset dimensions exceed u32"))?;
    let alive = AliveSet::new(row_count);
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)?;
    let id = input_id();
    write_segment(
        &StdVfs,
        cache_directory,
        SegmentBuild {
            id,
            scheme: 4,
            dims: dimensions,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        policy,
    )?;
    let zero_rows = format_zero_rows(&zero_norm_rows);
    write_marker_atomically(
        &marker_path,
        &format!("{identity}\nzero_rows={zero_rows}\n"),
    )?;
    let segment_path = cache_directory.join(id.file_name());
    let reader = SegmentReader::open(&segment_path, id)?;
    Ok((reader, zero_norm_rows))
}

fn normalize_cosine_rows(
    values: &mut [f32],
    dimensions: usize,
) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    if dimensions == 0 || !values.len().is_multiple_of(dimensions) {
        return Err(io::Error::other(format!(
            "cosine row geometry {}/{} is invalid",
            values.len(),
            dimensions
        ))
        .into());
    }
    let mut zero_rows = Vec::new();
    for (row_index, row) in values.chunks_exact_mut(dimensions).enumerate() {
        let norm_squared = row.iter().map(|value| *value * *value).sum::<f32>();
        if !norm_squared.is_finite() {
            return Err(
                io::Error::other(format!("cosine row {row_index} has a non-finite norm")).into(),
            );
        }
        if norm_squared == 0.0 {
            zero_rows.push(
                u32::try_from(row_index)
                    .map_err(|_| io::Error::other("zero-norm row id exceeds u32"))?,
            );
            continue;
        }
        let inverse = norm_squared.sqrt().recip();
        for value in row {
            *value *= inverse;
        }
    }
    Ok(zero_rows)
}

fn cross_graph_marker_path(dataset: &CrossGraphDataset, directory: &Path) -> PathBuf {
    directory.join(format!("{}-graph-build.meta", dataset.name()))
}

fn cross_input_marker_path(dataset: &CrossGraphDataset, directory: &Path) -> PathBuf {
    directory.join(format!("{}-input.meta", dataset.name()))
}

fn cross_graph_identity(
    dataset: &CrossGraphDataset,
    passes: GraphBuildPasses,
    seed: u64,
) -> String {
    format!(
        "{CROSS_GRAPH_MARKER_VERSION} dataset={} pass={} seed={seed} rows={} dims={} metric={} r=32/44 alpha=1.0/1.2 l=100",
        dataset.name(),
        graph_pass_label(passes),
        dataset.rows(),
        dataset.dimensions(),
        dataset.metric().label(),
    )
}

fn cross_input_identity(dataset: &CrossGraphDataset) -> String {
    format!(
        "{CROSS_INPUT_MARKER_VERSION} dataset={} rows={} dims={} metric={}",
        dataset.name(),
        dataset.rows(),
        dataset.dimensions(),
        dataset.metric().label(),
    )
}

fn graph_pass_label(passes: GraphBuildPasses) -> &'static str {
    match passes {
        GraphBuildPasses::One => "one",
        GraphBuildPasses::Two => "two",
    }
}

fn write_cross_graph_marker(
    dataset: &CrossGraphDataset,
    cache_directory: &Path,
    passes: GraphBuildPasses,
    seed: u64,
    metadata: &CrossGraphBuildMetadata,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity = cross_graph_identity(dataset, passes, seed);
    let zero_rows = format_zero_rows(&metadata.zero_norm_rows);
    let marker = format!(
        "{identity}\nbuild_wall_s={:.9} peak_rss_bytes={} zero_rows={zero_rows}\n",
        metadata.build_wall_seconds, metadata.peak_rss_bytes
    );
    write_marker_atomically(&cross_graph_marker_path(dataset, cache_directory), &marker)
}

fn write_marker_atomically(
    marker_path: &Path,
    contents: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = marker_path.with_extension("meta.tmp");
    fs::write(&temporary, contents.as_bytes())?;
    fs::rename(&temporary, marker_path)?;
    Ok(())
}

fn format_zero_rows(rows: &[u32]) -> String {
    if rows.is_empty() {
        return String::from("none");
    }
    rows.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_zero_rows(line: &str) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    let value = marker_field(line, "zero_rows")?;
    if value == "none" {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(str::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn parse_marker_value<T>(line: &str, key: &str) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + 'static,
{
    marker_field(line, key)?.parse().map_err(Into::into)
}

fn marker_field<'a>(line: &'a str, key: &str) -> Result<&'a str, Box<dyn std::error::Error>> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(&prefix))
        .ok_or_else(|| io::Error::other(format!("marker has no {key}: {line}")).into())
}

#[cfg(target_os = "macos")]
fn peak_rss_bytes() -> Result<u64, Box<dyn std::error::Error>> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for one `rusage` value.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: getrusage returned success and initialized the output value.
    let usage = unsafe { usage.assume_init() };
    u64::try_from(usage.ru_maxrss).map_err(|_| io::Error::other("peak RSS was negative").into())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn peak_rss_bytes() -> Result<u64, Box<dyn std::error::Error>> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for one `rusage` value.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: getrusage returned success and initialized the output value.
    let usage = unsafe { usage.assume_init() };
    u64::try_from(usage.ru_maxrss)
        .ok()
        .and_then(|kilobytes| kilobytes.checked_mul(1024))
        .ok_or_else(|| io::Error::other("peak RSS byte conversion overflow").into())
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Result<u64, Box<dyn std::error::Error>> {
    Err(io::Error::other("peak RSS is unavailable on this platform").into())
}

/// Sweeps recall only. No elapsed time is captured or reported.
pub fn measure_sift1m_recall(
    reader: &SegmentReader,
    paths: &Sift1mPaths,
    ef_values: &[usize],
    seed: u64,
) -> Result<Vec<RecallPoint>, Box<dyn std::error::Error>> {
    measure_recall(
        reader,
        &paths.queries,
        &paths.ground_truth,
        RecallShape {
            dimensions: DIMS,
            query_count: QUERIES,
            top_k: TOP_K,
            max_ef: 240,
        },
        ef_values,
        seed,
    )
}

#[derive(Clone, Copy)]
struct RecallShape {
    dimensions: usize,
    query_count: usize,
    top_k: usize,
    max_ef: usize,
}

fn measure_recall(
    reader: &SegmentReader,
    query_path: &Path,
    truth_path: &Path,
    shape: RecallShape,
    ef_values: &[usize],
    seed: u64,
) -> Result<Vec<RecallPoint>, Box<dyn std::error::Error>> {
    if ef_values
        .iter()
        .any(|ef| *ef < shape.top_k || *ef > shape.max_ef)
    {
        return Err(format!("every ef must be in {}..={}", shape.top_k, shape.max_ef).into());
    }
    let query_values = shape
        .query_count
        .checked_mul(shape.dimensions)
        .ok_or_else(|| io::Error::other("recall query geometry overflow"))?;
    let truth_values = shape
        .query_count
        .checked_mul(shape.top_k)
        .ok_or_else(|| io::Error::other("recall truth geometry overflow"))?;
    let queries = read_f32_raw(query_path, query_values)?;
    let truth = read_i32_raw(truth_path, truth_values)?;
    let graph = reader.graph_node_blocks()?;
    let base = reader.rescore_f32()?;
    let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())?;
    let mut searcher = GraphSearcher::new(graph, base, &mut scratch)?;
    let mut totals = vec![0_u64; ef_values.len()];
    for (query_index, query) in queries.chunks_exact(shape.dimensions).enumerate() {
        let truth_start = query_index
            .checked_mul(shape.top_k)
            .ok_or("ground-truth offset overflow")?;
        let truth_end = truth_start
            .checked_add(shape.top_k)
            .ok_or("truth end overflow")?;
        let expected = truth
            .get(truth_start..truth_end)
            .ok_or("truth row missing")?;
        for (total, ef) in totals.iter_mut().zip(ef_values) {
            let result = searcher.search(
                GraphSearchRequest::new(query, shape.top_k, seed ^ query_index as u64).with_ef(*ef),
                None,
            )?;
            *total += result
                .candidates()
                .iter()
                .filter(|candidate| expected.contains(&(candidate.row_id() as i32)))
                .count() as u64;
        }
    }
    let denominator = truth_values as f64;
    Ok(ef_values
        .iter()
        .zip(totals)
        .map(|(ef, total)| RecallPoint {
            ef: *ef,
            recall_at_100: total as f64 / denominator,
        })
        .collect())
}

fn read_f32_raw(
    path: &Path,
    expected_values: usize,
) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    read_raw_words(path, expected_values, f32::from_le_bytes)
}

fn read_i32_raw(
    path: &Path,
    expected_values: usize,
) -> Result<Vec<i32>, Box<dyn std::error::Error>> {
    read_raw_words(path, expected_values, i32::from_le_bytes)
}

fn read_raw_words<T>(
    path: &Path,
    expected_values: usize,
    decode: impl Fn([u8; 4]) -> T,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    let expected_bytes = expected_values
        .checked_mul(4)
        .ok_or("raw byte count overflow")?;
    let actual_bytes = usize::try_from(fs::metadata(path)?.len())?;
    if actual_bytes != expected_bytes {
        return Err(format!(
            "{} has {actual_bytes} bytes, expected {expected_bytes}",
            path.display()
        )
        .into());
    }
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut output = Vec::with_capacity(expected_values);
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut remaining = expected_bytes;
    while remaining != 0 {
        let length = remaining.min(buffer.len());
        let chunk = buffer.get_mut(..length).ok_or("raw read buffer missing")?;
        reader.read_exact(chunk)?;
        for word in chunk.chunks_exact(4) {
            let raw: [u8; 4] = word.try_into()?;
            output.push(decode(raw));
        }
        remaining -= length;
    }
    Ok(output)
}

#[cfg(test)]
mod cross_dataset_tests {
    use super::{
        CrossGraphDataset, GraphDistanceMetric, RecallShape, Sift1mPaths,
        build_cross_dataset_graph, build_sift1m_graph, cross_graph_marker_path, graph_marker,
        measure_recall, measure_sift1m_recall, normalize_cosine_rows, open_cross_dataset_graph,
        prepare_cross_input_segment, prepare_input_segment_for_shape, read_cross_dataset_queries,
        read_cross_dataset_truth,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use zeppelin_embed::graph::build::GraphBuildPasses;

    #[test]
    fn cross_graph_dataset_contracts_are_exact() {
        let data = Path::new("/data");
        let glove =
            CrossGraphDataset::named("glove-100-angular", data).expect("known glove dataset");
        assert_eq!(glove.rows(), 1_183_514);
        assert_eq!(glove.dimensions(), 100);
        assert_eq!(glove.query_count(), 10_000);
        assert_eq!(glove.metric(), GraphDistanceMetric::Cosine);
        assert_eq!(glove.metric().label(), "cosine");
        assert_eq!(
            glove.base_path(),
            Path::new("/data/glove-100-angular-base.f32bin")
        );

        let nytimes =
            CrossGraphDataset::named("nytimes-256-angular", data).expect("known nytimes dataset");
        assert_eq!(nytimes.rows(), 290_000);
        assert_eq!(nytimes.dimensions(), 256);
        assert_eq!(nytimes.metric(), GraphDistanceMetric::Cosine);

        let mnist =
            CrossGraphDataset::named("mnist-784-euclidean", data).expect("known mnist dataset");
        assert_eq!(mnist.rows(), 60_000);
        assert_eq!(mnist.dimensions(), 784);
        assert_eq!(mnist.metric(), GraphDistanceMetric::SquaredL2);
        assert_eq!(mnist.metric().label(), "l2");
        assert!(CrossGraphDataset::named("sift-128-euclidean", data).is_err());
    }

    #[test]
    fn cosine_normalization_records_zero_rows_without_inventing_a_direction() {
        let mut rows = vec![3.0_f32, 4.0, 0.0, 0.0, -2.0, 0.0];
        let zero_rows = normalize_cosine_rows(&mut rows, 2).expect("valid row geometry");
        assert_eq!(zero_rows, vec![1]);
        assert_eq!(rows, vec![0.6, 0.8, 0.0, 0.0, -1.0, 0.0]);
    }

    #[test]
    fn small_cross_graph_build_cache_and_loaders_preserve_dataset_identity() {
        const ROWS: usize = 9;
        const DIMS: usize = 8;
        const QUERIES: usize = 2;
        const SEED: u64 = 0x19_0004_b0c0_0001;

        let directory = tempfile::tempdir().expect("small cross-graph directory");
        let dataset = CrossGraphDataset {
            name: "coverage-cosine",
            rows: ROWS,
            dimensions: DIMS,
            query_count: QUERIES,
            metric: GraphDistanceMetric::Cosine,
            base: directory.path().join("base.f32bin"),
            queries: directory.path().join("queries.f32bin"),
            ground_truth: directory.path().join("truth.i32bin"),
        };
        let mut base = Vec::with_capacity(ROWS * DIMS);
        for row in 0..ROWS {
            let mut values = vec![0.0_f32; DIMS];
            if row != 4 {
                *values
                    .get_mut(row % DIMS)
                    .expect("fixture coordinate exists") = (row + 1) as f32;
            }
            base.extend(values);
        }
        write_f32_words(&dataset.base, &base);
        write_f32_words(
            &dataset.queries,
            &[
                3.0, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 5.0, 12.0, 0.0, 0.0,
            ],
        );
        let truth = (0..QUERIES * super::TOP_K)
            .map(|value| (value % ROWS) as i32)
            .collect::<Vec<_>>();
        write_i32_words(&dataset.ground_truth, &truth);

        let cache = directory.path().join("cache");
        let built = build_cross_dataset_graph(&dataset, &cache, GraphBuildPasses::One, SEED)
            .expect("small cross graph builds");
        assert_eq!(built.reader().meta().row_count, ROWS as u32);
        assert_eq!(built.reader().meta().dims, DIMS as u32);
        assert_eq!(built.metadata().zero_norm_rows(), &[4]);
        assert!(!built.metadata().cache_hit());
        assert!(built.metadata().build_wall_seconds().is_finite());
        assert!(built.metadata().build_wall_seconds() >= 0.0);
        assert!(built.metadata().peak_rss_bytes() > 0);
        drop(built);

        let cached = build_cross_dataset_graph(&dataset, &cache, GraphBuildPasses::One, SEED)
            .expect("matching graph cache opens");
        assert!(cached.metadata().cache_hit());
        assert_eq!(cached.metadata().zero_norm_rows(), &[4]);

        let (cached_input, cached_zero_rows) =
            prepare_cross_input_segment(&dataset, &cache).expect("matching input cache opens");
        assert_eq!(cached_input.meta().row_count, ROWS as u32);
        assert_eq!(cached_zero_rows, [4]);
        drop(cached_input);

        let queries = read_cross_dataset_queries(&dataset).expect("small queries load");
        assert_eq!(queries.len(), QUERIES * DIMS);
        assert_eq!(queries.get(..2), Some([0.6, 0.8].as_slice()));
        let second_x = *queries.get(DIMS + 4).expect("second query x");
        let second_y = *queries.get(DIMS + 5).expect("second query y");
        let second_norm = second_x * second_x + second_y * second_y;
        assert!((second_norm - 1.0).abs() <= 2.0 * f32::EPSILON);
        assert!((second_y / second_x - 12.0 / 5.0).abs() <= f32::EPSILON);
        assert_eq!(
            read_cross_dataset_truth(&dataset).expect("small truth loads"),
            truth
        );

        let compact_truth = directory.path().join("compact-truth.i32bin");
        write_i32_words(&compact_truth, &[1, 0, 5, 4]);
        let recall = measure_recall(
            cached.reader(),
            &dataset.queries,
            &compact_truth,
            RecallShape {
                dimensions: DIMS,
                query_count: QUERIES,
                top_k: 2,
                max_ef: 4,
            },
            &[2, 4],
            SEED,
        )
        .expect("small recall sweep");
        assert_eq!(
            recall.iter().map(|point| point.ef).collect::<Vec<_>>(),
            [2, 4]
        );
        assert!(
            recall
                .iter()
                .all(|point| (0.0..=1.0).contains(&point.recall_at_100))
        );
        let narrow = recall.first().expect("narrow recall point").recall_at_100;
        let wide = recall.get(1).expect("wide recall point").recall_at_100;
        assert!(wide >= narrow);
        assert!(
            measure_recall(
                cached.reader(),
                &dataset.queries,
                &compact_truth,
                RecallShape {
                    dimensions: DIMS,
                    query_count: QUERIES,
                    top_k: 2,
                    max_ef: 4,
                },
                &[1],
                SEED,
            )
            .is_err()
        );

        let frozen_paths = Sift1mPaths::in_directory(directory.path());
        assert_eq!(
            frozen_paths.base,
            directory.path().join("sift-128-euclidean-base.f32bin")
        );
        assert_eq!(
            frozen_paths.queries,
            directory.path().join("sift-128-euclidean-query.f32bin")
        );
        assert_eq!(
            frozen_paths.ground_truth,
            directory.path().join("sift-128-euclidean-gt.i32bin")
        );
        fs::write(
            cache.join("sift1m-graph-build.meta"),
            graph_marker(GraphBuildPasses::One, SEED),
        )
        .expect("write exact frozen cache marker");
        let frozen_cached = build_sift1m_graph(&frozen_paths, &cache, GraphBuildPasses::One, SEED)
            .expect("frozen wrapper accepts an exact cache identity");
        assert_eq!(frozen_cached.meta().row_count, ROWS as u32);
        assert!(measure_sift1m_recall(&frozen_cached, &frozen_paths, &[99], SEED).is_err());
        assert!(measure_sift1m_recall(&frozen_cached, &frozen_paths, &[241], SEED).is_err());
        assert!(graph_marker(GraphBuildPasses::Two, SEED).contains("pass=two"));
        drop(frozen_cached);
        drop(cached);

        let sift_cache = directory.path().join("sift-input-cache");
        fs::create_dir(&sift_cache).expect("create SIFT input cache");
        let sift_paths = Sift1mPaths {
            base: dataset.base.clone(),
            queries: dataset.queries.clone(),
            ground_truth: compact_truth,
        };
        let prepared = prepare_input_segment_for_shape(&sift_paths, &sift_cache, ROWS, DIMS)
            .expect("small SIFT-shaped input builds");
        assert_eq!(prepared.meta().row_count, ROWS as u32);
        assert_eq!(prepared.meta().dims, DIMS as u32);
        drop(prepared);
        let reused = prepare_input_segment_for_shape(&sift_paths, &sift_cache, ROWS, DIMS)
            .expect("small SIFT-shaped input cache reopens");
        assert_eq!(reused.meta().row_count, ROWS as u32);
        drop(reused);

        let marker_path = cross_graph_marker_path(&dataset, &cache);
        let valid_marker = fs::read_to_string(&marker_path).expect("read valid marker");
        fs::write(&marker_path, format!("{valid_marker}trailing\n"))
            .expect("write trailing marker");
        assert!(open_cross_dataset_graph(&dataset, &cache, GraphBuildPasses::One, SEED).is_err());
        fs::write(
            &marker_path,
            "wrong identity\nbuild_wall_s=1 peak_rss_bytes=1 zero_rows=none\n",
        )
        .expect("write wrong identity marker");
        assert!(open_cross_dataset_graph(&dataset, &cache, GraphBuildPasses::One, SEED).is_err());
        fs::write(&marker_path, valid_marker).expect("restore valid marker");
        assert!(open_cross_dataset_graph(&dataset, &cache, GraphBuildPasses::Two, SEED).is_err());
    }

    #[test]
    fn cross_graph_loaders_reject_bad_geometry_and_nonfinite_cosine_rows() {
        let directory = tempfile::tempdir().expect("bad cross-loader directory");
        let dataset = CrossGraphDataset {
            name: "bad-loader",
            rows: 2,
            dimensions: 2,
            query_count: 1,
            metric: GraphDistanceMetric::SquaredL2,
            base: PathBuf::from("unused-base"),
            queries: directory.path().join("short-query.f32bin"),
            ground_truth: directory.path().join("short-truth.i32bin"),
        };
        write_f32_words(&dataset.queries, &[1.0]);
        write_i32_words(&dataset.ground_truth, &[1]);
        assert!(read_cross_dataset_queries(&dataset).is_err());
        assert!(read_cross_dataset_truth(&dataset).is_err());

        let mut invalid_geometry = vec![1.0_f32, 2.0, 3.0];
        assert!(normalize_cosine_rows(&mut invalid_geometry, 2).is_err());
        assert!(normalize_cosine_rows(&mut invalid_geometry, 0).is_err());
        let mut nonfinite = vec![f32::INFINITY, 0.0];
        assert!(normalize_cosine_rows(&mut nonfinite, 2).is_err());
    }

    fn write_f32_words(path: &Path, values: &[f32]) {
        let bytes = values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        fs::write(path, bytes).expect("write f32 words");
    }

    fn write_i32_words(path: &Path, values: &[i32]) {
        let bytes = values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        fs::write(path, bytes).expect("write i32 words");
    }
}
