//! Copy-on-write post-consolidation graph refinement (Task 19-M9).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::{xxh3_64, xxh3_64_with_seed};

use crate::fts::index::{Document as LexicalDocument, SegmentIndex};
use crate::fts::sealed::SealedSegment;
use crate::fts::tokenizer::Analyzer;
use crate::graph::GraphParams;
use crate::graph::block::{
    GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeBlocks, GraphNodeError, GraphNodeLayout,
    encode_node_blocks_with_refinement_passes,
};
use crate::graph::build::{GraphBuildError, robust_prune_rows, write_segment_with_encoded_graph};
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::lifecycle::stats::{AccountedCounter, AllocationComponent};
use crate::lifecycle::{QueryCancellation, QueryControl, SnapshotLease, Store, StoreError};
use crate::meta::{AliveSet, ColumnStoreBuilder};
use crate::quant::{est_dot_bit4, prepare_bit4_query};
use crate::scan::ScanError;
use crate::segment::layout::RegionKind;
use crate::segment::reader::SegmentReader;
use crate::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, SegmentPayloads, SegmentPostings,
    SegmentStoredMetadata, SegmentStoredText, write_segment, write_segment_with_documents_payloads,
};
use crate::segment::{SegmentError, SegmentId, SegmentMeta};
use crate::vfs::Vfs;

const ENTRY_FLAG: u8 = 0b0000_0001;
const HUB_FLAG: u8 = 0b0000_0010;
const ENTRY_POINT_COUNT: usize = 4;
const INTERMEDIATE_ID_SEED: u64 = 0x0019_4d39_0000_0001;
const CHECKPOINT_MAGIC: [u8; 8] = *b"ZEREFCP1";
const CHECKPOINT_VERSION: u16 = 1;
const CHECKPOINT_HEADER_BYTES: usize = 104;
const CHECKPOINT_CHECKSUM_BYTES: usize = 8;

/// One deterministic post-consolidation graph refinement pass.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum RefinementPass {
    /// Remap dense row ids into deterministic graph-traversal order.
    Renumber = 0,
    /// Re-prune finished neighborhoods with the profile's refinement alpha.
    AlphaReprune = 1,
    /// Replace sampled entry seeds with the exact medoid and spread seeds.
    SeedRefit = 2,
    /// Order each adjacency list by Bit4 distance to its owner.
    NeighborReorder = 3,
    /// Stitch unreachable components and densify the resulting hubs.
    ConnectivityRepair = 4,
}

impl RefinementPass {
    /// Catalog order for independently scheduled and measured passes.
    pub const ALL: [Self; 4] = [
        Self::Renumber,
        Self::AlphaReprune,
        Self::SeedRefit,
        Self::NeighborReorder,
    ];

    const fn bit(self) -> u16 {
        1_u16 << self as u8
    }

    /// Returns the stable command-line and evidence label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Renumber => "renumber",
            Self::AlphaReprune => "alpha-reprune",
            Self::SeedRefit => "seed-refit",
            Self::NeighborReorder => "neighbor-reorder",
            Self::ConnectivityRepair => "connectivity-repair",
        }
    }

    /// Parses one stable command-line pass label.
    pub fn named(label: &str) -> Result<Self, RefinementError> {
        match label {
            "renumber" => Ok(Self::Renumber),
            "alpha-reprune" => Ok(Self::AlphaReprune),
            "seed-refit" => Ok(Self::SeedRefit),
            "neighbor-reorder" => Ok(Self::NeighborReorder),
            "connectivity-repair" => Ok(Self::ConnectivityRepair),
            _ => Err(RefinementError::Geometry(format!(
                "unknown refinement pass {label:?}"
            ))),
        }
    }
}

/// Authenticated node-trailer record of refinements already applied.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefinementPasses(u16);

impl RefinementPasses {
    pub(crate) const KNOWN_BITS: u16 = (1_u16 << 5) - 1;

    /// Returns whether the named pass is stamped on this graph artifact.
    #[must_use]
    pub const fn contains(self, pass: RefinementPass) -> bool {
        self.0 & pass.bit() != 0
    }

    pub(crate) const fn bits(self) -> u16 {
        self.0
    }

    pub(crate) const fn from_bits(bits: u16) -> Option<Self> {
        if bits & !Self::KNOWN_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    pub(crate) const fn with(self, pass: RefinementPass) -> Self {
        Self(self.0 | pass.bit())
    }
}

impl FromIterator<RefinementPass> for RefinementPasses {
    fn from_iter<T: IntoIterator<Item = RefinementPass>>(passes: T) -> Self {
        passes
            .into_iter()
            .fold(Self::default(), |record, pass| record.with(pass))
    }
}

/// Complete immutable graph replacement produced by one refinement pass.
pub struct RefinementArtifact {
    pass: RefinementPass,
    encoded_region: Vec<u8>,
    old_to_new: Option<Vec<u32>>,
    new_to_old: Option<Vec<u32>>,
    work_rows_completed: u64,
}

impl RefinementArtifact {
    /// Returns the pass represented by this artifact.
    #[must_use]
    pub const fn pass(&self) -> RefinementPass {
        self.pass
    }

    /// Returns the complete authenticated graph-region bytes.
    #[must_use]
    pub fn encoded_region(&self) -> &[u8] {
        &self.encoded_region
    }

    /// Returns old-row to new-row ids for renumbering, and `None` otherwise.
    #[must_use]
    pub fn old_to_new_rows(&self) -> Option<&[u32]> {
        self.old_to_new.as_deref()
    }

    /// Returns deterministic row work completed by this invocation.
    #[must_use]
    pub const fn work_rows_completed(&self) -> u64 {
        self.work_rows_completed
    }

    /// Writes the replacement beside its immutable source without publishing it.
    pub fn write_segment(
        &self,
        vfs: &dyn Vfs,
        directory: &Path,
        input: &SegmentReader,
        output_id: SegmentId,
        analyzer: &Analyzer,
        policy: DurabilityPolicy,
    ) -> Result<SegmentMeta, RefinementError> {
        let Some(new_to_old) = self.new_to_old.as_deref() else {
            return write_segment_with_encoded_graph(
                &self.encoded_region,
                vfs,
                directory,
                input,
                output_id,
                policy,
            )
            .map_err(RefinementError::Graph);
        };
        let intermediate_id = refinement_intermediate_id(output_id);
        write_permuted_segment(
            vfs,
            directory,
            input,
            new_to_old,
            intermediate_id,
            analyzer,
            policy,
        )?;
        let intermediate_path = directory.join(intermediate_id.file_name());
        let intermediate = match SegmentReader::open(vfs, &intermediate_path, intermediate_id) {
            Ok(intermediate) => intermediate,
            Err(error) => {
                let _ = vfs.delete(&intermediate_path);
                return Err(error.into());
            }
        };
        let written = write_segment_with_encoded_graph(
            &self.encoded_region,
            vfs,
            directory,
            &intermediate,
            output_id,
            policy,
        )
        .map_err(RefinementError::Graph);
        drop(intermediate);
        let removed = vfs.delete(&intermediate_path).map_err(|source| {
            RefinementError::Segment(SegmentError::io(&intermediate_path, source))
        });
        match (written, removed) {
            (Ok(written), Ok(())) => Ok(written),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

/// Typed failure before a refined artifact is published.
#[derive(Debug)]
pub enum RefinementError {
    /// The graph already records the requested pass.
    AlreadyApplied(RefinementPass),
    /// Existing graph construction or graph-region writing failed.
    Graph(GraphBuildError),
    /// Fixed-stride graph bytes violated their persisted contract.
    NodeBlock(GraphNodeError),
    /// Reading or writing a sealed segment failed.
    Segment(SegmentError),
    /// Shared row-gather or accounting work failed.
    Store(StoreError),
    /// Checked row or pass geometry was invalid.
    Geometry(String),
}

impl std::fmt::Display for RefinementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyApplied(pass) => {
                write!(
                    formatter,
                    "refinement pass {} is already applied",
                    pass.label()
                )
            }
            Self::Graph(error) => write!(formatter, "refinement graph: {error}"),
            Self::NodeBlock(error) => write!(formatter, "refinement node blocks: {error}"),
            Self::Segment(error) => write!(formatter, "refinement segment: {error}"),
            Self::Store(error) => write!(formatter, "refinement store: {error}"),
            Self::Geometry(detail) => write!(formatter, "refinement geometry: {detail}"),
        }
    }
}

impl std::error::Error for RefinementError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(error) => Some(error),
            Self::NodeBlock(error) => Some(error),
            Self::Segment(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::AlreadyApplied(_) | Self::Geometry(_) => None,
        }
    }
}

impl From<GraphNodeError> for RefinementError {
    fn from(error: GraphNodeError) -> Self {
        Self::NodeBlock(error)
    }
}

impl From<SegmentError> for RefinementError {
    fn from(error: SegmentError) -> Self {
        Self::Segment(error)
    }
}

impl From<StoreError> for RefinementError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<GraphBuildError> for RefinementError {
    fn from(error: GraphBuildError) -> Self {
        Self::Graph(error)
    }
}

struct OwnedGraph {
    layout: GraphNodeLayout,
    passes: RefinementPasses,
    flags: Vec<u8>,
    neighbors: Vec<Vec<u32>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefinementPhase {
    Rows,
    SeedMedoid,
    SeedSpread,
    ConnectivityTraverse,
    ConnectivityDensify,
    Complete,
}

struct RefinementState {
    graph: OwnedGraph,
    phase: RefinementPhase,
    next_row: u32,
    work_rows: u64,
    permutation: Vec<u32>,
    queue: VecDeque<u32>,
    seen: Vec<u8>,
    entries: Vec<u32>,
    best_node: Option<u32>,
    best_score: f64,
    scratch_bytes: usize,
    memory: Option<AccountedCounter>,
}

/// One resumable post-consolidation refinement request.
pub struct CheckpointedRefinement<'a> {
    pass: RefinementPass,
    params: GraphParams,
    seed: u64,
    generation: u64,
    checkpoint_path: &'a Path,
    control: &'a QueryControl,
    max_work_rows: u64,
}

impl<'a> CheckpointedRefinement<'a> {
    /// Creates one request whose published generation is stamped into the checkpoint
    /// for provenance; resume identity deliberately excludes it (see the decoder).
    #[must_use]
    pub const fn new(
        pass: RefinementPass,
        params: GraphParams,
        seed: u64,
        generation: u64,
        checkpoint_path: &'a Path,
        control: &'a QueryControl,
    ) -> Self {
        Self {
            pass,
            params,
            seed,
            generation,
            checkpoint_path,
            control,
            max_work_rows: u64::MAX,
        }
    }

    /// Caps deterministic row work performed by this invocation.
    #[must_use]
    pub const fn with_max_work_rows(mut self, max_work_rows: u64) -> Self {
        self.max_work_rows = max_work_rows;
        self
    }
}

impl OwnedGraph {
    fn read(graph: GraphNodeBlocks<'_>) -> Result<Self, RefinementError> {
        let mut flags = Vec::with_capacity(graph.node_count() as usize);
        let mut neighbors = Vec::with_capacity(graph.node_count() as usize);
        for node_id in 0..graph.node_count() {
            let block = graph.block(node_id)?;
            flags.push(block.flags());
            neighbors.push(
                block
                    .neighbors_padded()
                    .take(usize::from(block.degree()))
                    .collect(),
            );
        }
        Ok(Self {
            layout: graph.layout(),
            passes: graph.refinement_passes(),
            flags,
            neighbors,
        })
    }
}

