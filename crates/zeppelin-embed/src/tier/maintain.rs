//! Host-invoked execution of due tier transitions.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::graph::GraphParamsError;
use crate::graph::build::{
    CheckpointedGraphBuild, GraphBuildError, GraphBuildPasses, GraphRewriteRegion,
    build_graph_checkpointed, graph_rewrite_source_region,
};
use crate::graph::consolidate::{
    ConsolidateError, ConsolidationCheckpoint, consolidation_merge_id, consolidation_output_id,
    consolidation_supports_region, merge_segments, merged_clustering,
    read_consolidation_checkpoint, remove_consolidation_checkpoint, write_consolidation_checkpoint,
};
use crate::graph::refine::{
    CheckpointedRefinement, RefinementError, RefinementPass, refine_graph_checkpointed,
};
use crate::lifecycle::{
    Deadline, DeadlineError, PublishedSnapshot, QueryCancellation, QueryControl, SnapshotLease,
    Store, StoreError,
};
use crate::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use crate::segment::layout::RegionKind;
use crate::segment::reader::SegmentReader;
use crate::segment::{ClusteringKeyRange, SegmentError, SegmentId};
use crate::vfs::Vfs;

use super::SegmentTier;
use super::TierThresholds;
use super::policy::{
    RefinementPlan, SegmentStats, StorePlan, StoreStats, TierPlan, decide, decide_refinement,
    decide_store, decide_with_thresholds,
};

const MAINTENANCE_SEED: u64 = 0x20_00c0_ffee;
const MAINTENANCE_CHECKPOINT_ROWS: u64 = 64;

/// Work admitted by one host call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaintenanceBudget {
    /// Maximum monotonic elapsed time for this call.
    pub wall_time: Duration,
    /// Maximum graph work bytes for this call.
    pub bytes: u64,
}

/// Why one maintenance call stopped.
#[derive(Debug)]
pub enum MaintenanceStatus {
    /// Every non-deferred transition considered by this call is complete or not due.
    Complete,
    /// Work remains for a later call because this call spent its budget.
    BudgetExhausted,
    /// A typed failure prevented maintenance from continuing.
    Failed(MaintenanceError),
}

/// One segment promotion refused because its source regions would not survive the rewrite.
#[derive(Debug, Eq, PartialEq)]
pub struct MaintenanceDeferral {
    /// Immutable source segment that was left unchanged.
    pub segment_id: SegmentId,
    /// Every source region the current graph rewrite cannot carry forward.
    pub uncarried_regions: Vec<UncarriedRegionKind>,
}

/// A source region kind the current graph rewrite cannot carry forward.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UncarriedRegionKind {
    /// A known or reserved region kind.
    Known(RegionKind),
    /// A forward-compatible region kind unknown to this engine version.
    Unknown(u16),
}

/// Typed maintenance failure retained in [`MaintenanceReport`].
#[derive(Debug)]
pub enum MaintenanceError {
    /// Store lifecycle, publication, or accounting failed.
    Store(StoreError),
    /// Graph parameters could not be constructed.
    Parameters(GraphParamsError),
    /// Checkpointed graph construction or segment writing failed.
    Graph(GraphBuildError),
    /// The consolidation merge pass or its checkpoint failed.
    Consolidate(ConsolidateError),
    /// The wall-time deadline was outside the monotonic clock range.
    Deadline(DeadlineError),
    /// Checked maintenance byte arithmetic overflowed.
    ArithmeticOverflow,
}

impl std::fmt::Display for MaintenanceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "tier maintenance store: {error}"),
            Self::Parameters(error) => write!(formatter, "tier maintenance parameters: {error}"),
            Self::Graph(error) => write!(formatter, "tier maintenance graph: {error}"),
            Self::Consolidate(error) => {
                write!(formatter, "tier maintenance consolidation: {error}")
            }
            Self::Deadline(error) => write!(formatter, "tier maintenance deadline: {error}"),
            Self::ArithmeticOverflow => {
                formatter.write_str("tier maintenance byte arithmetic overflow")
            }
        }
    }
}

impl std::error::Error for MaintenanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Parameters(error) => Some(error),
            Self::Graph(error) => Some(error),
            Self::Consolidate(error) => Some(error),
            Self::Deadline(error) => Some(error),
            Self::ArithmeticOverflow => None,
        }
    }
}

/// The epoch-selected profile used for one graph published by maintenance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphBuildProfileReport {
    /// Replacement graph segment atomically published by this call.
    pub segment_id: SegmentId,
    /// Persisted embedding and tokenizer epoch that selected the profile.
    pub epoch: crate::epoch::EpochIdentity,
    /// End-to-end build/query profile selected by the shared policy.
    pub profile: crate::graph::search::EpochGraphProfile,
}

/// Catalog-specific counts for post-consolidation refinement publications.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefinementPassCounters {
    /// Published Gorder-lite row-renumbering passes.
    pub renumber: u64,
    /// Published true-neighborhood alpha re-prunes.
    pub alpha_reprune: u64,
    /// Published exact-medoid entry-seed refits.
    pub seed_refit: u64,
    /// Published owner-distance neighbor reorders.
    pub neighbor_reorder: u64,
    /// Published angular connectivity repairs.
    pub connectivity_repair: u64,
}

impl RefinementPassCounters {
    fn increment(&mut self, pass: RefinementPass) -> Result<(), MaintenanceError> {
        let counter = match pass {
            RefinementPass::Renumber => &mut self.renumber,
            RefinementPass::AlphaReprune => &mut self.alpha_reprune,
            RefinementPass::SeedRefit => &mut self.seed_refit,
            RefinementPass::NeighborReorder => &mut self.neighbor_reorder,
            RefinementPass::ConnectivityRepair => &mut self.connectivity_repair,
        };
        *counter = counter
            .checked_add(1)
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        Ok(())
    }
}

/// Deterministic work counters returned by one maintenance call.
#[derive(Debug)]
pub struct MaintenanceReport {
    /// Graph generations atomically published by this call.
    pub graphs_built: u64,
    /// Graph construction work charged to the byte budget.
    pub bytes_consumed: u64,
    /// Checkpointed graph builds resumed by this call.
    pub checkpoints_resumed: u64,
    /// Due promotions refused because their source regions would not survive the rewrite.
    pub promotion_deferrals: Vec<MaintenanceDeferral>,
    /// Epoch-selected profile for every graph published by this call.
    pub graph_profiles: Vec<GraphBuildProfileReport>,
    /// N-to-1 graph-segment consolidations atomically published by this call.
    pub consolidations: u64,
    /// Manifest generation of the last consolidation published by this call.
    pub consolidation_generation: Option<u64>,
    /// Due consolidations refused because an input region cannot be carried.
    pub consolidation_deferrals: Vec<MaintenanceDeferral>,
    /// Post-consolidation refinement artifacts atomically published by this call.
    pub passes_applied: u64,
    /// Per-pass publication counts for this call.
    pub pass_counters: RefinementPassCounters,
    /// Manifest generation of the last refinement published by this call.
    pub refinement_generation: Option<u64>,
    /// Final disposition of the call.
    pub status: MaintenanceStatus,
}

