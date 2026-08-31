//! Single-threaded flat Vamana graph construction.

use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::xxh3_64;

use crate::graph::GraphParams;
use crate::graph::block::{
    GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeError, GraphNodeLayout,
    NODE_BLOCK_TRAILER_LEN, decode_node_blocks, encode_node_blocks,
};
use crate::lifecycle::QueryCancellation;
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::lifecycle::stats::{AccountedCounter, Accounting, AllocationComponent};
use crate::lifecycle::{QueryControl, SnapshotLease, Store, StoreError};
use crate::quant::{Bit4Factors, QuantError, est_dot_bit4, prepare_bit4_query};
use crate::scan::ScanError;
use crate::segment::layout::RegionKind;
use crate::segment::reader::SegmentReader;
use crate::segment::writer::{
    SegmentBuild, SegmentCopiedRegion, SegmentDocumentVersions, SegmentFactors,
    write_segment_with_graph_and_copied_regions,
};
use crate::segment::{SegmentError, SegmentId, SegmentMeta};
use crate::vfs::Vfs;

const ENTRY_POINT_COUNT: usize = 4;
const MEDOID_SAMPLE_ROWS: usize = 1_000;
const CHECKPOINT_MAGIC: [u8; 8] = *b"ZEVAMCP1";
const CHECKPOINT_VERSION: u16 = 1;
const CHECKPOINT_HEADER_BYTES: usize = 80;
const CHECKPOINT_CHECKSUM_BYTES: usize = 8;
const CANCELLATION_CHECK_ROWS: usize = 64;

/// How graph promotion accounts for one source directory region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GraphRewriteRegion {
    /// The graph writer reconstructs this region from validated typed input.
    Reencoded,
    /// The graph writer replaces this derived region with a newly built one.
    Replaced,
    /// The graph writer copies the exact source version and payload bytes.
    CopyForward,
    /// Promotion must remain deferred because this region is not carried.
    Unsupported,
}

/// Defines the single guard/rewrite contract for every source region kind.
pub(crate) const fn graph_rewrite_source_region(kind: u16) -> GraphRewriteRegion {
    match RegionKind::from_id(kind) {
        Some(
            RegionKind::Columns
            | RegionKind::Alive
            | RegionKind::VectorCodes
            | RegionKind::VectorFactors
            | RegionKind::VectorRescore
            | RegionKind::DocumentVersions,
        ) => GraphRewriteRegion::Reencoded,
        Some(RegionKind::GraphNodeBlocks | RegionKind::ChecksumTable) => {
            GraphRewriteRegion::Replaced
        }
        Some(RegionKind::Postings | RegionKind::StoredMetadata | RegionKind::StoredText) | None => {
            GraphRewriteRegion::CopyForward
        }
        Some(
            RegionKind::GraphColocatedCodes
            | RegionKind::SignPlane
            | RegionKind::PdxClusteredBlocks
            | RegionKind::VectorSpaceN,
        ) => GraphRewriteRegion::Unsupported,
    }
}

/// Number of alpha-pruning passes applied by one measurement/build request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphBuildPasses {
    /// The shipped alpha=1.0 build: faster at matched recall across 10 processes.
    /// Provenance: `docs/19-m5-defaults.md`.
    #[default]
    One,
    /// Explicit research arm; M5 found its hop reduction did not pay for itself.
    Two,
}

/// Completed owned graph bytes ready for the existing node-block writer.
pub struct GraphBuildArtifact {
    encoded_region: Vec<u8>,
    entry_points: Vec<u32>,
    work_rows_completed: u64,
    _memory: Option<AccountedCounter>,
}

impl GraphBuildArtifact {
    /// Returns the complete fixed-stride graph region for deterministic evidence.
    #[must_use]
    pub fn encoded_region(&self) -> &[u8] {
        &self.encoded_region
    }

    /// Returns the medoid followed by up to three deterministic refined seeds.
    #[must_use]
    pub fn entry_points(&self) -> &[u32] {
        &self.entry_points
    }

    /// Returns rows advanced by this checkpointed invocation, excluding prior work.
    #[must_use]
    pub const fn work_rows_completed(&self) -> u64 {
        self.work_rows_completed
    }

    /// Atomically publishes the sealed input plus this graph through M2's writer.
    pub fn write_segment_with_graph(
        &self,
        vfs: &dyn Vfs,
        directory: &Path,
        input: &SegmentReader,
        output_id: SegmentId,
        policy: DurabilityPolicy,
    ) -> Result<SegmentMeta, GraphBuildError> {
        let decoded = decode_node_blocks(&self.encoded_region)?;
        let mut blocks = Vec::with_capacity(decoded.node_count() as usize);
        let mut neighbor_rows = Vec::with_capacity(decoded.node_count() as usize);
        for node_id in 0..decoded.node_count() {
            let block = decoded.block(node_id)?;
            let neighbors = block
                .neighbors_padded()
                .take(usize::from(block.degree()))
                .collect::<Vec<_>>();
            blocks.push(block);
            neighbor_rows.push(neighbors);
        }
        let mut nodes = Vec::with_capacity(decoded.node_count() as usize);
        for (block, neighbors) in blocks.iter().zip(&neighbor_rows) {
            nodes.push(GraphNodeBlockInput {
                codes: block.codes(),
                factors: block.factors(),
                flags: block.flags(),
                neighbors,
            });
        }
        let columns = input.columns()?;
        let alive = input.alive()?;
        let factors = input.bit4_factors()?;
        let mut doc_ids = Vec::with_capacity(input.meta().row_count as usize);
        let mut revisions = Vec::with_capacity(input.meta().row_count as usize);
        let mut has_documents = None;
        for row in 0..input.meta().row_count as usize {
            let document = input.document_version(row)?;
            let present = document.is_some();
            if has_documents.is_some_and(|expected| expected != present) {
                return Err(GraphBuildError::Segment(SegmentError::Geometry(
                    "document-version region is present for only part of the segment".to_owned(),
                )));
            }
            has_documents = Some(present);
            if let Some(document) = document {
                doc_ids.push(document.doc_id());
                revisions.push(document.revision());
            }
        }
        let build = SegmentBuild {
            id: output_id,
            scheme: input.meta().scheme,
            dims: input.meta().dims,
            codes: input.bit4_codes()?,
            factors: SegmentFactors::Bit4(factors),
            rescore: input.rescore_f32()?,
            columns: &columns,
            alive: &alive,
        };
        let graph = GraphNodeBlockBuild {
            layout: decoded.layout(),
            nodes: &nodes,
        };
        let documents = (has_documents == Some(true)).then_some(SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        });
        let mut copied_regions = Vec::new();
        for entry in input.directory() {
            match graph_rewrite_source_region(entry.kind) {
                GraphRewriteRegion::CopyForward => {
                    copied_regions.push(SegmentCopiedRegion {
                        kind: entry.kind,
                        version: entry.version,
                        bytes: input.region_by_id(entry.kind)?,
                    });
                }
                GraphRewriteRegion::Unsupported => {
                    return Err(GraphBuildError::Segment(SegmentError::Geometry(format!(
                        "graph rewrite cannot carry source region kind {}",
                        entry.kind
                    ))));
                }
                GraphRewriteRegion::Reencoded | GraphRewriteRegion::Replaced => {}
            }
        }
        write_segment_with_graph_and_copied_regions(
            vfs,
            directory,
            build,
            graph,
            documents,
            &copied_regions,
            policy,
        )
        .map_err(GraphBuildError::Segment)
    }
}

/// Typed failure from construction before any graph artifact is published.
#[derive(Debug)]
pub enum GraphBuildError {
    /// The pinned epoch has no measured graph-construction profile.
    Profile(crate::graph::search::GraphProfileError),
    /// Reading, replacing, or removing the resumable checkpoint failed.
    CheckpointIo {
        /// Checkpoint or checkpoint-temporary path.
        path: PathBuf,
        /// Underlying filesystem failure.
        source: std::io::Error,
    },
    /// Checkpoint bytes failed their closed contract and were removed for a fresh retry.
    CheckpointCorrupt(String),
    /// A caller work limit stopped after atomically checkpointing completed batches.
    BudgetExhausted {
        /// Rows advanced by this invocation and persisted in the checkpoint.
        rows_completed: u64,
    },
    /// Caller cancellation stopped a batch without publishing partial results.
    Cancelled {
        /// Permanently false: graph builds never return partial artifacts.
        partial: bool,
    },
    /// A deadline stopped a batch without publishing partial results.
    Timeout {
        /// Permanently false: graph builds never return partial artifacts.
        partial: bool,
    },
    /// Store close stopped a batch before its snapshot could be released.
    ReadCancelled {
        /// Permanently false: graph builds never return partial artifacts.
        partial: bool,
    },
    /// The sealed segment did not expose valid Bit4 or f32 regions.
    Segment(SegmentError),
    /// A Bit4 query or score failed validation.
    Quant(QuantError),
    /// The frozen node-block writer rejected constructed values.
    NodeBlock(GraphNodeError),
    /// Checked row, dimension, or allocation geometry was invalid.
    Geometry(String),
    /// A graph node id was checked before address arithmetic and was out of range.
    NodeIdOutOfRange {
        /// Rejected dense row id.
        node_id: u32,
        /// Valid dense row count.
        node_count: u32,
    },
    /// Store budget or accounting rejected the graph construction arena.
    Store(StoreError),
}

impl std::fmt::Display for GraphBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Profile(error) => write!(formatter, "graph build {error}"),
            Self::CheckpointIo { path, source } => {
                write!(
                    formatter,
                    "graph build checkpoint I/O {}: {source}",
                    path.display()
                )
            }
            Self::CheckpointCorrupt(detail) => {
                write!(formatter, "graph build checkpoint is corrupt: {detail}")
            }
            Self::BudgetExhausted { rows_completed } => write!(
                formatter,
                "graph build work budget exhausted after {rows_completed} checkpointed rows"
            ),
            Self::Cancelled { partial } => {
                write!(formatter, "graph build was cancelled (partial={partial})")
            }
            Self::Timeout { partial } => {
                write!(
                    formatter,
                    "graph build deadline expired (partial={partial})"
                )
            }
            Self::ReadCancelled { partial } => write!(
                formatter,
                "store close cancelled graph build (partial={partial})"
            ),
            Self::Segment(error) => write!(formatter, "graph build segment input: {error}"),
            Self::Quant(error) => write!(formatter, "graph build Bit4 score: {error}"),
            Self::NodeBlock(error) => write!(formatter, "graph build node blocks: {error}"),
            Self::Geometry(detail) => write!(formatter, "graph build geometry: {detail}"),
            Self::NodeIdOutOfRange {
                node_id,
                node_count,
            } => write!(
                formatter,
                "graph build node id {node_id} is outside dense row count {node_count}"
            ),
            Self::Store(error) => write!(formatter, "graph build memory accounting: {error}"),
        }
    }
}

impl std::error::Error for GraphBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Profile(error) => Some(error),
            Self::CheckpointIo { source, .. } => Some(source),
            Self::Segment(error) => Some(error),
            Self::Quant(error) => Some(error),
            Self::NodeBlock(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::CheckpointCorrupt(_)
            | Self::BudgetExhausted { .. }
            | Self::Cancelled { .. }
            | Self::Timeout { .. }
            | Self::ReadCancelled { .. }
            | Self::Geometry(_)
            | Self::NodeIdOutOfRange { .. } => None,
        }
    }
}

struct GraphBuildSession<'a> {
    vfs: &'a dyn Vfs,
    policy: DurabilityPolicy,
    vectors: SegmentVectors<'a>,
    params: GraphParams,
    seed: u64,
    passes: GraphBuildPasses,
    checkpoint_path: PathBuf,
    state: BuildState,
}