impl RefinementState {
    fn new(
        reader: &SegmentReader,
        pass: RefinementPass,
        params: GraphParams,
        accounting: Option<&std::sync::Arc<crate::lifecycle::stats::Accounting>>,
    ) -> Result<Self, RefinementError> {
        let source = reader.graph_node_blocks()?;
        let graph = OwnedGraph::read(source)?;
        let node_count = graph.neighbors.len();
        let (phase, permutation, queue, seen) = match pass {
            RefinementPass::Renumber => {
                let mut roots = graph
                    .flags
                    .iter()
                    .enumerate()
                    .filter_map(|(node, flags)| {
                        (*flags & ENTRY_FLAG != 0)
                            .then(|| u32::try_from(node).ok())
                            .flatten()
                    })
                    .collect::<Vec<_>>();
                if roots.is_empty() {
                    return Err(RefinementError::Geometry(
                        "renumbering requires at least one persisted entry seed".to_owned(),
                    ));
                }
                sort_by_degree(&mut roots, &graph)?;
                let mut queue = VecDeque::with_capacity(node_count);
                let mut seen = vec![0_u8; node_count];
                for root in roots {
                    mark_and_enqueue_byte(root, &mut seen, &mut queue)?;
                }
                (
                    RefinementPhase::Rows,
                    Vec::with_capacity(node_count),
                    queue,
                    seen,
                )
            }
            RefinementPass::ConnectivityRepair => {
                if graph.layout.max_degree() != params.r_max() {
                    return Err(RefinementError::Geometry(format!(
                        "graph max degree {}, profile max degree {}",
                        graph.layout.max_degree(),
                        params.r_max()
                    )));
                }
                let root = first_entry(&graph)?;
                let mut queue = VecDeque::with_capacity(node_count);
                let mut seen = vec![0_u8; node_count];
                mark_and_enqueue_byte(root, &mut seen, &mut queue)?;
                (
                    RefinementPhase::ConnectivityTraverse,
                    Vec::new(),
                    queue,
                    seen,
                )
            }
            RefinementPass::SeedRefit => (
                RefinementPhase::SeedMedoid,
                Vec::new(),
                VecDeque::new(),
                Vec::new(),
            ),
            RefinementPass::AlphaReprune | RefinementPass::NeighborReorder => (
                RefinementPhase::Rows,
                Vec::new(),
                VecDeque::new(),
                Vec::new(),
            ),
        };
        let mut state = Self {
            graph,
            phase,
            next_row: 0,
            work_rows: 0,
            permutation,
            queue,
            seen,
            entries: Vec::with_capacity(ENTRY_POINT_COUNT),
            best_node: None,
            best_score: 0.0,
            scratch_bytes: refinement_scratch_bytes(pass, params)?,
            memory: match accounting {
                Some(accounting) => Some(AccountedCounter::new(
                    accounting,
                    AllocationComponent::Temporary,
                )?),
                None => None,
            },
        };
        state.update_memory_charge()?;
        Ok(state)
    }

