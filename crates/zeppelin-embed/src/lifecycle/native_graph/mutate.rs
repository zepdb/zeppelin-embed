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
use crate::property_graph::query::completed::CompletedError;
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{
    NativeExecutionError, RuntimeContext, RuntimeError, RuntimeLimits, WorkCounters,
};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{
    GraphBatchReadView, ItemReceipt, StageError, StatementImages, WriteControl, WriteLimits,
    WriteMemory,
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
///
/// The view, the overlay and the statement image arena share one region `'w`:
/// an image built from the arena is borrowed for `'w`, which is exactly as long
/// as the overlay that retains it, and a pattern built over the view can hold
/// both. The arena's own lifetime `'i` stays separate because the arena is
/// interior-mutable, and therefore invariant, and has drop glue; tying it to
/// `'w` would require borrowing it for as long as it exists.
///
/// A consumer rejects with `E`. Most reject with the executor's own error; a
/// consumer that copies the statement's completed result also rejects with
/// that result's typed error, so it reports `NativeMutationError` directly.
pub(crate) trait NativeMutationConsumer<T, E = NativeExecutionError> {
    fn consume<'lease, 'm, 'g, 'w, 'i>(
        &mut self,
        view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        overlay: GraphBatchReadView<'w, 'static>,
        images: &'w StatementImages<'i>,
        control: &mut WriteControl<'_>,
    ) -> Result<(T, GraphBatchReadView<'w, 'static>), E>;
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
    /// Copying the statement's result rejected before commit: nothing was
    /// committed and no partial result exists.
    Completed(CompletedError),
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
            Self::Completed(error) => write!(formatter, "graph result: {error:?}"),
        }
    }
}