impl<'a> GraphBuildSession<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        vfs: &'a dyn Vfs,
        reader: &'a SegmentReader,
        params: GraphParams,
        seed: u64,
        passes: GraphBuildPasses,
        checkpoint_path: &Path,
        policy: DurabilityPolicy,
        accounting: Option<&std::sync::Arc<Accounting>>,
    ) -> Result<Self, GraphBuildError> {
        let vectors = SegmentVectors::new(reader)?;
        let state = match vfs.read(checkpoint_path) {
            Ok(bytes) => {
                match decode_checkpoint(&bytes, &vectors, params, seed, passes, accounting) {
                    Ok(state) => state,
                    Err(error @ GraphBuildError::CheckpointCorrupt(_)) => {
                        remove_checkpoint(vfs, checkpoint_path)?;
                        return Err(error);
                    }
                    Err(other) => return Err(other),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                BuildState::new(&vectors, params, seed, accounting)?
            }
            Err(source) => {
                return Err(GraphBuildError::CheckpointIo {
                    path: checkpoint_path.to_path_buf(),
                    source,
                });
            }
        };
        Ok(Self {
            vfs,
            policy,
            vectors,
            params,
            seed,
            passes,
            checkpoint_path: checkpoint_path.to_path_buf(),
            state,
        })
    }

    fn advance_batch(
        &mut self,
        cancellation: &QueryCancellation<'_>,
    ) -> Result<bool, GraphBuildError> {
        check_cancellation(cancellation)?;
        let complete = self.state.advance_batch(
            &self.vectors,
            self.params,
            self.seed,
            self.passes,
            Some(cancellation),
        )?;
        check_cancellation(cancellation)?;
        {
            let checkpoint_bytes = checkpoint_encoded_bytes(&self.state)?;
            let base_charge = match self.state.memory.as_mut() {
                Some(charge) => {
                    let base = charge.bytes();
                    let peak = usize::try_from(base)
                        .ok()
                        .and_then(|base| base.checked_add(checkpoint_bytes))
                        .ok_or_else(|| {
                            GraphBuildError::Geometry("checkpoint accounting overflow".to_owned())
                        })?;
                    charge.set(peak)?;
                    Some(base)
                }
                None => None,
            };
            let written = write_checkpoint(
                self.vfs,
                self.policy,
                &self.checkpoint_path,
                &self.vectors,
                self.params,
                self.seed,
                self.passes,
                &self.state,
            );
            if let (Some(charge), Some(base)) = (self.state.memory.as_mut(), base_charge) {
                let base = usize::try_from(base).map_err(|_| {
                    GraphBuildError::Geometry("base accounting exceeds usize".to_owned())
                })?;
                charge.set(base)?;
            }
            written?;
        }
        Ok(complete)
    }

    fn completed_work_rows(&self) -> Result<u64, GraphBuildError> {
        let rows = u64::try_from(self.state.order.len())
            .map_err(|_| GraphBuildError::Geometry("graph row count exceeds u64".to_owned()))?;
        let next = u64::try_from(self.state.next_index)
            .map_err(|_| GraphBuildError::Geometry("graph progress exceeds u64".to_owned()))?;
        match self.state.phase {
            BuildPhase::Build => Ok(next),
            BuildPhase::Refine => rows
                .checked_add(next)
                .ok_or_else(|| GraphBuildError::Geometry("graph progress overflow".to_owned())),
            BuildPhase::Complete => match self.passes {
                GraphBuildPasses::One => Ok(rows),
                GraphBuildPasses::Two => rows
                    .checked_mul(2)
                    .ok_or_else(|| GraphBuildError::Geometry("graph progress overflow".to_owned())),
            },
        }
    }

    fn artifact(self) -> Result<GraphBuildArtifact, GraphBuildError> {
        if !self.state.is_complete(self.passes) {
            return Err(GraphBuildError::Geometry(
                "graph artifact requested before every pass completed".to_owned(),
            ));
        }
        let BuildState {
            entries,
            adjacency,
            memory,
            ..
        } = self.state;
        let artifact = encode_artifact(&self.vectors, self.params, &entries, &adjacency, memory)?;
        remove_checkpoint(self.vfs, &self.checkpoint_path)?;
        Ok(artifact)
    }
}

impl From<SegmentError> for GraphBuildError {
    fn from(error: SegmentError) -> Self {
        Self::Segment(error)
    }
}

impl From<QuantError> for GraphBuildError {
    fn from(error: QuantError) -> Self {
        Self::Quant(error)
    }
}

impl From<GraphNodeError> for GraphBuildError {
    fn from(error: GraphNodeError) -> Self {
        Self::NodeBlock(error)
    }
}

impl From<StoreError> for GraphBuildError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[cfg(test)]
fn build_graph(
    reader: &SegmentReader,
    params: GraphParams,
    seed: u64,
    passes: GraphBuildPasses,
) -> Result<GraphBuildArtifact, GraphBuildError> {
    let vectors = SegmentVectors::new(reader)?;
    let mut state = BuildState::new(&vectors, params, seed, None)?;
    while !state.advance_batch(&vectors, params, seed, passes, None)? {}
    let BuildState {
        entries,
        adjacency,
        memory,
        ..
    } = state;
    encode_artifact(&vectors, params, &entries, &adjacency, memory)
}

/// Validated controls for an interruptible graph build.
#[derive(Clone, Copy, Debug)]
pub struct CheckpointedGraphBuild<'a> {
    params: GraphParams,
    seed: u64,
    passes: GraphBuildPasses,
    checkpoint_path: &'a Path,
    control: &'a QueryControl,
    max_work_rows: Option<u64>,
}

impl<'a> CheckpointedGraphBuild<'a> {
    /// Packages the validated graph parameters with checkpoint and cancellation controls.
    #[must_use]
    pub fn new(
        params: GraphParams,
        seed: u64,
        passes: GraphBuildPasses,
        checkpoint_path: &'a Path,
        control: &'a QueryControl,
    ) -> Self {
        Self {
            params,
            seed,
            passes,
            checkpoint_path,
            control,
            max_work_rows: None,
        }
    }

    /// Stops after checkpointing at least this many rows in the current invocation.
    ///
    /// Callers that need a strict row limit align it to `checkpoint_batch_rows`.
    #[must_use]
    pub const fn with_max_work_rows(mut self, max_work_rows: u64) -> Self {
        self.max_work_rows = Some(max_work_rows);
        self
    }
}

/// Builds or resumes from a checkpoint while reusing Task 09-D cancellation.
///
/// The checkpoint is replaced atomically after every completed batch. No graph
/// artifact is returned on cancellation, timeout, or store-close cancellation.
/// A corrupt checkpoint is reported once and removed; the next invocation starts
/// a fresh build.
pub fn build_graph_checkpointed(
    store: &Store,
    reader: &SegmentReader,
    request: CheckpointedGraphBuild<'_>,
    lease: &SnapshotLease,
) -> Result<GraphBuildArtifact, GraphBuildError> {
    let cancellation = QueryCancellation::new(request.control, lease);
    check_cancellation(&cancellation)?;
    let mut session = GraphBuildSession::new(
        store.vfs.as_ref(),
        reader,
        request.params,
        request.seed,
        request.passes,
        request.checkpoint_path,
        store.durability_policy,
        Some(&store.accounting),
    )?;
    let started_rows = session.completed_work_rows()?;
    loop {
        let complete = session.advance_batch(&cancellation)?;
        let completed_rows = session
            .completed_work_rows()?
            .checked_sub(started_rows)
            .ok_or_else(|| GraphBuildError::Geometry("graph work progress regressed".to_owned()))?;
        if complete {
            let mut artifact = session.artifact()?;
            artifact.work_rows_completed = completed_rows;
            return Ok(artifact);
        }
        if request
            .max_work_rows
            .is_some_and(|maximum| completed_rows >= maximum)
        {
            return Err(GraphBuildError::BudgetExhausted {
                rows_completed: completed_rows,
            });
        }
    }
}

struct SegmentVectors<'a> {
    segment_id: [u8; 16],
    node_count: u32,
    dimensions: usize,
    code_stride: usize,
    codes: &'a [u8],
    factors: &'a [Bit4Factors],
    rescore: &'a [f32],
}

impl<'a> SegmentVectors<'a> {
    fn new(reader: &'a SegmentReader) -> Result<Self, GraphBuildError> {
        if reader.meta().scheme != 4 {
            return Err(GraphBuildError::Geometry(format!(
                "flat Vamana requires Bit4 scheme 4, got {}",
                reader.meta().scheme
            )));
        }
        let dimensions = reader.meta().dims as usize;
        let node_count = reader.meta().row_count;
        if node_count == 0 {
            return Err(GraphBuildError::Geometry(
                "flat Vamana requires at least one row".to_owned(),
            ));
        }
        Ok(Self {
            segment_id: *reader.meta().id.as_bytes(),
            node_count,
            dimensions,
            code_stride: dimensions.div_ceil(2),
            codes: reader.bit4_codes()?,
            factors: reader.bit4_factors()?,
            rescore: reader.rescore_f32()?,
        })
    }

    fn validate_node(&self, node_id: u32) -> Result<usize, GraphBuildError> {
        if node_id >= self.node_count {
            return Err(GraphBuildError::NodeIdOutOfRange {
                node_id,
                node_count: self.node_count,
            });
        }
        usize::try_from(node_id)
            .map_err(|_| GraphBuildError::Geometry("node id exceeds usize".to_owned()))
    }

    fn f32_row(&self, node_id: u32) -> Result<&'a [f32], GraphBuildError> {
        let node = self.validate_node(node_id)?;
        let start = node
            .checked_mul(self.dimensions)
            .ok_or_else(|| GraphBuildError::Geometry("f32 row offset overflow".to_owned()))?;
        let end = start
            .checked_add(self.dimensions)
            .ok_or_else(|| GraphBuildError::Geometry("f32 row end overflow".to_owned()))?;
        self.rescore.get(start..end).ok_or_else(|| {
            GraphBuildError::Geometry(format!("f32 row {node_id} is outside validated rescore"))
        })
    }

    fn code_row(&self, node_id: u32) -> Result<&'a [u8], GraphBuildError> {
        let node = self.validate_node(node_id)?;
        let start = node
            .checked_mul(self.code_stride)
            .ok_or_else(|| GraphBuildError::Geometry("Bit4 row offset overflow".to_owned()))?;
        let end = start
            .checked_add(self.code_stride)
            .ok_or_else(|| GraphBuildError::Geometry("Bit4 row end overflow".to_owned()))?;
        self.codes.get(start..end).ok_or_else(|| {
            GraphBuildError::Geometry(format!("Bit4 row {node_id} is outside validated codes"))
        })
    }

    fn factor(&self, node_id: u32) -> Result<Bit4Factors, GraphBuildError> {
        let node = self.validate_node(node_id)?;
        self.factors.get(node).copied().ok_or_else(|| {
            GraphBuildError::Geometry(format!("Bit4 factor {node_id} is unavailable"))
        })
    }

    fn exact_distance(&self, left: u32, right: u32) -> Result<f64, GraphBuildError> {
        let left = self.f32_row(left)?;
        let right = self.f32_row(right)?;
        Ok(left
            .iter()
            .zip(right)
            .map(|(left, right)| {
                let difference = f64::from(*left) - f64::from(*right);
                difference * difference
            })
            .sum())
    }
}

struct Adjacency {
    slots: Vec<u32>,
    degrees: Vec<u8>,
    node_count: u32,
    r_max: u8,
}

impl Adjacency {
    fn new(node_count: u32, r_max: u8) -> Result<Self, GraphBuildError> {
        let slot_count = (node_count as usize)
            .checked_mul(usize::from(r_max))
            .ok_or_else(|| GraphBuildError::Geometry("adjacency size overflow".to_owned()))?;
        Ok(Self {
            slots: vec![u32::MAX; slot_count],
            degrees: vec![0; node_count as usize],
            node_count,
            r_max,
        })
    }

    fn row_range(&self, node_id: u32) -> Result<std::ops::Range<usize>, GraphBuildError> {
        if node_id >= self.node_count {
            return Err(GraphBuildError::NodeIdOutOfRange {
                node_id,
                node_count: self.node_count,
            });
        }
        let start = (node_id as usize)
            .checked_mul(usize::from(self.r_max))
            .ok_or_else(|| GraphBuildError::Geometry("adjacency offset overflow".to_owned()))?;
        let end = start
            .checked_add(usize::from(self.r_max))
            .ok_or_else(|| GraphBuildError::Geometry("adjacency end overflow".to_owned()))?;
        Ok(start..end)
    }

    fn neighbors(&self, node_id: u32) -> Result<&[u32], GraphBuildError> {
        let node = usize::try_from(node_id)
            .map_err(|_| GraphBuildError::Geometry("node id exceeds usize".to_owned()))?;
        let degree = usize::from(*self.degrees.get(node).ok_or(
            GraphBuildError::NodeIdOutOfRange {
                node_id,
                node_count: self.node_count,
            },
        )?);
        let range = self.row_range(node_id)?;
        let start = range.start;
        let active_end = start
            .checked_add(degree)
            .ok_or_else(|| GraphBuildError::Geometry("degree end overflow".to_owned()))?;
        self.slots.get(start..active_end).ok_or_else(|| {
            GraphBuildError::Geometry(format!("adjacency row {node_id} is unavailable"))
        })
    }