impl Store {
    /// Runs due tier transitions within a host-supplied work budget.
    #[must_use]
    pub fn maintain(&self, budget: MaintenanceBudget) -> MaintenanceReport {
        self.maintain_with_optional_thresholds(budget, None)
    }

    /// Runs maintenance with an explicit reachability threshold for deterministic tests.
    #[doc(hidden)]
    #[must_use]
    pub fn maintain_with_test_thresholds(
        &self,
        budget: MaintenanceBudget,
        thresholds: TierThresholds,
    ) -> MaintenanceReport {
        self.maintain_with_optional_thresholds(budget, Some(thresholds))
    }

    fn maintain_with_optional_thresholds(
        &self,
        budget: MaintenanceBudget,
        thresholds: Option<TierThresholds>,
    ) -> MaintenanceReport {
        let report = match maintain_one(self, budget, thresholds) {
            Ok(report) => report,
            Err(error) => empty_report(MaintenanceStatus::Failed(error)),
        };
        if let Err(error) = self.record_maintenance(&report) {
            return MaintenanceReport {
                status: MaintenanceStatus::Failed(MaintenanceError::Store(error)),
                ..report
            };
        }
        report
    }
}

fn empty_report(status: MaintenanceStatus) -> MaintenanceReport {
    MaintenanceReport {
        graphs_built: 0,
        bytes_consumed: 0,
        checkpoints_resumed: 0,
        promotion_deferrals: Vec::new(),
        graph_profiles: Vec::new(),
        consolidations: 0,
        consolidation_generation: None,
        consolidation_deferrals: Vec::new(),
        passes_applied: 0,
        pass_counters: RefinementPassCounters::default(),
        refinement_generation: None,
        status,
    }
}

fn admit_maintenance_rows(
    segment: &SegmentReader,
    params: crate::graph::GraphParams,
    remaining_bytes: u64,
) -> Result<(u64, u64), MaintenanceError> {
    let stride = graph_work_stride(segment, params)?;
    let maximum_rows = remaining_bytes / stride;
    let admitted_rows = if maximum_rows >= u64::from(segment.meta().row_count) {
        u64::from(segment.meta().row_count)
    } else {
        maximum_rows / MAINTENANCE_CHECKPOINT_ROWS * MAINTENANCE_CHECKPOINT_ROWS
    };
    Ok((stride, admitted_rows))
}

fn probe_checkpoint_resume(
    vfs: &dyn Vfs,
    path: std::path::PathBuf,
) -> Result<(std::path::PathBuf, bool), MaintenanceError> {
    match vfs.open(&path) {
        Ok(_) => Ok((path, true)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((path, false)),
        Err(source) => Err(MaintenanceError::Graph(GraphBuildError::CheckpointIo {
            path,
            source,
        })),
    }
}

fn maintain_one(
    store: &Store,
    budget: MaintenanceBudget,
    thresholds: Option<TierThresholds>,
) -> Result<MaintenanceReport, MaintenanceError> {
    if budget.wall_time.is_zero() || budget.bytes == 0 {
        return Ok(empty_report(MaintenanceStatus::BudgetExhausted));
    }
    let _maintenance = store.maintenance.lock().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "tier maintenance",
        })
    })?;
    let lease = store.snapshot().map_err(MaintenanceError::Store)?;
    {
        let writer = store.writer_lock.lock().map_err(|_| {
            MaintenanceError::Store(StoreError::Synchronization {
                component: "writer lock",
            })
        })?;
        if writer.is_none() {
            return Err(MaintenanceError::Store(StoreError::ReadOnly));
        }
    }
    let build_time = budget.wall_time.mul_f64(0.9);
    if build_time.is_zero() {
        return Ok(empty_report(MaintenanceStatus::BudgetExhausted));
    }
    // Keep ten percent of the host's wall budget for completed-artifact and
    // manifest publication after construction stops consulting the deadline.
    let deadline = Deadline::after_with_clock(build_time, Arc::clone(&store.clock))
        .map_err(MaintenanceError::Deadline)?;
    let control = QueryControl::Deadline(deadline);
    let mut report = empty_report(MaintenanceStatus::Complete);
    let mut selected_profile = None;
    for segment in lease
        .segments()
        .iter()
        .filter(|segment| transition_due(segment, thresholds))
    {
        if let Some(deferral) = promotion_deferral(segment) {
            report.promotion_deferrals.push(deferral);
            continue;
        }
        let (epoch, profile) = match selected_profile {
            Some(selected) => selected,
            None => {
                let epoch = lease.epoch_alias().ok_or(MaintenanceError::Graph(
                    GraphBuildError::Profile(
                        crate::graph::search::GraphProfileError::EpochUnstamped,
                    ),
                ))?;
                let profile = lease
                    .graph_profile()
                    .map_err(GraphBuildError::Profile)
                    .map_err(MaintenanceError::Graph)?;
                selected_profile = Some((epoch, profile));
                (epoch, profile)
            }
        };
        let params = profile
            .build_params()
            .with_checkpoint_batch_rows(MAINTENANCE_CHECKPOINT_ROWS as u32)
            .map_err(MaintenanceError::Parameters)?;
        let remaining_bytes = budget.bytes.saturating_sub(report.bytes_consumed);
        let (stride, admitted_rows) = admit_maintenance_rows(segment, params, remaining_bytes)?;
        if admitted_rows == 0 {
            report.status = MaintenanceStatus::BudgetExhausted;
            return Ok(report);
        }
        let (checkpoint, checkpoint_resumed) = probe_checkpoint_resume(
            store.vfs.as_ref(),
            checkpoint_path(&store.directory, segment.meta().id),
        )?;
        report.checkpoints_resumed = report
            .checkpoints_resumed
            .checked_add(u64::from(checkpoint_resumed))
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        let artifact = match build_graph_checkpointed(
            store,
            segment,
            CheckpointedGraphBuild::new(
                params,
                MAINTENANCE_SEED,
                GraphBuildPasses::One,
                &checkpoint,
                &control,
            )
            .with_max_work_rows(admitted_rows),
            &lease,
        ) {
            Ok(artifact) => artifact,
            Err(GraphBuildError::BudgetExhausted { rows_completed }) => {
                let consumed = rows_completed
                    .checked_mul(stride)
                    .ok_or(MaintenanceError::ArithmeticOverflow)?;
                report.bytes_consumed = report
                    .bytes_consumed
                    .checked_add(consumed)
                    .ok_or(MaintenanceError::ArithmeticOverflow)?;
                report.status = MaintenanceStatus::BudgetExhausted;
                return Ok(report);
            }
            Err(GraphBuildError::Timeout { .. }) => {
                report.status = MaintenanceStatus::BudgetExhausted;
                return Ok(report);
            }
            Err(error) => return Err(MaintenanceError::Graph(error)),
        };
        let consumed = artifact
            .work_rows_completed()
            .checked_mul(stride)
            .and_then(|bytes| bytes.checked_add(crate::graph::block::NODE_BLOCK_TRAILER_LEN as u64))
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        report.bytes_consumed = report
            .bytes_consumed
            .checked_add(consumed)
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        let output_id = graph_segment_id(segment.meta().id, lease.generation());
        let meta = artifact
            .write_segment_with_graph(
                store.vfs.as_ref(),
                &store.directory,
                segment,
                output_id,
                store.durability_policy,
            )
            .map_err(MaintenanceError::Graph)?;
        if publish_transition(store, segment.meta().id, meta)? {
            report.graphs_built = report
                .graphs_built
                .checked_add(1)
                .ok_or(MaintenanceError::ArithmeticOverflow)?;
            report.graph_profiles.push(GraphBuildProfileReport {
                segment_id: output_id,
                epoch,
                profile,
            });
        }
    }
    drop(lease);
    maintain_consolidation(store, budget, &control, &mut report)?;
    if matches!(report.status, MaintenanceStatus::Complete) {
        maintain_refinements(store, budget, &control, &mut report)?;
    }
    Ok(report)
}