    fn update_memory_charge(&mut self) -> Result<(), RefinementError> {
        let graph_rows = self
            .graph
            .neighbors
            .capacity()
            .checked_mul(std::mem::size_of::<Vec<u32>>())
            .ok_or_else(|| {
                RefinementError::Geometry("refinement graph charge overflow".to_owned())
            })?;
        let neighbor_bytes = self
            .graph
            .neighbors
            .iter()
            .try_fold(0_usize, |total, row| {
                row.capacity()
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|bytes| total.checked_add(bytes))
                    .ok_or_else(|| {
                        RefinementError::Geometry("refinement neighbor charge overflow".to_owned())
                    })
            })?;
        let bytes = graph_rows
            .checked_add(neighbor_bytes)
            .and_then(|total| total.checked_add(self.graph.flags.capacity()))
            .and_then(|total| {
                self.permutation
                    .capacity()
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|bytes| total.checked_add(bytes))
            })
            .and_then(|total| {
                self.queue
                    .capacity()
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|bytes| total.checked_add(bytes))
            })
            .and_then(|total| total.checked_add(self.seen.capacity()))
            .and_then(|total| {
                self.entries
                    .capacity()
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|bytes| total.checked_add(bytes))
            })
            .and_then(|total| total.checked_add(self.scratch_bytes))
            .ok_or_else(|| {
                RefinementError::Geometry("refinement state charge overflow".to_owned())
            })?;
        if let Some(memory) = self.memory.as_mut() {
            memory.set(bytes)?;
        }
        Ok(())
    }

    fn advance_batch(
        &mut self,
        reader: &SegmentReader,
        pass: RefinementPass,
        params: GraphParams,
        seed: u64,
        limit: u64,
        cancellation: &QueryCancellation<'_>,
    ) -> Result<bool, RefinementError> {
        let started = self.work_rows;
        while self.phase != RefinementPhase::Complete
            && self.work_rows.saturating_sub(started) < limit
        {
            check_refinement_cancellation(cancellation)?;
            match pass {
                RefinementPass::Renumber => self.advance_renumber()?,
                RefinementPass::AlphaReprune => {
                    self.advance_alpha(reader, params)?;
                }
                RefinementPass::SeedRefit => self.advance_seed(reader)?,
                RefinementPass::NeighborReorder => self.advance_reorder(reader, seed)?,
                RefinementPass::ConnectivityRepair => self.advance_connectivity(params)?,
            }
        }
        check_refinement_cancellation(cancellation)?;
        Ok(self.phase == RefinementPhase::Complete)
    }

    fn advance_renumber(&mut self) -> Result<(), RefinementError> {
        if self.queue.is_empty() {
            if let Some(unseen) = self.seen.iter().position(|seen| *seen == 0) {
                let root = u32::try_from(unseen).map_err(|_| {
                    RefinementError::Geometry("renumber row exceeds u32".to_owned())
                })?;
                mark_and_enqueue_byte(root, &mut self.seen, &mut self.queue)?;
            } else {
                let node_count = u32::try_from(self.graph.neighbors.len())
                    .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
                let old_to_new = invert_permutation(&self.permutation, node_count)?;
                apply_renumber(&mut self.graph, &self.permutation, &old_to_new)?;
                self.phase = RefinementPhase::Complete;
                return Ok(());
            }
        }
        let owner = self.queue.pop_front().ok_or_else(|| {
            RefinementError::Geometry("renumber queue became empty before completion".to_owned())
        })?;
        self.permutation.push(owner);
        let node_count = u32::try_from(self.graph.neighbors.len())
            .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
        let owner_index = node_index(owner, node_count)?;
        let mut candidates = self
            .graph
            .neighbors
            .get(owner_index)
            .ok_or_else(|| {
                RefinementError::Geometry(format!("neighbors for node {owner} are unavailable"))
            })?
            .iter()
            .copied()
            .filter(|neighbor| {
                self.seen
                    .get(*neighbor as usize)
                    .is_some_and(|seen| *seen == 0)
            })
            .collect::<Vec<_>>();
        sort_by_degree(&mut candidates, &self.graph)?;
        for candidate in candidates {
            mark_and_enqueue_byte(candidate, &mut self.seen, &mut self.queue)?;
        }
        self.work_rows = self
            .work_rows
            .checked_add(1)
            .ok_or_else(|| RefinementError::Geometry("renumber work overflow".to_owned()))?;
        if self.permutation.len() == self.graph.neighbors.len() {
            let old_to_new = invert_permutation(&self.permutation, node_count)?;
            apply_renumber(&mut self.graph, &self.permutation, &old_to_new)?;
            self.phase = RefinementPhase::Complete;
        }
        Ok(())
    }

    fn advance_connectivity(&mut self, params: GraphParams) -> Result<(), RefinementError> {
        let node_count = u32::try_from(self.graph.neighbors.len())
            .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
        match self.phase {
            RefinementPhase::ConnectivityTraverse => {
                if let Some(owner) = self.queue.pop_front() {
                    let neighbors = self
                        .graph
                        .neighbors
                        .get(node_index(owner, node_count)?)
                        .ok_or_else(|| {
                            RefinementError::Geometry(format!(
                                "neighbors for repair node {owner} are unavailable"
                            ))
                        })?
                        .clone();
                    for neighbor in neighbors {
                        mark_and_enqueue_byte(neighbor, &mut self.seen, &mut self.queue)?;
                    }
                    self.work_rows = self.work_rows.checked_add(1).ok_or_else(|| {
                        RefinementError::Geometry("connectivity work overflow".to_owned())
                    })?;
                } else if let Some(unseen) = self.seen.iter().position(|marker| *marker == 0) {
                    let target = u32::try_from(unseen).map_err(|_| {
                        RefinementError::Geometry("unreachable row exceeds u32".to_owned())
                    })?;
                    stitch_component(&mut self.graph, &self.seen, target, params.r_max())?;
                    mark_and_enqueue_byte(target, &mut self.seen, &mut self.queue)?;
                } else {
                    self.phase = RefinementPhase::ConnectivityDensify;
                    self.next_row = 0;
                }
            }
            RefinementPhase::ConnectivityDensify => {
                let owner = self.next_row;
                densify_hub(&mut self.graph, owner, params.r_max())?;
                self.finish_row(node_count, "connectivity densification")?;
            }
            RefinementPhase::Rows
            | RefinementPhase::SeedMedoid
            | RefinementPhase::SeedSpread
            | RefinementPhase::Complete => {
                return Err(RefinementError::Geometry(
                    "connectivity repair entered an invalid phase".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn advance_alpha(
        &mut self,
        reader: &SegmentReader,
        params: GraphParams,
    ) -> Result<(), RefinementError> {
        let source = reader.graph_node_blocks()?;
        if source.layout().max_degree() != params.r_max() {
            return Err(RefinementError::Geometry(format!(
                "graph max degree {}, profile max degree {}",
                source.layout().max_degree(),
                params.r_max()
            )));
        }
        let owner = self.next_row;
        let owner_block = source.block(owner)?;
        let own = owner_block
            .neighbors_padded()
            .take(usize::from(owner_block.degree()))
            .collect::<Vec<_>>();
        let candidate_capacity = usize::from(params.r_max())
            .checked_add(
                usize::from(params.r_max())
                    .checked_mul(usize::from(params.r_max()))
                    .ok_or_else(|| {
                        RefinementError::Geometry("re-prune candidates overflow".to_owned())
                    })?,
            )
            .ok_or_else(|| RefinementError::Geometry("re-prune candidates overflow".to_owned()))?;
        let mut candidates = Vec::with_capacity(candidate_capacity);
        candidates.extend_from_slice(&own);
        for neighbor in &own {
            let block = source.block(*neighbor)?;
            candidates.extend(block.neighbors_padded().take(usize::from(block.degree())));
        }
        let rescore = reader.rescore_f32()?;
        let dimensions = reader.meta().dims as usize;
        let node_count = reader.meta().row_count;
        let mut pruned = robust_prune_rows(
            rescore,
            dimensions,
            node_count,
            owner,
            &candidates,
            params.alpha_refine(),
            usize::from(params.r_target()),
        )?;
        supplement_long_range(
            rescore,
            dimensions,
            owner,
            &own,
            usize::from(params.r_max()),
            &mut pruned,
        )?;
        *self
            .graph
            .neighbors
            .get_mut(node_index(owner, node_count)?)
            .ok_or_else(|| {
                RefinementError::Geometry(format!(
                    "output neighbors for node {owner} are unavailable"
                ))
            })? = pruned;
        self.finish_row(node_count, "re-prune")
    }

    fn advance_reorder(
        &mut self,
        reader: &SegmentReader,
        seed: u64,
    ) -> Result<(), RefinementError> {
        let owner = self.next_row;
        let node_count = reader.meta().row_count;
        let neighbors = self
            .graph
            .neighbors
            .get_mut(node_index(owner, node_count)?)
            .ok_or_else(|| {
                RefinementError::Geometry(format!("neighbors for node {owner} are unavailable"))
            })?;
        reorder_neighbors_bit4(reader, owner, neighbors, seed)?;
        self.finish_row(node_count, "neighbor reorder")
    }

    fn advance_seed(&mut self, reader: &SegmentReader) -> Result<(), RefinementError> {
        let node_count = reader.meta().row_count;
        if node_count == 0 {
            return Err(RefinementError::Geometry(
                "seed refit requires at least one row".to_owned(),
            ));
        }
        if self.next_row == node_count {
            self.finish_seed_phase(node_count)?;
            if self.phase == RefinementPhase::Complete {
                return Ok(());
            }
        }
        let candidate = self.next_row;
        let rescore = reader.rescore_f32()?;
        let dimensions = reader.meta().dims as usize;
        let score = match self.phase {
            RefinementPhase::SeedMedoid => {
                let mut sum = 0.0_f64;
                for other in 0..node_count {
                    sum += exact_distance(rescore, dimensions, node_count, candidate, other)?;
                }
                sum
            }
            RefinementPhase::SeedSpread => {
                if self.entries.contains(&candidate) {
                    f64::NEG_INFINITY
                } else {
                    self.entries
                        .iter()
                        .try_fold(f64::INFINITY, |nearest, entry| {
                            exact_distance(rescore, dimensions, node_count, candidate, *entry)
                                .map(|distance| nearest.min(distance))
                        })?
                }
            }
            RefinementPhase::Rows
            | RefinementPhase::ConnectivityTraverse
            | RefinementPhase::ConnectivityDensify
            | RefinementPhase::Complete => {
                return Err(RefinementError::Geometry(
                    "seed refit entered an invalid phase".to_owned(),
                ));
            }
        };
        let better = match (self.phase, self.best_node) {
            (_, None) => true,
            (RefinementPhase::SeedMedoid, Some(best)) => {
                score < self.best_score || (score == self.best_score && candidate < best)
            }
            (RefinementPhase::SeedSpread, Some(best)) => {
                score > self.best_score || (score == self.best_score && candidate < best)
            }
            (
                RefinementPhase::Rows
                | RefinementPhase::ConnectivityTraverse
                | RefinementPhase::ConnectivityDensify
                | RefinementPhase::Complete,
                Some(_),
            ) => false,
        };
        if better {
            self.best_node = Some(candidate);
            self.best_score = score;
        }
        self.next_row = self
            .next_row
            .checked_add(1)
            .ok_or_else(|| RefinementError::Geometry("seed row overflow".to_owned()))?;
        self.work_rows = self
            .work_rows
            .checked_add(1)
            .ok_or_else(|| RefinementError::Geometry("seed work overflow".to_owned()))?;
        if self.next_row == node_count {
            self.finish_seed_phase(node_count)?;
        }
        Ok(())
    }

    fn finish_seed_phase(&mut self, node_count: u32) -> Result<(), RefinementError> {
        let selected = self.best_node.take().ok_or_else(|| {
            RefinementError::Geometry("seed phase produced no candidate".to_owned())
        })?;
        self.entries.push(selected);
        self.best_score = 0.0;
        self.next_row = 0;
        if self.entries.len() >= ENTRY_POINT_COUNT || self.entries.len() >= node_count as usize {
            for (node, flags) in self.graph.flags.iter_mut().enumerate() {
                let node = u32::try_from(node)
                    .map_err(|_| RefinementError::Geometry("entry row exceeds u32".to_owned()))?;
                *flags = (*flags & HUB_FLAG) | u8::from(self.entries.contains(&node));
            }
            self.phase = RefinementPhase::Complete;
        } else {
            self.phase = RefinementPhase::SeedSpread;
        }
        Ok(())
    }

    fn finish_row(&mut self, node_count: u32, label: &str) -> Result<(), RefinementError> {
        self.next_row = self
            .next_row
            .checked_add(1)
            .ok_or_else(|| RefinementError::Geometry(format!("{label} row overflow")))?;
        self.work_rows = self
            .work_rows
            .checked_add(1)
            .ok_or_else(|| RefinementError::Geometry(format!("{label} work overflow")))?;
        if self.next_row == node_count {
            self.phase = RefinementPhase::Complete;
        }
        Ok(())
    }
}

fn mark_and_enqueue_byte(
    node: u32,
    seen: &mut [u8],
    queue: &mut VecDeque<u32>,
) -> Result<(), RefinementError> {
    let marker = seen.get_mut(node as usize).ok_or_else(|| {
        RefinementError::Geometry(format!("renumber queue node {node} is out of range"))
    })?;
    if *marker == 0 {
        *marker = 1;
        queue.push_back(node);
    }
    Ok(())
}

fn first_entry(graph: &OwnedGraph) -> Result<u32, RefinementError> {
    graph
        .flags
        .iter()
        .position(|flags| *flags & ENTRY_FLAG != 0)
        .ok_or_else(|| {
            RefinementError::Geometry(
                "connectivity repair requires at least one persisted entry seed".to_owned(),
            )
        })
        .and_then(|node| {
            u32::try_from(node)
                .map_err(|_| RefinementError::Geometry("entry row exceeds u32".to_owned()))
        })
}

fn stitch_component(
    graph: &mut OwnedGraph,
    seen: &[u8],
    target: u32,
    r_max: u8,
) -> Result<(), RefinementError> {
    let maximum = usize::from(r_max);
    if maximum == 0 {
        return Err(RefinementError::Geometry(
            "connectivity repair requires positive maximum degree".to_owned(),
        ));
    }
    let (source, replacement) = connectivity_source(graph, seen, maximum)?;
    set_or_append_neighbor(graph, source, target, maximum, replacement)?;
    let target_index = node_index(
        target,
        u32::try_from(graph.neighbors.len())
            .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?,
    )?;
    let target_neighbors = graph.neighbors.get_mut(target_index).ok_or_else(|| {
        RefinementError::Geometry(format!(
            "neighbors for stitch target {target} are unavailable"
        ))
    })?;
    if !target_neighbors.contains(&source) {
        if target_neighbors.len() < maximum {
            target_neighbors.push(source);
        } else {
            let slot = target_neighbors.len().saturating_sub(1);
            let destination = target_neighbors.get_mut(slot).ok_or_else(|| {
                RefinementError::Geometry("full stitch target has no replaceable edge".to_owned())
            })?;
            *destination = source;
        }
    }
    let source_flag = graph.flags.get_mut(source as usize).ok_or_else(|| {
        RefinementError::Geometry(format!("flags for stitch source {source} are unavailable"))
    })?;
    *source_flag |= HUB_FLAG;
    Ok(())
}

fn connectivity_source(
    graph: &OwnedGraph,
    seen: &[u8],
    maximum: usize,
) -> Result<(u32, Option<usize>), RefinementError> {
    for required_flag in [HUB_FLAG, ENTRY_FLAG, 0] {
        for (owner, marker) in seen.iter().enumerate() {
            let flags = graph.flags.get(owner).copied().ok_or_else(|| {
                RefinementError::Geometry("repair flags are shorter than seen state".to_owned())
            })?;
            let neighbors = graph.neighbors.get(owner).ok_or_else(|| {
                RefinementError::Geometry("repair rows are shorter than seen state".to_owned())
            })?;
            if *marker != 0
                && neighbors.len() < maximum
                && (required_flag == 0 || flags & required_flag != 0)
            {
                return u32::try_from(owner)
                    .map(|owner| (owner, None))
                    .map_err(|_| {
                        RefinementError::Geometry("repair source exceeds u32".to_owned())
                    });
            }
        }
    }
    replaceable_non_tree_edge(graph, seen).map(|(owner, slot)| (owner, Some(slot)))
}

fn replaceable_non_tree_edge(
    graph: &OwnedGraph,
    seen: &[u8],
) -> Result<(u32, usize), RefinementError> {
    let node_count = u32::try_from(graph.neighbors.len())
        .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
    let root = first_entry(graph)?;
    let mut parents = vec![u32::MAX; graph.neighbors.len()];
    let root_index = node_index(root, node_count)?;
    *parents.get_mut(root_index).ok_or_else(|| {
        RefinementError::Geometry("repair root parent is unavailable".to_owned())
    })? = root;
    let mut queue = VecDeque::from([root]);
    while let Some(owner) = queue.pop_front() {
        let neighbors = graph
            .neighbors
            .get(node_index(owner, node_count)?)
            .ok_or_else(|| {
                RefinementError::Geometry(format!("repair tree row {owner} is unavailable"))
            })?;
        for neighbor in neighbors {
            let index = node_index(*neighbor, node_count)?;
            if seen.get(index).copied() == Some(1) && parents.get(index).copied() == Some(u32::MAX)
            {
                *parents.get_mut(index).ok_or_else(|| {
                    RefinementError::Geometry("repair parent row is unavailable".to_owned())
                })? = owner;
                queue.push_back(*neighbor);
            }
        }
    }
    for (owner, marker) in seen.iter().enumerate() {
        if *marker == 0 {
            continue;
        }
        let owner_u32 = u32::try_from(owner)
            .map_err(|_| RefinementError::Geometry("repair owner exceeds u32".to_owned()))?;
        let neighbors = graph.neighbors.get(owner).ok_or_else(|| {
            RefinementError::Geometry("repair source row is unavailable".to_owned())
        })?;
        for (slot, neighbor) in neighbors.iter().enumerate().rev() {
            let neighbor_index = node_index(*neighbor, node_count)?;
            if parents.get(neighbor_index).copied() != Some(owner_u32) {
                return Ok((owner_u32, slot));
            }
        }
    }
    Err(RefinementError::Geometry(
        "connected component has no spare or non-tree edge for stitching".to_owned(),
    ))
}

fn set_or_append_neighbor(
    graph: &mut OwnedGraph,
    owner: u32,
    target: u32,
    maximum: usize,
    replacement: Option<usize>,
) -> Result<(), RefinementError> {
    let node_count = u32::try_from(graph.neighbors.len())
        .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
    let neighbors = graph
        .neighbors
        .get_mut(node_index(owner, node_count)?)
        .ok_or_else(|| {
            RefinementError::Geometry(format!(
                "neighbors for stitch source {owner} are unavailable"
            ))
        })?;
    if neighbors.contains(&target) {
        return Ok(());
    }
    if neighbors.len() < maximum {
        neighbors.push(target);
        return Ok(());
    }
    let slot = replacement.ok_or_else(|| {
        RefinementError::Geometry(format!("full stitch source {owner} has no replacement"))
    })?;
    let destination = neighbors.get_mut(slot).ok_or_else(|| {
        RefinementError::Geometry(format!("stitch replacement {slot} is unavailable"))
    })?;
    *destination = target;
    Ok(())
}

fn densify_hub(graph: &mut OwnedGraph, owner: u32, r_max: u8) -> Result<(), RefinementError> {
    let node_count = u32::try_from(graph.neighbors.len())
        .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
    let owner_index = node_index(owner, node_count)?;
    if graph.flags.get(owner_index).copied().ok_or_else(|| {
        RefinementError::Geometry(format!("flags for repair row {owner} are unavailable"))
    })? & HUB_FLAG
        == 0
    {
        return Ok(());
    }
    let target_degree = usize::from(r_max).min(graph.neighbors.len().saturating_sub(1));
    let neighbors = graph.neighbors.get_mut(owner_index).ok_or_else(|| {
        RefinementError::Geometry(format!("neighbors for repair hub {owner} are unavailable"))
    })?;
    for candidate in 0..node_count {
        if neighbors.len() >= target_degree {
            break;
        }
        if candidate != owner && !neighbors.contains(&candidate) {
            neighbors.push(candidate);
        }
    }
    if neighbors.len() != target_degree {
        return Err(RefinementError::Geometry(format!(
            "repair hub {owner} densified to {}, expected {target_degree}",
            neighbors.len()
        )));
    }
    Ok(())
}

/// Runs one refinement in deterministic, durably checkpointed row batches.
pub fn refine_graph_checkpointed(
    store: &Store,
    reader: &SegmentReader,
    request: CheckpointedRefinement<'_>,
    lease: &SnapshotLease,
) -> Result<RefinementArtifact, RefinementError> {
    let source = reader.graph_node_blocks()?;
    if source.refinement_passes().contains(request.pass) {
        return Err(RefinementError::AlreadyApplied(request.pass));
    }
    if source.node_count() != reader.meta().row_count {
        return Err(RefinementError::Geometry(format!(
            "graph rows {}, segment rows {}",
            source.node_count(),
            reader.meta().row_count
        )));
    }
    let mut state = match store.vfs.read(request.checkpoint_path) {
        Ok(bytes) => match decode_refinement_checkpoint(
            &bytes,
            reader,
            request.pass,
            request.params,
            request.seed,
            Some(&store.accounting),
        ) {
            Ok(state) => state,
            Err(error @ RefinementError::Graph(GraphBuildError::CheckpointCorrupt(_))) => {
                remove_refinement_checkpoint(store.vfs.as_ref(), request.checkpoint_path)?;
                return Err(error);
            }
            Err(error) => return Err(error),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RefinementState::new(
            reader,
            request.pass,
            request.params,
            Some(&store.accounting),
        )?,
        Err(source) => {
            return Err(GraphBuildError::CheckpointIo {
                path: request.checkpoint_path.to_path_buf(),
                source,
            }
            .into());
        }
    };
    let started = state.work_rows;
    let cancellation = QueryCancellation::new(request.control, lease);
    while state.phase != RefinementPhase::Complete {
        let completed = state.work_rows.saturating_sub(started);
        if completed >= request.max_work_rows {
            return Err(GraphBuildError::BudgetExhausted {
                rows_completed: completed,
            }
            .into());
        }
        let batch = u64::from(request.params.checkpoint_batch_rows())
            .min(request.max_work_rows.saturating_sub(completed));
        let complete = state.advance_batch(
            reader,
            request.pass,
            request.params,
            request.seed,
            batch,
            &cancellation,
        )?;
        write_refinement_checkpoint(
            store.vfs.as_ref(),
            store.durability_policy,
            request.checkpoint_path,
            reader,
            request.pass,
            request.params,
            request.seed,
            request.generation,
            &mut state,
        )?;
        if complete {
            break;
        }
    }
    let work_rows_completed = state.work_rows.saturating_sub(started);
    state.graph.passes = state.graph.passes.with(request.pass);
    let new_to_old =
        (request.pass == RefinementPass::Renumber).then(|| std::mem::take(&mut state.permutation));
    let old_to_new = match new_to_old.as_deref() {
        Some(order) => Some(invert_permutation(order, reader.meta().row_count)?),
        None => None,
    };
    let encoded_region = encode_owned_graph(reader, &state.graph, new_to_old.as_deref())?;
    remove_refinement_checkpoint(store.vfs.as_ref(), request.checkpoint_path)?;
    Ok(RefinementArtifact {
        pass: request.pass,
        encoded_region,
        old_to_new,
        new_to_old,
        work_rows_completed,
    })
}

#[allow(clippy::too_many_arguments)]
fn write_refinement_checkpoint(
    vfs: &dyn Vfs,
    policy: DurabilityPolicy,
    path: &Path,
    reader: &SegmentReader,
    pass: RefinementPass,
    params: GraphParams,
    seed: u64,
    generation: u64,
    state: &mut RefinementState,
) -> Result<(), RefinementError> {
    let base_charge = state.memory.as_ref().map_or(0, AccountedCounter::bytes);
    let checkpoint_bytes = refinement_checkpoint_size(reader, params, state)?;
    if let Some(memory) = state.memory.as_mut() {
        let peak = usize::try_from(base_charge)
            .ok()
            .and_then(|base| base.checked_add(checkpoint_bytes))
            .ok_or_else(|| {
                RefinementError::Geometry("checkpoint peak charge overflow".to_owned())
            })?;
        memory.set(peak)?;
    }
    let result = encode_refinement_checkpoint(reader, pass, params, seed, generation, state)
        .and_then(|bytes| publish_refinement_checkpoint(vfs, policy, path, &bytes));
    if let Some(memory) = state.memory.as_mut() {
        memory.set(usize::try_from(base_charge).map_err(|_| {
            RefinementError::Geometry("checkpoint base charge exceeds usize".to_owned())
        })?)?;
    }
    result
}

fn refinement_checkpoint_size(
    reader: &SegmentReader,
    params: GraphParams,
    state: &RefinementState,
) -> Result<usize, RefinementError> {
    let node_count = reader.meta().row_count as usize;
    let graph_bytes = node_count
        .checked_mul(usize::from(params.r_max()))
        .and_then(|slots| slots.checked_mul(std::mem::size_of::<u32>()))
        .and_then(|bytes| bytes.checked_add(node_count.checked_mul(2)?))
        .ok_or_else(|| RefinementError::Geometry("checkpoint graph size overflow".to_owned()))?;
    let aux_bytes = checkpoint_aux_bytes(state)?;
    CHECKPOINT_HEADER_BYTES
        .checked_add(graph_bytes)
        .and_then(|bytes| bytes.checked_add(aux_bytes))
        .and_then(|bytes| bytes.checked_add(CHECKPOINT_CHECKSUM_BYTES))
        .ok_or_else(|| RefinementError::Geometry("checkpoint length overflow".to_owned()))
}

#[allow(clippy::too_many_arguments)]
fn encode_refinement_checkpoint(
    reader: &SegmentReader,
    pass: RefinementPass,
    params: GraphParams,
    seed: u64,
    generation: u64,
    state: &RefinementState,
) -> Result<Vec<u8>, RefinementError> {
    let node_count = reader.meta().row_count as usize;
    let r_max = usize::from(params.r_max());
    if state.graph.neighbors.len() != node_count || state.graph.flags.len() != node_count {
        return Err(RefinementError::Geometry(
            "checkpoint graph shape differs from source".to_owned(),
        ));
    }
    let graph_bytes = node_count
        .checked_mul(r_max)
        .and_then(|slots| slots.checked_mul(std::mem::size_of::<u32>()))
        .and_then(|bytes| bytes.checked_add(node_count.checked_mul(2)?))
        .ok_or_else(|| RefinementError::Geometry("checkpoint graph size overflow".to_owned()))?;
    let aux_bytes = checkpoint_aux_bytes(state)?;
    let total_bytes = CHECKPOINT_HEADER_BYTES
        .checked_add(graph_bytes)
        .and_then(|bytes| bytes.checked_add(aux_bytes))
        .and_then(|bytes| bytes.checked_add(CHECKPOINT_CHECKSUM_BYTES))
        .ok_or_else(|| RefinementError::Geometry("checkpoint length overflow".to_owned()))?;
    let mut bytes = Vec::with_capacity(total_bytes);
    bytes.extend_from_slice(&CHECKPOINT_MAGIC);
    bytes.extend_from_slice(&CHECKPOINT_VERSION.to_le_bytes());
    bytes.push(pass as u8);
    bytes.push(refinement_phase_code(state.phase));
    bytes.push(params.r_target());
    bytes.push(params.r_max());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&params.checkpoint_batch_rows().to_le_bytes());
    bytes.extend_from_slice(&reader.meta().row_count.to_le_bytes());
    bytes.extend_from_slice(&reader.meta().dims.to_le_bytes());
    bytes.extend_from_slice(&state.next_row.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&seed.to_le_bytes());
    bytes.extend_from_slice(&params.alpha_refine().to_bits().to_le_bytes());
    bytes.extend_from_slice(reader.meta().id.as_bytes());
    bytes.extend_from_slice(&state.graph.passes.bits().to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&state.work_rows.to_le_bytes());
    bytes.extend_from_slice(
        &u64::try_from(graph_bytes)
            .map_err(|_| RefinementError::Geometry("checkpoint graph bytes exceed u64".to_owned()))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(
        &u64::try_from(aux_bytes)
            .map_err(|_| RefinementError::Geometry("checkpoint aux bytes exceed u64".to_owned()))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    if bytes.len() != CHECKPOINT_HEADER_BYTES {
        return Err(RefinementError::Geometry(format!(
            "checkpoint header encoded {} bytes, expected {CHECKPOINT_HEADER_BYTES}",
            bytes.len()
        )));
    }
    bytes.extend_from_slice(&state.graph.flags);
    for neighbors in &state.graph.neighbors {
        let degree = u8::try_from(neighbors.len())
            .map_err(|_| RefinementError::Geometry("checkpoint degree exceeds u8".to_owned()))?;
        if usize::from(degree) > r_max {
            return Err(RefinementError::Geometry(format!(
                "checkpoint degree {degree} exceeds {}",
                params.r_max()
            )));
        }
        bytes.push(degree);
    }
    for (owner, neighbors) in state.graph.neighbors.iter().enumerate() {
        for neighbor in neighbors {
            if usize::try_from(*neighbor)
                .ok()
                .is_none_or(|node| node >= node_count)
                || usize::try_from(*neighbor).ok() == Some(owner)
            {
                return Err(RefinementError::Geometry(format!(
                    "checkpoint node {owner} has invalid neighbor {neighbor}"
                )));
            }
            bytes.extend_from_slice(&neighbor.to_le_bytes());
        }
        for _ in neighbors.len()..r_max {
            bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        }
    }
    encode_u32_slice(&mut bytes, &state.permutation)?;
    encode_u32_slice(&mut bytes, state.queue.as_slices().0)?;
    encode_u32_slice(&mut bytes, state.queue.as_slices().1)?;
    bytes.extend_from_slice(
        &u32::try_from(state.seen.len())
            .map_err(|_| {
                RefinementError::Geometry("checkpoint seen length exceeds u32".to_owned())
            })?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&state.seen);
    encode_u32_slice(&mut bytes, &state.entries)?;
    bytes.extend_from_slice(&state.best_node.unwrap_or(u32::MAX).to_le_bytes());
    bytes.extend_from_slice(&state.best_score.to_bits().to_le_bytes());
    if bytes.len() != CHECKPOINT_HEADER_BYTES + graph_bytes + aux_bytes {
        return Err(RefinementError::Geometry(
            "checkpoint payload length changed while encoding".to_owned(),
        ));
    }
    bytes.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
    Ok(bytes)
}

fn checkpoint_aux_bytes(state: &RefinementState) -> Result<usize, RefinementError> {
    let u32_items = state
        .permutation
        .len()
        .checked_add(state.queue.len())
        .and_then(|count| count.checked_add(state.entries.len()))
        .ok_or_else(|| RefinementError::Geometry("checkpoint aux count overflow".to_owned()))?;
    u32_items
        .checked_mul(std::mem::size_of::<u32>())
        .and_then(|bytes| bytes.checked_add(state.seen.len()))
        .and_then(|bytes| bytes.checked_add(6 * std::mem::size_of::<u32>()))
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<u64>()))
        .ok_or_else(|| RefinementError::Geometry("checkpoint aux size overflow".to_owned()))
}

fn encode_u32_slice(bytes: &mut Vec<u8>, values: &[u32]) -> Result<(), RefinementError> {
    bytes.extend_from_slice(
        &u32::try_from(values.len())
            .map_err(|_| RefinementError::Geometry("checkpoint list exceeds u32".to_owned()))?
            .to_le_bytes(),
    );
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Ok(())
}

fn publish_refinement_checkpoint(
    vfs: &dyn Vfs,
    policy: DurabilityPolicy,
    path: &Path,
    bytes: &[u8],
) -> Result<(), RefinementError> {
    let directory = path.parent().ok_or_else(|| GraphBuildError::CheckpointIo {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refinement checkpoint path has no parent directory",
        ),
    })?;
    let temporary = refinement_checkpoint_temp_path(path);
    if let Err(source) = vfs.write(&temporary, bytes) {
        let _ = vfs.delete(&temporary);
        return Err(GraphBuildError::CheckpointIo {
            path: temporary,
            source,
        }
        .into());
    }
    if let SyncRequirement::Sync(kind) = policy.data_file_sync()
        && let Err(source) = vfs.sync(&temporary, kind)
    {
        let _ = vfs.delete(&temporary);
        return Err(GraphBuildError::CheckpointIo {
            path: temporary,
            source,
        }
        .into());
    }
    if let Err(source) = vfs.rename(&temporary, path) {
        let _ = vfs.delete(&temporary);
        return Err(GraphBuildError::CheckpointIo {
            path: path.to_path_buf(),
            source,
        }
        .into());
    }
    match policy.directory_sync() {
        SyncRequirement::Skip => Ok(()),
        SyncRequirement::Sync(kind) => vfs.sync(directory, kind).map_err(|source| {
            RefinementError::Graph(GraphBuildError::CheckpointIo {
                path: directory.to_path_buf(),
                source,
            })
        }),
    }
}

fn refinement_checkpoint_temp_path(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}

/// Removes both durable and temporary forms of one refinement checkpoint.
pub fn remove_refinement_checkpoint(vfs: &dyn Vfs, path: &Path) -> Result<(), RefinementError> {
    for candidate in [refinement_checkpoint_temp_path(path), path.to_path_buf()] {
        match vfs.delete(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GraphBuildError::CheckpointIo {
                    path: candidate,
                    source,
                }
                .into());
            }
        }
    }
    Ok(())
}

fn decode_refinement_checkpoint(
    bytes: &[u8],
    reader: &SegmentReader,
    pass: RefinementPass,
    params: GraphParams,
    seed: u64,
    accounting: Option<&std::sync::Arc<crate::lifecycle::stats::Accounting>>,
) -> Result<RefinementState, RefinementError> {
    validate_refinement_checkpoint(bytes)?;
    let source_graph = reader.graph_node_blocks()?;
    // Store generation is provenance, not resume identity: live ingest advances per
    // acknowledged batch, replay advances per unabsorbed WAL record, and maintenance
    // can publish the counter without absorbing WAL, so it is not stable across reopen.
    if checkpoint_u8(bytes, 10, "pass")? != pass as u8
        || checkpoint_u8(bytes, 12, "target degree")? != params.r_target()
        || checkpoint_u8(bytes, 13, "maximum degree")? != params.r_max()
        || checkpoint_u32(bytes, 16, "checkpoint batch")? != params.checkpoint_batch_rows()
        || checkpoint_u32(bytes, 20, "node count")? != reader.meta().row_count
        || checkpoint_u32(bytes, 24, "dimensions")? != reader.meta().dims
        || checkpoint_u64(bytes, 36, "seed")? != seed
        || checkpoint_u32(bytes, 44, "refinement alpha")? != params.alpha_refine().to_bits()
        || bytes.get(48..64) != Some(reader.meta().id.as_bytes().as_slice())
        || checkpoint_u16(bytes, 64, "source passes")? != source_graph.refinement_passes().bits()
        || source_graph.layout().max_degree() != params.r_max()
    {
        return Err(checkpoint_corrupt(
            "checkpoint identity or refinement parameters do not match input",
        ));
    }
    let phase = decode_refinement_phase(checkpoint_u8(bytes, 11, "phase")?)?;
    let next_row = checkpoint_u32(bytes, 28, "next row")?;
    let work_rows = checkpoint_u64(bytes, 76, "work rows")?;
    let graph_bytes = usize::try_from(checkpoint_u64(bytes, 84, "graph bytes")?)
        .map_err(|_| checkpoint_corrupt("graph bytes exceed usize"))?;
    let graph_start = CHECKPOINT_HEADER_BYTES;
    let aux_start = graph_start
        .checked_add(graph_bytes)
        .ok_or_else(|| checkpoint_corrupt("aux offset overflow"))?;
    let node_count = reader.meta().row_count as usize;
    let degrees_start = graph_start
        .checked_add(node_count)
        .ok_or_else(|| checkpoint_corrupt("degree offset overflow"))?;
    let slots_start = degrees_start
        .checked_add(node_count)
        .ok_or_else(|| checkpoint_corrupt("slot offset overflow"))?;
    let mut flags = Vec::with_capacity(node_count);
    flags.extend_from_slice(
        bytes
            .get(graph_start..degrees_start)
            .ok_or_else(|| checkpoint_corrupt("flag bytes are truncated"))?,
    );
    let mut neighbors = Vec::with_capacity(node_count);
    let r_max = usize::from(params.r_max());
    for owner in 0..node_count {
        let degree = usize::from(checkpoint_u8(bytes, degrees_start + owner, "degree")?);
        let row_start = owner
            .checked_mul(r_max)
            .and_then(|slot| slot.checked_mul(std::mem::size_of::<u32>()))
            .and_then(|offset| slots_start.checked_add(offset))
            .ok_or_else(|| checkpoint_corrupt("neighbor row offset overflow"))?;
        let mut row = Vec::with_capacity(r_max);
        for slot in 0..degree {
            let offset = slot
                .checked_mul(std::mem::size_of::<u32>())
                .and_then(|offset| row_start.checked_add(offset))
                .ok_or_else(|| checkpoint_corrupt("neighbor offset overflow"))?;
            row.push(checkpoint_u32(bytes, offset, "neighbor")?);
        }
        neighbors.push(row);
    }
    let mut cursor = aux_start;
    let permutation = decode_u32_list(bytes, &mut cursor, node_count, "permutation")?;
    let queue_first = decode_u32_list(bytes, &mut cursor, node_count, "queue first")?;
    let queue_second = decode_u32_list(bytes, &mut cursor, node_count, "queue second")?;
    let seen_len = usize::try_from(checkpoint_u32(bytes, cursor, "seen length")?)
        .map_err(|_| checkpoint_corrupt("seen length exceeds usize"))?;
    cursor = cursor
        .checked_add(4)
        .ok_or_else(|| checkpoint_corrupt("seen offset overflow"))?;
    let seen_end = cursor
        .checked_add(seen_len)
        .ok_or_else(|| checkpoint_corrupt("seen end overflow"))?;
    let seen = bytes
        .get(cursor..seen_end)
        .ok_or_else(|| checkpoint_corrupt("seen bytes are truncated"))?
        .to_vec();
    cursor = seen_end;
    let entries = decode_u32_list(bytes, &mut cursor, ENTRY_POINT_COUNT, "entries")?;
    let best = checkpoint_u32(bytes, cursor, "best node")?;
    cursor = cursor
        .checked_add(4)
        .ok_or_else(|| checkpoint_corrupt("best score offset overflow"))?;
    let best_score = f64::from_bits(checkpoint_u64(bytes, cursor, "best score")?);
    let mut queue = VecDeque::with_capacity(queue_first.len() + queue_second.len());
    queue.extend(queue_first);
    queue.extend(queue_second);
    let layout = source_graph.layout();
    let source_passes = RefinementPasses::from_bits(checkpoint_u16(bytes, 64, "source passes")?)
        .ok_or_else(|| checkpoint_corrupt("source pass bits are unknown"))?;
    let mut state = RefinementState {
        graph: OwnedGraph {
            layout,
            passes: source_passes,
            flags,
            neighbors,
        },
        phase,
        next_row,
        work_rows,
        permutation,
        queue,
        seen,
        entries,
        best_node: (best != u32::MAX).then_some(best),
        best_score,
        scratch_bytes: refinement_scratch_bytes(pass, params)?,
        memory: match accounting {
            Some(accounting) => Some(AccountedCounter::new(
                accounting,
                AllocationComponent::Temporary,
            )?),
            None => None,
        },
    };
    state.update_memory_charge()?;
    Ok(state)
}

fn refinement_scratch_bytes(
    pass: RefinementPass,
    params: GraphParams,
) -> Result<usize, RefinementError> {
    let r_max = usize::from(params.r_max());
    match pass {
        RefinementPass::Renumber => r_max
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| RefinementError::Geometry("renumber scratch overflow".to_owned())),
        RefinementPass::AlphaReprune => {
            let candidates = r_max
                .checked_mul(r_max)
                .and_then(|count| count.checked_add(r_max))
                .ok_or_else(|| RefinementError::Geometry("re-prune scratch overflow".to_owned()))?;
            candidates
                .checked_mul(24)
                .and_then(|bytes| {
                    usize::from(params.r_target())
                        .checked_mul(std::mem::size_of::<u32>())
                        .and_then(|selected| bytes.checked_add(selected))
                })
                .ok_or_else(|| RefinementError::Geometry("re-prune scratch overflow".to_owned()))
        }
        RefinementPass::SeedRefit => Ok(0),
        RefinementPass::NeighborReorder => r_max
            .checked_mul(std::mem::size_of::<(u32, f64)>())
            .ok_or_else(|| RefinementError::Geometry("reorder scratch overflow".to_owned())),
        RefinementPass::ConnectivityRepair => Ok(0),
    }
}