    fn set_neighbors(&mut self, node_id: u32, neighbors: &[u32]) -> Result<(), GraphBuildError> {
        if neighbors.len() > usize::from(self.r_max) {
            return Err(GraphBuildError::Geometry(format!(
                "node {node_id} degree {} exceeds hard cap {}",
                neighbors.len(),
                self.r_max
            )));
        }
        for neighbor in neighbors {
            if *neighbor >= self.node_count {
                return Err(GraphBuildError::NodeIdOutOfRange {
                    node_id: *neighbor,
                    node_count: self.node_count,
                });
            }
        }
        let range = self.row_range(node_id)?;
        let row = self.slots.get_mut(range).ok_or_else(|| {
            GraphBuildError::Geometry(format!("adjacency row {node_id} is unavailable"))
        })?;
        row.fill(u32::MAX);
        let destination = row.get_mut(..neighbors.len()).ok_or_else(|| {
            GraphBuildError::Geometry(format!("adjacency row {node_id} active slice failed"))
        })?;
        destination.copy_from_slice(neighbors);
        let node = usize::try_from(node_id)
            .map_err(|_| GraphBuildError::Geometry("node id exceeds usize".to_owned()))?;
        let degree = self
            .degrees
            .get_mut(node)
            .ok_or(GraphBuildError::NodeIdOutOfRange {
                node_id,
                node_count: self.node_count,
            })?;
        *degree = u8::try_from(neighbors.len())
            .map_err(|_| GraphBuildError::Geometry("degree exceeds u8".to_owned()))?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildPhase {
    Build,
    Refine,
    Complete,
}

fn build_arena_bytes(node_count: u32, params: GraphParams) -> Result<usize, GraphBuildError> {
    let rows = usize::try_from(node_count)
        .map_err(|_| GraphBuildError::Geometry("node count exceeds usize".to_owned()))?;
    let slots = rows
        .checked_mul(usize::from(params.r_max()))
        .and_then(|count| count.checked_mul(std::mem::size_of::<u32>()))
        .ok_or_else(|| GraphBuildError::Geometry("accounted adjacency overflow".to_owned()))?;
    let row_state = rows
        .checked_mul(
            std::mem::size_of::<u32>()
                .checked_mul(2)
                .and_then(|bytes| bytes.checked_add(2))
                .ok_or_else(|| GraphBuildError::Geometry("row state width overflow".to_owned()))?,
        )
        .ok_or_else(|| GraphBuildError::Geometry("accounted row state overflow".to_owned()))?;
    let construction = usize::from(params.l_build());
    let scratch_per_candidate = std::mem::size_of::<ScoredNode>()
        .checked_mul(4)
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<u32>() * 6))
        .ok_or_else(|| GraphBuildError::Geometry("scratch width overflow".to_owned()))?;
    let candidate_scratch = construction
        .checked_add(usize::from(params.r_max()))
        .and_then(|candidates| candidates.checked_mul(scratch_per_candidate))
        .ok_or_else(|| GraphBuildError::Geometry("scratch arena overflow".to_owned()))?;
    let medoid_scratch = MEDOID_SAMPLE_ROWS
        .checked_mul(std::mem::size_of::<ScoredNode>())
        .ok_or_else(|| GraphBuildError::Geometry("medoid scratch overflow".to_owned()))?;
    let scratch = candidate_scratch.max(medoid_scratch);
    slots
        .checked_add(row_state)
        .and_then(|bytes| bytes.checked_add(ENTRY_POINT_COUNT * std::mem::size_of::<u32>()))
        .and_then(|bytes| bytes.checked_add(scratch))
        .ok_or_else(|| GraphBuildError::Geometry("graph build arena overflow".to_owned()))
}

struct BuildState {
    phase: BuildPhase,
    next_index: usize,
    order: Vec<u32>,
    entries: Vec<u32>,
    adjacency: Adjacency,
    inserted: Vec<u8>,
    visited: Vec<u32>,
    visit_epoch: u32,
    memory: Option<AccountedCounter>,
}

impl BuildState {
    fn new(
        vectors: &SegmentVectors<'_>,
        params: GraphParams,
        seed: u64,
        accounting: Option<&std::sync::Arc<Accounting>>,
    ) -> Result<Self, GraphBuildError> {
        let mut memory = match accounting {
            Some(accounting) => Some(AccountedCounter::new(
                accounting,
                AllocationComponent::Temporary,
            )?),
            None => None,
        };
        if let Some(charge) = memory.as_mut() {
            charge.set(build_arena_bytes(vectors.node_count, params)?)?;
        }
        let mut random = SplitMix64::new(seed);
        let mut order = (0..vectors.node_count).collect::<Vec<_>>();
        shuffle(&mut order, &mut random)?;
        let entries = refined_entry_points(vectors, &order)?;
        move_entries_to_front(&mut order, &entries);
        Ok(Self {
            phase: BuildPhase::Build,
            next_index: 0,
            order,
            entries,
            adjacency: Adjacency::new(vectors.node_count, params.r_max())?,
            inserted: vec![0; vectors.node_count as usize],
            visited: vec![0; vectors.node_count as usize],
            visit_epoch: 0,
            memory,
        })
    }

    fn is_complete(&self, _passes: GraphBuildPasses) -> bool {
        self.phase == BuildPhase::Complete
    }

    fn advance_batch(
        &mut self,
        vectors: &SegmentVectors<'_>,
        params: GraphParams,
        seed: u64,
        passes: GraphBuildPasses,
        cancellation: Option<&QueryCancellation<'_>>,
    ) -> Result<bool, GraphBuildError> {
        if self.phase == BuildPhase::Complete {
            return Ok(true);
        }
        let batch_rows = usize::try_from(params.checkpoint_batch_rows()).map_err(|_| {
            GraphBuildError::Geometry("checkpoint batch rows exceed usize".to_owned())
        })?;
        let end = self
            .next_index
            .checked_add(batch_rows)
            .map(|end| end.min(self.order.len()))
            .ok_or_else(|| GraphBuildError::Geometry("batch end overflow".to_owned()))?;
        let alpha = match self.phase {
            BuildPhase::Build => params.alpha_build(),
            BuildPhase::Refine => params.alpha_refine(),
            BuildPhase::Complete => return Ok(true),
        };
        let pass_seed = match self.phase {
            BuildPhase::Build => seed,
            BuildPhase::Refine => seed ^ 0xa1fa_1200_0000_0002,
            BuildPhase::Complete => seed,
        };
        for position in self.next_index..end {
            if position.is_multiple_of(CANCELLATION_CHECK_ROWS)
                && let Some(cancellation) = cancellation
            {
                check_cancellation(cancellation)?;
            }
            let order_position = match self.phase {
                BuildPhase::Build => position,
                BuildPhase::Refine => {
                    self.order.len().checked_sub(position + 1).ok_or_else(|| {
                        GraphBuildError::Geometry("refinement order underflow".to_owned())
                    })?
                }
                BuildPhase::Complete => position,
            };
            let node_id = *self.order.get(order_position).ok_or_else(|| {
                GraphBuildError::Geometry(format!(
                    "build order position {order_position} is unavailable"
                ))
            })?;
            process_node(
                vectors,
                params,
                pass_seed,
                alpha,
                node_id,
                &self.entries,
                &mut self.adjacency,
                &mut self.inserted,
                &mut self.visited,
                &mut self.visit_epoch,
            )?;
        }
        self.next_index = end;
        if self.next_index == self.order.len() {
            match (self.phase, passes) {
                (BuildPhase::Build, GraphBuildPasses::Two) => {
                    self.phase = BuildPhase::Refine;
                    self.next_index = 0;
                    self.inserted.fill(1);
                }
                (BuildPhase::Build, GraphBuildPasses::One) | (BuildPhase::Refine, _) => {
                    self.phase = BuildPhase::Complete;
                }
                (BuildPhase::Complete, _) => {}
            }
        }
        Ok(self.phase == BuildPhase::Complete)
    }
}

fn check_cancellation(cancellation: &QueryCancellation<'_>) -> Result<(), GraphBuildError> {
    match cancellation.check_graph() {
        Ok(()) => Ok(()),
        Err(ScanError::Cancelled { .. }) => Err(GraphBuildError::Cancelled { partial: false }),
        Err(ScanError::Timeout { .. }) => Err(GraphBuildError::Timeout { partial: false }),
        Err(ScanError::ReadCancelled { .. }) => {
            Err(GraphBuildError::ReadCancelled { partial: false })
        }
        Err(error) => Err(GraphBuildError::Geometry(format!(
            "query cancellation seam returned non-cancellation error: {error}"
        ))),
    }
}

fn checkpoint_temp_path(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}

#[allow(clippy::too_many_arguments)]
fn write_checkpoint(
    vfs: &dyn Vfs,
    policy: DurabilityPolicy,
    path: &Path,
    vectors: &SegmentVectors<'_>,
    params: GraphParams,
    seed: u64,
    passes: GraphBuildPasses,
    state: &BuildState,
) -> Result<(), GraphBuildError> {
    let directory = path.parent().ok_or_else(|| GraphBuildError::CheckpointIo {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "checkpoint path has no parent directory",
        ),
    })?;
    let slot_bytes = state
        .adjacency
        .slots
        .len()
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(|| GraphBuildError::Geometry("checkpoint slot bytes overflow".to_owned()))?;
    let payload_bytes = state
        .adjacency
        .degrees
        .len()
        .checked_add(slot_bytes)
        .ok_or_else(|| GraphBuildError::Geometry("checkpoint payload overflow".to_owned()))?;
    let total_bytes = CHECKPOINT_HEADER_BYTES
        .checked_add(payload_bytes)
        .and_then(|bytes| bytes.checked_add(CHECKPOINT_CHECKSUM_BYTES))
        .ok_or_else(|| GraphBuildError::Geometry("checkpoint length overflow".to_owned()))?;
    let mut bytes = Vec::with_capacity(total_bytes);
    bytes.extend_from_slice(&CHECKPOINT_MAGIC);
    bytes.extend_from_slice(&CHECKPOINT_VERSION.to_le_bytes());
    bytes.push(phase_code(state.phase));
    bytes.push(pass_code(passes));
    bytes.push(params.r_target());
    bytes.push(params.r_max());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&params.l_build().to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&params.checkpoint_batch_rows().to_le_bytes());
    bytes.extend_from_slice(&vectors.node_count.to_le_bytes());
    let dimensions = u32::try_from(vectors.dimensions)
        .map_err(|_| GraphBuildError::Geometry("checkpoint dimensions exceed u32".to_owned()))?;
    bytes.extend_from_slice(&dimensions.to_le_bytes());
    let next_index = u32::try_from(state.next_index)
        .map_err(|_| GraphBuildError::Geometry("checkpoint position exceeds u32".to_owned()))?;
    bytes.extend_from_slice(&next_index.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&seed.to_le_bytes());
    bytes.extend_from_slice(&params.alpha_build().to_bits().to_le_bytes());
    bytes.extend_from_slice(&params.alpha_refine().to_bits().to_le_bytes());
    bytes.extend_from_slice(&vectors.segment_id);
    bytes.extend_from_slice(
        &u64::try_from(payload_bytes)
            .map_err(|_| GraphBuildError::Geometry("checkpoint payload exceeds u64".to_owned()))?
            .to_le_bytes(),
    );
    if bytes.len() != CHECKPOINT_HEADER_BYTES {
        return Err(GraphBuildError::Geometry(format!(
            "checkpoint header encoded {} bytes, expected {CHECKPOINT_HEADER_BYTES}",
            bytes.len()
        )));
    }
    bytes.extend_from_slice(&state.adjacency.degrees);
    for neighbor in &state.adjacency.slots {
        bytes.extend_from_slice(&neighbor.to_le_bytes());
    }
    bytes.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
    let temporary = checkpoint_temp_path(path);
    if let Err(source) = vfs.write(&temporary, &bytes) {
        let _ = vfs.delete(&temporary);
        return Err(GraphBuildError::CheckpointIo {
            path: temporary,
            source,
        });
    }
    if let SyncRequirement::Sync(kind) = policy.data_file_sync()
        && let Err(source) = vfs.sync(&temporary, kind)
    {
        let _ = vfs.delete(&temporary);
        return Err(GraphBuildError::CheckpointIo {
            path: temporary,
            source,
        });
    }
    if let Err(source) = vfs.rename(&temporary, path) {
        let _ = vfs.delete(&temporary);
        return Err(GraphBuildError::CheckpointIo {
            path: path.to_path_buf(),
            source,
        });
    }
    match policy.directory_sync() {
        SyncRequirement::Skip => Ok(()),
        SyncRequirement::Sync(kind) => {
            vfs.sync(directory, kind)
                .map_err(|source| GraphBuildError::CheckpointIo {
                    path: directory.to_path_buf(),
                    source,
                })
        }
    }
}

fn checkpoint_encoded_bytes(state: &BuildState) -> Result<usize, GraphBuildError> {
    let slot_bytes = state
        .adjacency
        .slots
        .len()
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(|| GraphBuildError::Geometry("checkpoint slot bytes overflow".to_owned()))?;
    CHECKPOINT_HEADER_BYTES
        .checked_add(state.adjacency.degrees.len())
        .and_then(|bytes| bytes.checked_add(slot_bytes))
        .and_then(|bytes| bytes.checked_add(CHECKPOINT_CHECKSUM_BYTES))
        .ok_or_else(|| GraphBuildError::Geometry("checkpoint encoded bytes overflow".to_owned()))
}