fn maintain_refinements(
    store: &Store,
    budget: MaintenanceBudget,
    control: &QueryControl,
    report: &mut MaintenanceReport,
) -> Result<(), MaintenanceError> {
    loop {
        let lease = store.snapshot().map_err(MaintenanceError::Store)?;
        let selected_profile = lease.graph_profile();
        let mut due = None;
        for segment in lease.segments().iter().filter(|segment| has_graph(segment)) {
            let graph = segment
                .graph_node_blocks()
                .map_err(StoreError::Segment)
                .map_err(MaintenanceError::Store)?;
            let pass = match decide_refinement(graph.refinement_passes()) {
                RefinementPlan::Apply(pass) => Some(pass),
                RefinementPlan::Stay
                    if selected_profile
                        == Ok(crate::graph::search::EpochGraphProfile::AngularClass)
                        && !graph
                            .refinement_passes()
                            .contains(RefinementPass::ConnectivityRepair) =>
                {
                    Some(RefinementPass::ConnectivityRepair)
                }
                RefinementPlan::Stay => None,
            };
            if let Some(pass) = pass {
                due = Some((segment, pass));
                break;
            }
        }
        let Some((segment, pass)) = due else {
            return Ok(());
        };
        let profile = selected_profile
            .map_err(GraphBuildError::Profile)
            .map_err(MaintenanceError::Graph)?;
        let params = profile
            .build_params()
            .with_checkpoint_batch_rows(MAINTENANCE_CHECKPOINT_ROWS as u32)
            .map_err(MaintenanceError::Parameters)?;
        let remaining = budget.bytes.saturating_sub(report.bytes_consumed);
        let (stride, total_work_rows) = refinement_work(segment, pass)?;
        let maximum_rows = remaining / stride;
        let admitted_rows = if maximum_rows >= total_work_rows {
            total_work_rows
        } else {
            maximum_rows / MAINTENANCE_CHECKPOINT_ROWS * MAINTENANCE_CHECKPOINT_ROWS
        };
        if admitted_rows == 0 {
            report.status = MaintenanceStatus::BudgetExhausted;
            return Ok(());
        }
        let checkpoint = refinement_checkpoint_path(&store.directory, segment.meta().id, pass);
        let (_, resumed) = probe_checkpoint_resume(store.vfs.as_ref(), checkpoint.clone())?;
        report.checkpoints_resumed = report
            .checkpoints_resumed
            .checked_add(u64::from(resumed))
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        let output_id = refinement_segment_id(segment.meta().id, lease.generation(), pass);
        let artifact = match refine_graph_checkpointed(
            store,
            segment,
            CheckpointedRefinement::new(
                pass,
                params,
                MAINTENANCE_SEED,
                lease.generation(),
                &checkpoint,
                control,
            )
            .with_max_work_rows(admitted_rows),
            &lease,
        ) {
            Ok(artifact) => artifact,
            Err(RefinementError::Graph(GraphBuildError::BudgetExhausted { rows_completed })) => {
                report.bytes_consumed = report
                    .bytes_consumed
                    .checked_add(
                        rows_completed
                            .checked_mul(stride)
                            .ok_or(MaintenanceError::ArithmeticOverflow)?,
                    )
                    .ok_or(MaintenanceError::ArithmeticOverflow)?;
                report.status = MaintenanceStatus::BudgetExhausted;
                return Ok(());
            }
            Err(RefinementError::Graph(GraphBuildError::Timeout { .. })) => {
                report.status = MaintenanceStatus::BudgetExhausted;
                return Ok(());
            }
            Err(error) => return Err(refinement_maintenance_error(error)),
        };
        let meta = artifact
            .write_segment(
                store.vfs.as_ref(),
                &store.directory,
                segment,
                output_id,
                &store.tokenizer,
                store.durability_policy,
            )
            .map_err(refinement_maintenance_error)?;
        let published = publish_transition(store, segment.meta().id, meta)?;
        drop(lease);
        if !published {
            let path = store.directory.join(output_id.file_name());
            match store.vfs.delete(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(MaintenanceError::Store(StoreError::Io { path, source }));
                }
            }
            return Ok(());
        }
        let consumed = artifact
            .work_rows_completed()
            .checked_mul(stride)
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        report.bytes_consumed = report
            .bytes_consumed
            .checked_add(consumed)
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        report.passes_applied = report
            .passes_applied
            .checked_add(1)
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        report.pass_counters.increment(pass)?;
        report.refinement_generation =
            Some(current_generation(store).map_err(MaintenanceError::Store)?);
    }
}

fn refinement_maintenance_error(error: RefinementError) -> MaintenanceError {
    match error {
        RefinementError::Graph(error) => MaintenanceError::Graph(error),
        RefinementError::NodeBlock(error) => {
            MaintenanceError::Graph(GraphBuildError::NodeBlock(error))
        }
        RefinementError::Segment(error) => MaintenanceError::Store(StoreError::Segment(error)),
        RefinementError::Store(error) => MaintenanceError::Store(error),
        error @ (RefinementError::AlreadyApplied(_) | RefinementError::Geometry(_)) => {
            MaintenanceError::Graph(GraphBuildError::Geometry(error.to_string()))
        }
    }
}