fn decode_u32_list(
    bytes: &[u8],
    cursor: &mut usize,
    maximum: usize,
    field: &str,
) -> Result<Vec<u32>, RefinementError> {
    let count = usize::try_from(checkpoint_u32(bytes, *cursor, field)?)
        .map_err(|_| checkpoint_corrupt(format!("{field} count exceeds usize")))?;
    if count > maximum {
        return Err(checkpoint_corrupt(format!(
            "{field} count {count} exceeds {maximum}"
        )));
    }
    *cursor = cursor
        .checked_add(4)
        .ok_or_else(|| checkpoint_corrupt(format!("{field} offset overflow")))?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(checkpoint_u32(bytes, *cursor, field)?);
        *cursor = cursor
            .checked_add(4)
            .ok_or_else(|| checkpoint_corrupt(format!("{field} offset overflow")))?;
    }
    Ok(values)
}

/// Validates arbitrary refinement checkpoint bytes without allocating pass state.
pub fn validate_refinement_checkpoint(bytes: &[u8]) -> Result<(), RefinementError> {
    let minimum = CHECKPOINT_HEADER_BYTES
        .checked_add(CHECKPOINT_CHECKSUM_BYTES)
        .ok_or_else(|| checkpoint_corrupt("minimum checkpoint length overflow"))?;
    if bytes.len() < minimum {
        return Err(checkpoint_corrupt(format!(
            "truncated: need at least {minimum} bytes, got {}",
            bytes.len()
        )));
    }
    if bytes.get(..8) != Some(CHECKPOINT_MAGIC.as_slice()) {
        return Err(checkpoint_corrupt("magic does not match ZEREFCP1"));
    }
    let checksum_start = bytes
        .len()
        .checked_sub(CHECKPOINT_CHECKSUM_BYTES)
        .ok_or_else(|| checkpoint_corrupt("checksum offset underflow"))?;
    let stored_checksum = checkpoint_u64(bytes, checksum_start, "checksum")?;
    let prefix = bytes
        .get(..checksum_start)
        .ok_or_else(|| checkpoint_corrupt("checksummed prefix is unavailable"))?;
    let actual_checksum = xxh3_64(prefix);
    if stored_checksum != actual_checksum {
        return Err(checkpoint_corrupt(format!(
            "xxh3-64 expected {stored_checksum:#018x}, computed {actual_checksum:#018x}"
        )));
    }
    if checkpoint_u16(bytes, 8, "version")? != CHECKPOINT_VERSION {
        return Err(checkpoint_corrupt("checkpoint version is unsupported"));
    }
    let pass = decode_refinement_pass(checkpoint_u8(bytes, 10, "pass")?)?;
    let phase = decode_refinement_phase(checkpoint_u8(bytes, 11, "phase")?)?;
    let r_target = checkpoint_u8(bytes, 12, "target degree")?;
    let r_max = checkpoint_u8(bytes, 13, "maximum degree")?;
    let batch = checkpoint_u32(bytes, 16, "checkpoint batch")?;
    let node_count = checkpoint_u32(bytes, 20, "node count")?;
    let dimensions = checkpoint_u32(bytes, 24, "dimensions")?;
    let next_row = checkpoint_u32(bytes, 28, "next row")?;
    let alpha = f32::from_bits(checkpoint_u32(bytes, 44, "refinement alpha")?);
    let source_passes = checkpoint_u16(bytes, 64, "source passes")?;
    if checkpoint_u16(bytes, 14, "reserved degree bytes")? != 0
        || checkpoint_u32(bytes, 32, "reserved row bytes")? != 0
        || checkpoint_u16(bytes, 66, "reserved pass bytes")? != 0
        || checkpoint_u32(bytes, 100, "reserved tail bytes")? != 0
    {
        return Err(checkpoint_corrupt("reserved header bytes are nonzero"));
    }
    if r_target == 0
        || r_target > r_max
        || batch == 0
        || node_count == 0
        || dimensions == 0
        || next_row > node_count
        || !alpha.is_finite()
        || alpha < 1.0
        || RefinementPasses::from_bits(source_passes).is_none()
        || source_passes & pass.bit() != 0
    {
        return Err(checkpoint_corrupt(
            "stored shape, alpha, progress, or pass record is invalid",
        ));
    }
    let phase_matches_pass = match pass {
        RefinementPass::SeedRefit => matches!(
            phase,
            RefinementPhase::SeedMedoid | RefinementPhase::SeedSpread | RefinementPhase::Complete
        ),
        RefinementPass::ConnectivityRepair => matches!(
            phase,
            RefinementPhase::ConnectivityTraverse
                | RefinementPhase::ConnectivityDensify
                | RefinementPhase::Complete
        ),
        RefinementPass::Renumber
        | RefinementPass::AlphaReprune
        | RefinementPass::NeighborReorder => {
            matches!(phase, RefinementPhase::Rows | RefinementPhase::Complete)
        }
    };
    if !phase_matches_pass {
        return Err(checkpoint_corrupt(
            "refinement phase does not match its pass",
        ));
    }
    let node_count_usize = node_count as usize;
    let expected_graph = node_count_usize
        .checked_mul(usize::from(r_max))
        .and_then(|slots| slots.checked_mul(std::mem::size_of::<u32>()))
        .and_then(|bytes| bytes.checked_add(node_count_usize.checked_mul(2)?))
        .ok_or_else(|| checkpoint_corrupt("graph payload geometry overflow"))?;
    let graph_bytes = usize::try_from(checkpoint_u64(bytes, 84, "graph bytes")?)
        .map_err(|_| checkpoint_corrupt("graph payload exceeds usize"))?;
    let aux_bytes = usize::try_from(checkpoint_u64(bytes, 92, "aux bytes")?)
        .map_err(|_| checkpoint_corrupt("aux payload exceeds usize"))?;
    if graph_bytes != expected_graph {
        return Err(checkpoint_corrupt(format!(
            "graph payload {graph_bytes}, expected {expected_graph}"
        )));
    }
    let expected_len = CHECKPOINT_HEADER_BYTES
        .checked_add(graph_bytes)
        .and_then(|length| length.checked_add(aux_bytes))
        .and_then(|length| length.checked_add(CHECKPOINT_CHECKSUM_BYTES))
        .ok_or_else(|| checkpoint_corrupt("checkpoint length overflow"))?;
    if bytes.len() != expected_len {
        return Err(checkpoint_corrupt(format!(
            "checkpoint length {}, expected {expected_len}",
            bytes.len()
        )));
    }
    let flags_start = CHECKPOINT_HEADER_BYTES;
    let degrees_start = flags_start
        .checked_add(node_count_usize)
        .ok_or_else(|| checkpoint_corrupt("degree offset overflow"))?;
    let slots_start = degrees_start
        .checked_add(node_count_usize)
        .ok_or_else(|| checkpoint_corrupt("slot offset overflow"))?;
    for owner in 0..node_count_usize {
        let flags = checkpoint_u8(bytes, flags_start + owner, "node flags")?;
        if flags & !(ENTRY_FLAG | HUB_FLAG) != 0 {
            return Err(checkpoint_corrupt(format!(
                "node {owner} has unknown flags {flags:#04x}"
            )));
        }
        let degree = usize::from(checkpoint_u8(bytes, degrees_start + owner, "degree")?);
        if degree > usize::from(r_max) {
            return Err(checkpoint_corrupt(format!(
                "node {owner} degree {degree} exceeds {r_max}"
            )));
        }
        let row_start = owner
            .checked_mul(usize::from(r_max))
            .and_then(|slot| slot.checked_mul(std::mem::size_of::<u32>()))
            .and_then(|offset| slots_start.checked_add(offset))
            .ok_or_else(|| checkpoint_corrupt("neighbor row offset overflow"))?;
        for slot in 0..usize::from(r_max) {
            let offset = slot
                .checked_mul(std::mem::size_of::<u32>())
                .and_then(|offset| row_start.checked_add(offset))
                .ok_or_else(|| checkpoint_corrupt("neighbor offset overflow"))?;
            let neighbor = checkpoint_u32(bytes, offset, "neighbor")?;
            if slot < degree {
                if neighbor >= node_count || neighbor as usize == owner {
                    return Err(checkpoint_corrupt(format!(
                        "node {owner} has invalid active neighbor {neighbor}"
                    )));
                }
                for prior in 0..slot {
                    let prior_offset = prior
                        .checked_mul(std::mem::size_of::<u32>())
                        .and_then(|offset| row_start.checked_add(offset))
                        .ok_or_else(|| checkpoint_corrupt("prior neighbor offset overflow"))?;
                    if checkpoint_u32(bytes, prior_offset, "prior neighbor")? == neighbor {
                        return Err(checkpoint_corrupt(format!(
                            "node {owner} repeats neighbor {neighbor}"
                        )));
                    }
                }
            } else if neighbor != u32::MAX {
                return Err(checkpoint_corrupt(format!(
                    "node {owner} inactive slot {slot} is not the sentinel"
                )));
            }
        }
    }
    validate_refinement_aux(
        bytes,
        slots_start + node_count_usize * usize::from(r_max) * 4,
        checksum_start,
        node_count_usize,
        pass,
        phase,
    )
}

