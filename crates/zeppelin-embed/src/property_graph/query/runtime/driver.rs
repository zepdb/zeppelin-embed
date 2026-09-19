use super::{
    ArenaCapacity, RetainedView, RowBatch, RuntimeContext, RuntimeError, RuntimeLimits,
    WorkCounters, WorkKind,
};
use crate::lifecycle::QueryControl;
use crate::property_graph::query::{
    QueryValue,
    plan::PlanNodeId,
    resources::{QueryMemory, RuntimePlan},
};

/// A bounded pull may finish with its last nonempty batch. Empty More rejects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullState {
    /// Further bounded work remains.
    More,
    /// All row-producing work has finished.
    Done,
}
/// Required internal physical-operator adapter, not an application callback.
/// Implementations charge examined/discarded work and all their owned storage
/// against this context. They cannot publish results or durable writes here.
pub trait PullOperator<'v, 'm, 'g> {
    /// Typed operator identity for plan matching and failure diagnostics.
    fn node(&self) -> PlanNodeId;
    /// Executes and retains one eager search report before any row pulling.
    /// The driver invokes each validated obligation exactly once in source order.
    fn prepare_search(
        &mut self,
        node: PlanNodeId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError>;
    /// Fills a fixed flat batch using the same view, control and memory owner.
    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, RuntimeError>;
}
/// Builds a retained operator chain inside the owned execution view. All buffer
/// capacities belong to the supplied context; construction is fallible and
/// precedes eager preparation, pulling and completion. The associated operator
/// cannot escape the driver's local view lifetime.
///
/// A factory cannot retain the fresh view in longer-lived state:
/// ```compile_fail
/// use zeppelin_embed::property_graph::query::{QueryView, plan::PlanNodeId, runtime::*};
/// struct Noop;
/// impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for Noop {
///     fn node(&self) -> PlanNodeId { PlanNodeId(0) }
///     fn prepare_search(&mut self, _: PlanNodeId, _: &mut RuntimeContext<'v, 'm, 'g>) -> Result<(), RuntimeError> { Err(RuntimeError::Batch) }
///     fn pull(&mut self, _: &mut RuntimeContext<'v, 'm, 'g>, _: &mut RowBatch<'v, 'm, 'g>) -> Result<PullState, RuntimeError> { Ok(PullState::Done) }
/// }
/// struct Escaping { view: Option<&'static QueryView> }
/// impl<'m, 'g: 'm> OperatorFactory<'m, 'g> for Escaping {
///     type Operator<'v> = Noop;
///     fn build<'v>(&mut self, context: &mut RuntimeContext<'v, 'm, 'g>) -> Result<Noop, RuntimeError> {
///         self.view = Some(context.view()); // fresh execution view cannot escape
///         Ok(Noop)
///     }
/// }
/// ```
pub trait OperatorFactory<'m, 'g: 'm> {
    /// A concrete chain retaining this execution's view and charged backing.
    type Operator<'v>: PullOperator<'v, 'm, 'g>;
    /// Construct the whole chain, retaining each real buffer charge with its owner.
    fn build<'v>(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self::Operator<'v>, RuntimeError>;
}