fn refinement_work(
    segment: &SegmentReader,
    pass: RefinementPass,
) -> Result<(u64, u64), MaintenanceError> {
    let node_count = u64::from(segment.meta().row_count);
    match pass {
        RefinementPass::Renumber => Ok((
            segment
                .meta()
                .file_size
                .checked_add(node_count.saturating_sub(1))
                .ok_or(MaintenanceError::ArithmeticOverflow)?
                / node_count.max(1),
            node_count,
        )),
        RefinementPass::SeedRefit => {
            let row_bytes = u64::from(segment.meta().dims)
                .checked_mul(std::mem::size_of::<f32>() as u64)
                .ok_or(MaintenanceError::ArithmeticOverflow)?;
            Ok((
                row_bytes
                    .checked_mul(node_count)
                    .ok_or(MaintenanceError::ArithmeticOverflow)?,
                node_count
                    .checked_mul(node_count.min(4))
                    .ok_or(MaintenanceError::ArithmeticOverflow)?,
            ))
        }
        RefinementPass::AlphaReprune | RefinementPass::NeighborReorder => {
            let graph = segment
                .graph_node_blocks()
                .map_err(StoreError::Segment)
                .map_err(MaintenanceError::Store)?;
            Ok((u64::from(graph.layout().stride()), node_count))
        }
        RefinementPass::ConnectivityRepair => {
            let graph = segment
                .graph_node_blocks()
                .map_err(StoreError::Segment)
                .map_err(MaintenanceError::Store)?;
            Ok((
                u64::from(graph.layout().stride()),
                node_count
                    .checked_mul(2)
                    .ok_or(MaintenanceError::ArithmeticOverflow)?,
            ))
        }
    }
}

fn refinement_checkpoint_path(
    directory: &Path,
    segment_id: SegmentId,
    pass: RefinementPass,
) -> std::path::PathBuf {
    directory.join(format!(
        ".tier-{segment_id}.{}.refine.checkpoint",
        pass.label()
    ))
}

fn refinement_segment_id(source: SegmentId, generation: u64, pass: RefinementPass) -> SegmentId {
    let mut bytes = [0_u8; 16];
    if let Some(prefix) = bytes.get_mut(..8) {
        prefix.copy_from_slice(&generation.saturating_add(1).to_be_bytes());
    }
    let hash = xxhash_rust::xxh3::xxh3_64_with_seed(source.as_bytes(), pass as u64).to_be_bytes();
    if let Some(suffix) = bytes.get_mut(8..) {
        suffix.copy_from_slice(&hash);
    }
    SegmentId::from_bytes(bytes)
}

fn current_generation(store: &Store) -> Result<u64, StoreError> {
    store.snapshot().map(|snapshot| snapshot.generation())
}

/// Runs the store-level N-to-1 graph consolidation when it is due.
///
/// The decision consults a fresh published snapshot so segments promoted by
/// this same call participate. A merge is admitted whole against the byte
/// budget; the graph phase resumes across calls through the existing graph
/// checkpoint. Nothing is published until one manifest commit replaces the
/// N input records with the single output record; the commit precedes every
/// unlink.
fn maintain_consolidation(
    store: &Store,
    budget: MaintenanceBudget,
    control: &QueryControl,
    report: &mut MaintenanceReport,
) -> Result<(), MaintenanceError> {
    let lease = store.snapshot().map_err(MaintenanceError::Store)?;
    let mut inputs = lease
        .segments()
        .iter()
        .filter(|segment| has_graph(segment))
        .collect::<Vec<_>>();
    inputs.sort_by(|left, right| left.meta().id.as_bytes().cmp(right.meta().id.as_bytes()));
    let stats = consolidation_store_stats(&inputs)?;
    if decide_store(stats) == StorePlan::Stay {
        clear_stale_consolidation(store)?;
        return Ok(());
    }
    let deferrals = consolidation_deferrals(&inputs);
    if !deferrals.is_empty() {
        report.consolidation_deferrals.extend(deferrals);
        return Ok(());
    }
    let input_ids = inputs
        .iter()
        .map(|segment| segment.meta().id)
        .collect::<Vec<_>>();
    let epoch_id = consolidation_epoch(&inputs)?;
    let merge_id = consolidation_merge_id(&input_ids);
    let output_id = consolidation_output_id(&input_ids);
    let resumed_merge = probe_consolidation_resume(store, &input_ids, merge_id, output_id)?;
    if resumed_merge {
        report.checkpoints_resumed = report
            .checkpoints_resumed
            .checked_add(1)
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
    } else {
        let merge_charge = inputs
            .iter()
            .try_fold(0_u64, |total, segment| {
                total.checked_add(segment.meta().file_size)
            })
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
        let remaining = budget.bytes.saturating_sub(report.bytes_consumed);
        if remaining < merge_charge {
            report.status = MaintenanceStatus::BudgetExhausted;
            return Ok(());
        }
        if !consolidation_may_continue(control, &lease)? {
            report.status = MaintenanceStatus::BudgetExhausted;
            return Ok(());
        }
        let Some(merged) = merge_segments(
            store.vfs.as_ref(),
            &store.directory,
            &inputs,
            merge_id,
            &store.tokenizer,
            store.durability_policy,
        )
        .map_err(MaintenanceError::Consolidate)?
        else {
            // Every input row is tombstoned; there is nothing to carry, and
            // dropping empty segments belongs to retention, not this path.
            return Ok(());
        };
        write_consolidation_checkpoint(
            store.vfs.as_ref(),
            &store.directory,
            &ConsolidationCheckpoint {
                inputs: input_ids.clone(),
                merge_id,
                output_id,
                rows_emitted: merged.live_rows,
                output_hash: merged.file_hash,
            },
            store.durability_policy,
        )
        .map_err(MaintenanceError::Consolidate)?;
        report.bytes_consumed = report
            .bytes_consumed
            .checked_add(merge_charge)
            .ok_or(MaintenanceError::ArithmeticOverflow)?;
    }
    let intermediate_path = store.directory.join(merge_id.file_name());
    let intermediate = SegmentReader::open(store.vfs.as_ref(), &intermediate_path, merge_id)
        .map_err(StoreError::Segment)
        .map_err(MaintenanceError::Store)?;
    let epoch = lease
        .epoch_alias()
        .ok_or(MaintenanceError::Graph(GraphBuildError::Profile(
            crate::graph::search::GraphProfileError::EpochUnstamped,
        )))?;
    let profile = lease
        .graph_profile()
        .map_err(GraphBuildError::Profile)
        .map_err(MaintenanceError::Graph)?;
    let params = profile
        .build_params()
        .with_checkpoint_batch_rows(MAINTENANCE_CHECKPOINT_ROWS as u32)
        .map_err(MaintenanceError::Parameters)?;
    let remaining = budget.bytes.saturating_sub(report.bytes_consumed);
    let (stride, admitted_rows) = admit_maintenance_rows(&intermediate, params, remaining)?;
    if admitted_rows == 0 {
        report.status = MaintenanceStatus::BudgetExhausted;
        return Ok(());
    }
    let (graph_checkpoint, graph_resumed) = probe_checkpoint_resume(
        store.vfs.as_ref(),
        checkpoint_path(&store.directory, merge_id),
    )?;
    report.checkpoints_resumed = report
        .checkpoints_resumed
        .checked_add(u64::from(graph_resumed))
        .ok_or(MaintenanceError::ArithmeticOverflow)?;
    let artifact = match build_graph_checkpointed(
        store,
        &intermediate,
        CheckpointedGraphBuild::new(
            params,
            MAINTENANCE_SEED,
            GraphBuildPasses::One,
            &graph_checkpoint,
            control,
        )
        .with_max_work_rows(admitted_rows),
        &lease,
    ) {
        Ok(artifact) => artifact,
        Err(GraphBuildError::BudgetExhausted { rows_completed }) => {
            let consumed = rows_completed
                .checked_mul(stride)
                .ok_or(MaintenanceError::ArithmeticOverflow)?;
            report.bytes_consumed = report
                .bytes_consumed
                .checked_add(consumed)
                .ok_or(MaintenanceError::ArithmeticOverflow)?;
            report.status = MaintenanceStatus::BudgetExhausted;
            return Ok(());
        }
        Err(GraphBuildError::Timeout { .. }) => {
            report.status = MaintenanceStatus::BudgetExhausted;
            return Ok(());
        }
        Err(error) => return Err(MaintenanceError::Graph(error)),
    };
    let consumed = artifact
        .work_rows_completed()
        .checked_mul(stride)
        .and_then(|bytes| bytes.checked_add(crate::graph::block::NODE_BLOCK_TRAILER_LEN as u64))
        .ok_or(MaintenanceError::ArithmeticOverflow)?;
    report.bytes_consumed = report
        .bytes_consumed
        .checked_add(consumed)
        .ok_or(MaintenanceError::ArithmeticOverflow)?;
    let clustering = consolidation_clustering(&intermediate)?;
    let meta = artifact
        .write_segment_with_graph(
            store.vfs.as_ref(),
            &store.directory,
            &intermediate,
            output_id,
            store.durability_policy,
        )
        .map_err(MaintenanceError::Graph)?;
    drop(intermediate);
    match publish_consolidation(store, &input_ids, meta, clustering, epoch_id, merge_id)? {
        Some(generation) => {
            report.consolidations = report
                .consolidations
                .checked_add(1)
                .ok_or(MaintenanceError::ArithmeticOverflow)?;
            report.consolidation_generation = Some(generation);
            report.graph_profiles.push(GraphBuildProfileReport {
                segment_id: output_id,
                epoch,
                profile,
            });
        }
        None => {
            // A concurrent mutation replaced or dropped an input after the
            // merge began; the prepared output is stale and is discarded
            // without publishing anything.
            discard_consolidation_artifacts(store, merge_id, Some(output_id))?;
        }
    }
    Ok(())
}

