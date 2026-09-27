//! Relational operators consume pre-evaluated columns. Expression evaluation,
//! native entity reads and public plan composition belong to their producers.
use super::QueryValue;
use super::plan::{NodeFacts, PlanNodeId, SlotId};
use super::resources::{QueryArena, QueryReservation};
use super::runtime::{
    ArenaCapacity, PullOperator, PullState, RowBatch, RuntimeContext, RuntimeError, WorkKind,
};
use std::marker::PhantomData;
mod ordering;
pub use ordering::OrderKey;
mod aggregate;
pub use aggregate::{Aggregate, AggregateColumn};
mod blocking;
pub use blocking::{BlockingOperation, BlockingRows};

/// Explicit ordered, distinct logical slots; IDs are never physical ordinals.
pub struct Schema<'m, 'g> {
    slots: QueryArena<'m, 'g, SlotId>,
}
impl<'m, 'g> Schema<'m, 'g> {
    /// Copies the bounded schema into the same query's actual charged owner.
    pub fn new(
        context: &RuntimeContext<'_, 'm, 'g>,
        slots: &[SlotId],
    ) -> Result<Self, RuntimeError> {
        context.checkpoint()?;
        if slots.len() > 256 {
            return Err(RuntimeError::Batch);
        }
        let mut owner = QueryArena::new(context.memory(), slots.len())?;
        for slot in slots {
            context.checkpoint()?;
            if owner.as_slice().contains(slot) {
                return Err(RuntimeError::Batch);
            }
            owner.push(*slot)?;
        }
        Ok(Self { slots: owner })
    }
    /// Checks an explicitly ordered schema against immutable validated facts.
    pub fn verify(&self, facts: &NodeFacts) -> Result<(), RuntimeError> {
        if facts.width() != self.slots.len()
            || self
                .slots
                .as_slice()
                .iter()
                .any(|slot| facts.slot(*slot).is_none())
        {
            return Err(RuntimeError::Batch);
        }
        Ok(())
    }
    /// Logical slots in physical column order.
    pub fn slots(&self) -> &[SlotId] {
        self.slots.as_slice()
    }
    /// Checked resolution of a logical slot in this exact scope.
    pub fn column(&self, slot: SlotId) -> Result<usize, RuntimeError> {
        self.slots
            .as_slice()
            .iter()
            .position(|candidate| *candidate == slot)
            .ok_or(RuntimeError::Batch)
    }
}

/// Independent blocking storage capacity, never the scheduling-batch limit.
#[derive(Clone, Copy)]
pub struct StorageCapacity {
    /// Rows per explicitly allocated chunk.
    pub rows: usize,
    pub(crate) max_rows: usize,
    /// Initialized payload limit, within the same query allowance.
    pub payload_bytes: usize,
    /// Actual retained variable-value arena capacities.
    pub variable: ArenaCapacity,
}

impl StorageCapacity {
    /// Fixed maximum for explicit kernel callers; native blocking operators may
    /// select a larger crate-private maximum while retaining this chunk size.
    pub fn new(rows: usize, payload_bytes: usize, variable: ArenaCapacity) -> Self {
        Self {
            rows,
            max_rows: rows,
            payload_bytes,
            variable,
        }
    }
}

