//! Query-driven mutation admission: one writer lease that carries both a live
//! read view and a writer overlay.
//!
//! `apply_native_graph` admits a writer lease for a caller-supplied list of
//! structured writes: every target is already named, so it needs no read view.
//! A Cypher statement discovers its targets from a MATCH clause at query time,
//! so it needs the read side of the same admission. `with_native_mutation`
//! opens both over one lease and one `NativeReadLease`-bound generation, hands
//! them to a consumer, and then reaches durability through the same
//! `commit_staged_batch` tail the structured path uses.

use super::NativeGraphError;
use super::base::NativeAdmittedBase;
use super::write::{CommitStep, commit_staged_batch};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{
    NativeExecutionError, RuntimeContext, RuntimeError, RuntimeLimits, WorkCounters,
};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{
    GraphBatchReadView, StageError, WriteControl, WriteLimits, WriteMemory,
};
use crate::property_graph::storage::memory::StorageMemory;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::storage::{
    GraphReadView, NativeCatalog, NativePreparationSource, NativeQuerySource, NativeReadCapability,
};
use crate::property_graph::{BatchDisposition, GraphGeneration};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// One statement's worth of work under an admitted writer lease.
///
/// The consumer receives the read view and runtime it needs to resolve MATCH
/// targets, plus the writer overlay it stages into. The overlay is handed over
/// by value and returned by value: an error therefore drops it, and no partial
/// statement can reach the commit tail.
pub(crate) trait NativeMutationConsumer<T> {
    fn consume<'s, 'lease, 'm, 'g, 'w>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        overlay: GraphBatchReadView<'w, 'static>,
        control: &mut WriteControl<'_>,
    ) -> Result<(T, GraphBatchReadView<'w, 'static>), NativeExecutionError>;
}

/// What one admitted mutation actually did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeMutationReport {
    /// Logical disposition of the staged batch before publication.
    pub(crate) disposition: BatchDisposition,
    /// The generation the read view and overlay were admitted against.
    pub(crate) admitted: GraphGeneration,
    /// The published generation, present only for a committed change.
    pub(crate) changed: Option<GraphGeneration>,
    /// Cumulative runtime work of the attempt that actually committed.
    pub(crate) counters: WorkCounters,
}

/// Admission and execution rejections stay distinct: a cancelled or
/// over-budget consumer is not a lifecycle failure.
#[derive(Debug)]
pub(crate) enum NativeMutationError {
    /// Writer admission, staging or publication rejected the statement.
    Graph(NativeGraphError),
    /// The consumer itself rejected before anything was staged for commit.
    Execution(NativeExecutionError),
}

impl From<NativeGraphError> for NativeMutationError {
    fn from(error: NativeGraphError) -> Self {
        Self::Graph(error)
    }
}

impl From<NativeExecutionError> for NativeMutationError {
    fn from(error: NativeExecutionError) -> Self {
        Self::Execution(error)
    }
}

impl From<StageError> for NativeMutationError {
    fn from(error: StageError) -> Self {
        Self::Graph(NativeGraphError::Stage(error))
    }
}

impl From<TreeError> for NativeMutationError {
    fn from(error: TreeError) -> Self {
        Self::Graph(NativeGraphError::Read(error))
    }
}

impl From<crate::lifecycle::StoreError> for NativeMutationError {
    fn from(error: crate::lifecycle::StoreError) -> Self {
        Self::Graph(NativeGraphError::Store(error))
    }
}

impl std::fmt::Display for NativeMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graph(error) => error.fmt(formatter),
            Self::Execution(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(error) => Some(error),
            Self::Execution(error) => Some(error),
        }
    }
}