fn validate_refinement_aux(
    bytes: &[u8],
    mut cursor: usize,
    end: usize,
    node_count: usize,
    pass: RefinementPass,
    phase: RefinementPhase,
) -> Result<(), RefinementError> {
    let permutation = validate_u32_list(bytes, &mut cursor, node_count, node_count, "permutation")?;
    let queue_first = validate_u32_list(bytes, &mut cursor, node_count, node_count, "queue first")?;
    let queue_second =
        validate_u32_list(bytes, &mut cursor, node_count, node_count, "queue second")?;
    let seen_len = usize::try_from(checkpoint_u32(bytes, cursor, "seen length")?)
        .map_err(|_| checkpoint_corrupt("seen length exceeds usize"))?;
    cursor = cursor
        .checked_add(4)
        .ok_or_else(|| checkpoint_corrupt("seen offset overflow"))?;
    if seen_len != 0 && seen_len != node_count {
        return Err(checkpoint_corrupt(
            "seen length is neither zero nor node count",
        ));
    }
    let seen_end = cursor
        .checked_add(seen_len)
        .ok_or_else(|| checkpoint_corrupt("seen end overflow"))?;
    if bytes
        .get(cursor..seen_end)
        .ok_or_else(|| checkpoint_corrupt("seen bytes are truncated"))?
        .iter()
        .any(|marker| *marker > 1)
    {
        return Err(checkpoint_corrupt(
            "seen bytes contain a non-boolean marker",
        ));
    }
    cursor = seen_end;
    let entries = validate_u32_list(bytes, &mut cursor, ENTRY_POINT_COUNT, node_count, "entries")?;
    let best_node = checkpoint_u32(bytes, cursor, "best node")?;
    cursor = cursor
        .checked_add(4)
        .ok_or_else(|| checkpoint_corrupt("best score offset overflow"))?;
    let best_score = f64::from_bits(checkpoint_u64(bytes, cursor, "best score")?);
    cursor = cursor
        .checked_add(8)
        .ok_or_else(|| checkpoint_corrupt("aux end overflow"))?;
    if cursor != end {
        return Err(checkpoint_corrupt(
            "aux payload has trailing or missing bytes",
        ));
    }
    if best_node != u32::MAX && best_node as usize >= node_count {
        return Err(checkpoint_corrupt("best seed node is out of range"));
    }
    if best_score.is_nan() {
        return Err(checkpoint_corrupt("best seed score is NaN"));
    }
    if pass == RefinementPass::Renumber {
        if permutation > node_count || queue_first + queue_second > node_count || entries != 0 {
            return Err(checkpoint_corrupt("renumber auxiliary state is invalid"));
        }
    } else if pass == RefinementPass::ConnectivityRepair {
        if permutation != 0
            || queue_first + queue_second > node_count
            || seen_len != node_count
            || entries != 0
        {
            return Err(checkpoint_corrupt(
                "connectivity-repair auxiliary state is invalid",
            ));
        }
    } else if permutation != 0 || queue_first != 0 || queue_second != 0 || seen_len != 0 {
        return Err(checkpoint_corrupt(
            "non-renumber pass carries renumber state",
        ));
    }
    if pass != RefinementPass::SeedRefit && (entries != 0 || best_node != u32::MAX) {
        return Err(checkpoint_corrupt("non-seed pass carries seed state"));
    }
    if phase == RefinementPhase::Complete && pass == RefinementPass::SeedRefit && entries == 0 {
        return Err(checkpoint_corrupt("completed seed pass has no entries"));
    }
    Ok(())
}