/// Actual owned rows and an order vector. Blocking operators may reorder or
/// select indices without copying payloads or dropping their authentic charge.
pub struct Rows<'v, 'm, 'g> {
    schema: Schema<'m, 'g>,
    chunks: QueryArena<'m, 'g, RowBatch<'v, 'm, 'g>>,
    capacity: StorageCapacity,
    stored: usize,
    order: QueryArena<'m, 'g, usize>,
}
impl<'v, 'm, 'g> Rows<'v, 'm, 'g> {
    /// Allocates separately charged blocking row storage.
    pub fn new(
        context: &RuntimeContext<'v, 'm, 'g>,
        slots: &[SlotId],
        capacity: StorageCapacity,
    ) -> Result<Self, RuntimeError> {
        if capacity.rows == 0 && capacity.max_rows != 0 {
            return Err(RuntimeError::Batch);
        }
        let mut chunks = QueryArena::new(
            context.memory(),
            capacity.max_rows.div_ceil(capacity.rows.max(1)).max(1),
        )?;
        chunks.push(RowBatch::storage(
            context,
            slots.len(),
            capacity.rows.min(capacity.max_rows),
            capacity.payload_bytes,
            capacity.variable,
        )?)?;
        Ok(Self {
            schema: Schema::new(context, slots)?,
            chunks,
            capacity,
            stored: 0,
            order: QueryArena::new(context.memory(), capacity.max_rows)?,
        })
    }
    /// Appends one complete copied row, preserving bag multiplicity.
    pub fn push(
        &mut self,
        row: &[QueryValue<'_>],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        if row.len() != self.schema.slots().len() {
            return Err(RuntimeError::Batch);
        }
        self.push_from(
            |column| row.get(column).copied().ok_or(RuntimeError::Batch),
            context,
        )
    }
    fn push_from<'a>(
        &mut self,
        value: impl FnMut(usize) -> Result<QueryValue<'a>, RuntimeError>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        if !self.belongs_to(context) {
            return Err(RuntimeError::Batch);
        }
        if self.stored == self.capacity.max_rows {
            return Err(RuntimeError::BatchCapacity);
        }
        let chunk = self.stored / self.capacity.rows;
        if chunk == self.chunks.len() {
            // RowBatch reserves every backing arena before allocation. Growth
            // is explicit here; QueryArena itself remains fixed-capacity.
            self.chunks.push(RowBatch::storage(
                context,
                self.schema.slots().len(),
                self.capacity.rows.min(self.capacity.max_rows - self.stored),
                self.capacity.payload_bytes,
                self.capacity.variable,
            )?)?;
        }
        self.chunks
            .as_mut_slice()
            .get_mut(chunk)
            .ok_or(RuntimeError::Batch)?
            .push_from(value, context)?;
        self.order.push(self.stored)?;
        self.stored += 1;
        Ok(())
    }
    fn belongs_to(&self, context: &RuntimeContext<'v, 'm, 'g>) -> bool {
        self.chunks
            .as_slice()
            .first()
            .is_some_and(|chunk| chunk.belongs_to(context))
    }
    pub(crate) fn cell(&self, raw: usize, column: usize) -> Option<QueryValue<'_>> {
        let size = self.capacity.rows.max(1);
        self.chunks
            .as_slice()
            .get(raw / size)?
            .value(raw % size, column)
    }
    /// Exact schema retained with the rows.
    pub fn schema(&self) -> &Schema<'m, 'g> {
        &self.schema
    }
    /// Number of selected rows, including duplicates.
    pub fn len(&self) -> usize {
        self.order.len()
    }
    /// Whether the row bag is empty.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
    /// Checked physical-column access under the original view.
    pub fn value(&self, row: usize, column: usize) -> Option<QueryValue<'_>> {
        self.cell(*self.order.as_slice().get(row)?, column)
    }
    pub(crate) fn selected_source_row(&self, position: usize) -> Result<usize, RuntimeError> {
        self.order
            .as_slice()
            .get(position)
            .copied()
            .ok_or(RuntimeError::Batch)
    }
    /// Consumes the actual backing into a bounded pull source.
    pub fn into_source(
        self,
        node: PlanNodeId,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<RowSource<'v, 'm, 'g>, RuntimeError> {
        context.checkpoint()?;
        if !self.belongs_to(context) {
            return Err(RuntimeError::Batch);
        }
        let control = context
            .memory()
            .reserve(std::mem::size_of::<RowSource<'_, '_, '_>>() - std::mem::size_of::<Self>())?;
        Ok(RowSource {
            rows: self,
            node,
            next: 0,
            _control: control,
        })
    }
}