fn remove_checkpoint(vfs: &dyn Vfs, path: &Path) -> Result<(), GraphBuildError> {
    let temporary = checkpoint_temp_path(path);
    match vfs.delete(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(GraphBuildError::CheckpointIo {
                path: temporary,
                source,
            });
        }
    }
    match vfs.delete(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GraphBuildError::CheckpointIo {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn decode_checkpoint(
    bytes: &[u8],
    vectors: &SegmentVectors<'_>,
    params: GraphParams,
    seed: u64,
    passes: GraphBuildPasses,
    accounting: Option<&std::sync::Arc<Accounting>>,
) -> Result<BuildState, GraphBuildError> {
    validate_graph_build_checkpoint(bytes)?;
    let minimum = CHECKPOINT_HEADER_BYTES
        .checked_add(CHECKPOINT_CHECKSUM_BYTES)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("minimum length overflow".to_owned()))?;
    if bytes.len() < minimum {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "truncated: need at least {minimum} bytes, got {}",
            bytes.len()
        )));
    }
    if bytes.get(..8) != Some(CHECKPOINT_MAGIC.as_slice()) {
        return Err(GraphBuildError::CheckpointCorrupt(
            "magic does not match ZEVAMCP1".to_owned(),
        ));
    }
    let checksum_start = bytes
        .len()
        .checked_sub(CHECKPOINT_CHECKSUM_BYTES)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("checksum underflow".to_owned()))?;
    let stored_checksum = read_checkpoint_u64(bytes, checksum_start, "checksum")?;
    let checksummed = bytes.get(..checksum_start).ok_or_else(|| {
        GraphBuildError::CheckpointCorrupt("checksummed prefix is unavailable".to_owned())
    })?;
    let actual_checksum = xxh3_64(checksummed);
    if stored_checksum != actual_checksum {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "xxh3-64 expected {stored_checksum:#018x}, computed {actual_checksum:#018x}"
        )));
    }
    let version = read_checkpoint_u16(bytes, 8, "version")?;
    if version != CHECKPOINT_VERSION {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "version {version}, expected {CHECKPOINT_VERSION}"
        )));
    }
    let phase = decode_phase(read_checkpoint_u8(bytes, 10, "phase")?)?;
    let stored_passes = decode_passes(read_checkpoint_u8(bytes, 11, "passes")?)?;
    if stored_passes != passes {
        return Err(GraphBuildError::CheckpointCorrupt(
            "pass count does not match requested build".to_owned(),
        ));
    }
    let stored_r_target = read_checkpoint_u8(bytes, 12, "target degree")?;
    let stored_r_max = read_checkpoint_u8(bytes, 13, "maximum degree")?;
    let reserved_14 = read_checkpoint_u16(bytes, 14, "reserved degree bytes")?;
    let stored_l_build = read_checkpoint_u16(bytes, 16, "construction width")?;
    let reserved_18 = read_checkpoint_u16(bytes, 18, "reserved width bytes")?;
    let stored_batch = read_checkpoint_u32(bytes, 20, "checkpoint batch")?;
    let node_count = read_checkpoint_u32(bytes, 24, "node count")?;
    let dimensions = read_checkpoint_u32(bytes, 28, "dimensions")?;
    let next_index = read_checkpoint_u32(bytes, 32, "next index")?;
    let reserved_36 = read_checkpoint_u32(bytes, 36, "reserved index bytes")?;
    let stored_seed = read_checkpoint_u64(bytes, 40, "seed")?;
    let stored_alpha_build = f32::from_bits(read_checkpoint_u32(bytes, 48, "build alpha")?);
    let stored_alpha_refine = f32::from_bits(read_checkpoint_u32(bytes, 52, "refine alpha")?);
    let segment_id = bytes
        .get(56..72)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("segment id is truncated".to_owned()))?;
    let payload_bytes = read_checkpoint_u64(bytes, 72, "payload length")?;
    if reserved_14 != 0 || reserved_18 != 0 || reserved_36 != 0 {
        return Err(GraphBuildError::CheckpointCorrupt(
            "reserved header bytes are non-zero".to_owned(),
        ));
    }
    if stored_r_target != params.r_target()
        || stored_r_max != params.r_max()
        || stored_l_build != params.l_build()
        || stored_batch != params.checkpoint_batch_rows()
        || stored_alpha_build.to_bits() != params.alpha_build().to_bits()
        || stored_alpha_refine.to_bits() != params.alpha_refine().to_bits()
        || node_count != vectors.node_count
        || dimensions != vectors.dimensions as u32
        || stored_seed != seed
        || segment_id != vectors.segment_id
    {
        return Err(GraphBuildError::CheckpointCorrupt(
            "checkpoint identity or graph parameters do not match input".to_owned(),
        ));
    }
    if passes == GraphBuildPasses::One && phase == BuildPhase::Refine {
        return Err(GraphBuildError::CheckpointCorrupt(
            "one-pass build cannot resume refinement".to_owned(),
        ));
    }
    let actual_payload = checksum_start
        .checked_sub(CHECKPOINT_HEADER_BYTES)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("payload underflow".to_owned()))?;
    if usize::try_from(payload_bytes).ok() != Some(actual_payload) {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "declared payload {payload_bytes} bytes, actual {actual_payload}"
        )));
    }
    let degree_bytes = node_count as usize;
    let degrees_end = CHECKPOINT_HEADER_BYTES
        .checked_add(degree_bytes)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("degree end overflow".to_owned()))?;
    let degrees = bytes
        .get(CHECKPOINT_HEADER_BYTES..degrees_end)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("degree bytes are truncated".to_owned()))?
        .to_vec();
    let expected_slots = (node_count as usize)
        .checked_mul(usize::from(params.r_max()))
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("slot count overflow".to_owned()))?;
    let slot_payload = bytes.get(degrees_end..checksum_start).ok_or_else(|| {
        GraphBuildError::CheckpointCorrupt("neighbor payload is unavailable".to_owned())
    })?;
    let expected_slot_bytes = expected_slots
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("slot bytes overflow".to_owned()))?;
    if slot_payload.len() != expected_slot_bytes {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "neighbor payload {} bytes, expected {expected_slot_bytes}",
            slot_payload.len()
        )));
    }
    let slots = slot_payload
        .chunks_exact(4)
        .map(|chunk| {
            let raw: [u8; 4] = chunk.try_into().map_err(|_| {
                GraphBuildError::CheckpointCorrupt("neighbor width is not four bytes".to_owned())
            })?;
            Ok(u32::from_le_bytes(raw))
        })
        .collect::<Result<Vec<_>, GraphBuildError>>()?;
    let adjacency = Adjacency {
        slots,
        degrees,
        node_count,
        r_max: params.r_max(),
    };
    validate_checkpoint_adjacency(&adjacency)?;
    let mut state = BuildState::new(vectors, params, seed, accounting)?;
    state.phase = phase;
    state.next_index = usize::try_from(next_index)
        .map_err(|_| GraphBuildError::CheckpointCorrupt("next index exceeds usize".to_owned()))?;
    if state.next_index > state.order.len() {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "next index {} exceeds row count {}",
            state.next_index,
            state.order.len()
        )));
    }
    state.adjacency = adjacency;
    state.inserted.fill(u8::from(phase != BuildPhase::Build));
    if phase == BuildPhase::Build {
        for node_id in state.order.iter().take(state.next_index) {
            let node = vectors.validate_node(*node_id)?;
            let marker = state.inserted.get_mut(node).ok_or_else(|| {
                GraphBuildError::CheckpointCorrupt("inserted marker is unavailable".to_owned())
            })?;
            *marker = 1;
        }
    }
    Ok(state)
}

/// Validates arbitrary resumable-build checkpoint bytes without allocating a graph.
///
/// This is the shared parser seam used by the dependency-free fuzz target.
pub fn validate_graph_build_checkpoint(bytes: &[u8]) -> Result<(), GraphBuildError> {
    let minimum = CHECKPOINT_HEADER_BYTES
        .checked_add(CHECKPOINT_CHECKSUM_BYTES)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("minimum length overflow".to_owned()))?;
    if bytes.len() < minimum {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "truncated: need at least {minimum} bytes, got {}",
            bytes.len()
        )));
    }
    if bytes.get(..8) != Some(CHECKPOINT_MAGIC.as_slice()) {
        return Err(GraphBuildError::CheckpointCorrupt(
            "magic does not match ZEVAMCP1".to_owned(),
        ));
    }
    let checksum_start = bytes
        .len()
        .checked_sub(CHECKPOINT_CHECKSUM_BYTES)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("checksum underflow".to_owned()))?;
    let stored_checksum = read_checkpoint_u64(bytes, checksum_start, "checksum")?;
    let checksummed = bytes.get(..checksum_start).ok_or_else(|| {
        GraphBuildError::CheckpointCorrupt("checksummed prefix is unavailable".to_owned())
    })?;
    let actual_checksum = xxh3_64(checksummed);
    if stored_checksum != actual_checksum {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "xxh3-64 expected {stored_checksum:#018x}, computed {actual_checksum:#018x}"
        )));
    }
    let version = read_checkpoint_u16(bytes, 8, "version")?;
    if version != CHECKPOINT_VERSION {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "version {version}, expected {CHECKPOINT_VERSION}"
        )));
    }
    let _ = decode_phase(read_checkpoint_u8(bytes, 10, "phase")?)?;
    let _ = decode_passes(read_checkpoint_u8(bytes, 11, "passes")?)?;
    let r_target = read_checkpoint_u8(bytes, 12, "target degree")?;
    let r_max = read_checkpoint_u8(bytes, 13, "maximum degree")?;
    let reserved_14 = read_checkpoint_u16(bytes, 14, "reserved degree bytes")?;
    let l_build = read_checkpoint_u16(bytes, 16, "construction width")?;
    let reserved_18 = read_checkpoint_u16(bytes, 18, "reserved width bytes")?;
    let checkpoint_batch = read_checkpoint_u32(bytes, 20, "checkpoint batch")?;
    let node_count = read_checkpoint_u32(bytes, 24, "node count")?;
    let dimensions = read_checkpoint_u32(bytes, 28, "dimensions")?;
    let next_index = read_checkpoint_u32(bytes, 32, "next position")?;
    let reserved_36 = read_checkpoint_u32(bytes, 36, "reserved position bytes")?;
    if reserved_14 != 0 || reserved_18 != 0 || reserved_36 != 0 {
        return Err(GraphBuildError::CheckpointCorrupt(
            "reserved header bytes are nonzero".to_owned(),
        ));
    }
    if r_target == 0 || r_target > r_max || l_build < u16::from(r_target) {
        return Err(GraphBuildError::CheckpointCorrupt(
            "stored degree and construction width are invalid".to_owned(),
        ));
    }
    if checkpoint_batch == 0 || node_count == 0 || dimensions == 0 || next_index > node_count {
        return Err(GraphBuildError::CheckpointCorrupt(
            "stored batch, shape, or next position is invalid".to_owned(),
        ));
    }
    let alpha_build = f32::from_bits(read_checkpoint_u32(bytes, 48, "build alpha")?);
    let alpha_refine = f32::from_bits(read_checkpoint_u32(bytes, 52, "refine alpha")?);
    if !alpha_build.is_finite()
        || !alpha_refine.is_finite()
        || alpha_build < 1.0
        || alpha_refine < alpha_build
    {
        return Err(GraphBuildError::CheckpointCorrupt(
            "stored alpha values are invalid".to_owned(),
        ));
    }
    let payload_bytes = usize::try_from(read_checkpoint_u64(bytes, 72, "payload bytes")?)
        .map_err(|_| GraphBuildError::CheckpointCorrupt("payload exceeds usize".to_owned()))?;
    let node_count_usize = usize::try_from(node_count)
        .map_err(|_| GraphBuildError::CheckpointCorrupt("node count exceeds usize".to_owned()))?;
    let slot_count = node_count_usize
        .checked_mul(usize::from(r_max))
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("slot count overflow".to_owned()))?;
    let expected_payload = slot_count
        .checked_mul(std::mem::size_of::<u32>())
        .and_then(|slot_bytes| slot_bytes.checked_add(node_count_usize))
        .ok_or_else(|| {
            GraphBuildError::CheckpointCorrupt("payload geometry overflow".to_owned())
        })?;
    if payload_bytes != expected_payload {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "payload bytes {payload_bytes}, expected {expected_payload}"
        )));
    }
    let expected_length = CHECKPOINT_HEADER_BYTES
        .checked_add(payload_bytes)
        .and_then(|length| length.checked_add(CHECKPOINT_CHECKSUM_BYTES))
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("file length overflow".to_owned()))?;
    if bytes.len() != expected_length {
        return Err(GraphBuildError::CheckpointCorrupt(format!(
            "file length {}, expected {expected_length}",
            bytes.len()
        )));
    }
    let degrees_start = CHECKPOINT_HEADER_BYTES;
    let slots_start = degrees_start
        .checked_add(node_count_usize)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt("slot offset overflow".to_owned()))?;
    for owner in 0..node_count_usize {
        let degree = usize::from(read_checkpoint_u8(
            bytes,
            degrees_start + owner,
            "node degree",
        )?);
        if degree > usize::from(r_max) {
            return Err(GraphBuildError::CheckpointCorrupt(format!(
                "node {owner} degree {degree} exceeds {r_max}"
            )));
        }
        let row_start = owner
            .checked_mul(usize::from(r_max))
            .and_then(|slot| slot.checked_mul(std::mem::size_of::<u32>()))
            .and_then(|offset| slots_start.checked_add(offset))
            .ok_or_else(|| GraphBuildError::CheckpointCorrupt("row offset overflow".to_owned()))?;
        let mut active = Vec::with_capacity(degree);
        for slot in 0..usize::from(r_max) {
            let offset = slot
                .checked_mul(std::mem::size_of::<u32>())
                .and_then(|offset| row_start.checked_add(offset))
                .ok_or_else(|| {
                    GraphBuildError::CheckpointCorrupt("neighbor offset overflow".to_owned())
                })?;
            let neighbor = read_checkpoint_u32(bytes, offset, "neighbor")?;
            if slot < degree {
                if neighbor >= node_count
                    || neighbor as usize == owner
                    || active.contains(&neighbor)
                {
                    return Err(GraphBuildError::CheckpointCorrupt(format!(
                        "node {owner} has invalid active neighbor {neighbor}"
                    )));
                }
                active.push(neighbor);
            } else if neighbor != u32::MAX {
                return Err(GraphBuildError::CheckpointCorrupt(format!(
                    "node {owner} inactive slot {slot} is not the sentinel"
                )));
            }
        }
    }
    Ok(())
}