fn validate_u32_list(
    bytes: &[u8],
    cursor: &mut usize,
    maximum_count: usize,
    node_count: usize,
    field: &str,
) -> Result<usize, RefinementError> {
    let count = usize::try_from(checkpoint_u32(bytes, *cursor, field)?)
        .map_err(|_| checkpoint_corrupt(format!("{field} count exceeds usize")))?;
    if count > maximum_count {
        return Err(checkpoint_corrupt(format!(
            "{field} count {count} exceeds {maximum_count}"
        )));
    }
    *cursor = cursor
        .checked_add(4)
        .ok_or_else(|| checkpoint_corrupt(format!("{field} offset overflow")))?;
    for index in 0..count {
        let offset = index
            .checked_mul(4)
            .and_then(|offset| cursor.checked_add(offset))
            .ok_or_else(|| checkpoint_corrupt(format!("{field} offset overflow")))?;
        let node = checkpoint_u32(bytes, offset, field)?;
        if node as usize >= node_count {
            return Err(checkpoint_corrupt(format!(
                "{field} node {node} is out of range"
            )));
        }
        for prior in 0..index {
            let prior_offset = prior
                .checked_mul(4)
                .and_then(|offset| cursor.checked_add(offset))
                .ok_or_else(|| checkpoint_corrupt(format!("{field} prior offset overflow")))?;
            if checkpoint_u32(bytes, prior_offset, field)? == node {
                return Err(checkpoint_corrupt(format!("{field} repeats node {node}")));
            }
        }
    }
    *cursor = cursor
        .checked_add(
            count
                .checked_mul(4)
                .ok_or_else(|| checkpoint_corrupt(format!("{field} byte count overflow")))?,
        )
        .ok_or_else(|| checkpoint_corrupt(format!("{field} end overflow")))?;
    Ok(count)
}

fn check_refinement_cancellation(
    cancellation: &QueryCancellation<'_>,
) -> Result<(), RefinementError> {
    match cancellation.check_graph() {
        Ok(()) => Ok(()),
        Err(ScanError::Cancelled { .. }) => {
            Err(GraphBuildError::Cancelled { partial: false }.into())
        }
        Err(ScanError::Timeout { .. }) => Err(GraphBuildError::Timeout { partial: false }.into()),
        Err(ScanError::ReadCancelled { .. }) => {
            Err(GraphBuildError::ReadCancelled { partial: false }.into())
        }
        Err(error) => Err(RefinementError::Geometry(format!(
            "refinement cancellation seam returned non-cancellation error: {error}"
        ))),
    }
}

fn refinement_phase_code(phase: RefinementPhase) -> u8 {
    match phase {
        RefinementPhase::Rows => 1,
        RefinementPhase::SeedMedoid => 2,
        RefinementPhase::SeedSpread => 3,
        RefinementPhase::Complete => 4,
        RefinementPhase::ConnectivityTraverse => 5,
        RefinementPhase::ConnectivityDensify => 6,
    }
}

fn decode_refinement_phase(value: u8) -> Result<RefinementPhase, RefinementError> {
    match value {
        1 => Ok(RefinementPhase::Rows),
        2 => Ok(RefinementPhase::SeedMedoid),
        3 => Ok(RefinementPhase::SeedSpread),
        4 => Ok(RefinementPhase::Complete),
        5 => Ok(RefinementPhase::ConnectivityTraverse),
        6 => Ok(RefinementPhase::ConnectivityDensify),
        _ => Err(checkpoint_corrupt(format!(
            "unknown refinement phase {value}"
        ))),
    }
}

fn decode_refinement_pass(value: u8) -> Result<RefinementPass, RefinementError> {
    match value {
        0 => Ok(RefinementPass::Renumber),
        1 => Ok(RefinementPass::AlphaReprune),
        2 => Ok(RefinementPass::SeedRefit),
        3 => Ok(RefinementPass::NeighborReorder),
        4 => Ok(RefinementPass::ConnectivityRepair),
        _ => Err(checkpoint_corrupt(format!(
            "unknown refinement pass {value}"
        ))),
    }
}

fn checkpoint_corrupt(detail: impl Into<String>) -> RefinementError {
    RefinementError::Graph(GraphBuildError::CheckpointCorrupt(detail.into()))
}

fn checkpoint_u8(bytes: &[u8], offset: usize, field: &str) -> Result<u8, RefinementError> {
    bytes
        .get(offset)
        .copied()
        .ok_or_else(|| checkpoint_corrupt(format!("{field} is truncated at byte {offset}")))
}

fn checkpoint_u16(bytes: &[u8], offset: usize, field: &str) -> Result<u16, RefinementError> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| checkpoint_corrupt(format!("{field} offset overflow")))?;
    let array = bytes
        .get(offset..end)
        .ok_or_else(|| checkpoint_corrupt(format!("{field} is truncated")))?
        .try_into()
        .map_err(|_| checkpoint_corrupt(format!("{field} width is invalid")))?;
    Ok(u16::from_le_bytes(array))
}

fn checkpoint_u32(bytes: &[u8], offset: usize, field: &str) -> Result<u32, RefinementError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| checkpoint_corrupt(format!("{field} offset overflow")))?;
    let array = bytes
        .get(offset..end)
        .ok_or_else(|| checkpoint_corrupt(format!("{field} is truncated")))?
        .try_into()
        .map_err(|_| checkpoint_corrupt(format!("{field} width is invalid")))?;
    Ok(u32::from_le_bytes(array))
}

fn checkpoint_u64(bytes: &[u8], offset: usize, field: &str) -> Result<u64, RefinementError> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| checkpoint_corrupt(format!("{field} offset overflow")))?;
    let array = bytes
        .get(offset..end)
        .ok_or_else(|| checkpoint_corrupt(format!("{field} is truncated")))?
        .try_into()
        .map_err(|_| checkpoint_corrupt(format!("{field} width is invalid")))?;
    Ok(u64::from_le_bytes(array))
}

/// Computes one complete deterministic refinement artifact without publishing it.
pub fn refine_graph(
    reader: &SegmentReader,
    pass: RefinementPass,
    params: GraphParams,
    seed: u64,
) -> Result<RefinementArtifact, RefinementError> {
    let source = reader.graph_node_blocks()?;
    if source.refinement_passes().contains(pass) {
        return Err(RefinementError::AlreadyApplied(pass));
    }
    if source.node_count() != reader.meta().row_count {
        return Err(RefinementError::Geometry(format!(
            "graph rows {}, segment rows {}",
            source.node_count(),
            reader.meta().row_count
        )));
    }
    let mut graph = OwnedGraph::read(source)?;
    let rescore = reader.rescore_f32()?;
    let dimensions = reader.meta().dims as usize;
    let node_count = reader.meta().row_count;
    let (old_to_new, new_to_old) = match pass {
        RefinementPass::Renumber => {
            let new_to_old = gorder_lite(&graph)?;
            let old_to_new = invert_permutation(&new_to_old, node_count)?;
            apply_renumber(&mut graph, &new_to_old, &old_to_new)?;
            (Some(old_to_new), Some(new_to_old))
        }
        RefinementPass::AlphaReprune => {
            alpha_reprune(&mut graph, rescore, dimensions, node_count, params)?;
            (None, None)
        }
        RefinementPass::SeedRefit => {
            seed_refit(&mut graph, rescore, dimensions, node_count)?;
            (None, None)
        }
        RefinementPass::NeighborReorder => {
            neighbor_reorder(&mut graph, reader, node_count, seed)?;
            (None, None)
        }
        RefinementPass::ConnectivityRepair => {
            connectivity_repair(&mut graph, params)?;
            (None, None)
        }
    };
    graph.passes = graph.passes.with(pass);
    let encoded_region = encode_owned_graph(reader, &graph, new_to_old.as_deref())?;
    let work_rows_completed = match pass {
        RefinementPass::SeedRefit => u64::from(node_count)
            .checked_mul(u64::from(node_count.min(ENTRY_POINT_COUNT as u32)))
            .ok_or_else(|| RefinementError::Geometry("seed work rows overflow".to_owned()))?,
        RefinementPass::ConnectivityRepair => u64::from(node_count)
            .checked_mul(2)
            .ok_or_else(|| RefinementError::Geometry("repair work rows overflow".to_owned()))?,
        RefinementPass::Renumber
        | RefinementPass::AlphaReprune
        | RefinementPass::NeighborReorder => u64::from(node_count),
    };
    Ok(RefinementArtifact {
        pass,
        encoded_region,
        old_to_new,
        new_to_old,
        work_rows_completed,
    })
}

