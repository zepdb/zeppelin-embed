use super::ordering::Index;
use super::*;
use crate::property_graph::query::{
    MAX_LIST_DEPTH, MAX_LIST_ELEMENTS, MAX_QUERY_BYTES, QueryError, QueryList, list::ListArena,
};

/// Accepted aggregation over already evaluated input cells.
#[derive(Clone, Copy)]
pub enum Aggregate {
    /// Counts every input row including null-valued rows.
    CountAll,
    /// Counts non-null values using query equivalence when distinct.
    Count {
        /// Already evaluated operand slot.
        slot: SlotId,
        /// Deduplicate non-null operands by query equivalence.
        distinct: bool,
    },
    /// Collects non-null values in upstream row order.
    Collect {
        /// Already evaluated operand slot.
        slot: SlotId,
        /// Keep only the first query-equivalent non-null operand.
        distinct: bool,
    },
}
/// Output slot and aggregation operation.
#[derive(Clone, Copy)]
pub struct AggregateColumn {
    /// Unique output slot in the new scope.
    pub output: SlotId,
    /// Accepted count/collect semantics.
    pub operation: Aggregate,
}
#[derive(Clone, Copy)]
struct Group {
    first: usize,
    head: usize,
    tail: usize,
    count: usize,
}

impl<'v, 'm, 'g> Rows<'v, 'm, 'g> {
    /// Consumes complete input into grouped or global aggregates. Empty global
    /// input emits one 0/[] row; empty grouped input emits no rows. All copies,
    /// key/index storage and aggregate scratch overlap in the same allowance.
    pub fn aggregate(
        self,
        keys: &[SlotProjection],
        aggregates: &[AggregateColumn],
        capacity: StorageCapacity,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, RuntimeError> {
        self.aggregate_inner(keys, aggregates, capacity, None, context)
            .map(|(rows, _)| rows)
    }

    pub(crate) fn aggregate_with_representatives(
        self,
        keys: &[SlotProjection],
        aggregates: &[AggregateColumn],
        capacity: StorageCapacity,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(Self, QueryArena<'m, 'g, Option<usize>>), RuntimeError> {
        let representative_capacity = if keys.is_empty() { 1 } else { self.len() };
        let representatives = QueryArena::new(context.memory(), representative_capacity)?;
        let (rows, representatives) =
            self.aggregate_inner(keys, aggregates, capacity, Some(representatives), context)?;
        Ok((rows, representatives.ok_or(RuntimeError::Batch)?))
    }

    #[allow(
        clippy::type_complexity,
        reason = "optional charged representative owner accompanies the row owner"
    )]
    fn aggregate_inner(
        self,
        keys: &[SlotProjection],
        aggregates: &[AggregateColumn],
        capacity: StorageCapacity,
        mut representatives: Option<QueryArena<'m, 'g, Option<usize>>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(Self, Option<QueryArena<'m, 'g, Option<usize>>>), RuntimeError> {
        if !self.belongs_to(context)
            || keys
                .len()
                .checked_add(aggregates.len())
                .is_none_or(|n| n > 256)
        {
            return Err(RuntimeError::Batch);
        }
        let mut key_columns = QueryArena::new(context.memory(), keys.len())?;
        let mut slots = QueryArena::new(context.memory(), keys.len() + aggregates.len())?;
        let mut operands = QueryArena::new(context.memory(), aggregates.len())?;
        for key in keys {
            context.checkpoint()?;
            key_columns.push(self.schema.column(key.source)?)?;
            slots.push(key.output)?;
        }
        for aggregate in aggregates {
            let column = match aggregate.operation {
                Aggregate::CountAll => None,
                Aggregate::Count { slot, .. } | Aggregate::Collect { slot, .. } => {
                    Some(self.schema.column(slot)?)
                }
            };
            operands.push(column)?;
            slots.push(aggregate.output)?;
        }
        // Validate duplicate output names even when no group produces a row.
        let mut output = Self::new(context, slots.as_slice(), capacity)?;
        let mut groups = QueryArena::<Group>::new(
            context.memory(),
            if keys.is_empty() { 1 } else { self.len() },
        )?;
        let mut links = QueryArena::new(context.memory(), self.len())?;
        let mut index = if keys.is_empty() {
            None
        } else {
            Some(Index::new(context, self.len())?)
        };
        if keys.is_empty() {
            groups.push(Group {
                first: usize::MAX,
                head: usize::MAX,
                tail: usize::MAX,
                count: 0,
            })?;
        }
        for row in 0..self.len() {
            context.charge(WorkKind::OperatorRows, 1)?;
            context.charge(WorkKind::RowsIn, 1)?;
            let raw = *self.order.as_slice().get(row).ok_or(RuntimeError::Batch)?;
            let group_index = if keys.is_empty() {
                0
            } else {
                context.charge(WorkKind::GroupKeys, 1)?;
                let index = index.as_mut().ok_or(RuntimeError::Batch)?;
                let (bucket, found) = index.locate(&self, raw, key_columns.as_slice(), context)?;
                if let Some(group) = found {
                    group
                } else {
                    let group = groups.len();
                    index.insert(bucket, &self, raw, key_columns.as_slice(), group, context)?;
                    groups.push(Group {
                        first: raw,
                        head: usize::MAX,
                        tail: usize::MAX,
                        count: 0,
                    })?;
                    group
                }
            };
            let group = groups
                .as_mut_slice()
                .get_mut(group_index)
                .ok_or(RuntimeError::Batch)?;
            if group.tail != usize::MAX {
                *links
                    .as_mut_slice()
                    .get_mut(group.tail)
                    .ok_or(RuntimeError::Batch)? = row;
            } else {
                group.head = row;
                group.first = raw;
            }
            group.tail = row;
            group.count += 1;
            links.push(usize::MAX)?;
        }
        drop(index);
        let mut values = RowBatch::storage(
            context,
            1,
            aggregates.len(),
            capacity.payload_bytes,
            capacity.variable,
        )?;
        for group in groups.as_slice() {
            context.checkpoint()?;
            values.clear();
            for (position, aggregate) in aggregates.iter().enumerate() {
                match aggregate.operation {
                    Aggregate::CountAll => values.push_row(
                        &[QueryValue::I64(
                            i64::try_from(group.count)
                                .map_err(|_| QueryError::ArithmeticOverflow)?,
                        )],
                        context,
                    )?,
                    Aggregate::Count { distinct, .. } | Aggregate::Collect { distinct, .. } => {
                        let column = operands
                            .as_slice()
                            .get(position)
                            .copied()
                            .flatten()
                            .ok_or(RuntimeError::Batch)?;
                        let mut selected = QueryArena::new(context.memory(), group.count)?;
                        let mut seen = if distinct {
                            Some(Index::new(context, group.count)?)
                        } else {
                            None
                        };
                        let mut row = group.head;
                        while row != usize::MAX {
                            context.charge(WorkKind::OperatorRows, 1)?;
                            let raw = *self.order.as_slice().get(row).ok_or(RuntimeError::Batch)?;
                            let value = self.cell(raw, column).ok_or(RuntimeError::Batch)?;
                            if !matches!(value, QueryValue::Null) {
                                let keep = if let Some(seen) = &mut seen {
                                    let (bucket, existing) =
                                        seen.locate(&self, raw, &[column], context)?;
                                    if existing.is_none() {
                                        seen.insert(
                                            bucket,
                                            &self,
                                            raw,
                                            &[column],
                                            selected.len(),
                                            context,
                                        )?;
                                    }
                                    existing.is_none()
                                } else {
                                    true
                                };
                                if keep {
                                    selected.push(raw)?;
                                }
                            }
                            row = *links.as_slice().get(row).ok_or(RuntimeError::Batch)?;
                        }
                        drop(seen);
                        if matches!(aggregate.operation, Aggregate::Count { .. }) {
                            values.push_row(
                                &[QueryValue::I64(
                                    i64::try_from(selected.len())
                                        .map_err(|_| QueryError::ArithmeticOverflow)?,
                                )],
                                context,
                            )?;
                        } else {
                            collect(&self, column, selected.as_slice(), &mut values, context)?;
                        }
                    }
                }
            }
            output.push_from(
                |column| {
                    if column < keys.len() {
                        self.cell(
                            group.first,
                            *key_columns
                                .as_slice()
                                .get(column)
                                .ok_or(RuntimeError::Batch)?,
                        )
                        .ok_or(RuntimeError::Batch)
                    } else {
                        values
                            .value(column - keys.len(), 0)
                            .ok_or(RuntimeError::Batch)
                    }
                },
                context,
            )?;
            if let Some(representatives) = &mut representatives {
                representatives.push((group.first != usize::MAX).then_some(group.first))?;
            }
        }
        Ok((output, representatives))
    }
}
struct Gathered<'a, 'v, 'm, 'g> {
    rows: &'a Rows<'v, 'm, 'g>,
    selected: &'a [usize],
    column: usize,
}
impl std::fmt::Debug for Gathered<'_, '_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectedValues")
            .field("len", &self.selected.len())
            .finish()
    }
}
impl ListArena for Gathered<'_, '_, '_, '_> {
    fn value(&self, index: usize) -> Option<QueryValue<'_>> {
        self.rows.cell(*self.selected.get(index)?, self.column)
    }
}
fn collect<'v, 'm, 'g>(
    rows: &Rows<'v, 'm, 'g>,
    column: usize,
    selected: &[usize],
    output: &mut RowBatch<'v, 'm, 'g>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<(), RuntimeError> {
    if selected.len() > MAX_LIST_ELEMENTS {
        return Err(QueryError::ListLimit.into());
    }
    let gathered = Gathered {
        rows,
        selected,
        column,
    };
    let (mut elements, mut depth, mut bytes, mut entities, mut nodes) = (
        selected.len(),
        1,
        selected
            .len()
            .checked_mul(std::mem::size_of::<QueryValue<'_>>())
            .ok_or(QueryError::ListLimit)?,
        false,
        !selected.is_empty(),
    );
    for index in 0..selected.len() {
        context.checkpoint()?;
        let value = gathered.value(index).ok_or(RuntimeError::Batch)?;
        value.validate(context.values())?;
        entities |= value.view().is_some();
        nodes &= matches!(value, QueryValue::NodeRef(_));
        match value {
            QueryValue::List(list) => {
                elements = elements
                    .checked_add(list.elements())
                    .ok_or(QueryError::ListLimit)?;
                depth = depth.max(list.depth() + 1);
                bytes = bytes
                    .checked_add(list.borrowed_bytes())
                    .ok_or(QueryError::ListLimit)?;
            }
            QueryValue::String(text) => {
                bytes = bytes.checked_add(text.len()).ok_or(QueryError::ListLimit)?
            }
            _ => {}
        }
        if elements > MAX_LIST_ELEMENTS || depth > MAX_LIST_DEPTH {
            return Err(QueryError::ListLimit.into());
        }
    }
    if nodes {
        let mut ids = QueryArena::new(context.memory(), selected.len())?;
        for index in 0..selected.len() {
            let Some(QueryValue::NodeRef(node)) = gathered.value(index) else {
                return Err(RuntimeError::Batch);
            };
            context.charge(WorkKind::CopiedBytes, 16)?;
            ids.push(node.id())?;
        }
        let list = QueryList::nodes(context.view(), ids.as_slice(), context.values())?;
        output.push_row(&[QueryValue::List(list)], context)
    } else {
        if bytes > MAX_QUERY_BYTES {
            return Err(QueryError::ListLimit.into());
        }
        let list = QueryList::arena(
            &gathered,
            0,
            selected.len(),
            elements,
            depth,
            bytes,
            if entities { Some(context.view()) } else { None },
        );
        output.push_row(&[QueryValue::List(list)], context)
    }
}