fn validate_checkpoint_adjacency(adjacency: &Adjacency) -> Result<(), GraphBuildError> {
    for node_id in 0..adjacency.node_count {
        let neighbors = adjacency.neighbors(node_id)?;
        if neighbors.len() > usize::from(adjacency.r_max) {
            return Err(GraphBuildError::CheckpointCorrupt(format!(
                "node {node_id} exceeds maximum degree"
            )));
        }
        let mut seen = Vec::with_capacity(neighbors.len());
        for neighbor in neighbors {
            if *neighbor >= adjacency.node_count || *neighbor == node_id || seen.contains(neighbor)
            {
                return Err(GraphBuildError::CheckpointCorrupt(format!(
                    "node {node_id} has an out-of-range, self, or duplicate neighbor {neighbor}"
                )));
            }
            seen.push(*neighbor);
        }
        let range = adjacency.row_range(node_id)?;
        let inactive_start = range.start.checked_add(neighbors.len()).ok_or_else(|| {
            GraphBuildError::CheckpointCorrupt("inactive offset overflow".to_owned())
        })?;
        let inactive = adjacency
            .slots
            .get(inactive_start..range.end)
            .ok_or_else(|| {
                GraphBuildError::CheckpointCorrupt("inactive slots are unavailable".to_owned())
            })?;
        if inactive.iter().any(|neighbor| *neighbor != u32::MAX) {
            return Err(GraphBuildError::CheckpointCorrupt(format!(
                "node {node_id} has non-sentinel inactive slots"
            )));
        }
    }
    Ok(())
}

fn phase_code(phase: BuildPhase) -> u8 {
    match phase {
        BuildPhase::Build => 1,
        BuildPhase::Refine => 2,
        BuildPhase::Complete => 3,
    }
}

fn decode_phase(value: u8) -> Result<BuildPhase, GraphBuildError> {
    match value {
        1 => Ok(BuildPhase::Build),
        2 => Ok(BuildPhase::Refine),
        3 => Ok(BuildPhase::Complete),
        _ => Err(GraphBuildError::CheckpointCorrupt(format!(
            "unknown phase {value}"
        ))),
    }
}

fn pass_code(passes: GraphBuildPasses) -> u8 {
    match passes {
        GraphBuildPasses::One => 1,
        GraphBuildPasses::Two => 2,
    }
}

fn decode_passes(value: u8) -> Result<GraphBuildPasses, GraphBuildError> {
    match value {
        1 => Ok(GraphBuildPasses::One),
        2 => Ok(GraphBuildPasses::Two),
        _ => Err(GraphBuildError::CheckpointCorrupt(format!(
            "unknown pass count {value}"
        ))),
    }
}

fn read_checkpoint_u8(bytes: &[u8], offset: usize, field: &str) -> Result<u8, GraphBuildError> {
    bytes.get(offset).copied().ok_or_else(|| {
        GraphBuildError::CheckpointCorrupt(format!("{field} is truncated at byte {offset}"))
    })
}

fn read_checkpoint_u16(bytes: &[u8], offset: usize, field: &str) -> Result<u16, GraphBuildError> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt(format!("{field} offset overflow")))?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt(format!("{field} is truncated")))?
        .try_into()
        .map_err(|_| GraphBuildError::CheckpointCorrupt(format!("{field} width is invalid")))?;
    Ok(u16::from_le_bytes(raw))
}

fn read_checkpoint_u32(bytes: &[u8], offset: usize, field: &str) -> Result<u32, GraphBuildError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt(format!("{field} offset overflow")))?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt(format!("{field} is truncated")))?
        .try_into()
        .map_err(|_| GraphBuildError::CheckpointCorrupt(format!("{field} width is invalid")))?;
    Ok(u32::from_le_bytes(raw))
}

fn read_checkpoint_u64(bytes: &[u8], offset: usize, field: &str) -> Result<u64, GraphBuildError> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt(format!("{field} offset overflow")))?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| GraphBuildError::CheckpointCorrupt(format!("{field} is truncated")))?
        .try_into()
        .map_err(|_| GraphBuildError::CheckpointCorrupt(format!("{field} width is invalid")))?;
    Ok(u64::from_le_bytes(raw))
}

#[derive(Clone, Copy, Debug)]
struct ScoredNode {
    node_id: u32,
    distance: f64,
}