fn gorder_lite(graph: &OwnedGraph) -> Result<Vec<u32>, RefinementError> {
    let node_count = u32::try_from(graph.neighbors.len())
        .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
    let mut roots = graph
        .flags
        .iter()
        .enumerate()
        .filter_map(|(node, flags)| {
            (*flags & ENTRY_FLAG != 0)
                .then(|| u32::try_from(node).ok())
                .flatten()
        })
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Err(RefinementError::Geometry(
            "renumbering requires at least one persisted entry seed".to_owned(),
        ));
    }
    sort_by_degree(&mut roots, graph)?;
    let mut queued = vec![false; graph.neighbors.len()];
    let mut queue = VecDeque::new();
    for root in roots {
        mark_and_enqueue(root, &mut queued, &mut queue)?;
    }
    let mut order = Vec::with_capacity(graph.neighbors.len());
    loop {
        while let Some(owner) = queue.pop_front() {
            order.push(owner);
            let owner_index = node_index(owner, node_count)?;
            let mut candidates = graph
                .neighbors
                .get(owner_index)
                .ok_or_else(|| {
                    RefinementError::Geometry(format!("neighbors for node {owner} are unavailable"))
                })?
                .iter()
                .copied()
                .filter(|neighbor| {
                    usize::try_from(*neighbor)
                        .ok()
                        .and_then(|index| queued.get(index))
                        .is_some_and(|seen| !*seen)
                })
                .collect::<Vec<_>>();
            sort_by_degree(&mut candidates, graph)?;
            for candidate in candidates {
                mark_and_enqueue(candidate, &mut queued, &mut queue)?;
            }
        }
        let Some(unseen) = queued.iter().position(|seen| !*seen) else {
            break;
        };
        let root = u32::try_from(unseen)
            .map_err(|_| RefinementError::Geometry("unseen row exceeds u32".to_owned()))?;
        mark_and_enqueue(root, &mut queued, &mut queue)?;
    }
    if order.len() != graph.neighbors.len() {
        return Err(RefinementError::Geometry(format!(
            "renumbering emitted {} rows, expected {}",
            order.len(),
            graph.neighbors.len()
        )));
    }
    Ok(order)
}

fn sort_by_degree(nodes: &mut [u32], graph: &OwnedGraph) -> Result<(), RefinementError> {
    for node in nodes.iter().copied() {
        let _ = graph.neighbors.get(node as usize).ok_or_else(|| {
            RefinementError::Geometry(format!("degree for node {node} is unavailable"))
        })?;
    }
    nodes.sort_unstable_by(|left, right| {
        let left_degree = graph.neighbors.get(*left as usize).map_or(0, Vec::len);
        let right_degree = graph.neighbors.get(*right as usize).map_or(0, Vec::len);
        right_degree.cmp(&left_degree).then_with(|| left.cmp(right))
    });
    Ok(())
}

fn mark_and_enqueue(
    node: u32,
    queued: &mut [bool],
    queue: &mut VecDeque<u32>,
) -> Result<(), RefinementError> {
    let marker = queued.get_mut(node as usize).ok_or_else(|| {
        RefinementError::Geometry(format!("renumber queue node {node} is out of range"))
    })?;
    if !*marker {
        *marker = true;
        queue.push_back(node);
    }
    Ok(())
}

fn invert_permutation(new_to_old: &[u32], node_count: u32) -> Result<Vec<u32>, RefinementError> {
    if new_to_old.len() != node_count as usize {
        return Err(RefinementError::Geometry(
            "renumber permutation length differs from row count".to_owned(),
        ));
    }
    let mut old_to_new = vec![u32::MAX; new_to_old.len()];
    for (new, old) in new_to_old.iter().copied().enumerate() {
        let slot = old_to_new
            .get_mut(node_index(old, node_count)?)
            .ok_or_else(|| {
                RefinementError::Geometry(format!("permutation row {old} is unavailable"))
            })?;
        if *slot != u32::MAX {
            return Err(RefinementError::Geometry(format!(
                "renumber permutation repeats row {old}"
            )));
        }
        *slot = u32::try_from(new)
            .map_err(|_| RefinementError::Geometry("new row id exceeds u32".to_owned()))?;
    }
    if old_to_new.contains(&u32::MAX) {
        return Err(RefinementError::Geometry(
            "renumber permutation omits a row".to_owned(),
        ));
    }
    Ok(old_to_new)
}