/// Explicit fixed capacities, including unused slots, charged before pulling.
#[derive(Clone, Copy)]
pub struct ExecutionCapacity {
    /// Between one and 256 scheduling rows.
    pub batch_rows: usize,
    /// Requested retained rows, at most 65536; zero is valid.
    pub result_rows: usize,
    /// Intermediate scalar/string/list payload cap, at most query allowance.
    pub batch_payload_bytes: usize,
    /// Logical prepared-row payload cap, at most 4 MiB; controls are additional.
    pub result_payload_bytes: usize,
    /// Complete simultaneous intermediate variable arena capacities.
    pub batch: ArenaCapacity,
    /// Complete simultaneous output variable arena capacities.
    pub result: ArenaCapacity,
}
impl Default for ExecutionCapacity {
    fn default() -> Self {
        Self {
            batch_rows: 256,
            result_rows: 256,
            batch_payload_bytes: 4 * 1024 * 1024,
            result_payload_bytes: 4 * 1024 * 1024,
            batch: ArenaCapacity::default(),
            result: ArenaCapacity::default(),
        }
    }
}
/// Private collected rows, accessible only while the actual view remains retained.
/// Completion copies entity data using that view; this is not a public result.
pub struct PreparedRows<'v, 'm, 'g> {
    rows: RowBatch<'v, 'm, 'g>,
}
impl PreparedRows<'_, '_, '_> {
    /// Complete bag cardinality.
    pub fn rows(&self) -> usize {
        self.rows.rows()
    }
    /// Validated output width.
    pub fn columns(&self) -> usize {
        self.rows.columns()
    }
    /// Same-view intermediate cell for synchronous freezing only.
    pub fn value(&self, row: usize, column: usize) -> Option<QueryValue<'_>> {
        self.rows.value(row, column)
    }
    /// Initialized logical prepared payload; not represented completed arena bytes.
    pub fn payload_bytes(&self) -> usize {
        self.rows.payload_bytes()
    }
}
/// Internal typed freeze adapter. Its associated output cannot borrow the fresh
/// view/rows lifetime. It must retain actual owned buffer reservations, and must
/// finish fallible copying before success or any later durable commit attempt.
pub trait Completion<'m, 'g> {
    /// Fully independent copied output with its own capacity ownership.
    type Output;
    /// Runs before the final view-first/control check and while the lease is held.
    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, RuntimeError>;
}
/// Freeze result supplied by the internal completion owner. Byte counts are
/// initialized represented core/ABI bytes, including every in-arena descriptor.
/// They are NOT allocator capacities: actual complete capacities and external
/// registry/control overhead stay separately charged by the output owner.
/// This metadata seam does not implement ZE-52's copied representation.
pub struct FrozenOutput<T> {
    output: T,
    rows: usize,
    core_bytes: usize,
    abi_bytes: usize,
}
impl<T> FrozenOutput<T> {
    /// Checks represented limits before the output can reach final admission.
    pub fn new(
        output: T,
        rows: usize,
        core_bytes: usize,
        abi_bytes: usize,
    ) -> Result<Self, RuntimeError> {
        if rows > 65536 {
            return Err(RuntimeError::Limit(WorkKind::CompletedRows));
        }
        if core_bytes > 4 * 1024 * 1024 {
            return Err(RuntimeError::Limit(WorkKind::CompletedBytes));
        }
        if abi_bytes > 4 * 1024 * 1024 {
            return Err(RuntimeError::Limit(WorkKind::CompletedAbiBytes));
        }
        Ok(Self {
            output,
            rows,
            core_bytes,
            abi_bytes,
        })
    }
}
/// Complete success only. No result is produced on a failed final checkpoint.
pub struct Execution<T> {
    /// Frozen output independent of temporary view, batches and caller backing.
    pub output: T,
    /// Exact cumulative counters at successful completion.
    pub counters: WorkCounters,
    /// Actual query reservation high-water mark, including simultaneous owners.
    pub peak_query_bytes: usize,
}
/// Failure diagnostic contains no row collection, including after partial pulls.
#[derive(Debug)]
pub struct RuntimeFailure {
    /// Operator/root identity or the eager source which failed.
    pub operator: PlanNodeId,
    /// Typed cause, without partial prepared output.
    pub error: RuntimeError,
    /// Actual work consumed before failure; failed unconsumed units are absent.
    pub counters: WorkCounters,
}
impl std::fmt::Display for RuntimeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "graph operator {}: {}", self.operator.0, self.error)
    }
}
impl std::error::Error for RuntimeFailure {}

/// Drains under one owned retained view, freezes privately, checks close/cancel
/// one final time, then releases temporary owners and the lease before returning.
/// This read/preparation driver has no irreversible publication window.
#[allow(
    clippy::too_many_arguments,
    clippy::result_large_err,
    reason = "typed owned execution inputs and allocation-free full failure counters"
)]
pub fn execute<
    'm,
    'g,
    V: RetainedView,
    O: for<'v> PullOperator<'v, 'm, 'g>,
    C: Completion<'m, 'g>,
>(
    view: V,
    control: &QueryControl,
    memory: &'m QueryMemory<'g>,
    plan: &RuntimePlan<'_, '_, '_, '_, '_, '_>,
    source: &mut O,
    completion: &mut C,
    capacity: ExecutionCapacity,
    limits: RuntimeLimits,
) -> Result<Execution<C::Output>, RuntimeFailure> {
    let root = plan.plan().description().root;
    let mut context =
        RuntimeContext::new(&view, control, memory, limits).map_err(|error| RuntimeFailure {
            operator: root,
            error,
            counters: WorkCounters::default(),
        })?;
    let _view_charge =
        memory
            .reserve(std::mem::size_of::<V>())
            .map_err(|error| RuntimeFailure {
                operator: root,
                error: error.into(),
                counters: context.counters(),
            })?;
    drain(&mut context, plan, source, completion, capacity)
}

/// Executes a buffer-owning physical chain built under this one retained view.
/// The stateless entry point and this entry point share exactly the same eager
/// barrier, drain, completion and final close-first check.
#[allow(
    clippy::too_many_arguments,
    clippy::result_large_err,
    reason = "typed owned execution inputs and allocation-free failure counters"
)]
pub fn execute_factory<
    'm,
    'g,
    V: RetainedView,
    F: OperatorFactory<'m, 'g>,
    C: Completion<'m, 'g>,