impl std::error::Error for NativeMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(error) => Some(error),
            Self::Execution(error) => Some(error),
            Self::Completed(_) => None,
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
        reason = "one admission carries its control, limits and five capacities"
    )]
    #[allow(
        clippy::result_large_err,
        reason = "keep typed graph errors allocation-free on failure paths"
    )]
    pub(crate) fn with_native_mutation<T, C: NativeMutationConsumer<T>>(
        &self,
        control: &crate::lifecycle::QueryControl,
        limits: RuntimeLimits,
        memory_limit: usize,
        source_slots: usize,
        lazy_targets: usize,
        overlay_capacity: usize,
        image_capacity: usize,
        consumer: C,
    ) -> Result<(T, NativeMutationReport), NativeMutationError> {
        match self.native_mutation_attempts(
            control,
            limits,
            memory_limit,
            source_slots,
            lazy_targets,
            overlay_capacity,
            image_capacity,
            consumer,
            |value, _, _| value,
        ) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error.into()),
            Err(error) => Err(error),
        }
    }

    /// `with_native_mutation`, whose consumer produces an `S` that is only
    /// complete once the commit tail has decided the statement's outcome.
    ///
    /// `settle` runs exactly once, for the attempt that is returned, after
    /// `commit_staged_batch` has committed it or found it a `NoOp`, and never
    /// for an attempt a checkpoint discarded. It receives the staged
    /// statement's per-entity receipts and the generation the commit actually
    /// published, `None` for a `NoOp`. It cannot fail: by the time it runs
    /// the write may be durable, so every fallible step belongs in `consume`.
    ///
    /// The consumer's own rejection `E` comes back unchanged, and every
    /// admission, staging or commit rejection is converted into `E`, so a
    /// statement driver with a richer error type keeps it.
    #[allow(
        clippy::too_many_arguments,
        reason = "one admission carries its control, limits, five capacities and its settle step"
    )]
    pub(crate) fn with_native_mutation_settled<S, T, E, C, F>(
        &self,
        control: &crate::lifecycle::QueryControl,
        limits: RuntimeLimits,
        memory_limit: usize,
        source_slots: usize,
        lazy_targets: usize,
        overlay_capacity: usize,
        image_capacity: usize,
        consumer: C,
        settle: F,
    ) -> Result<(T, NativeMutationReport), E>
    where
        C: NativeMutationConsumer<S, E>,
        E: From<NativeMutationError>,
        F: FnOnce(S, &[ItemReceipt], Option<GraphGeneration>) -> T,
    {
        self.native_mutation_attempts(
            control,
            limits,
            memory_limit,
            source_slots,
            lazy_targets,
            overlay_capacity,
            image_capacity,
            consumer,
            settle,
        )
        .unwrap_or_else(|error| Err(error.into()))
    }

    /// The attempt loop. The outer error is the admission's own; the inner
    /// one is the consumer's, exactly as the consumer returned it.
    #[allow(
        clippy::too_many_arguments,
        clippy::type_complexity,
        reason = "one admission carries its control, limits, five capacities and its settle step"
    )]
    #[allow(
        clippy::result_large_err,
        reason = "keep typed graph errors allocation-free on failure paths"
    )]
    fn native_mutation_attempts<S, T, E, C, F>(
        &self,
        control: &crate::lifecycle::QueryControl,
        limits: RuntimeLimits,
        memory_limit: usize,
        source_slots: usize,
        lazy_targets: usize,
        overlay_capacity: usize,
        image_capacity: usize,
        mut consumer: C,
        settle: F,
    ) -> Result<Result<(T, NativeMutationReport), E>, NativeMutationError>
    where
        C: NativeMutationConsumer<S, E>,
        F: FnOnce(S, &[ItemReceipt], Option<GraphGeneration>) -> T,
    {
        self.native_graph.require_writable()?;
        self.auto_maintain_native_graph(control)?;
        let mut allow_pending_checkpoint = true;
        loop {
            let mut writer_slot = self.native_graph.writer.lock().map_err(|_| {
                NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                    component: "native graph writer",
                })
            })?;
            let writer = writer_slot
                .as_mut()
                .ok_or_else(|| self.absent_native_graph_writer())?;
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
            // Declared before the overlay so it outlives the staged batch: a
            // replacement image, and any new symbol name it introduces, is
            // borrowed from this arena until `commit_staged_batch` returns.
            let images = StatementImages::new(
                &write_memory,
                admitted.document(),
                image_capacity,
                &mut write_control,
            )?;
            let overlay = GraphBatchReadView::new(
                &base,
                &write_memory,
                overlay_capacity,
                &mut write_control,
            )?;
            // The consumer reads the base through `CachedCanonical`, which
            // stashes the first storage error and hands its caller only an
            // opaque `io::Error`. The stash is therefore checked before the
            // `?` on the call it guards, exactly as the structured writer
            // does, so the typed root cause wins over the opaque rejection it
            // caused.
            let consumed =
                consumer.consume(&view, &mut runtime, overlay, &images, &mut write_control);
            if let Some(error) = base.take_error() {
                return Err(NativeGraphError::Stage(StageError::NativeStorage(error)).into());
            }
            let (value, overlay) = match consumed {
                Ok(consumed) => consumed,
                Err(error) => return Ok(Err(error)),
            };
            runtime.checkpoint().map_err(TreeError::Runtime)?;
            let counters = runtime.counters();
            // Finalization reads the base again, exactly as the structured
            // writer's staging does, so its storage errors are checked here
            // rather than inferred from a successful return.
            let staged = overlay.finish(&mut write_control);
            if let Some(error) = base.take_error() {
                return Err(NativeGraphError::Stage(StageError::NativeStorage(error)).into());
            }
            let staged = staged?;
            let disposition = staged.disposition();
            // A statement that only creates and deletes again its own
            // entities is `Changed` with no delta: it consumed identities
            // that must stay fenced, but native preparation derives the new
            // generation's roots from deltas and has none to publish. Refuse
            // it here, typed, before any private object is prepared.
            if disposition == BatchDisposition::Changed && staged.deltas().is_empty() {
                return Err(NativeGraphError::FenceOnlyStatement.into());
            }
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
            return Ok(Ok((
                settle(value, staged.receipts(), changed),
                NativeMutationReport {
                    disposition,
                    admitted: admitted_generation,
                    changed,
                    counters,
                },
            )));
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