/// A physical relational source/operator with an explicit output scope.
pub trait RowOperator<'v, 'm, 'g, E = RuntimeError>: PullOperator<'v, 'm, 'g, E> {
    /// Checked logical-to-physical schema for this operator's output.
    fn schema(&self) -> &Schema<'m, 'g>;
}
/// Bounded source consuming owned rows; no second query budget or view.
pub struct RowSource<'v, 'm, 'g> {
    rows: Rows<'v, 'm, 'g>,
    node: PlanNodeId,
    next: usize,
    _control: QueryReservation<'m, 'g>,
}
impl<'v, 'm, 'g> RowOperator<'v, 'm, 'g> for RowSource<'v, 'm, 'g> {
    fn schema(&self) -> &Schema<'m, 'g> {
        self.rows.schema()
    }
}
impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for RowSource<'v, 'm, 'g> {
    fn node(&self) -> PlanNodeId {
        self.node
    }
    fn prepare_search(
        &mut self,
        _: PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Batch)
    }
    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, RuntimeError> {
        context.checkpoint()?;
        if !self.rows.belongs_to(context) || output.columns() != self.schema().slots().len() {
            return Err(RuntimeError::Batch);
        }
        while self.next < self.rows.len() && output.rows() < output.capacity() {
            context.charge(WorkKind::OperatorRows, 1)?;
            context.charge(WorkKind::RowsIn, 1)?;
            output.push_from(
                |column| {
                    self.rows
                        .value(self.next, column)
                        .ok_or(RuntimeError::Batch)
                },
                context,
            )?;
            self.next += 1;
        }
        Ok(if self.next == self.rows.len() {
            PullState::Done
        } else {
            PullState::More
        })
    }
}
/// Projection/rename from a computed input column into a fresh output scope.
#[derive(Clone, Copy)]
pub struct SlotProjection {
    /// Existing logical input slot.
    pub source: SlotId,
    /// Logical output slot, unique within the output scope.
    pub output: SlotId,
}
/// Streaming three-valued filter, scoped projection, and offset/limit. Each
/// instance applies those operations in that order; compose instances to retain
/// a plan's original stage order. Eager obligations always delegate upstream.
pub struct MapRows<'v, 'm, 'g, O, E = RuntimeError> {
    child: O,
    schema: Schema<'m, 'g>,
    columns: QueryArena<'m, 'g, usize>,
    input: RowBatch<'v, 'm, 'g>,
    node: PlanNodeId,
    predicate: Option<usize>,
    offset: u64,
    remaining: Option<u64>,
    next: usize,
    done: bool,
    _error: PhantomData<fn() -> E>,
    _control: QueryReservation<'m, 'g>,
}
impl<'v, 'm, 'g, O: RowOperator<'v, 'm, 'g, E>, E> MapRows<'v, 'm, 'g, O, E> {
    /// Resolves all slots once, then retains real bounded input backing.
    #[allow(
        clippy::too_many_arguments,
        reason = "explicit relational stage and actual capacities"
    )]
    pub fn new(
        context: &RuntimeContext<'v, 'm, 'g>,
        node: PlanNodeId,
        child: O,
        projection: &[SlotProjection],
        predicate: Option<SlotId>,
        offset: u64,
        limit: Option<u64>,
        batch: StorageCapacity,
    ) -> Result<Self, RuntimeError> {
        let mut schema_slots = QueryArena::new(context.memory(), projection.len())?;
        let mut columns = QueryArena::new(context.memory(), projection.len())?;
        for projection in projection {
            context.checkpoint()?;
            schema_slots.push(projection.output)?;
            columns.push(child.schema().column(projection.source)?)?;
        }
        let schema = Schema::new(context, schema_slots.as_slice())?;
        let predicate = predicate
            .map(|slot| child.schema().column(slot))
            .transpose()?;
        let input = RowBatch::with_arenas(
            context,
            child.schema().slots().len(),
            batch.rows,
            batch.payload_bytes,
            batch.variable,
        )?;
        let control = context.memory().reserve(
            std::mem::size_of::<Self>()
                - std::mem::size_of::<O>()
                - std::mem::size_of::<Schema<'_, '_>>()
                - std::mem::size_of::<QueryArena<'_, '_, usize>>()
                - std::mem::size_of::<RowBatch<'_, '_, '_>>(),
        )?;
        Ok(Self {
            child,
            schema,
            columns,
            input,
            node,
            predicate,
            offset,
            remaining: limit,
            next: 0,
            done: false,
            _error: PhantomData,
            _control: control,
        })
    }
}
impl<'v, 'm, 'g, O: RowOperator<'v, 'm, 'g, E>, E: From<RuntimeError>> RowOperator<'v, 'm, 'g, E>
    for MapRows<'v, 'm, 'g, O, E>
{
    fn schema(&self) -> &Schema<'m, 'g> {
        &self.schema
    }
}
impl<'v, 'm, 'g, O: RowOperator<'v, 'm, 'g, E>, E: From<RuntimeError>> PullOperator<'v, 'm, 'g, E>
    for MapRows<'v, 'm, 'g, O, E>
{
    fn node(&self) -> PlanNodeId {
        self.node
    }
    fn prepare_search(
        &mut self,
        node: PlanNodeId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), E> {
        self.child.prepare_search(node, context)
    }
    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, E> {
        context.checkpoint()?;
        if !self.input.belongs_to(context) || output.columns() != self.schema.slots().len() {
            return Err(RuntimeError::Batch.into());
        }
        while output.rows() < output.capacity() && self.remaining != Some(0) {
            if self.next == self.input.rows() {
                if self.done {
                    break;
                }
                self.input.clear();
                self.next = 0;
                self.done = self.child.pull(context, &mut self.input)? == PullState::Done;
                if !self.done && self.input.rows() == 0 {
                    return Err(RuntimeError::Batch.into());
                }
                if self.input.rows() == 0 {
                    break;
                }
            }
            context.charge(WorkKind::OperatorRows, 1)?;
            context.charge(WorkKind::RowsIn, 1)?;
            let row = self.next;
            self.next += 1;
            if let Some(column) = self.predicate
                && !self
                    .input
                    .value(row, column)
                    .ok_or(RuntimeError::Batch)?
                    .truth()
                    .map_err(RuntimeError::from)?
                    .retained()
            {
                continue;
            }
            if self.offset != 0 {
                self.offset -= 1;
                continue;
            }
            output.push_from(
                |column| {
                    self.input
                        .value(
                            row,
                            *self
                                .columns
                                .as_slice()
                                .get(column)
                                .ok_or(RuntimeError::Batch)?,
                        )
                        .ok_or(RuntimeError::Batch)
                },
                context,
            )?;
            if let Some(remaining) = self.remaining.as_mut() {
                *remaining -= 1;
            }
        }
        Ok(
            if self.remaining == Some(0) || (self.done && self.next == self.input.rows()) {
                PullState::Done
            } else {
                PullState::More
            },
        )
    }
}