#[allow(clippy::too_many_arguments)]
fn process_node(
    vectors: &SegmentVectors<'_>,
    params: GraphParams,
    seed: u64,
    alpha: f32,
    node_id: u32,
    entries: &[u32],
    adjacency: &mut Adjacency,
    inserted: &mut [u8],
    visited: &mut [u32],
    visit_epoch: &mut u32,
) -> Result<(), GraphBuildError> {
    vectors.validate_node(node_id)?;
    let candidates = search_candidates(
        vectors,
        adjacency,
        inserted,
        node_id,
        entries,
        usize::from(params.l_build()),
        seed,
        visited,
        visit_epoch,
    )?;
    let mut candidate_ids = candidates
        .into_iter()
        .map(|candidate| candidate.node_id)
        .collect::<Vec<_>>();
    candidate_ids.extend_from_slice(adjacency.neighbors(node_id)?);
    let pruned = robust_prune(
        vectors,
        node_id,
        &candidate_ids,
        alpha,
        usize::from(params.r_target()),
    )?;
    adjacency.set_neighbors(node_id, &pruned)?;
    let node = vectors.validate_node(node_id)?;
    let inserted_node = inserted.get_mut(node).ok_or_else(|| {
        GraphBuildError::Geometry(format!("inserted marker {node_id} is unavailable"))
    })?;
    *inserted_node = 1;
    add_reciprocal_edges(vectors, adjacency, node_id, &pruned, params, alpha)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn search_candidates(
    vectors: &SegmentVectors<'_>,
    adjacency: &Adjacency,
    inserted: &[u8],
    query_id: u32,
    entries: &[u32],
    width: usize,
    seed: u64,
    visited: &mut [u32],
    visit_epoch: &mut u32,
) -> Result<Vec<ScoredNode>, GraphBuildError> {
    let query = vectors.f32_row(query_id)?;
    let query_norm_squared = query
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>();
    let prepared = prepare_bit4_query(query, seed ^ u64::from(query_id))?;
    *visit_epoch = visit_epoch.wrapping_add(1);
    if *visit_epoch == 0 {
        visited.fill(0);
        *visit_epoch = 1;
    }
    let epoch = *visit_epoch;
    let mut result = Vec::with_capacity(width);
    let mut frontier = Vec::with_capacity(width.saturating_mul(2));
    for &entry in entries {
        let entry_index = vectors.validate_node(entry)?;
        if entry == query_id
            || inserted.get(entry_index).copied() != Some(1)
            || visited.get(entry_index).copied() == Some(epoch)
        {
            continue;
        }
        if let Some(marker) = visited.get_mut(entry_index) {
            *marker = epoch;
        }
        let scored = score_node(vectors, &prepared, query_norm_squared, entry)?;
        insert_result(&mut result, scored, width);
        frontier.push(scored);
    }
    if frontier.is_empty()
        && let Some(start) = inserted.iter().position(|present| *present == 1)
    {
        let start = u32::try_from(start)
            .map_err(|_| GraphBuildError::Geometry("inserted start exceeds u32".to_owned()))?;
        let scored = score_node(vectors, &prepared, query_norm_squared, start)?;
        if let Some(marker) = visited.get_mut(start as usize) {
            *marker = epoch;
        }
        insert_result(&mut result, scored, width);
        frontier.push(scored);
    }
    while !frontier.is_empty() {
        frontier.sort_unstable_by(scored_worst_first);
        let Some(candidate) = frontier.pop() else {
            break;
        };
        let worst = result.last().map(|node| node.distance);
        if result.len() >= width && worst.is_some_and(|distance| candidate.distance > distance) {
            break;
        }
        for &neighbor in adjacency.neighbors(candidate.node_id)? {
            let neighbor_index = vectors.validate_node(neighbor)?;
            if neighbor == query_id
                || inserted.get(neighbor_index).copied() != Some(1)
                || visited.get(neighbor_index).copied() == Some(epoch)
            {
                continue;
            }
            if let Some(marker) = visited.get_mut(neighbor_index) {
                *marker = epoch;
            }
            let scored = score_node(vectors, &prepared, query_norm_squared, neighbor)?;
            let qualifies = result.len() < width
                || result
                    .last()
                    .is_some_and(|current_worst| scored.distance < current_worst.distance);
            if qualifies {
                insert_result(&mut result, scored, width);
                frontier.push(scored);
            }
        }
    }
    Ok(result)
}

fn score_node(
    vectors: &SegmentVectors<'_>,
    query: &crate::quant::Bit4Query,
    query_norm_squared: f64,
    node_id: u32,
) -> Result<ScoredNode, GraphBuildError> {
    vectors.validate_node(node_id)?;
    let factors = vectors.factor(node_id)?;
    let dot = f64::from(est_dot_bit4(query, vectors.code_row(node_id)?, factors)?);
    let norm = factors.norm();
    Ok(ScoredNode {
        node_id,
        distance: (query_norm_squared + norm * norm - 2.0 * dot).max(0.0),
    })
}

fn insert_result(result: &mut Vec<ScoredNode>, node: ScoredNode, width: usize) {
    result.push(node);
    result.sort_unstable_by(scored_best_first);
    if result.len() > width {
        let _ = result.pop();
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

fn robust_prune(
    vectors: &SegmentVectors<'_>,
    owner: u32,
    candidates: &[u32],
    alpha: f32,
    target: usize,
) -> Result<Vec<u32>, GraphBuildError> {
    vectors.validate_node(owner)?;
    let mut remaining = Vec::with_capacity(candidates.len());
    for &candidate in candidates {
        vectors.validate_node(candidate)?;
        if candidate != owner && !remaining.contains(&candidate) {
            remaining.push(candidate);
        }
    }
    let mut owner_distances = remaining
        .iter()
        .map(|candidate| {
            vectors
                .exact_distance(owner, *candidate)
                .map(|distance| ScoredNode {
                    node_id: *candidate,
                    distance,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    owner_distances.sort_unstable_by(scored_best_first);
    let mut selected = Vec::with_capacity(target);
    let alpha_squared = f64::from(alpha) * f64::from(alpha);
    while selected.len() < target {
        let Some(best) = owner_distances.first().copied() else {
            break;
        };
        selected.push(best.node_id);
        owner_distances.retain(|candidate| {
            if candidate.node_id == best.node_id {
                return false;
            }
            vectors
                .exact_distance(best.node_id, candidate.node_id)
                .map_or(true, |between| alpha_squared * between > candidate.distance)
        });
    }
    Ok(selected)
}

fn add_reciprocal_edges(
    vectors: &SegmentVectors<'_>,
    adjacency: &mut Adjacency,
    owner: u32,
    neighbors: &[u32],
    params: GraphParams,
    alpha: f32,
) -> Result<(), GraphBuildError> {
    for &neighbor in neighbors {
        vectors.validate_node(neighbor)?;
        let existing = adjacency.neighbors(neighbor)?.to_vec();
        if existing.contains(&owner) {
            continue;
        }
        let updated = if existing.len() < usize::from(params.r_max()) {
            let mut updated = existing;
            updated.push(owner);
            updated
        } else {
            let mut candidates = existing;
            candidates.push(owner);
            robust_prune(
                vectors,
                neighbor,
                &candidates,
                alpha,
                usize::from(params.r_target()),
            )?
        };
        adjacency.set_neighbors(neighbor, &updated)?;
    }
    Ok(())
}

fn refined_entry_points(
    vectors: &SegmentVectors<'_>,
    shuffled: &[u32],
) -> Result<Vec<u32>, GraphBuildError> {
    let sample = shuffled
        .get(..shuffled.len().min(MEDOID_SAMPLE_ROWS))
        .ok_or_else(|| GraphBuildError::Geometry("medoid sample is unavailable".to_owned()))?;
    let mut medoid_scores = sample
        .iter()
        .map(|candidate| {
            sample
                .iter()
                .try_fold(0.0_f64, |sum, other| {
                    vectors
                        .exact_distance(*candidate, *other)
                        .map(|distance| sum + distance)
                })
                .map(|distance| ScoredNode {
                    node_id: *candidate,
                    distance,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    medoid_scores.sort_unstable_by(scored_best_first);
    let medoid = medoid_scores
        .first()
        .map(|node| node.node_id)
        .ok_or_else(|| GraphBuildError::Geometry("medoid sample contains no rows".to_owned()))?;
    let mut entries = vec![medoid];
    while entries.len() < ENTRY_POINT_COUNT && entries.len() < sample.len() {
        let mut farthest = None::<ScoredNode>;
        for &candidate in sample {
            if entries.contains(&candidate) {
                continue;
            }
            let nearest = entries.iter().try_fold(f64::INFINITY, |nearest, entry| {
                vectors
                    .exact_distance(candidate, *entry)
                    .map(|distance| nearest.min(distance))
            })?;
            let scored = ScoredNode {
                node_id: candidate,
                distance: nearest,
            };
            if farthest.is_none_or(|current| {
                scored.distance > current.distance
                    || (scored.distance == current.distance && scored.node_id < current.node_id)
            }) {
                farthest = Some(scored);
            }
        }
        let Some(next) = farthest else {
            break;
        };
        entries.push(next.node_id);
    }
    Ok(entries)
}

fn move_entries_to_front(order: &mut Vec<u32>, entries: &[u32]) {
    order.retain(|node_id| !entries.contains(node_id));
    for &entry in entries.iter().rev() {
        order.insert(0, entry);
    }
}

fn encode_artifact(
    vectors: &SegmentVectors<'_>,
    params: GraphParams,
    entries: &[u32],
    adjacency: &Adjacency,
    mut memory: Option<AccountedCounter>,
) -> Result<GraphBuildArtifact, GraphBuildError> {
    let dimensions = u32::try_from(vectors.dimensions)
        .map_err(|_| GraphBuildError::Geometry("dimensions exceed u32".to_owned()))?;
    let padded_dimensions = dimensions
        .checked_add(127)
        .map(|value| value / 128 * 128)
        .ok_or_else(|| GraphBuildError::Geometry("padded dimensions overflow".to_owned()))?;
    let layout = GraphNodeLayout::new(dimensions, padded_dimensions, params.r_max())?;
    let padded_row_bytes = layout.code_bytes();
    let padded_length = (vectors.node_count as usize)
        .checked_mul(padded_row_bytes)
        .ok_or_else(|| GraphBuildError::Geometry("padded code bytes overflow".to_owned()))?;
    let encoded_length = (vectors.node_count as usize)
        .checked_mul(layout.stride() as usize)
        .and_then(|bytes| bytes.checked_add(NODE_BLOCK_TRAILER_LEN))
        .ok_or_else(|| GraphBuildError::Geometry("encoded graph bytes overflow".to_owned()))?;
    let node_inputs = (vectors.node_count as usize)
        .checked_mul(std::mem::size_of::<GraphNodeBlockInput<'_>>())
        .ok_or_else(|| GraphBuildError::Geometry("node input bytes overflow".to_owned()))?;
    let entry_bytes = entries
        .len()
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(|| GraphBuildError::Geometry("entry bytes overflow".to_owned()))?;
    if let Some(charge) = memory.as_mut() {
        let existing = usize::try_from(charge.bytes())
            .map_err(|_| GraphBuildError::Geometry("accounted bytes exceed usize".to_owned()))?;
        let peak = existing
            .checked_add(padded_length)
            .and_then(|bytes| bytes.checked_add(node_inputs))
            .and_then(|bytes| bytes.checked_add(encoded_length))
            .and_then(|bytes| bytes.checked_add(entry_bytes))
            .ok_or_else(|| GraphBuildError::Geometry("encode arena overflow".to_owned()))?;
        charge.set(peak)?;
    }
    let mut padded_codes = vec![0_u8; padded_length];
    for node_id in 0..vectors.node_count {
        let source = vectors.code_row(node_id)?;
        let start = (node_id as usize)
            .checked_mul(padded_row_bytes)
            .ok_or_else(|| GraphBuildError::Geometry("padded code offset overflow".to_owned()))?;
        let end = start
            .checked_add(source.len())
            .ok_or_else(|| GraphBuildError::Geometry("padded code end overflow".to_owned()))?;
        let destination = padded_codes.get_mut(start..end).ok_or_else(|| {
            GraphBuildError::Geometry(format!("padded code row {node_id} is unavailable"))
        })?;
        destination.copy_from_slice(source);
    }
    let mut nodes = Vec::with_capacity(vectors.node_count as usize);
    for node_id in 0..vectors.node_count {
        let start = (node_id as usize)
            .checked_mul(padded_row_bytes)
            .ok_or_else(|| GraphBuildError::Geometry("encoded code offset overflow".to_owned()))?;
        let end = start
            .checked_add(padded_row_bytes)
            .ok_or_else(|| GraphBuildError::Geometry("encoded code end overflow".to_owned()))?;
        let codes = padded_codes.get(start..end).ok_or_else(|| {
            GraphBuildError::Geometry(format!("encoded code row {node_id} is unavailable"))
        })?;
        nodes.push(GraphNodeBlockInput {
            codes,
            factors: vectors.factor(node_id)?,
            flags: u8::from(entries.contains(&node_id)),
            neighbors: adjacency.neighbors(node_id)?,
        });
    }
    let encoded = encode_node_blocks(GraphNodeBlockBuild {
        layout,
        nodes: &nodes,
    })?;
    let encoded_region = encoded.into_bytes();
    let entry_points = entries.to_vec();
    if let Some(charge) = memory.as_mut() {
        let retained = encoded_region
            .capacity()
            .checked_add(
                entry_points
                    .capacity()
                    .checked_mul(std::mem::size_of::<u32>())
                    .ok_or_else(|| {
                        GraphBuildError::Geometry("retained entry bytes overflow".to_owned())
                    })?,
            )
            .ok_or_else(|| GraphBuildError::Geometry("retained graph bytes overflow".to_owned()))?;
        charge.set(retained)?;
    }
    Ok(GraphBuildArtifact {
        encoded_region,
        entry_points,
        work_rows_completed: 0,
        _memory: memory,
    })
}

fn shuffle(values: &mut [u32], random: &mut SplitMix64) -> Result<(), GraphBuildError> {
    for upper in (1..values.len()).rev() {
        let modulus = u64::try_from(upper + 1)
            .map_err(|_| GraphBuildError::Geometry("shuffle width exceeds u64".to_owned()))?;
        let selected = usize::try_from(random.next_u64() % modulus)
            .map_err(|_| GraphBuildError::Geometry("shuffle index exceeds usize".to_owned()))?;
        values.swap(upper, selected);
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
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::{Rng, RngCore};
    use xxhash_rust::xxh3::xxh3_64;

    use crate::graph::block::decode_node_blocks;
    use crate::lifecycle::QueryCancellation;
    use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
    use crate::lifecycle::{
        CancelToken, ManualMonotonicClock, OpenOptions, QueryControl, Store, StoreTestDependencies,
    };
    use crate::meta::{AliveSet, ColumnStoreBuilder, Schema};
    use crate::quant::{Bit4Factors, quantize_bit4};
    use crate::segment::SegmentId;
    use crate::segment::reader::SegmentReader;
    use crate::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
    use crate::vfs::{CountingVfs, StdVfs, Vfs};
    use std::sync::Arc;

    use super::{
        Adjacency, CheckpointedGraphBuild, GraphBuildError, GraphBuildPasses, GraphBuildSession,
        SegmentVectors, build_graph, build_graph_checkpointed, decode_checkpoint,
        validate_graph_build_checkpoint,
    };
    use crate::graph::GraphParams;

    fn fixture_reader(directory: &std::path::Path, rows: usize) -> SegmentReader {
        let dimensions = 128_usize;
        let mut random = crate::test_support::seeded_rng(
            "graph::build::vamana_build_is_deterministic_under_seed::fixture",
        );
        let mut rescore = Vec::with_capacity(rows * dimensions);
        let mut codes = Vec::with_capacity(rows * dimensions.div_ceil(2));
        let mut factors = Vec::<Bit4Factors>::with_capacity(rows);
        for _ in 0..rows {
            let row = (0..dimensions)
                .map(|_| random.random_range(-2.0_f32..2.0_f32))
                .collect::<Vec<_>>();
            let start = codes.len();
            codes.resize(start + dimensions.div_ceil(2), 0);
            factors.push(quantize_bit4(&row, &mut codes[start..]).expect("fixture row quantizes"));
            rescore.extend(row);
        }
        let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("empty schema"));
        for row in 0..rows {
            columns
                .push_row(row as i64, &[])
                .expect("timestamp-only fixture row");
        }
        let columns = columns.finish().expect("fixture columns");
        let alive = AliveSet::new(rows as u32);
        let id = SegmentId::new(19, [7; 10]);
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("derived policy");
        write_segment(
            &StdVfs,
            directory,
            SegmentBuild {
                id,
                scheme: 4,
                dims: dimensions as u32,
                codes: &codes,
                factors: SegmentFactors::Bit4(&factors),
                rescore: &rescore,
                columns: &columns,
                alive: &alive,
            },
            policy,
        )
        .expect("sealed Bit4 fixture");
        SegmentReader::open(&StdVfs, &directory.join(id.file_name()), id).expect("fixture reader")
    }

    #[test]
    fn graph_checkpoint_io_goes_through_the_store_vfs() {
        let segment_directory = tempfile::tempdir().expect("checkpoint segment directory");
        let reader = fixture_reader(segment_directory.path(), 4);
        let store_directory = tempfile::tempdir().expect("checkpoint store directory");
        let checkpoint = store_directory.path().join("graph-build.checkpoint");
        let vfs = Arc::new(CountingVfs::new(StdVfs).with_read_path_filter(checkpoint.clone()));
        let store = Store::open_with_test_dependencies(
            store_directory.path(),
            OpenOptions::default(),
            StoreTestDependencies::new(vfs.clone(), Arc::new(ManualMonotonicClock::new())),
        )
        .expect("checkpoint store");
        let lease = store.snapshot().expect("checkpoint snapshot");
        let params = GraphParams::new(2, 3, 1.0, 1.0, 3, 1).expect("checkpoint params");
        let control = QueryControl::Cancel(CancelToken::new());

        vfs.reset();
        let interrupted = build_graph_checkpointed(
            &store,
            &reader,
            CheckpointedGraphBuild::new(params, 0x06, GraphBuildPasses::One, &checkpoint, &control)
                .with_max_work_rows(1),
            &lease,
        );
        assert!(matches!(
            interrupted,
            Err(GraphBuildError::BudgetExhausted { rows_completed: 1 })
        ));
        build_graph_checkpointed(
            &store,
            &reader,
            CheckpointedGraphBuild::new(params, 0x06, GraphBuildPasses::One, &checkpoint, &control),
            &lease,
        )
        .expect("resumed graph build");

        assert!(vfs.read_calls() >= 1, "checkpoint read bypassed Vfs");
        assert_eq!(
            vfs.filtered_read_calls(),
            1,
            "graph-build.checkpoint read bypassed Vfs"
        );
        assert!(vfs.write_calls() >= 1, "checkpoint write bypassed Vfs");
        assert!(vfs.rename_calls() >= 1, "checkpoint rename bypassed Vfs");
        assert!(vfs.delete_calls() >= 1, "checkpoint delete bypassed Vfs");
        drop(lease);
        store.close().expect("close checkpoint store");
    }

    #[test]
    fn graph_checkpoint_publish_syncs_the_temp_file_and_its_directory() {
        let segment_directory = tempfile::tempdir().expect("checkpoint segment directory");
        let reader = fixture_reader(segment_directory.path(), 4);
        let store_directory = tempfile::tempdir().expect("checkpoint store directory");
        let checkpoint = store_directory.path().join("graph-build.checkpoint");
        let vfs = Arc::new(CountingVfs::new(StdVfs));
        let store = Store::open_with_test_dependencies(
            store_directory.path(),
            OpenOptions::default().with_durability(DurabilityMode::Durable, CommitTier::Durable),
            StoreTestDependencies::new(vfs.clone(), Arc::new(ManualMonotonicClock::new())),
        )
        .expect("durable checkpoint store");
        let lease = store.snapshot().expect("checkpoint snapshot");
        let params = GraphParams::new(2, 3, 1.0, 1.0, 3, 1).expect("checkpoint params");
        let control = QueryControl::Cancel(CancelToken::new());

        vfs.reset();
        let interrupted = build_graph_checkpointed(
            &store,
            &reader,
            CheckpointedGraphBuild::new(params, 0x03, GraphBuildPasses::One, &checkpoint, &control)
                .with_max_work_rows(1),
            &lease,
        );
        assert!(matches!(
            interrupted,
            Err(GraphBuildError::BudgetExhausted { rows_completed: 1 })
        ));
        assert!(
            vfs.full_sync_calls() >= 2,
            "checkpoint publish skipped syncs"
        );
        assert!(vfs.rename_calls() >= 1, "checkpoint publish skipped rename");
    }

    fn checkpoint_with_mutation(
        mut bytes: Vec<u8>,
        mutation: impl FnOnce(&mut Vec<u8>),
    ) -> Vec<u8> {
        mutation(&mut bytes);
        let checksum_start = bytes.len() - super::CHECKPOINT_CHECKSUM_BYTES;
        let checksum = xxh3_64(&bytes[..checksum_start]);
        bytes[checksum_start..].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }

    #[test]
    fn graph_build_errors_preserve_actionable_values_and_sources() {
        let io_error = GraphBuildError::CheckpointIo {
            path: std::path::PathBuf::from("blocked.checkpoint"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "blocked"),
        };
        assert!(io_error.to_string().contains("blocked.checkpoint"));
        assert!(std::error::Error::source(&io_error).is_some());

        let errors = [
            GraphBuildError::CheckpointCorrupt("bad header".to_owned()),
            GraphBuildError::BudgetExhausted { rows_completed: 16 },
            GraphBuildError::Cancelled { partial: false },
            GraphBuildError::Timeout { partial: false },
            GraphBuildError::ReadCancelled { partial: false },
            GraphBuildError::Geometry("bad shape".to_owned()),
            GraphBuildError::NodeIdOutOfRange {
                node_id: 9,
                node_count: 3,
            },
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
            assert!(std::error::Error::source(&error).is_none());
        }

        let sourced_errors = [
            GraphBuildError::from(crate::segment::SegmentError::Geometry(
                "bad segment".to_owned(),
            )),
            GraphBuildError::from(crate::quant::QuantError::EmptyVector),
            GraphBuildError::from(crate::graph::block::GraphNodeError::InvalidHeader(
                "bad node header".to_owned(),
            )),
            GraphBuildError::from(crate::lifecycle::StoreError::NotDirectory {
                path: std::path::PathBuf::from("not-a-store"),
            }),
        ];
        for error in sourced_errors {
            assert!(!error.to_string().is_empty());
            assert!(std::error::Error::source(&error).is_some());
        }
    }

    #[test]
    fn built_graph_publishes_through_the_frozen_m2_writer() {
        let directory = tempfile::tempdir().expect("publication fixture directory");
        let reader = fixture_reader(directory.path(), 32);
        let params = GraphParams::new(8, 12, 1.0, 1.2, 24, 32).expect("publication params");
        let artifact = build_graph(
            &reader,
            params,
            0x0001_9000_3a71_fac7,
            GraphBuildPasses::Two,
        )
        .expect("publication graph");
        let output_id = SegmentId::new(19, [8; 10]);
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("publication policy");
        let meta = artifact
            .write_segment_with_graph(&StdVfs, directory.path(), &reader, output_id, policy)
            .expect("published graph segment");
        assert_eq!(meta.id, output_id);
        let published = SegmentReader::open(
            &StdVfs,
            &directory.path().join(output_id.file_name()),
            output_id,
        )
        .expect("published reader");
        let graph = published
            .graph_node_blocks()
            .expect("published graph region");
        assert_eq!(graph.node_count(), 32);
        assert_eq!(graph.layout().max_degree(), 12);
    }

    #[test]
    fn checked_adjacency_rejects_invalid_node_and_degree_inputs() {
        let directory = tempfile::tempdir().expect("checked vector directory");
        let reader = fixture_reader(directory.path(), 3);
        let vectors = SegmentVectors::new(&reader).expect("checked vectors");
        assert!(matches!(
            vectors.validate_node(3),
            Err(GraphBuildError::NodeIdOutOfRange { .. })
        ));

        let mut adjacency = Adjacency::new(3, 2).expect("small adjacency");
        assert!(matches!(
            adjacency.neighbors(3),
            Err(GraphBuildError::NodeIdOutOfRange { .. })
        ));
        assert!(matches!(
            adjacency.set_neighbors(0, &[1, 2, 1]),
            Err(GraphBuildError::Geometry(_))
        ));
        assert!(matches!(
            adjacency.set_neighbors(0, &[3]),
            Err(GraphBuildError::NodeIdOutOfRange { .. })
        ));
        adjacency
            .set_neighbors(0, &[1, 2])
            .expect("valid adjacency row");
        assert_eq!(adjacency.neighbors(0).expect("active row"), &[1, 2]);

        let mut self_edge = Adjacency::new(3, 2).expect("self-edge adjacency");
        self_edge.degrees[0] = 1;
        self_edge.slots[0] = 0;
        assert!(matches!(
            super::validate_checkpoint_adjacency(&self_edge),
            Err(GraphBuildError::CheckpointCorrupt(_))
        ));

        let mut inactive_value = Adjacency::new(3, 2).expect("inactive-slot adjacency");
        inactive_value.slots[0] = 1;
        assert!(matches!(
            super::validate_checkpoint_adjacency(&inactive_value),
            Err(GraphBuildError::CheckpointCorrupt(_))
        ));
        assert!(matches!(
            super::decode_phase(3),
            Ok(super::BuildPhase::Complete)
        ));
        assert!(matches!(
            super::read_checkpoint_u8(&[], 0, "truncated"),
            Err(GraphBuildError::CheckpointCorrupt(_))
        ));
    }

    #[test]
    fn checkpoint_parser_rejects_semantic_corruption_and_identity_drift() {
        let directory = tempfile::tempdir().expect("checkpoint parser directory");
        let reader = fixture_reader(directory.path(), 16);
        let checkpoint = directory.path().join("semantic.checkpoint");
        let params = GraphParams::new(4, 6, 1.0, 1.2, 8, 4).expect("checkpoint params");
        let store_directory = tempfile::tempdir().expect("checkpoint store directory");
        let store = Store::open(store_directory.path(), OpenOptions::default()).expect("store");
        let lease = store.snapshot().expect("checkpoint lease");
        let control = QueryControl::Cancel(CancelToken::new());
        let cancellation = QueryCancellation::new(&control, &lease);
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("derived policy");
        let mut session = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x0001_9000_3c0d_ec01,
            GraphBuildPasses::Two,
            &checkpoint,
            policy,
            None,
        )
        .expect("checkpoint session");
        assert!(
            !session
                .advance_batch(&cancellation)
                .expect("checkpoint batch")
        );
        let valid = StdVfs.read(&checkpoint).expect("valid checkpoint bytes");
        validate_graph_build_checkpoint(&valid).expect("valid checkpoint parses");

        let mut bad_checksum = valid.clone();
        bad_checksum[40] ^= 1;
        let corruptions = [
            bad_checksum,
            checkpoint_with_mutation(valid.clone(), |bytes| {
                bytes[8..10].copy_from_slice(&2_u16.to_le_bytes());
            }),
            checkpoint_with_mutation(valid.clone(), |bytes| bytes[10] = 9),
            checkpoint_with_mutation(valid.clone(), |bytes| bytes[11] = 9),
            checkpoint_with_mutation(valid.clone(), |bytes| bytes[14] = 1),
            checkpoint_with_mutation(valid.clone(), |bytes| bytes[12] = 0),
            checkpoint_with_mutation(valid.clone(), |bytes| bytes[20..24].fill(0)),
            checkpoint_with_mutation(valid.clone(), |bytes| {
                bytes[32..36].copy_from_slice(&17_u32.to_le_bytes());
            }),
            checkpoint_with_mutation(valid.clone(), |bytes| {
                bytes[48..52].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
            }),
            checkpoint_with_mutation(valid.clone(), |bytes| bytes[72..80].fill(0)),
            checkpoint_with_mutation(valid.clone(), |bytes| {
                bytes[80] = 1;
                bytes[96..100].copy_from_slice(&0_u32.to_le_bytes());
            }),
            checkpoint_with_mutation(valid.clone(), |bytes| {
                bytes[80] = 0;
                bytes[96..100].copy_from_slice(&1_u32.to_le_bytes());
            }),
        ];
        for corrupt in corruptions {
            assert!(matches!(
                validate_graph_build_checkpoint(&corrupt),
                Err(GraphBuildError::CheckpointCorrupt(_))
            ));
        }

        let vectors = SegmentVectors::new(&reader).expect("checkpoint vectors");
        assert!(matches!(
            decode_checkpoint(
                &valid,
                &vectors,
                params,
                0x0001_9000_3c0d_ec02,
                GraphBuildPasses::Two,
                None,
            ),
            Err(GraphBuildError::CheckpointCorrupt(_))
        ));
        assert!(matches!(
            decode_checkpoint(
                &valid,
                &vectors,
                params,
                0x0001_9000_3c0d_ec01,
                GraphBuildPasses::One,
                None,
            ),
            Err(GraphBuildError::CheckpointCorrupt(_))
        ));
        let one_pass_refine = checkpoint_with_mutation(valid, |bytes| {
            bytes[10] = 2;
            bytes[11] = 1;
        });
        assert!(matches!(
            decode_checkpoint(
                &one_pass_refine,
                &vectors,
                params,
                0x0001_9000_3c0d_ec01,
                GraphBuildPasses::One,
                None,
            ),
            Err(GraphBuildError::CheckpointCorrupt(_))
        ));
    }

    #[test]
    fn corrupt_checkpoint_refuses_once_clears_the_file_and_the_next_session_starts_fresh() {
        let directory = tempfile::tempdir().expect("corrupt checkpoint directory");
        let reader = fixture_reader(directory.path(), 16);
        let checkpoint = directory.path().join("corrupt.checkpoint");
        let temporary = super::checkpoint_temp_path(&checkpoint);
        let params = GraphParams::new(4, 6, 1.0, 1.2, 8, 4).expect("checkpoint params");
        let store_directory = tempfile::tempdir().expect("checkpoint store directory");
        let store = Store::open(store_directory.path(), OpenOptions::default()).expect("store");
        let lease = store.snapshot().expect("checkpoint lease");
        let control = QueryControl::Cancel(CancelToken::new());
        let cancellation = QueryCancellation::new(&control, &lease);
        let mut session = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x0001_9000_c077_0001,
            GraphBuildPasses::Two,
            &checkpoint,
            DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
                .expect("derived policy"),
            None,
        )
        .expect("checkpoint session");
        assert!(
            !session
                .advance_batch(&cancellation)
                .expect("checkpoint batch")
        );
        drop(session);

        let mut corrupt = StdVfs.read(&checkpoint).expect("valid checkpoint bytes");
        corrupt[40] ^= 1;
        StdVfs
            .write(&checkpoint, &corrupt)
            .expect("write corrupt checkpoint");
        StdVfs
            .write(&temporary, b"stale temporary checkpoint")
            .expect("write stale temporary checkpoint");

        assert!(matches!(
            GraphBuildSession::new(
                &StdVfs,
                &reader,
                params,
                0x0001_9000_c077_0001,
                GraphBuildPasses::Two,
                &checkpoint,
                DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
                    .expect("derived policy"),
                None,
            ),
            Err(GraphBuildError::CheckpointCorrupt(_))
        ));
        assert_eq!(
            StdVfs
                .read(&checkpoint)
                .expect_err("corrupt checkpoint was retained")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            StdVfs
                .read(&temporary)
                .expect_err("stale checkpoint temporary was retained")
                .kind(),
            std::io::ErrorKind::NotFound
        );

        let fresh = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x0001_9000_c077_0001,
            GraphBuildPasses::Two,
            &checkpoint,
            DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
                .expect("derived policy"),
            None,
        )
        .expect("next session starts fresh");
        assert_eq!(fresh.completed_work_rows().expect("fresh work rows"), 0);
    }

    #[test]
    fn accounting_rejection_on_resume_preserves_the_valid_checkpoint() {
        let directory = tempfile::tempdir().expect("accounting checkpoint directory");
        let reader = fixture_reader(directory.path(), 16);
        let checkpoint = directory.path().join("accounting.checkpoint");
        let params = GraphParams::new(4, 6, 1.0, 1.2, 8, 4).expect("checkpoint params");
        let store_directory = tempfile::tempdir().expect("checkpoint store directory");
        let store = Store::open(store_directory.path(), OpenOptions::default()).expect("store");
        let lease = store.snapshot().expect("checkpoint lease");
        let control = QueryControl::Cancel(CancelToken::new());
        let cancellation = QueryCancellation::new(&control, &lease);
        let mut session = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x0001_9000_acc0_0001,
            GraphBuildPasses::Two,
            &checkpoint,
            DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
                .expect("derived policy"),
            None,
        )
        .expect("checkpoint session");
        assert!(
            !session
                .advance_batch(&cancellation)
                .expect("checkpoint batch")
        );
        drop(session);
        let valid = StdVfs.read(&checkpoint).expect("valid checkpoint bytes");

        let refused_directory = tempfile::tempdir().expect("refused store directory");
        let refused = Store::open(
            refused_directory.path(),
            OpenOptions::default().with_max_temp_bytes(1),
        )
        .expect("refused store");
        assert!(matches!(
            GraphBuildSession::new(
                &StdVfs,
                &reader,
                params,
                0x0001_9000_acc0_0001,
                GraphBuildPasses::Two,
                &checkpoint,
                DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
                    .expect("derived policy"),
                Some(&refused.accounting),
            ),
            Err(GraphBuildError::Store(_))
        ));
        assert_eq!(
            StdVfs
                .read(&checkpoint)
                .expect("accounting rejection removed the valid checkpoint"),
            valid
        );
    }

    #[test]
    fn incomplete_session_and_checkpoint_io_fail_typed() {
        let directory = tempfile::tempdir().expect("typed failure directory");
        let reader = fixture_reader(directory.path(), 16);
        let params = GraphParams::new(4, 6, 1.0, 1.2, 8, 4).expect("typed failure params");
        let control = QueryControl::Cancel(CancelToken::new());
        let store_directory = tempfile::tempdir().expect("typed failure store");
        let store = Store::open(store_directory.path(), OpenOptions::default()).expect("store");
        let lease = store.snapshot().expect("typed failure lease");
        let cancellation = QueryCancellation::new(&control, &lease);
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("derived policy");

        let unfinished = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x0001_9000_3100_0001,
            GraphBuildPasses::Two,
            &directory.path().join("unfinished.checkpoint"),
            policy,
            None,
        )
        .expect("unfinished session");
        assert!(matches!(
            unfinished.artifact(),
            Err(GraphBuildError::Geometry(_))
        ));

        assert!(matches!(
            GraphBuildSession::new(
                &StdVfs,
                &reader,
                params,
                0x0001_9000_3100_0001,
                GraphBuildPasses::Two,
                directory.path(),
                policy,
                None,
            ),
            Err(GraphBuildError::CheckpointIo { .. })
        ));

        let missing_parent = directory.path().join("missing").join("write.checkpoint");
        let mut unwritable = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x0001_9000_3100_0001,
            GraphBuildPasses::Two,
            &missing_parent,
            policy,
            None,
        )
        .expect("unwritable session starts before checkpoint write");
        assert!(matches!(
            unwritable.advance_batch(&cancellation),
            Err(GraphBuildError::CheckpointIo { .. })
        ));

        let timeout = QueryControl::Cancel(CancelToken::new());
        timeout.mark_timed_out();
        let timed_out = QueryCancellation::new(&timeout, &lease);
        assert!(matches!(
            super::check_cancellation(&timed_out),
            Err(GraphBuildError::Timeout { partial: false })
        ));
    }

    #[test]
    fn vamana_build_is_deterministic_under_seed() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let reader = fixture_reader(directory.path(), 48);
        let params = GraphParams::new(8, 12, 1.0, 1.2, 24, 48).expect("test params");
        let mut random = crate::test_support::seeded_rng(
            "graph::build::vamana_build_is_deterministic_under_seed",
        );
        let seed = random.next_u64();
        let first = build_graph(&reader, params, seed, GraphBuildPasses::Two)
            .expect("first deterministic build");
        let second = build_graph(&reader, params, seed, GraphBuildPasses::Two)
            .expect("second deterministic build");
        assert_eq!(first.encoded_region(), second.encoded_region());
    }

    #[test]
    fn cancelled_build_leaves_no_partial_artifact() {
        let directory = tempfile::tempdir().expect("sealed segment directory");
        let reader = fixture_reader(directory.path(), 48);
        let segment_path = directory.path().join(reader.meta().id.file_name());
        let before = StdVfs
            .read(&segment_path)
            .expect("sealed bytes before build");
        let checkpoint = directory.path().join("graph-build.checkpoint");
        let checkpoint_temp = directory.path().join("graph-build.checkpoint.tmp");
        let params = GraphParams::new(8, 12, 1.0, 1.2, 24, 8).expect("test params");
        let store_directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(store_directory.path(), OpenOptions::default()).expect("store");
        let lease = store.snapshot().expect("snapshot lease");
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let cancellation = QueryCancellation::new(&control, &lease);
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("derived policy");
        let mut session = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x19_0003,
            GraphBuildPasses::Two,
            &checkpoint,
            policy,
            None,
        )
        .expect("build session");

        assert!(!session.advance_batch(&cancellation).expect("first batch"));
        token.cancel();
        assert!(matches!(
            session.advance_batch(&cancellation),
            Err(GraphBuildError::Cancelled { partial: false })
        ));
        assert_eq!(
            StdVfs
                .read(&segment_path)
                .expect("sealed bytes after cancel"),
            before
        );
        assert!(
            checkpoint.exists(),
            "completed batch checkpoint must remain resumable"
        );
        assert!(
            !checkpoint_temp.exists(),
            "checkpoint temp must not survive"
        );
    }

    #[test]
    fn build_resumes_from_checkpoint_to_the_same_graph() {
        let directory = tempfile::tempdir().expect("sealed segment directory");
        let reader = fixture_reader(directory.path(), 64);
        let checkpoint = directory.path().join("resume.checkpoint");
        let params = GraphParams::new(8, 12, 1.0, 1.2, 24, 8).expect("test params");
        let store_directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(store_directory.path(), OpenOptions::default()).expect("store");
        let lease = store.snapshot().expect("snapshot lease");
        let control = QueryControl::Cancel(CancelToken::new());
        let cancellation = QueryCancellation::new(&control, &lease);
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("derived policy");
        let mut interrupted = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x19_0003_00c0_ffee,
            GraphBuildPasses::Two,
            &checkpoint,
            policy,
            None,
        )
        .expect("interrupted session");
        assert!(
            !interrupted
                .advance_batch(&cancellation)
                .expect("first batch")
        );
        drop(interrupted);

        let mut resumed = GraphBuildSession::new(
            &StdVfs,
            &reader,
            params,
            0x19_0003_00c0_ffee,
            GraphBuildPasses::Two,
            &checkpoint,
            policy,
            None,
        )
        .expect("resumed session");
        while !resumed.advance_batch(&cancellation).expect("resumed batch") {}
        let resumed = resumed.artifact().expect("resumed artifact");
        let uninterrupted =
            build_graph(&reader, params, 0x19_0003_00c0_ffee, GraphBuildPasses::Two)
                .expect("uninterrupted artifact");
        assert_eq!(resumed.encoded_region(), uninterrupted.encoded_region());
        assert!(!checkpoint.exists(), "completed build removes checkpoint");
    }

    #[test]
    fn checkpointed_build_reports_exact_work_for_each_declared_pass() {
        let directory = tempfile::tempdir().expect("work-count fixture directory");
        let rows = 24_usize;
        let reader = fixture_reader(directory.path(), rows);
        let params = GraphParams::new(6, 10, 1.0, 1.2, 16, 6).expect("work-count params");
        let store_directory = tempfile::tempdir().expect("work-count store directory");
        let store = Store::open(store_directory.path(), OpenOptions::default()).expect("store");
        let lease = store.snapshot().expect("work-count lease");
        let control = QueryControl::Cancel(CancelToken::new());

        for (passes, expected_rows, suffix) in [
            (GraphBuildPasses::One, rows as u64, "one"),
            (GraphBuildPasses::Two, (rows * 2) as u64, "two"),
        ] {
            let checkpoint = directory
                .path()
                .join(format!("work-count-{suffix}.checkpoint"));
            let artifact = build_graph_checkpointed(
                &store,
                &reader,
                CheckpointedGraphBuild::new(params, 0x19_0003_c0a7, passes, &checkpoint, &control),
                &lease,
            )
            .expect("checkpointed build completes");

            assert_eq!(artifact.work_rows_completed(), expected_rows);
            assert_eq!(
                decode_node_blocks(artifact.encoded_region())
                    .expect("work-count graph decodes")
                    .node_count(),
                rows as u32
            );
            assert!(
                !checkpoint.exists(),
                "completed build removes {suffix} checkpoint"
            );
        }
    }

    #[test]
    fn graph_build_handles_the_minimum_shape_and_rejects_an_empty_segment() {
        let directory = tempfile::tempdir().expect("minimum-shape directory");
        let empty = fixture_reader(directory.path(), 0);
        assert!(matches!(
            SegmentVectors::new(&empty),
            Err(GraphBuildError::Geometry(detail)) if detail.contains("at least one row")
        ));

        let one_row_directory = tempfile::tempdir().expect("one-row directory");
        let one_row = fixture_reader(one_row_directory.path(), 1);
        let params = GraphParams::new(1, 1, 1.0, 1.0, 1, 1).expect("one-row params");
        let artifact = build_graph(&one_row, params, 0x19_0003_0000_0001, GraphBuildPasses::One)
            .expect("one-row build succeeds");
        let graph = decode_node_blocks(artifact.encoded_region()).expect("one-row graph decodes");
        let node = graph.block(0).expect("row zero exists");
        assert_eq!(graph.node_count(), 1);
        assert_eq!(node.degree(), 0);
        assert_eq!(node.flags() & 1, 1);
        assert_eq!(artifact.entry_points(), &[0]);
    }

    #[test]
    fn every_node_has_at_most_r_max_neighbours() {
        let mut seeded = crate::test_support::seeded_rng(
            "graph::build::every_node_has_at_most_r_max_neighbours",
        );
        let mut runner = TestRunner::new(Config {
            cases: 24,
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        });
        let strategy = (6_usize..40, 2_u8..=8, any::<u64>());
        let result = runner.run(&strategy, |(rows, r_target, seed)| {
            let directory = tempfile::tempdir().expect("property directory");
            let reader = fixture_reader(directory.path(), rows);
            let r_max = r_target.saturating_add(4);
            let params = GraphParams::new(
                r_target,
                r_max,
                1.0,
                1.2,
                u16::from(r_max).saturating_add(8),
                rows as u32,
            )
            .expect("property params");
            let artifact =
                build_graph(&reader, params, seed, GraphBuildPasses::Two).expect("property graph");
            let graph = decode_node_blocks(artifact.encoded_region()).expect("decoded graph");
            prop_assert_eq!(graph.layout().max_degree(), r_max);
            for node_id in 0..graph.node_count() {
                let block = graph.block(node_id).expect("property node");
                prop_assert!(block.degree() <= r_max);
            }
            Ok(())
        });
        assert!(result.is_ok(), "property result: {result:?}");
    }

    #[test]
    fn no_node_lists_itself_or_a_duplicate() {
        let mut seeded =
            crate::test_support::seeded_rng("graph::build::no_node_lists_itself_or_a_duplicate");
        let mut runner = TestRunner::new(Config {
            cases: 24,
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        });
        let strategy = (6_usize..40, 2_u8..=8, any::<u64>());
        let result = runner.run(&strategy, |(rows, r_target, seed)| {
            let directory = tempfile::tempdir().expect("property directory");
            let reader = fixture_reader(directory.path(), rows);
            let r_max = r_target.saturating_add(4);
            let params = GraphParams::new(
                r_target,
                r_max,
                1.0,
                1.2,
                u16::from(r_max).saturating_add(8),
                rows as u32,
            )
            .expect("property params");
            let artifact =
                build_graph(&reader, params, seed, GraphBuildPasses::Two).expect("property graph");
            let graph = decode_node_blocks(artifact.encoded_region()).expect("decoded graph");
            for node_id in 0..graph.node_count() {
                let block = graph.block(node_id).expect("property node");
                let mut unique = std::collections::BTreeSet::new();
                for neighbor in block.neighbors_padded().take(usize::from(block.degree())) {
                    prop_assert_ne!(neighbor, node_id);
                    prop_assert!(unique.insert(neighbor));
                }
            }
            Ok(())
        });
        assert!(result.is_ok(), "property result: {result:?}");
    }

    #[test]
    fn graph_is_connected_from_the_entry_points() {
        let directory = tempfile::tempdir().expect("connectivity directory");
        let reader = fixture_reader(directory.path(), 96);
        let params = GraphParams::new(8, 12, 1.0, 1.2, 24, 96).expect("connectivity params");
        let artifact = build_graph(
            &reader,
            params,
            0x0001_9000_3c01_1ec7,
            GraphBuildPasses::Two,
        )
        .expect("connectivity graph");
        let graph = decode_node_blocks(artifact.encoded_region()).expect("decoded graph");
        let mut reached = vec![false; graph.node_count() as usize];
        let mut pending = std::collections::VecDeque::new();
        for &entry in artifact.entry_points() {
            pending.push_back(entry);
        }
        while let Some(node_id) = pending.pop_front() {
            let node = node_id as usize;
            if reached[node] {
                continue;
            }
            reached[node] = true;
            let block = graph.block(node_id).expect("reachable node");
            for neighbor in block.neighbors_padded().take(usize::from(block.degree())) {
                if !reached[neighbor as usize] {
                    pending.push_back(neighbor);
                }
            }
        }
        let reached_count = reached.iter().filter(|seen| **seen).count();
        assert_eq!(reached_count, graph.node_count() as usize);
    }

    #[test]
    fn graph_build_memory_is_budgeted_and_exactly_accounted() {
        let directory = tempfile::tempdir().expect("accounting fixture directory");
        let reader = fixture_reader(directory.path(), 48);
        let params = GraphParams::new(8, 12, 1.0, 1.2, 24, 48).expect("accounting params");
        let control = QueryControl::Cancel(CancelToken::new());

        let refused_directory = tempfile::tempdir().expect("refused store directory");
        let refused = Store::open(
            refused_directory.path(),
            OpenOptions::default().with_max_temp_bytes(1),
        )
        .expect("refused store");
        let refused_lease = refused.snapshot().expect("refused lease");
        let refused_result = build_graph_checkpointed(
            &refused,
            &reader,
            CheckpointedGraphBuild::new(
                params,
                0x19_0003_acc0,
                GraphBuildPasses::One,
                &directory.path().join("refused.checkpoint"),
                &control,
            ),
            &refused_lease,
        );
        assert!(matches!(refused_result, Err(GraphBuildError::Store(_))));
        assert_eq!(refused.stats().expect("refused stats").temporary_bytes, 0);

        let store_directory = tempfile::tempdir().expect("accounted store directory");
        let store = Store::open(store_directory.path(), OpenOptions::default()).expect("store");
        let lease = store.snapshot().expect("accounted lease");
        let artifact = build_graph_checkpointed(
            &store,
            &reader,
            CheckpointedGraphBuild::new(
                params,
                0x19_0003_acc0,
                GraphBuildPasses::One,
                &directory.path().join("accounted.checkpoint"),
                &control,
            ),
            &lease,
        )
        .expect("accounted graph");
        let expected =
            artifact.encoded_region().len() + std::mem::size_of_val(artifact.entry_points());
        assert_eq!(
            store.stats().expect("live graph stats").temporary_bytes,
            expected as u64
        );
        drop(artifact);
        assert_eq!(
            store.stats().expect("released graph stats").temporary_bytes,
            0
        );
    }

    #[test]
    fn arbitrary_checkpoint_bytes_fail_typed_without_panicking() {
        let mut seeded = crate::test_support::seeded_rng(
            "graph::build::arbitrary_checkpoint_bytes_fail_typed_without_panicking",
        );
        let mut runner = TestRunner::new(Config {
            cases: 64,
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        });
        let result = runner.run(&proptest::collection::vec(any::<u8>(), 0..2048), |bytes| {
            prop_assert!(matches!(
                validate_graph_build_checkpoint(&bytes),
                Err(GraphBuildError::CheckpointCorrupt(_))
            ));
            Ok(())
        });
        assert!(result.is_ok(), "property result: {result:?}");
    }
}
