//! Host-invoked execution of due tier transitions.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::graph::GraphParamsError;
use crate::graph::build::{
    CheckpointedGraphBuild, GraphBuildError, GraphBuildPasses, build_graph_checkpointed,
};
use crate::lifecycle::{
    Deadline, DeadlineError, PublishedSnapshot, QueryControl, Store, StoreError,
};
use crate::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use crate::segment::SegmentId;
use crate::segment::layout::RegionKind;
use crate::segment::reader::SegmentReader;
use crate::vfs::StdVfs;

use super::SegmentTier;
use super::policy::{SegmentStats, StoreStats, TierPlan, decide};

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
    /// Every transition considered by this call is complete or not due.
    Complete,
    /// Work remains for a later call because this call spent its budget.
    BudgetExhausted,
    /// A typed failure prevented a transition from being published.
    Failed(MaintenanceError),
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
            Self::Deadline(error) => Some(error),
            Self::ArithmeticOverflow => None,
        }
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
    /// Final disposition of the call.
    pub status: MaintenanceStatus,
}

impl Store {
    /// Runs due tier transitions within a host-supplied work budget.
    #[must_use]
    pub fn maintain(&self, budget: MaintenanceBudget) -> MaintenanceReport {
        match maintain_one(self, budget) {
            Ok(report) => report,
            Err(error) => MaintenanceReport {
                graphs_built: 0,
                bytes_consumed: 0,
                checkpoints_resumed: 0,
                status: MaintenanceStatus::Failed(error),
            },
        }
    }
}

fn maintain_one(
    store: &Store,
    budget: MaintenanceBudget,
) -> Result<MaintenanceReport, MaintenanceError> {
    if budget.wall_time.is_zero() || budget.bytes == 0 {
        return Ok(MaintenanceReport {
            graphs_built: 0,
            bytes_consumed: 0,
            checkpoints_resumed: 0,
            status: MaintenanceStatus::BudgetExhausted,
        });
    }
    let _maintenance = store.maintenance.lock().map_err(|_| {
        MaintenanceError::Store(StoreError::Synchronization {
            component: "tier maintenance",
        })
    })?;
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
    let lease = store.snapshot().map_err(MaintenanceError::Store)?;
    let build_time = budget.wall_time.mul_f64(0.9);
    if build_time.is_zero() {
        return Ok(MaintenanceReport {
            graphs_built: 0,
            bytes_consumed: 0,
            checkpoints_resumed: 0,
            status: MaintenanceStatus::BudgetExhausted,
        });
    }
    // Keep ten percent of the host's wall budget for completed-artifact and
    // manifest publication after construction stops consulting the deadline.
    let deadline = Deadline::after(build_time).map_err(MaintenanceError::Deadline)?;
    let control = QueryControl::Deadline(deadline);
    let params = crate::graph::GraphParams::sift_1m()
        .with_checkpoint_batch_rows(MAINTENANCE_CHECKPOINT_ROWS as u32)
        .map_err(MaintenanceError::Parameters)?;
    let mut report = MaintenanceReport {
        graphs_built: 0,
        bytes_consumed: 0,
        checkpoints_resumed: 0,
        status: MaintenanceStatus::Complete,
    };
    for segment in lease
        .segments()
        .iter()
        .filter(|segment| transition_due(segment))
    {
        let remaining_bytes = budget.bytes.saturating_sub(report.bytes_consumed);
        let stride = graph_work_stride(segment)?;
        let maximum_rows = remaining_bytes / stride;
        let admitted_rows = if maximum_rows >= u64::from(segment.meta().row_count) {
            u64::from(segment.meta().row_count)
        } else {
            maximum_rows / MAINTENANCE_CHECKPOINT_ROWS * MAINTENANCE_CHECKPOINT_ROWS
        };
        if admitted_rows == 0 {
            report.status = MaintenanceStatus::BudgetExhausted;
            return Ok(report);
        }
        let checkpoint = checkpoint_path(&store.directory, segment.meta().id);
        let checkpoint_resumed = checkpoint.exists();
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
                &StdVfs,
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
        }
    }
    Ok(report)
}

fn transition_due(segment: &SegmentReader) -> bool {
    let actual = if has_graph(segment) {
        SegmentTier::SealedGraph
    } else {
        SegmentTier::SealedScan
    };
    matches!(
        decide(
            SegmentStats {
                tier: actual,
                row_count: segment.meta().row_count,
                dimensions: segment.meta().dims,
                scheme: segment.meta().scheme,
            },
            StoreStats,
        ),
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

fn graph_work_stride(segment: &SegmentReader) -> Result<u64, MaintenanceError> {
    let padded_dims = segment
        .meta()
        .dims
        .checked_add(127)
        .map(|dimensions| dimensions / 128 * 128)
        .ok_or(MaintenanceError::ArithmeticOverflow)?;
    let layout = crate::graph::block::GraphNodeLayout::new(
        segment.meta().dims,
        padded_dims,
        crate::graph::GraphParams::sift_1m().r_max(),
    )
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
    replacement: crate::segment::SegmentMeta,
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
    let mut manifest = load_manifest(&StdVfs, &manifest_path, durable_end)
        .map_err(StoreError::Manifest)
        .map_err(MaintenanceError::Store)?;
    let Some(slot) = manifest
        .segments
        .iter_mut()
        .find(|meta| meta.id == source_id)
    else {
        return Ok(false);
    };
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
    commit_manifest(
        &StdVfs,
        &store.directory,
        &manifest,
        store.durability_policy,
    )
    .map_err(StoreError::Manifest)
    .map_err(MaintenanceError::Store)?;
    let remapped = PublishedSnapshot::load(&store.directory, &store.accounting)
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
    Ok(true)
}
