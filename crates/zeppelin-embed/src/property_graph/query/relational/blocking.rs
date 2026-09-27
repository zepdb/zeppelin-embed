use super::*;

/// Blocking operation whose borrowed description is copied at construction.
pub enum BlockingOperation<'a> {
    /// Query-equivalent DISTINCT over all columns.
    Distinct,
    /// Stable explicit ordering of computed input columns.
    Sort(&'a [OrderKey]),
    /// A new grouped/global scope.
    Aggregate {
        /// Grouping key projection/rename.
        keys: &'a [SlotProjection],
        /// Aggregate output operations.
        columns: &'a [AggregateColumn],
    },
}
#[derive(Clone, Copy)]
enum Operation {
    Distinct,
    Sort,
    Aggregate,
}
/// Drains its real child on first pull, after the driver's eager obligations,
/// then executes one blocking kernel and emits bounded batches. Construction
/// does not pull, evaluate, or suppress any upstream source obligation.
pub struct BlockingRows<'v, 'm, 'g, O, E = RuntimeError> {
    child: O,
    schema: Schema<'m, 'g>,
    input: Rows<'v, 'm, 'g>,
    output: RowSource<'v, 'm, 'g>,
    batch: RowBatch<'v, 'm, 'g>,
    keys: QueryArena<'m, 'g, SlotProjection>,
    aggregates: QueryArena<'m, 'g, AggregateColumn>,
    order: QueryArena<'m, 'g, OrderKey>,
    operation: Operation,
    node: PlanNodeId,
    output_capacity: StorageCapacity,
    started: bool,
    ready: bool,
    _error: std::marker::PhantomData<fn() -> E>,
    _control: QueryReservation<'m, 'g>,
}
impl<'v, 'm, 'g, O: RowOperator<'v, 'm, 'g, E>, E> BlockingRows<'v, 'm, 'g, O, E> {
    /// Retains genuine independent batch, input storage and operation owners.
    #[allow(
        clippy::too_many_arguments,
        reason = "explicit operator and three distinct capacity owners"
    )]
    pub fn new(
        context: &RuntimeContext<'v, 'm, 'g>,
        node: PlanNodeId,
        child: O,
        operation: BlockingOperation<'_>,
        batch: StorageCapacity,
        input: StorageCapacity,
        output: StorageCapacity,
    ) -> Result<Self, RuntimeError> {
        let (kind, keys, aggregates, order) = match operation {
            BlockingOperation::Distinct => (Operation::Distinct, &[][..], &[][..], &[][..]),
            BlockingOperation::Sort(order) => (Operation::Sort, &[][..], &[][..], order),
            BlockingOperation::Aggregate { keys, columns } => {
                (Operation::Aggregate, keys, columns, &[][..])
            }
        };
        if keys
            .len()
            .checked_add(aggregates.len())
            .is_none_or(|n| n > 256)
            || order.len() > 256
        {
            return Err(RuntimeError::Batch);
        }
        let mut owned_keys = QueryArena::new(context.memory(), keys.len())?;
        let mut owned_aggregates = QueryArena::new(context.memory(), aggregates.len())?;
        let mut owned_order = QueryArena::new(context.memory(), order.len())?;
        let mut outputs = QueryArena::new(
            context.memory(),
            if matches!(kind, Operation::Aggregate) {
                keys.len() + aggregates.len()
            } else {
                child.schema().slots().len()
            },
        )?;
        for key in keys {
            context.checkpoint()?;
            child.schema().column(key.source)?;
            owned_keys.push(*key)?;
            outputs.push(key.output)?;
        }
        for aggregate in aggregates {
            context.checkpoint()?;
            match aggregate.operation {
                Aggregate::CountAll => {}
                Aggregate::Count { slot, .. }
                | Aggregate::Collect { slot, .. }
                | Aggregate::Sum { slot, .. }
                | Aggregate::Min { slot }
                | Aggregate::Max { slot } => {
                    child.schema().column(slot)?;
                }
            }
            owned_aggregates.push(*aggregate)?;
            outputs.push(aggregate.output)?;
        }
        for key in order {
            context.checkpoint()?;
            child.schema().column(key.slot)?;
            owned_order.push(*key)?;
        }
        if !matches!(kind, Operation::Aggregate) {
            for slot in child.schema().slots() {
                outputs.push(*slot)?;
            }
        }
        let schema = Schema::new(context, outputs.as_slice())?;
        let rows = Rows::new(context, child.schema().slots(), input)?;
        let empty_capacity = StorageCapacity {
            rows: 0,
            max_rows: 0,
            payload_bytes: 0,
            variable: ArenaCapacity::default(),
        };
        let empty_output =
            Rows::new(context, schema.slots(), empty_capacity)?.into_source(node, context)?;
        let batch = RowBatch::with_arenas(
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
                - std::mem::size_of::<Rows<'_, '_, '_>>()
                - std::mem::size_of::<RowSource<'_, '_, '_>>()
                - std::mem::size_of::<RowBatch<'_, '_, '_>>()
                - std::mem::size_of::<QueryArena<'_, '_, SlotProjection>>()
                - std::mem::size_of::<QueryArena<'_, '_, AggregateColumn>>()
                - std::mem::size_of::<QueryArena<'_, '_, OrderKey>>(),
        )?;
        Ok(Self {
            child,
            schema,
            input: rows,
            output: empty_output,
            batch,
            keys: owned_keys,
            aggregates: owned_aggregates,
            order: owned_order,
            operation: kind,
            node,
            output_capacity: output,
            started: false,
            ready: false,
            _error: std::marker::PhantomData,
            _control: control,
        })
    }
}
impl<'v, 'm, 'g, O: RowOperator<'v, 'm, 'g, E>, E: From<RuntimeError>> RowOperator<'v, 'm, 'g, E>
    for BlockingRows<'v, 'm, 'g, O, E>
{
    fn schema(&self) -> &Schema<'m, 'g> {
        &self.schema
    }
}
impl<'v, 'm, 'g, O: RowOperator<'v, 'm, 'g, E>, E: From<RuntimeError>> PullOperator<'v, 'm, 'g, E>
    for BlockingRows<'v, 'm, 'g, O, E>
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
        if !self.batch.belongs_to(context) || output.columns() != self.schema.slots().len() {
            return Err(RuntimeError::Batch.into());
        }
        if !self.started {
            self.started = true;
            let placeholder = Rows::new(
                context,
                &[],
                StorageCapacity {
                    rows: 0,
                    max_rows: 0,
                    payload_bytes: 0,
                    variable: ArenaCapacity::default(),
                },
            )?;
            let mut input = std::mem::replace(&mut self.input, placeholder);
            let mut aggregate = if matches!(self.operation, Operation::Aggregate) {
                Some(streaming::StreamAggregate::new(
                    std::mem::replace(
                        &mut input,
                        Rows::new(
                            context,
                            &[],
                            StorageCapacity::new(0, 0, ArenaCapacity::default()),
                        )?,
                    ),
                    self.keys.as_slice(),
                    self.aggregates.as_slice(),
                    self.output_capacity,
                    context,
                )?)
            } else {
                None
            };
            let mut distinct = streaming::StreamIndex::new(context)?;
            let mut columns = QueryArena::new(context.memory(), self.batch.columns())
                .map_err(RuntimeError::Memory)?;
            for column in 0..self.batch.columns() {
                columns.push(column).map_err(RuntimeError::Memory)?;
            }
            loop {
                self.batch.clear();
                let state = self.child.pull(context, &mut self.batch)?;
                if state == PullState::More && self.batch.rows() == 0 {
                    return Err(RuntimeError::Batch.into());
                }
                for row in 0..self.batch.rows() {
                    context.charge(WorkKind::OperatorRows, 1)?;
                    context.charge(WorkKind::RowsIn, 1)?;
                    let mut values = QueryArena::new(context.memory(), self.batch.columns())
                        .map_err(RuntimeError::Memory)?;
                    for column in 0..self.batch.columns() {
                        values
                            .push(self.batch.value(row, column).ok_or(RuntimeError::Batch)?)
                            .map_err(RuntimeError::Memory)?;
                    }
                    if let Some(aggregate) = &mut aggregate {
                        aggregate.push(values.as_slice(), self.aggregates.as_slice(), context)?;
                    } else if matches!(self.operation, Operation::Distinct) {
                        let (hash, found) = distinct.locate(
                            values.as_slice(),
                            &input,
                            columns.as_slice(),
                            context,
                        )?;
                        if found.is_none() {
                            distinct.insert(hash, input.len(), context)?;
                            input.push(values.as_slice(), context)?;
                        }
                    } else {
                        input.push(values.as_slice(), context)?;
                    }
                }
                if state == PullState::Done {
                    break;
                }
            }
            self.batch.clear();
            let rows = match self.operation {
                Operation::Distinct => input,
                Operation::Sort => input.sort(self.order.as_slice(), context)?,
                Operation::Aggregate => aggregate.ok_or(RuntimeError::Batch)?.finish(
                    self.keys.as_slice(),
                    self.aggregates.as_slice(),
                    context,
                )?,
            };
            self.output = rows.into_source(self.node, context)?;
            self.ready = true;
        }
        if !self.ready {
            return Err(RuntimeError::Batch.into());
        }
        self.output.pull(context, output).map_err(E::from)
    }
}
