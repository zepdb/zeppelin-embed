//! Recall-only measurement scaffold for the M3 flat-Vamana build.
//!
//! This deliberately is not the latency-tuned product traversal owned by M4.

use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use zeppelin_embed::graph::GraphParams;
use zeppelin_embed::graph::block::GraphNodeBlocks;
use zeppelin_embed::graph::build::{
    CheckpointedGraphBuild, GraphBuildArtifact, GraphBuildPasses, build_graph_checkpointed,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{Bit4Query, est_dot_bit4, prepare_bit4_query, quantize_bit4};
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
    let entries = graph_entry_points(&graph)?;
    let mut visited = vec![0_u32; graph.node_count() as usize];
    let mut epoch = 0_u32;
    let mut totals = vec![0_u64; ef_values.len()];
    for (query_index, query) in queries.chunks_exact(DIMS).enumerate() {
        let prepared = prepare_bit4_query(query, seed ^ query_index as u64)?;
        let query_norm = squared_norm(query);
        let truth_start = query_index
            .checked_mul(TOP_K)
            .ok_or("ground-truth offset overflow")?;
        let truth_end = truth_start.checked_add(TOP_K).ok_or("truth end overflow")?;
        let expected = truth
            .get(truth_start..truth_end)
            .ok_or("truth row missing")?;
        for (total, ef) in totals.iter_mut().zip(ef_values) {
            epoch = epoch.wrapping_add(1);
            if epoch == 0 {
                visited.fill(0);
                epoch = 1;
            }
            let pool = best_first_pool(
                &graph,
                &prepared,
                query_norm,
                &entries,
                *ef,
                &mut visited,
                epoch,
            )?;
            let exact = exact_rescore(&pool, base, query, DIMS, TOP_K)?;
            *total += exact
                .iter()
                .filter(|node| expected.contains(&(**node as i32)))
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

#[derive(Clone, Copy, Debug)]
struct ScoredNode {
    node_id: u32,
    distance: f64,
}

fn graph_entry_points(graph: &GraphNodeBlocks<'_>) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    for node_id in 0..graph.node_count() {
        if graph.block(node_id)?.flags() & 1 != 0 {
            entries.push(node_id);
        }
    }
    if entries.is_empty() {
        return Err("graph contains no entry points".into());
    }
    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
fn best_first_pool(
    graph: &GraphNodeBlocks<'_>,
    query: &Bit4Query,
    query_norm: f64,
    entries: &[u32],
    ef: usize,
    visited: &mut [u32],
    epoch: u32,
) -> Result<Vec<ScoredNode>, Box<dyn std::error::Error>> {
    let mut pool = Vec::with_capacity(ef);
    let mut frontier = Vec::with_capacity(ef * 2);
    for &entry in entries {
        mark_visited(visited, entry, epoch)?;
        let scored = graph_score(graph, query, query_norm, entry)?;
        insert_scored(&mut pool, scored, ef);
        frontier.push(scored);
    }
    while !frontier.is_empty() {
        frontier.sort_unstable_by(scored_worst_first);
        let candidate = frontier.pop().ok_or("frontier became empty")?;
        if pool.len() >= ef
            && pool
                .last()
                .is_some_and(|worst| candidate.distance > worst.distance)
        {
            break;
        }
        let block = graph.block(candidate.node_id)?;
        for neighbor in block.neighbors_padded().take(usize::from(block.degree())) {
            let node = usize::try_from(neighbor)?;
            let marker = visited.get_mut(node).ok_or("neighbor id is out of range")?;
            if *marker == epoch {
                continue;
            }
            *marker = epoch;
            let scored = graph_score(graph, query, query_norm, neighbor)?;
            if pool.len() < ef
                || pool
                    .last()
                    .is_some_and(|worst| scored.distance < worst.distance)
            {
                insert_scored(&mut pool, scored, ef);
                frontier.push(scored);
            }
        }
    }
    Ok(pool)
}

fn mark_visited(visited: &mut [u32], node_id: u32, epoch: u32) -> Result<(), &'static str> {
    let node = usize::try_from(node_id).map_err(|_| "entry id exceeds usize")?;
    let marker = visited.get_mut(node).ok_or("entry id is out of range")?;
    *marker = epoch;
    Ok(())
}

fn graph_score(
    graph: &GraphNodeBlocks<'_>,
    query: &Bit4Query,
    query_norm: f64,
    node_id: u32,
) -> Result<ScoredNode, Box<dyn std::error::Error>> {
    let block = graph.block(node_id)?;
    let dot = f64::from(est_dot_bit4(query, block.codes(), block.factors())?);
    let norm = block.factors().norm();
    Ok(ScoredNode {
        node_id,
        distance: (query_norm + norm * norm - 2.0 * dot).max(0.0),
    })
}

fn insert_scored(pool: &mut Vec<ScoredNode>, scored: ScoredNode, limit: usize) {
    pool.push(scored);
    pool.sort_unstable_by(scored_best_first);
    if pool.len() > limit {
        let _ = pool.pop();
    }
}

fn scored_best_first(left: &ScoredNode, right: &ScoredNode) -> std::cmp::Ordering {
    left.distance
        .total_cmp(&right.distance)
        .then_with(|| left.node_id.cmp(&right.node_id))
}

fn scored_worst_first(left: &ScoredNode, right: &ScoredNode) -> std::cmp::Ordering {
    scored_best_first(right, left)
}

fn exact_rescore(
    candidates: &[ScoredNode],
    base: &[f32],
    query: &[f32],
    dimensions: usize,
    k: usize,
) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    let mut exact = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let row = usize::try_from(candidate.node_id)?;
        let start = row
            .checked_mul(dimensions)
            .ok_or("rescore offset overflow")?;
        let end = start
            .checked_add(dimensions)
            .ok_or("rescore end overflow")?;
        let vector = base.get(start..end).ok_or("rescore row missing")?;
        let distance = vector
            .iter()
            .zip(query)
            .map(|(left, right)| {
                let delta = f64::from(*left) - f64::from(*right);
                delta * delta
            })
            .sum();
        exact.push(ScoredNode {
            node_id: candidate.node_id,
            distance,
        });
    }
    exact.sort_unstable_by(scored_best_first);
    Ok(exact
        .into_iter()
        .take(k)
        .map(|candidate| candidate.node_id)
        .collect())
}

fn squared_norm(values: &[f32]) -> f64 {
    values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum()
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