fn consolidation_store_stats(inputs: &[&SegmentReader]) -> Result<StoreStats, MaintenanceError> {
    let graph_segment_count =
        u32::try_from(inputs.len()).map_err(|_| MaintenanceError::ArithmeticOverflow)?;
    let total_graph_rows = inputs
        .iter()
        .try_fold(0_u64, |total, segment| {
            total.checked_add(u64::from(segment.meta().row_count))
        })
        .ok_or(MaintenanceError::ArithmeticOverflow)?;
    let largest_graph_segment_rows = inputs
        .iter()
        .map(|segment| u64::from(segment.meta().row_count))
        .max()
        .unwrap_or(0);
    Ok(StoreStats {
        graph_segment_count,
        total_graph_rows,
        largest_graph_segment_rows,
    })
}

/// Reports every input region the merge pass cannot carry to new row ids.
fn consolidation_deferrals(inputs: &[&SegmentReader]) -> Vec<MaintenanceDeferral> {
    let any_documents = inputs.iter().any(|segment| {
        segment
            .directory()
            .iter()
            .any(|entry| entry.kind == RegionKind::DocumentVersions.id())
    });
    let mut deferrals = Vec::new();
    for segment in inputs {
        let mut uncarried = segment
            .directory()
            .iter()
            .filter(|entry| !consolidation_supports_region(entry.kind))
            .map(|entry| {
                RegionKind::from_id(entry.kind).map_or(
                    UncarriedRegionKind::Unknown(entry.kind),
                    UncarriedRegionKind::Known,
                )
            })
            .collect::<Vec<_>>();
        let has_postings = segment
            .directory()
            .iter()
            .any(|entry| entry.kind == RegionKind::Postings.id());
        let has_text = segment
            .directory()
            .iter()
            .any(|entry| entry.kind == RegionKind::StoredText.id());
        let has_documents = segment
            .directory()
            .iter()
            .any(|entry| entry.kind == RegionKind::DocumentVersions.id());
        if has_postings && !has_text {
            // Postings cannot be copy-forwarded across a merge: row ids
            // change and document frequencies must be re-aggregated from
            // the stored text this segment does not carry.
            uncarried.push(UncarriedRegionKind::Known(RegionKind::Postings));
        }
        if any_documents && !has_documents {
            uncarried.push(UncarriedRegionKind::Known(RegionKind::DocumentVersions));
        }
        if !uncarried.is_empty() {
            deferrals.push(MaintenanceDeferral {
                segment_id: segment.meta().id,
                uncarried_regions: uncarried,
            });
        }
    }
    deferrals
}

fn consolidation_epoch(
    inputs: &[&SegmentReader],
) -> Result<Option<crate::epoch::EpochId>, MaintenanceError> {
    let mut epoch = None;
    for segment in inputs {
        let candidate = segment.meta().epoch_id;
        if epoch.is_some_and(|expected: Option<crate::epoch::EpochId>| expected != candidate) {
            return Err(MaintenanceError::Consolidate(ConsolidateError::Geometry(
                "consolidation inputs disagree on their embedding epoch".to_owned(),
            )));
        }
        epoch = Some(candidate);
    }
    Ok(epoch.flatten())
}

/// Returns whether a valid checkpoint lets this call skip the merge pass.
///
/// A checkpoint for a different input set, a corrupt checkpoint, or an
/// intermediate whose bytes no longer match the recorded hash is refused
/// and cleared so the next attempt restarts deterministically.
fn probe_consolidation_resume(
    store: &Store,
    input_ids: &[SegmentId],
    merge_id: SegmentId,
    output_id: SegmentId,
) -> Result<bool, MaintenanceError> {
    let checkpoint = match read_consolidation_checkpoint(store.vfs.as_ref(), &store.directory) {
        Ok(checkpoint) => checkpoint,
        Err(ConsolidateError::CheckpointCorrupt(_)) => {
            remove_consolidation_checkpoint(store.vfs.as_ref(), &store.directory)
                .map_err(MaintenanceError::Consolidate)?;
            return Ok(false);
        }
        Err(error) => return Err(MaintenanceError::Consolidate(error)),
    };
    let Some(checkpoint) = checkpoint else {
        return Ok(false);
    };
    if checkpoint.inputs != input_ids
        || checkpoint.merge_id != merge_id
        || checkpoint.output_id != output_id
    {
        clear_stale_consolidation(store)?;
        return Ok(false);
    }
    let intermediate_path = store.directory.join(merge_id.file_name());
    let matches = match store.vfs.read(&intermediate_path) {
        Ok(bytes) => xxhash_rust::xxh3::xxh3_64(&bytes) == checkpoint.output_hash,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(MaintenanceError::Store(StoreError::Io {
                path: intermediate_path,
                source,
            }));
        }
    };
    if !matches {
        clear_stale_consolidation(store)?;
        return Ok(false);
    }
    Ok(true)
}