>(
    view: V,
    control: &QueryControl,
    memory: &'m QueryMemory<'g>,
    plan: &RuntimePlan<'_, '_, '_, '_, '_, '_>,
    factory: &mut F,
    completion: &mut C,
    capacity: ExecutionCapacity,
    limits: RuntimeLimits,
) -> Result<Execution<C::Output>, RuntimeFailure> {
    let root = plan.plan().description().root;
    let mut context =
        RuntimeContext::new(&view, control, memory, limits).map_err(|error| RuntimeFailure {
            operator: root,
            error,
            counters: WorkCounters::default(),
        })?;
    let _view_charge =
        memory
            .reserve(std::mem::size_of::<V>())
            .map_err(|error| RuntimeFailure {
                operator: root,
                error: error.into(),
                counters: context.counters(),
            })?;
    if !plan.belongs_to(memory) {
        return Err(RuntimeFailure {
            operator: root,
            error: RuntimeError::Batch,
            counters: context.counters(),
        });
    }
    let mut source = factory
        .build(&mut context)
        .map_err(|error| RuntimeFailure {
            operator: root,
            error,
            counters: context.counters(),
        })?;
    drain(&mut context, plan, &mut source, completion, capacity)
}

/// Drains within an existing execution context, preserving all prior value and
/// operator work. The caller retains and charges the actual view and plan owners
/// around this scoped call; this entry neither admits nor releases that view.
/// It uses the same eager barrier, completion and final close-first checks as
/// the owned-view entry points and creates no replacement context or budget.
#[allow(
    clippy::result_large_err,
    reason = "allocation-free full failure counters"
)]
pub fn execute_in<'v, 'm, 'g, O: PullOperator<'v, 'm, 'g>, C: Completion<'m, 'g>>(
    context: &mut RuntimeContext<'v, 'm, 'g>,
    plan: &RuntimePlan<'_, '_, '_, '_, '_, '_>,
    source: &mut O,
    completion: &mut C,
    capacity: ExecutionCapacity,
) -> Result<Execution<C::Output>, RuntimeFailure> {
    drain(context, plan, source, completion, capacity)
}

#[allow(
    clippy::result_large_err,
    reason = "allocation-free full failure counters"
)]
fn drain<'v, 'm, 'g, O: PullOperator<'v, 'm, 'g>, C: Completion<'m, 'g>>(
    context: &mut RuntimeContext<'v, 'm, 'g>,
    plan: &RuntimePlan<'_, '_, '_, '_, '_, '_>,
    source: &mut O,
    completion: &mut C,
    capacity: ExecutionCapacity,
) -> Result<Execution<C::Output>, RuntimeFailure> {
    let root = plan.plan().description().root;
    let memory = context.memory();
    let mut diagnostic = root;
    let result = (|| {
        context.checkpoint()?;
        if !plan.belongs_to(memory)
            || source.node() != root
            || capacity.result_rows > 65536
            || capacity.result_payload_bytes > 4 * 1024 * 1024
        {
            return Err(RuntimeError::Batch);
        }
        let width = plan.plan().facts(root).ok_or(RuntimeError::Batch)?.width();
        let _driver = memory.reserve(
            std::mem::size_of::<ExecutionCapacity>() + std::mem::size_of::<WorkCounters>(),
        )?;
        let mut batch = RowBatch::with_arenas(
            context,
            width,
            capacity.batch_rows,
            capacity.batch_payload_bytes,
            capacity.batch,
        )?;
        let mut prepared = PreparedRows {
            rows: RowBatch::storage(
                context,
                width,
                capacity.result_rows,
                capacity.result_payload_bytes,
                capacity.result,
            )?,
        };
        for node in plan.plan().description().eager_searches {
            diagnostic = *node;
            context.charge(WorkKind::SearchInvocations, 1)?;
            source.prepare_search(*node, context)?;
            context.checkpoint()?;
        }
        diagnostic = root;
        loop {
            context.checkpoint()?;
            batch.clear();
            let state = source.pull(context, &mut batch)?;
            context.checkpoint()?;
            if state == PullState::More && batch.rows() == 0 {
                return Err(RuntimeError::Batch);
            }
            for row in 0..batch.rows() {
                context.check_work(WorkKind::CompletedRows, 1)?;
                context.check_work(
                    WorkKind::PreparedPayloadBytes,
                    batch.row_payload_bytes(row)? as u64,
                )?;
                context.charge(WorkKind::RowsIn, 1)?;
                // The full private collection is dropped if any row/copy fails.
                let previous = prepared.rows.payload_bytes();
                prepared.rows.copy_row(&batch, row, context)?;
                context.charge(WorkKind::CompletedRows, 1)?;
                context.charge(
                    WorkKind::PreparedPayloadBytes,
                    (prepared.rows.payload_bytes() - previous) as u64,
                )?;
            }
            if state == PullState::Done {
                break;
            }
        }
        context.checkpoint()?;
        let frozen = completion.complete(&prepared, context)?;
        if frozen.rows != prepared.rows() {
            return Err(RuntimeError::Batch);
        }
        context.charge(WorkKind::CompletedBytes, frozen.core_bytes as u64)?;
        context.charge(WorkKind::CompletedAbiBytes, frozen.abi_bytes as u64)?;
        context.checkpoint()?;
        Ok(frozen.output)
    })();
    match result {
        Ok(output) => Ok(Execution {
            output,
            counters: context.counters(),
            peak_query_bytes: memory.peak_reserved_bytes(),
        }),
        Err(error) => Err(RuntimeFailure {
            operator: diagnostic,
            error,
            counters: context.counters(),
        }),
    }
}