impl crate::lifecycle::Store {
    /// Admits one writer lease that carries both a read view and a writer
    /// overlay, runs `consumer` under it, and commits whatever the consumer
    /// staged.
    ///
    /// The attempt is rebuilt from scratch whenever the commit tail
    /// checkpoints instead of committing: the lease, the read view, the
    /// overlay and any `T` the consumer already produced are all dropped, and
    /// `consumer` runs again against the new generation. A consumer must
    /// therefore be re-runnable; it is not told which attempt it is on.
    #[allow(
        clippy::too_many_arguments,
        reason = "one admission carries its control, limits and four capacities"
    )]
    pub(crate) fn with_native_mutation<T, C: NativeMutationConsumer<T>>(
        &self,
        control: &crate::lifecycle::QueryControl,
        limits: RuntimeLimits,
        memory_limit: usize,
        source_slots: usize,
        lazy_targets: usize,
        overlay_capacity: usize,
        mut consumer: C,
    ) -> Result<(T, NativeMutationReport), NativeMutationError> {
        self.native_graph.require_writable()?;
        let mut allow_pending_checkpoint = true;
        loop {
            let mut writer_slot = self.native_graph.writer.lock().map_err(|_| {
                NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                    component: "native graph writer",
                })
            })?;
            let writer = writer_slot
                .as_mut()
                .ok_or(NativeGraphError::Invalid("native graph writer is absent"))?;
            if writer.stopped {
                return Err(NativeGraphError::WritesStopped.into());
            }

            let lease = self.admit_native_read()?;
            let admitted = Arc::clone(lease.bundle());
            self.active_queries.fetch_add(1, Ordering::Relaxed);
            let _active_query = crate::lifecycle::ActiveQuery {
                count: &self.active_queries,
            };
            let shared = GraphResources::from_store(self)?;
            let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
            #[cfg(any(test, feature = "test-support"))]
            let (storage_limit, preparation_work) =
                crate::property_graph::storage::search::native_vector_index_test_limits(
                    32 * 1024 * 1024,
                    64 * 1024 * 1024,
                );
            #[cfg(not(any(test, feature = "test-support")))]
            let (storage_limit, preparation_work) = (32 * 1024 * 1024, 64 * 1024 * 1024);
            let storage = StorageMemory::new(&write_memory, control, storage_limit)?;
            let preparation_checkpoint = || match self.state() {
                Ok(crate::lifecycle::StoreState::Open) => Ok(()),
                Ok(
                    crate::lifecycle::StoreState::Closing | crate::lifecycle::StoreState::Closed,
                ) => Err(TreeError::Control(
                    crate::lifecycle::QueryError::ReadCancelled { partial: false },
                )),
                Err(error) => Err(TreeError::Control(crate::lifecycle::QueryError::Store(
                    error,
                ))),
            };
            let preparation = NativePreparationSource::new(&lease, &storage, 64)?;
            let mut base_resources = preparation
                .resources(preparation_work)?
                .with_preparation_checkpoint(&preparation_checkpoint)?;
            let resources_cell = RefCell::new(&mut base_resources);
            let first_storage_error = Cell::new(None);
            // No structured-write list names a MATCH target, so the base
            // preloads nothing and resolves every target lazily.
            let base = NativeAdmittedBase::with_lazy_targets(
                &lease,
                &preparation,
                &storage,
                &[],
                &resources_cell,
                &first_storage_error,
                lazy_targets,
            )?;

            let memory = QueryMemory::new(&shared, memory_limit)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
            let mut runtime = RuntimeContext::new(&lease, control, &memory, limits)
                .map_err(TreeError::Runtime)?;
            let capability = NativeReadCapability::admit(&lease, &runtime)?;
            let mut query_resources = TreeResources::for_query(&mut runtime)?;
            let query_source = NativeQuerySource::new(capability, &query_resources, source_slots)?;
            let catalog = NativeCatalog::open(&query_source, &mut query_resources)?;
            drop(query_resources);
            let view = GraphReadView::new(&query_source, &catalog)?;

            let mut write_control = |phase| checkpoint(control, phase);
            let overlay = GraphBatchReadView::new(
                &base,
                &write_memory,
                overlay_capacity,
                &mut write_control,
            )?;
            let (value, overlay) =
                consumer.consume(&view, &mut runtime, overlay, &mut write_control)?;
            runtime.checkpoint().map_err(TreeError::Runtime)?;
            if let Some(error) = base.take_error() {
                return Err(NativeGraphError::Stage(StageError::NativeStorage(error)).into());
            }
            let counters = runtime.counters();
            let staged = overlay.finish(&mut write_control)?;
            // Finalization reads the base again, exactly as the structured
            // writer's staging does, so its storage errors are checked here
            // rather than inferred from a successful return.
            if let Some(error) = base.take_error() {
                return Err(NativeGraphError::Stage(StageError::NativeStorage(error)).into());
            }
            let disposition = staged.disposition();
            let admitted_generation = admitted.base().generation;

            let step = commit_staged_batch(
                self,
                writer,
                &lease,
                &admitted,
                &shared,
                &storage,
                control,
                &base,
                &staged,
                &mut allow_pending_checkpoint,
            )?;
            let changed = match step {
                CommitStep::NoOp => None,
                CommitStep::Committed { generation, .. } => Some(generation),
                // Nothing committed. The whole attempt, including `value` and
                // every capacity it charged, is dropped and rebuilt against
                // the generation the checkpoint published.
                CommitStep::Checkpointed => continue,
            };
            return Ok((
                value,
                NativeMutationReport {
                    disposition,
                    admitted: admitted_generation,
                    changed,
                    counters,
                },
            ));
        }
    }
}

/// The same caller-control adapter the structured writer uses.
fn checkpoint(
    control: &crate::lifecycle::QueryControl,
    _: crate::property_graph::staging::WritePhase,
) -> Result<(), StageError> {
    control.checkpoint().map_err(|_| StageError::Cancelled)
}