/// Removes a checkpoint (and the artifacts it names) that no longer
/// matches the published graph segment set.
fn clear_stale_consolidation(store: &Store) -> Result<(), MaintenanceError> {
    let checkpoint = match read_consolidation_checkpoint(store.vfs.as_ref(), &store.directory) {
        Ok(None) => return Ok(()),
        Ok(Some(checkpoint)) => Some(checkpoint),
        Err(ConsolidateError::CheckpointCorrupt(_)) => None,
        Err(error) => return Err(MaintenanceError::Consolidate(error)),
    };
    if let Some(checkpoint) = checkpoint {
        discard_consolidation_artifacts(store, checkpoint.merge_id, None)?;
    }
    remove_consolidation_checkpoint(store.vfs.as_ref(), &store.directory)
        .map_err(MaintenanceError::Consolidate)
}

/// Deletes the unpublished intermediate, its graph checkpoint, the
/// consolidation checkpoint, and optionally an unpublished output segment.
fn discard_consolidation_artifacts(
    store: &Store,
    merge_id: SegmentId,
    output_id: Option<SegmentId>,
) -> Result<(), MaintenanceError> {
    let mut paths = vec![
        store.directory.join(merge_id.file_name()),
        checkpoint_path(&store.directory, merge_id),
    ];
    if let Some(output_id) = output_id {
        paths.push(store.directory.join(output_id.file_name()));
    }
    for path in paths {
        match store.vfs.delete(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(MaintenanceError::Store(StoreError::Io { path, source }));
            }
        }
    }
    remove_consolidation_checkpoint(store.vfs.as_ref(), &store.directory)
        .map_err(MaintenanceError::Consolidate)
}

fn consolidation_clustering(
    intermediate: &SegmentReader,
) -> Result<ClusteringKeyRange, MaintenanceError> {
    let columns = intermediate
        .columns()
        .map_err(StoreError::Segment)
        .map_err(MaintenanceError::Store)?;
    let alive = intermediate
        .alive()
        .map_err(StoreError::Segment)
        .map_err(MaintenanceError::Store)?;
    merged_clustering(&columns, &alive).map_err(MaintenanceError::Consolidate)
}

/// Maps the shared cancellation seam to the maintenance disposition:
/// deadline expiry stops cleanly, cancellation is a typed failure.
fn consolidation_may_continue(
    control: &QueryControl,
    lease: &SnapshotLease,
) -> Result<bool, MaintenanceError> {
    match QueryCancellation::new(control, lease).check_graph() {
        Ok(()) => Ok(true),
        Err(crate::scan::ScanError::Timeout { .. }) => Ok(false),
        Err(crate::scan::ScanError::Cancelled { .. }) => {
            Err(MaintenanceError::Graph(GraphBuildError::Cancelled {
                partial: false,
            }))
        }
        Err(crate::scan::ScanError::ReadCancelled { .. }) => {
            Err(MaintenanceError::Graph(GraphBuildError::ReadCancelled {
                partial: false,
            }))
        }
        Err(error) => Err(MaintenanceError::Consolidate(ConsolidateError::Geometry(
            format!("cancellation seam returned non-cancellation error: {error}"),
        ))),
    }
}

/// Commits one manifest replacing the N input records with the output
/// record, publishes the snapshot, and only then unlinks the inputs, the
/// intermediate, and the consolidation checkpoint.
///
/// Returns the generation the mutation changed, or `None` when any input
/// is no longer in the manifest (a concurrent mutation superseded it).
fn publish_consolidation(
    store: &Store,
    input_ids: &[SegmentId],
    mut replacement: crate::segment::SegmentMeta,
    clustering: ClusteringKeyRange,
    epoch_id: Option<crate::epoch::EpochId>,
    merge_id: SegmentId,
) -> Result<Option<u64>, MaintenanceError> {
    let state = store
        .state
        .lock()
        .map_err(|_| MaintenanceError::Store(StoreError::Synchronization { component: "state" }))?;
    match *state {
        crate::lifecycle::StoreState::Open => {}
        crate::lifecycle::StoreState::Closing => {
            return Err(MaintenanceError::Store(StoreError::Closing));
        }
        crate::lifecycle::StoreState::Closed => {
            return Err(MaintenanceError::Store(StoreError::Closed));
        }
    }
    let writer = store.writer_lock.lock().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "writer lock",
        })
    })?;
    if writer.is_none() {
        return Err(MaintenanceError::Store(StoreError::ReadOnly));
    }
    let wal = store.wal_writer.lock().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "WAL writer",
        })
    })?;
    let durable_end = wal
        .as_ref()
        .ok_or(MaintenanceError::Store(StoreError::ReadOnly))?
        .durable_end();
    let manifest_path = store.directory.join(MANIFEST_FILE);
    let mut manifest = load_manifest(store.vfs.as_ref(), &manifest_path, durable_end)
        .map_err(StoreError::Manifest)
        .map_err(MaintenanceError::Store)?;
    let all_present = input_ids
        .iter()
        .all(|input| manifest.segments.iter().any(|segment| segment.id == *input));
    if !all_present {
        return Ok(None);
    }
    replacement.clustering_key_range = clustering;
    replacement.epoch_id = epoch_id;
    let mut segments = Vec::with_capacity(manifest.segments.len());
    let mut replaced = false;
    for segment in manifest.segments.drain(..) {
        if input_ids.contains(&segment.id) {
            if !replaced {
                segments.push(replacement.clone());
                replaced = true;
            }
        } else {
            segments.push(segment);
        }
    }
    if !replaced {
        return Ok(None);
    }
    manifest.segments = segments;
    let mut active = store.active.lock().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "active segment",
        })
    })?;
    let active_state = active
        .as_mut()
        .ok_or(MaintenanceError::Store(StoreError::Closed))?;
    manifest.generation = manifest
        .generation
        .max(active_state.generation)
        .checked_add(1)
        .ok_or(MaintenanceError::Store(StoreError::GenerationOverflow))?;
    manifest.epochs = store.epoch_registry(&manifest.epochs);
    commit_manifest(
        store.vfs.as_ref(),
        &store.directory,
        &manifest,
        store.durability_policy,
    )
    .map_err(StoreError::Manifest)
    .map_err(MaintenanceError::Store)?;
    let remapped =
        PublishedSnapshot::load_on_vfs(&store.directory, &store.accounting, store.vfs.as_ref())
            .map_err(MaintenanceError::Store)?;
    let mut published = store.snapshot.write().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "published snapshot",
        })
    })?;
    let previous = published.replace(Arc::new(remapped));
    active_state.generation = manifest.generation;
    let generation = manifest.generation;
    drop(published);
    drop(previous);
    drop(active);
    drop(wal);
    drop(writer);
    drop(state);
    for input in input_ids {
        let path = store.directory.join(input.file_name());
        store
            .vfs
            .delete(&path)
            .map_err(|error| SegmentError::io(&path, error))
            .map_err(StoreError::Segment)
            .map_err(MaintenanceError::Store)?;
    }
    discard_consolidation_artifacts(store, merge_id, None)?;
    Ok(Some(generation))
}