fn apply_renumber(
    graph: &mut OwnedGraph,
    new_to_old: &[u32],
    old_to_new: &[u32],
) -> Result<(), RefinementError> {
    let node_count = u32::try_from(graph.neighbors.len())
        .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
    let mut flags = Vec::with_capacity(new_to_old.len());
    let mut neighbors = Vec::with_capacity(new_to_old.len());
    for old in new_to_old {
        let old_index = node_index(*old, node_count)?;
        flags.push(*graph.flags.get(old_index).ok_or_else(|| {
            RefinementError::Geometry(format!("flags for old row {old} are unavailable"))
        })?);
        let remapped = graph
            .neighbors
            .get(old_index)
            .ok_or_else(|| {
                RefinementError::Geometry(format!("neighbors for old row {old} are unavailable"))
            })?
            .iter()
            .map(|neighbor| {
                old_to_new
                    .get(node_index(*neighbor, node_count)?)
                    .copied()
                    .ok_or_else(|| {
                        RefinementError::Geometry(format!(
                            "new id for old neighbor {neighbor} is unavailable"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        neighbors.push(remapped);
    }
    graph.flags = flags;
    graph.neighbors = neighbors;
    Ok(())
}

fn alpha_reprune(
    graph: &mut OwnedGraph,
    rescore: &[f32],
    dimensions: usize,
    node_count: u32,
    params: GraphParams,
) -> Result<(), RefinementError> {
    if graph.layout.max_degree() != params.r_max() {
        return Err(RefinementError::Geometry(format!(
            "graph max degree {}, profile max degree {}",
            graph.layout.max_degree(),
            params.r_max()
        )));
    }
    let source = graph.neighbors.clone();
    for owner in 0..node_count {
        let owner_index = node_index(owner, node_count)?;
        let own = source.get(owner_index).ok_or_else(|| {
            RefinementError::Geometry(format!("neighbors for node {owner} are unavailable"))
        })?;
        let mut candidates = own.clone();
        for neighbor in own {
            let neighbor_index = node_index(*neighbor, node_count)?;
            candidates.extend_from_slice(source.get(neighbor_index).ok_or_else(|| {
                RefinementError::Geometry(format!(
                    "neighbors for candidate {neighbor} are unavailable"
                ))
            })?);
        }
        let mut pruned = robust_prune_rows(
            rescore,
            dimensions,
            node_count,
            owner,
            &candidates,
            params.alpha_refine(),
            usize::from(params.r_target()),
        )
        .map_err(RefinementError::Graph)?;
        supplement_long_range(
            rescore,
            dimensions,
            owner,
            own,
            usize::from(params.r_max()),
            &mut pruned,
        )?;
        let destination = graph.neighbors.get_mut(owner_index).ok_or_else(|| {
            RefinementError::Geometry(format!("output neighbors for node {owner} are unavailable"))
        })?;
        *destination = pruned;
    }
    Ok(())
}

fn seed_refit(
    graph: &mut OwnedGraph,
    rescore: &[f32],
    dimensions: usize,
    node_count: u32,
) -> Result<(), RefinementError> {
    let mut medoid = None::<(u32, f64)>;
    for candidate in 0..node_count {
        let mut sum = 0.0_f64;
        for other in 0..node_count {
            sum += exact_distance(rescore, dimensions, node_count, candidate, other)?;
        }
        if medoid
            .is_none_or(|current| sum < current.1 || (sum == current.1 && candidate < current.0))
        {
            medoid = Some((candidate, sum));
        }
    }
    let Some((medoid, _)) = medoid else {
        return Err(RefinementError::Geometry(
            "seed refit requires at least one row".to_owned(),
        ));
    };
    let mut entries = vec![medoid];
    while entries.len() < ENTRY_POINT_COUNT && entries.len() < node_count as usize {
        let mut farthest = None::<(u32, f64)>;
        for candidate in 0..node_count {
            if entries.contains(&candidate) {
                continue;
            }
            let nearest = entries.iter().try_fold(f64::INFINITY, |nearest, entry| {
                exact_distance(rescore, dimensions, node_count, candidate, *entry)
                    .map(|distance| nearest.min(distance))
            })?;
            if farthest.is_none_or(|current| {
                nearest > current.1 || (nearest == current.1 && candidate < current.0)
            }) {
                farthest = Some((candidate, nearest));
            }
        }
        let Some((next, _)) = farthest else {
            break;
        };
        entries.push(next);
    }
    for (node, flags) in graph.flags.iter_mut().enumerate() {
        let node = u32::try_from(node)
            .map_err(|_| RefinementError::Geometry("entry row exceeds u32".to_owned()))?;
        *flags = (*flags & HUB_FLAG) | u8::from(entries.contains(&node));
    }
    Ok(())
}

fn neighbor_reorder(
    graph: &mut OwnedGraph,
    reader: &SegmentReader,
    node_count: u32,
    seed: u64,
) -> Result<(), RefinementError> {
    for owner in 0..node_count {
        let owner_index = node_index(owner, node_count)?;
        let neighbors = graph.neighbors.get_mut(owner_index).ok_or_else(|| {
            RefinementError::Geometry(format!("neighbors for node {owner} are unavailable"))
        })?;
        reorder_neighbors_bit4(reader, owner, neighbors, seed)?;
    }
    Ok(())
}

fn connectivity_repair(graph: &mut OwnedGraph, params: GraphParams) -> Result<(), RefinementError> {
    if graph.layout.max_degree() != params.r_max() {
        return Err(RefinementError::Geometry(format!(
            "graph max degree {}, profile max degree {}",
            graph.layout.max_degree(),
            params.r_max()
        )));
    }
    let node_count = u32::try_from(graph.neighbors.len())
        .map_err(|_| RefinementError::Geometry("graph rows exceed u32".to_owned()))?;
    let root = first_entry(graph)?;
    let mut seen = vec![0_u8; graph.neighbors.len()];
    let mut queue = VecDeque::with_capacity(graph.neighbors.len());
    mark_and_enqueue_byte(root, &mut seen, &mut queue)?;
    loop {
        while let Some(owner) = queue.pop_front() {
            let neighbors = graph
                .neighbors
                .get(node_index(owner, node_count)?)
                .ok_or_else(|| {
                    RefinementError::Geometry(format!(
                        "neighbors for repair node {owner} are unavailable"
                    ))
                })?
                .clone();
            for neighbor in neighbors {
                mark_and_enqueue_byte(neighbor, &mut seen, &mut queue)?;
            }
        }
        let Some(unseen) = seen.iter().position(|marker| *marker == 0) else {
            break;
        };
        let target = u32::try_from(unseen)
            .map_err(|_| RefinementError::Geometry("unreachable row exceeds u32".to_owned()))?;
        stitch_component(graph, &seen, target, params.r_max())?;
        mark_and_enqueue_byte(target, &mut seen, &mut queue)?;
    }
    for owner in 0..node_count {
        densify_hub(graph, owner, params.r_max())?;
    }
    Ok(())
}

fn reorder_neighbors_bit4(
    reader: &SegmentReader,
    owner: u32,
    neighbors: &mut Vec<u32>,
    seed: u64,
) -> Result<(), RefinementError> {
    let dimensions = reader.meta().dims as usize;
    let node_count = reader.meta().row_count;
    let query = exact_row(reader.rescore_f32()?, dimensions, node_count, owner)?;
    let query_norm_squared = query
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>();
    let prepared =
        prepare_bit4_query(query, seed ^ u64::from(owner)).map_err(GraphBuildError::Quant)?;
    let code_stride = dimensions.div_ceil(2);
    let codes = reader.bit4_codes()?;
    let factors = reader.bit4_factors()?;
    let mut scored = Vec::with_capacity(neighbors.len());
    for neighbor in neighbors.iter().copied() {
        let index = node_index(neighbor, node_count)?;
        let start = index
            .checked_mul(code_stride)
            .ok_or_else(|| RefinementError::Geometry("Bit4 row offset overflow".to_owned()))?;
        let end = start
            .checked_add(code_stride)
            .ok_or_else(|| RefinementError::Geometry("Bit4 row end overflow".to_owned()))?;
        let code = codes.get(start..end).ok_or_else(|| {
            RefinementError::Geometry(format!("Bit4 row {neighbor} is unavailable"))
        })?;
        let factor = factors.get(index).copied().ok_or_else(|| {
            RefinementError::Geometry(format!("Bit4 factors {neighbor} are unavailable"))
        })?;
        let dot = est_dot_bit4(&prepared, code, factor).map_err(GraphBuildError::Quant)?;
        let norm = factor.norm();
        let distance = (query_norm_squared + norm * norm - 2.0 * f64::from(dot)).max(0.0);
        scored.push((neighbor, distance));
    }
    scored.sort_unstable_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    neighbors.clear();
    neighbors.extend(scored.into_iter().map(|(neighbor, _)| neighbor));
    Ok(())
}

fn supplement_long_range(
    rescore: &[f32],
    dimensions: usize,
    owner: u32,
    neighbors: &[u32],
    maximum_degree: usize,
    selected: &mut Vec<u32>,
) -> Result<(), RefinementError> {
    let node_count = u32::try_from(rescore.len() / dimensions.max(1))
        .map_err(|_| RefinementError::Geometry("exact row count exceeds u32".to_owned()))?;
    let mut long_range = Vec::with_capacity(neighbors.len());
    for neighbor in neighbors {
        let distance = exact_distance(rescore, dimensions, node_count, owner, *neighbor)?;
        long_range.push((*neighbor, distance));
    }
    long_range.sort_unstable_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    for (neighbor, _) in long_range {
        if selected.len() >= maximum_degree {
            break;
        }
        if !selected.contains(&neighbor) {
            selected.push(neighbor);
        }
    }
    Ok(())
}

fn exact_distance(
    rescore: &[f32],
    dimensions: usize,
    node_count: u32,
    left: u32,
    right: u32,
) -> Result<f64, RefinementError> {
    let left = exact_row(rescore, dimensions, node_count, left)?;
    let right = exact_row(rescore, dimensions, node_count, right)?;
    Ok(left
        .iter()
        .zip(right)
        .map(|(left, right)| {
            let difference = f64::from(*left) - f64::from(*right);
            difference * difference
        })
        .sum())
}

fn exact_row(
    rescore: &[f32],
    dimensions: usize,
    node_count: u32,
    node: u32,
) -> Result<&[f32], RefinementError> {
    let index = node_index(node, node_count)?;
    let expected = (node_count as usize)
        .checked_mul(dimensions)
        .ok_or_else(|| RefinementError::Geometry("exact row length overflow".to_owned()))?;
    if dimensions == 0 || rescore.len() != expected {
        return Err(RefinementError::Geometry(format!(
            "exact rows contain {} values, expected {expected}",
            rescore.len()
        )));
    }
    let start = index
        .checked_mul(dimensions)
        .ok_or_else(|| RefinementError::Geometry("exact row offset overflow".to_owned()))?;
    let end = start
        .checked_add(dimensions)
        .ok_or_else(|| RefinementError::Geometry("exact row end overflow".to_owned()))?;
    rescore
        .get(start..end)
        .ok_or_else(|| RefinementError::Geometry(format!("exact row {node} is unavailable")))
}

fn node_index(node: u32, node_count: u32) -> Result<usize, RefinementError> {
    if node >= node_count {
        return Err(RefinementError::Geometry(format!(
            "node {node} is outside row count {node_count}"
        )));
    }
    usize::try_from(node).map_err(|_| RefinementError::Geometry("node exceeds usize".to_owned()))
}

fn encode_owned_graph(
    reader: &SegmentReader,
    graph: &OwnedGraph,
    new_to_old: Option<&[u32]>,
) -> Result<Vec<u8>, RefinementError> {
    let source = reader.graph_node_blocks()?;
    let node_count = source.node_count();
    let mut blocks = Vec::with_capacity(node_count as usize);
    for new_node in 0..node_count {
        let source_node = match new_to_old {
            Some(order) => *order
                .get(node_index(new_node, node_count)?)
                .ok_or_else(|| {
                    RefinementError::Geometry(format!(
                        "source row for new row {new_node} is missing"
                    ))
                })?,
            None => new_node,
        };
        blocks.push(source.block(source_node)?);
    }
    let mut nodes = Vec::with_capacity(node_count as usize);
    for new_node in 0..node_count {
        let index = node_index(new_node, node_count)?;
        let block = blocks.get(index).ok_or_else(|| {
            RefinementError::Geometry(format!("source block for new row {new_node} is missing"))
        })?;
        nodes.push(GraphNodeBlockInput {
            codes: block.codes(),
            factors: block.factors(),
            flags: *graph.flags.get(index).ok_or_else(|| {
                RefinementError::Geometry(format!("flags for new row {new_node} are missing"))
            })?,
            neighbors: graph.neighbors.get(index).ok_or_else(|| {
                RefinementError::Geometry(format!("neighbors for new row {new_node} are missing"))
            })?,
        });
    }
    encode_node_blocks_with_refinement_passes(
        GraphNodeBlockBuild {
            layout: graph.layout,
            nodes: &nodes,
        },
        graph.passes,
    )
    .map(|encoded| encoded.into_bytes())
    .map_err(RefinementError::NodeBlock)
}

fn refinement_intermediate_id(output_id: SegmentId) -> SegmentId {
    let first = xxh3_64_with_seed(output_id.as_bytes(), INTERMEDIATE_ID_SEED).to_be_bytes();
    let second = xxh3_64_with_seed(output_id.as_bytes(), !INTERMEDIATE_ID_SEED).to_be_bytes();
    let mut bytes = [0_u8; 16];
    if let Some(prefix) = bytes.get_mut(..8) {
        prefix.copy_from_slice(&first);
    }
    if let Some(suffix) = bytes.get_mut(8..) {
        suffix.copy_from_slice(&second);
    }
    SegmentId::from_bytes(bytes)
}

fn write_permuted_segment(
    vfs: &dyn Vfs,
    directory: &Path,
    input: &SegmentReader,
    new_to_old: &[u32],
    output_id: SegmentId,
    analyzer: &Analyzer,
    policy: DurabilityPolicy,
) -> Result<SegmentMeta, RefinementError> {
    for entry in input.directory() {
        if !crate::graph::consolidate::consolidation_supports_region(entry.kind) {
            return Err(RefinementError::Geometry(format!(
                "renumber cannot carry source region kind {}",
                entry.kind
            )));
        }
    }
    let rows = new_to_old
        .iter()
        .map(|row| usize::try_from(*row))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RefinementError::Geometry("permuted row exceeds usize".to_owned()))?;
    let dimensions = input.meta().dims as usize;
    let (codes, factors) =
        crate::ingest::purge_support::gather_survivor_codes(input, &rows, dimensions)?;
    let crate::ingest::purge_support::OwnedFactors::Bit4(factors) = factors else {
        return Err(RefinementError::Geometry(
            "renumbering requires Bit4 factors".to_owned(),
        ));
    };
    let rescore = crate::ingest::purge_support::gather_survivor_rescore(input, &rows, dimensions)?;
    let source_columns = input.columns()?;
    let mut columns = ColumnStoreBuilder::new(source_columns.schema().clone());
    crate::ingest::purge_support::append_survivor_columns(&mut columns, &source_columns, &rows)?;
    let columns = columns
        .finish()
        .map_err(|error| RefinementError::Segment(SegmentError::Columns(error.to_string())))?;
    let source_alive = input.alive()?;
    let mut alive = AliveSet::new(input.meta().row_count);
    for (new_row, old_row) in new_to_old.iter().copied().enumerate() {
        if !source_alive.is_alive(old_row) {
            alive
                .tombstone(u32::try_from(new_row).map_err(|_| {
                    RefinementError::Geometry("new alive row exceeds u32".to_owned())
                })?)
                .map_err(|_| RefinementError::Geometry("alive permutation failed".to_owned()))?;
        }
    }
    let documents_present = input
        .directory()
        .iter()
        .any(|entry| entry.kind == RegionKind::DocumentVersions.id());
    let (doc_ids, revisions) = if documents_present {
        crate::ingest::purge_support::gather_survivor_documents(input, &rows)?
    } else {
        (Vec::new(), Vec::new())
    };
    let metadata = gather_metadata(input, &rows)?;
    let text = gather_text(input, &rows)?;
    let postings = rebuild_postings(input, &rows, analyzer)?;
    if !documents_present && (metadata.is_some() || text.is_some() || postings.is_some()) {
        return Err(RefinementError::Geometry(
            "row payloads require document identities".to_owned(),
        ));
    }
    let build = SegmentBuild {
        id: output_id,
        scheme: input.meta().scheme,
        dims: input.meta().dims,
        codes: &codes,
        factors: SegmentFactors::Bit4(&factors),
        rescore: &rescore,
        columns: &columns,
        alive: &alive,
    };
    if documents_present {
        let documents = SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        };
        let metadata = metadata
            .as_ref()
            .map(|(end_offsets, bytes)| SegmentStoredMetadata { end_offsets, bytes });
        let text = text
            .as_ref()
            .map(|(present, end_offsets, bytes)| SegmentStoredText {
                present,
                end_offsets,
                bytes,
            });
        let postings = postings.as_deref().map(|bytes| SegmentPostings { bytes });
        write_segment_with_documents_payloads(
            vfs,
            directory,
            build,
            SegmentPayloads {
                documents,
                metadata,
                text,
                postings,
            },
            policy,
        )
        .map_err(RefinementError::Segment)
    } else {
        write_segment(vfs, directory, build, policy).map_err(RefinementError::Segment)
    }
}

type StoredMetadata = (Vec<u64>, Vec<u8>);

fn gather_metadata(
    input: &SegmentReader,
    rows: &[usize],
) -> Result<Option<StoredMetadata>, RefinementError> {
    let Some(source) = input.stored_metadata()? else {
        return Ok(None);
    };
    let mut end_offsets = Vec::with_capacity(rows.len());
    let mut bytes = Vec::new();
    for row in rows {
        bytes.extend_from_slice(source.row(*row).ok_or_else(|| {
            RefinementError::Geometry(format!("stored metadata row {row} is unavailable"))
        })?);
        end_offsets.push(u64::try_from(bytes.len()).map_err(|_| {
            RefinementError::Geometry("stored metadata bytes exceed u64".to_owned())
        })?);
    }
    Ok(Some((end_offsets, bytes)))
}

type StoredText = (Vec<u8>, Vec<u64>, Vec<u8>);

fn gather_text(
    input: &SegmentReader,
    rows: &[usize],
) -> Result<Option<StoredText>, RefinementError> {
    let Some(source) = input.stored_text()? else {
        return Ok(None);
    };
    let mut present = Vec::with_capacity(rows.len());
    let mut end_offsets = Vec::with_capacity(rows.len());
    let mut bytes = Vec::new();
    for row in rows {
        match source.row(*row).ok_or_else(|| {
            RefinementError::Geometry(format!("stored text row {row} is unavailable"))
        })? {
            Some(text) => {
                present.push(1);
                bytes.extend_from_slice(text.as_bytes());
            }
            None => present.push(0),
        }
        end_offsets.push(
            u64::try_from(bytes.len()).map_err(|_| {
                RefinementError::Geometry("stored text bytes exceed u64".to_owned())
            })?,
        );
    }
    Ok(Some((present, end_offsets, bytes)))
}

fn rebuild_postings(
    input: &SegmentReader,
    rows: &[usize],
    analyzer: &Analyzer,
) -> Result<Option<Vec<u8>>, RefinementError> {
    if input.postings()?.is_none() {
        return Ok(None);
    }
    let source = input.stored_text()?.ok_or_else(|| {
        RefinementError::Geometry("renumber cannot rebuild postings without stored text".to_owned())
    })?;
    let mut index = SegmentIndex::new();
    for row in rows {
        let text = source.row(*row).ok_or_else(|| {
            RefinementError::Geometry(format!("stored text row {row} is unavailable"))
        })?;
        let document = text.map_or_else(LexicalDocument::new, LexicalDocument::with_text);
        index
            .push_document(analyzer, &document)
            .map_err(|error| RefinementError::Geometry(error.to_string()))?;
    }
    SealedSegment::seal(&index)
        .map_err(|error| RefinementError::Geometry(error.to_string()))?
        .encode_region()
        .map(Some)
        .map_err(|error| RefinementError::Geometry(error.to_string()))
}
