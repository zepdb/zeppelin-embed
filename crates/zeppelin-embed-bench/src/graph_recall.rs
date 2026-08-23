//! SIFT-1M graph construction and recall support.

use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use zeppelin_embed::graph::GraphParams;
use zeppelin_embed::graph::build::{
    CheckpointedGraphBuild, GraphBuildArtifact, GraphBuildPasses, build_graph_checkpointed,
};
use zeppelin_embed::graph::search::{GraphSearchRequest, GraphSearcher};
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
    let id = input_id();
    let segment_path = cache_directory.join(id.file_name());
    if let Ok(reader) = SegmentReader::open(&segment_path, id)
        && reader.meta().row_count == ROWS as u32
        && reader.meta().dims == DIMS as u32
        && reader.meta().scheme == 4
    {
        return Ok(reader);
    }
    let rescore = read_f32_raw(&paths.base, ROWS * DIMS)?;
    let row_bytes = DIMS.div_ceil(2);
    let mut codes = vec![0_u8; ROWS * row_bytes];
    let mut factors = Vec::with_capacity(ROWS);
    for (row, destination) in rescore
        .chunks_exact(DIMS)
        .zip(codes.chunks_exact_mut(row_bytes))
    {
        factors.push(quantize_bit4(row, destination)?);
    }
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new())?);
    for row in 0..ROWS {
        columns.push_row(row as i64, &[])?;
    }
    let columns = columns.finish()?;
    let alive = AliveSet::new(ROWS as u32);
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)?;
    write_segment(
        &StdVfs,
        cache_directory,
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMS as u32,
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

/// Sweeps recall only. No elapsed time is captured or reported.
pub fn measure_sift1m_recall(
    reader: &SegmentReader,
    paths: &Sift1mPaths,
    ef_values: &[usize],
    seed: u64,
) -> Result<Vec<RecallPoint>, Box<dyn std::error::Error>> {
    if ef_values.iter().any(|ef| *ef < TOP_K || *ef > 240) {
        return Err("every ef must be in 100..=240".into());
    }
    let queries = read_f32_raw(&paths.queries, QUERIES * DIMS)?;
    let truth = read_i32_raw(&paths.ground_truth, QUERIES * TOP_K)?;
    let graph = reader.graph_node_blocks()?;
    let base = reader.rescore_f32()?;
    let mut searcher = GraphSearcher::new(graph, base)?;
    let mut totals = vec![0_u64; ef_values.len()];
    for (query_index, query) in queries.chunks_exact(DIMS).enumerate() {
        let truth_start = query_index
            .checked_mul(TOP_K)
            .ok_or("ground-truth offset overflow")?;
        let truth_end = truth_start.checked_add(TOP_K).ok_or("truth end overflow")?;
        let expected = truth
            .get(truth_start..truth_end)
            .ok_or("truth row missing")?;
        for (total, ef) in totals.iter_mut().zip(ef_values) {
            let result = searcher.search(GraphSearchRequest::new(
                query,
                TOP_K,
                *ef,
                seed ^ query_index as u64,
            ))?;
            *total += result
                .candidates()
                .iter()
                .filter(|candidate| expected.contains(&(candidate.row_id() as i32)))
                .count() as u64;
        }
    }
    let denominator = (QUERIES * TOP_K) as f64;
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