fn promotion_deferral(segment: &SegmentReader) -> Option<MaintenanceDeferral> {
    let uncarried_regions = segment
        .directory()
        .iter()
        .filter(|entry| graph_rewrite_source_region(entry.kind) == GraphRewriteRegion::Unsupported)
        .map(|entry| {
            RegionKind::from_id(entry.kind).map_or(
                UncarriedRegionKind::Unknown(entry.kind),
                UncarriedRegionKind::Known,
            )
        })
        .collect::<Vec<_>>();
    (!uncarried_regions.is_empty()).then_some(MaintenanceDeferral {
        segment_id: segment.meta().id,
        uncarried_regions,
    })
}

fn transition_due(segment: &SegmentReader, thresholds: Option<TierThresholds>) -> bool {
    let actual = if has_graph(segment) {
        SegmentTier::SealedGraph
    } else {
        SegmentTier::SealedScan
    };
    let stats = SegmentStats {
        tier: actual,
        row_count: segment.meta().row_count,
        dimensions: segment.meta().dims,
        scheme: segment.meta().scheme,
    };
    let plan = thresholds.map_or_else(
        || decide(stats, StoreStats::default()),
        |thresholds| decide_with_thresholds(stats, thresholds),
    );
    matches!(
        plan,
        TierPlan::Transition {
            from: SegmentTier::SealedScan,
            to: SegmentTier::SealedGraph,
        }
    )
}

fn has_graph(segment: &SegmentReader) -> bool {
    segment
        .directory()
        .iter()
        .any(|entry| entry.kind == RegionKind::GraphNodeBlocks.id())
}

fn graph_work_stride(
    segment: &SegmentReader,
    params: crate::graph::GraphParams,
) -> Result<u64, MaintenanceError> {
    let padded_dims = segment
        .meta()
        .dims
        .checked_add(127)
        .map(|dimensions| dimensions / 128 * 128)
        .ok_or(MaintenanceError::ArithmeticOverflow)?;
    let layout =
        crate::graph::block::GraphNodeLayout::new(segment.meta().dims, padded_dims, params.r_max())
            .map_err(|error| MaintenanceError::Graph(GraphBuildError::NodeBlock(error)))?;
    Ok(u64::from(layout.stride()))
}

fn checkpoint_path(directory: &Path, segment_id: SegmentId) -> std::path::PathBuf {
    directory.join(format!(".tier-{segment_id}.graph.checkpoint"))
}

fn graph_segment_id(source: SegmentId, generation: u64) -> SegmentId {
    let mut bytes = [0_u8; 16];
    if let Some(prefix) = bytes.get_mut(..8) {
        prefix.copy_from_slice(&generation.saturating_add(1).to_be_bytes());
    }
    let hash = xxhash_rust::xxh3::xxh3_64(source.as_bytes()).to_be_bytes();
    if let Some(suffix) = bytes.get_mut(8..) {
        suffix.copy_from_slice(&hash);
    }
    SegmentId::from_bytes(bytes)
}

fn publish_transition(
    store: &Store,
    source_id: SegmentId,
    mut replacement: crate::segment::SegmentMeta,
) -> Result<bool, MaintenanceError> {
    let state = store
        .state
        .lock()
        .map_err(|_| MaintenanceError::Store(StoreError::Synchronization { component: "state" }))?;
    match *state {
        crate::lifecycle::StoreState::Open => {}
        crate::lifecycle::StoreState::Closing => {
            return Err(MaintenanceError::Store(StoreError::Closing));
        }
        crate::lifecycle::StoreState::Closed => {
            return Err(MaintenanceError::Store(StoreError::Closed));
        }
    }
    let writer = store.writer_lock.lock().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "writer lock",
        })
    })?;
    if writer.is_none() {
        return Err(MaintenanceError::Store(StoreError::ReadOnly));
    }
    let wal = store.wal_writer.lock().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "WAL writer",
        })
    })?;
    let durable_end = wal
        .as_ref()
        .ok_or(MaintenanceError::Store(StoreError::ReadOnly))?
        .durable_end();
    let manifest_path = store.directory.join(MANIFEST_FILE);
    let mut manifest = load_manifest(store.vfs.as_ref(), &manifest_path, durable_end)
        .map_err(StoreError::Manifest)
        .map_err(MaintenanceError::Store)?;
    let Some(slot) = manifest
        .segments
        .iter_mut()
        .find(|meta| meta.id == source_id)
    else {
        return Ok(false);
    };
    replacement.clustering_key_range = slot.clustering_key_range;
    replacement.epoch_id = slot.epoch_id;
    *slot = replacement;
    let mut active = store.active.lock().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "active segment",
        })
    })?;
    let active_state = active
        .as_mut()
        .ok_or(MaintenanceError::Store(StoreError::Closed))?;
    manifest.generation = manifest
        .generation
        .max(active_state.generation)
        .checked_add(1)
        .ok_or(MaintenanceError::Store(StoreError::GenerationOverflow))?;
    manifest.epochs = store.epoch_registry(&manifest.epochs);
    commit_manifest(
        store.vfs.as_ref(),
        &store.directory,
        &manifest,
        store.durability_policy,
    )
    .map_err(StoreError::Manifest)
    .map_err(MaintenanceError::Store)?;
    let remapped =
        PublishedSnapshot::load_on_vfs(&store.directory, &store.accounting, store.vfs.as_ref())
            .map_err(MaintenanceError::Store)?;
    let mut published = store.snapshot.write().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "published snapshot",
        })
    })?;
    let previous = published.replace(Arc::new(remapped));
    active_state.generation = manifest.generation;
    drop(published);
    drop(previous);
    drop(active);
    drop(wal);
    drop(writer);
    drop(state);
    let source_path = store.directory.join(source_id.file_name());
    store
        .vfs
        .delete(&source_path)
        .map_err(|error| SegmentError::io(&source_path, error))
        .map_err(StoreError::Segment)
        .map_err(MaintenanceError::Store)?;
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        MaintenanceBudget, MaintenanceError, MaintenanceStatus, checkpoint_path, graph_segment_id,
    };
    use crate::graph::search::GraphSearchProfile;
    use crate::graph::{GraphParams, build::GraphBuildError};
    use crate::lifecycle::{
        CancelToken, DeadlineError, InMemorySegment, InMemorySegmentFactors, OpenOptions,
        QueryControl, SearchOptions, Store, StoreError,
    };
    use crate::meta::{AliveSet, ColumnStoreBuilder, Schema};
    use crate::quant::quantize_bit4;
    use crate::segment::SegmentId;
    use crate::tier::TierThresholds;
    use std::time::Duration;

    #[test]
    fn maintenance_budget_lifecycle_and_identity_fail_closed() {
        let directory = tempfile::tempdir().expect("maintenance test directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open writer");

        for budget in [
            MaintenanceBudget {
                wall_time: Duration::ZERO,
                bytes: u64::MAX,
            },
            MaintenanceBudget {
                wall_time: Duration::from_secs(1),
                bytes: 0,
            },
        ] {
            let report = store.maintain(budget);
            assert_eq!(report.graphs_built, 0);
            assert_eq!(report.bytes_consumed, 0);
            assert_eq!(report.checkpoints_resumed, 0);
            assert!(matches!(report.status, MaintenanceStatus::BudgetExhausted));
        }

        let source = SegmentId::new(19, [7; 10]);
        let first = graph_segment_id(source, 41);
        assert_eq!(first, graph_segment_id(source, 41));
        assert_ne!(first, graph_segment_id(source, 42));
        assert_eq!(
            checkpoint_path(directory.path(), source),
            directory
                .path()
                .join(format!(".tier-{source}.graph.checkpoint"))
        );

        store.close().expect("close writer");
        let closed = store.maintain(MaintenanceBudget {
            wall_time: Duration::from_secs(1),
            bytes: 1,
        });
        assert!(matches!(
            closed.status,
            MaintenanceStatus::Failed(MaintenanceError::Store(StoreError::Closed))
        ));

        let read_only =
            Store::open(directory.path(), OpenOptions::read_only()).expect("open read-only store");
        let refused = read_only.maintain(MaintenanceBudget {
            wall_time: Duration::from_secs(1),
            bytes: 1,
        });
        assert!(matches!(
            refused.status,
            MaintenanceStatus::Failed(MaintenanceError::Store(StoreError::ReadOnly))
        ));
    }

    #[test]
    fn maintenance_errors_preserve_typed_sources_and_messages() {
        let parameter =
            GraphParams::new(0, 1, 1.0, 1.0, 1, 1).expect_err("zero target degree is invalid");
        let errors = [
            MaintenanceError::Store(StoreError::Closed),
            MaintenanceError::Parameters(parameter),
            MaintenanceError::Graph(GraphBuildError::Cancelled { partial: false }),
            MaintenanceError::Consolidate(crate::graph::consolidate::ConsolidateError::Geometry(
                "probe".to_owned(),
            )),
            MaintenanceError::Deadline(DeadlineError::OutOfRange),
            MaintenanceError::ArithmeticOverflow,
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
            assert_eq!(
                std::error::Error::source(&error).is_some(),
                !matches!(error, MaintenanceError::ArithmeticOverflow)
            );
        }
    }

    #[test]
    fn maintain_builds_with_the_same_profile_auto_query_selects() {
        let directory = tempfile::tempdir().expect("profile parity directory");
        let epoch = sift_epoch();
        let store = Store::open(
            directory.path(),
            OpenOptions::default().with_epoch(epoch.clone()),
        )
        .expect("open profile parity store");
        let vectors = (0..4)
            .flat_map(|row| std::iter::repeat_n(row as f32, 128))
            .collect::<Vec<_>>();
        let mut codes = vec![0_u8; 4 * 64];
        let factors = vectors
            .chunks_exact(128)
            .zip(codes.chunks_exact_mut(64))
            .map(|(row, encoded)| quantize_bit4(row, encoded))
            .collect::<Result<Vec<_>, _>>()
            .expect("quantize profile parity rows");
        let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
        let mut columns = ColumnStoreBuilder::new(schema);
        for row in 0..4 {
            columns
                .push_row(row, &[])
                .expect("profile parity column row");
        }
        let columns = columns.finish().expect("profile parity columns");
        let alive = AliveSet::new(4);
        let prepared = store
            .prepare_segment(InMemorySegment {
                id: SegmentId::new(0x0016, [0x16; 10]),
                scheme: 4,
                dims: 128,
                codes,
                factors: InMemorySegmentFactors::Bit4(factors),
                rescore: vectors,
                columns: &columns,
                alive: &alive,
            })
            .expect("prepare profile parity segment");
        store
            .seal_snapshot(prepared)
            .expect("publish profile parity segment");

        let maintenance = store.maintain_with_test_thresholds(
            MaintenanceBudget {
                wall_time: Duration::from_secs(30),
                bytes: u64::MAX,
            },
            TierThresholds { graph_min_rows: 1 },
        );
        assert!(matches!(maintenance.status, MaintenanceStatus::Complete));
        assert_eq!(maintenance.graphs_built, 1);
        let built = maintenance
            .graph_profiles
            .first()
            .expect("published graph profile report");

        let outcome = store
            .search(
                crate::ingest::SearchRequest::new(&[0.0_f32; 128]),
                1,
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("Auto query after profile-selected build");
        let queried = outcome
            .diagnostics
            .plan
            .first()
            .and_then(|plan| plan.graph_profile)
            .expect("Auto query graph profile report");

        assert_eq!(built.epoch, epoch.identity());
        assert_eq!(built.profile.search_profile(), queried);
        assert_eq!(queried, GraphSearchProfile::SiftClass);
        store.close().expect("close profile parity store");
    }

    fn sift_epoch() -> crate::epoch::StoreEpoch {
        let document = crate::epoch::EmbeddingTower {
            model_id: "sift-profile-fixture".to_owned(),
            model_version: "1".to_owned(),
            weights_digest: vec![0x16],
            dims: 128,
            normalization: crate::epoch::Normalization::None,
            prompt_prefix: String::new(),
            max_tokens: 512,
            runtime: crate::epoch::EmbeddingRuntime::CpuReference,
            compute_units: crate::epoch::ComputeUnits::Cpu,
            os_build: None,
        };
        crate::epoch::StoreEpoch {
            embedding: crate::epoch::EmbeddingEpoch {
                query: document.clone(),
                document,
                alignment_digest: Vec::new(),
            },
            tokenizer: crate::fts::tokenizer::TokenizerConfig::text_default().epoch(),
        }
    }
}
