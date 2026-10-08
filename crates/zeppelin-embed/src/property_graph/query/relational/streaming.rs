//! Incremental equivalence index. Only first representatives are retained.
use super::*;

#[derive(Clone, Copy)]
struct Entry {
    hash: u64,
    row: usize,
}
pub(crate) struct StreamIndex<'m, 'g> {
    buckets: QueryArena<'m, 'g, Option<Entry>>,
    len: usize,
}
impl<'m, 'g> StreamIndex<'m, 'g> {
    pub(crate) fn new(context: &RuntimeContext<'_, 'm, 'g>) -> Result<Self, RuntimeError> {
        Ok(Self {
            buckets: Self::buckets(16, context)?,
            len: 0,
        })
    }
    fn buckets(
        size: usize,
        context: &RuntimeContext<'_, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, Option<Entry>>, RuntimeError> {
        let mut buckets = QueryArena::new(context.memory(), size)?;
        for _ in 0..size {
            buckets.push(None)?;
        }
        Ok(buckets)
    }
    pub(crate) fn locate(
        &self,
        values: &[QueryValue<'_>],
        rows: &Rows<'_, '_, '_>,
        columns: &[usize],
        context: &mut RuntimeContext<'_, 'm, 'g>,
    ) -> Result<(u64, Option<usize>), RuntimeError> {
        let mut hash = 0x9e3779b97f4a7c15_u64;
        for column in columns {
            hash = (hash.rotate_left(13)
                ^ values
                    .get(*column)
                    .ok_or(RuntimeError::Batch)?
                    .group_hash(context.values())?)
            .wrapping_mul(0x9e3779b185ebca87);
        }
        let mut bucket = hash as usize & (self.buckets.len() - 1);
        for _ in 0..self.buckets.len() {
            context.charge(WorkKind::HashProbes, 1)?;
            let Some(entry) = self
                .buckets
                .as_slice()
                .get(bucket)
                .ok_or(RuntimeError::Batch)?
            else {
                return Ok((hash, None));
            };
            if entry.hash == hash {
                let mut equal = true;
                for column in columns {
                    if !values.get(*column).ok_or(RuntimeError::Batch)?.equivalent(
                        rows.value(entry.row, *column).ok_or(RuntimeError::Batch)?,
                        context.values(),
                    )? {
                        equal = false;
                        break;
                    }
                }
                if equal {
                    return Ok((hash, Some(entry.row)));
                }
            }
            bucket = (bucket + 1) & (self.buckets.len() - 1);
        }
        Err(RuntimeError::Batch)
    }
    pub(crate) fn insert(
        &mut self,
        hash: u64,
        row: usize,
        context: &mut RuntimeContext<'_, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        if self.len == self.buckets.len() / 2 {
            let mut replacement = Self::buckets(
                self.buckets
                    .len()
                    .checked_mul(2)
                    .ok_or(RuntimeError::Batch)?,
                context,
            )?;
            for entry in self.buckets.as_slice().iter().flatten() {
                Self::place(&mut replacement, *entry, context)?;
            }
            self.buckets = replacement;
        }
        Self::place(&mut self.buckets, Entry { hash, row }, context)?;
        self.len += 1;
        Ok(())
    }
    fn place(
        buckets: &mut QueryArena<'m, 'g, Option<Entry>>,
        entry: Entry,
        context: &mut RuntimeContext<'_, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let mut bucket = entry.hash as usize & (buckets.len() - 1);
        for _ in 0..buckets.len() {
            context.charge(WorkKind::HashProbes, 1)?;
            let slot = buckets
                .as_mut_slice()
                .get_mut(bucket)
                .ok_or(RuntimeError::Batch)?;
            if slot.is_none() {
                *slot = Some(entry);
                return Ok(());
            }
            bucket = (bucket + 1) & (buckets.len() - 1);
        }
        Err(RuntimeError::Batch)
    }
}

/// Explicit reserve-before-growth owner; unlike QueryArena, this owner moves
/// initialized entries into a separately charged replacement on growth.
pub(crate) struct Growing<'m, 'g, T> {
    entries: QueryArena<'m, 'g, Option<T>>,
}
impl<'m, 'g, T> Growing<'m, 'g, T> {
    pub(crate) fn new(context: &RuntimeContext<'_, 'm, 'g>) -> Result<Self, RuntimeError> {
        Ok(Self {
            entries: QueryArena::new(context.memory(), 0)?,
        })
    }
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn swap(&mut self, left: usize, right: usize) -> Result<(), RuntimeError> {
        if left >= self.len() || right >= self.len() {
            return Err(RuntimeError::Batch);
        }
        self.entries.as_mut_slice().swap(left, right);
        Ok(())
    }
    pub(crate) fn get(&self, i: usize) -> Result<&T, RuntimeError> {
        self.entries
            .as_slice()
            .get(i)
            .and_then(Option::as_ref)
            .ok_or(RuntimeError::Batch)
    }
    pub(crate) fn get_mut(&mut self, i: usize) -> Result<&mut T, RuntimeError> {
        self.entries
            .as_mut_slice()
            .get_mut(i)
            .and_then(Option::as_mut)
            .ok_or(RuntimeError::Batch)
    }
    pub(crate) fn push(
        &mut self,
        value: T,
        context: &RuntimeContext<'_, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        if self.entries.len() == self.entries.capacity() {
            let mut replacement = QueryArena::new(
                context.memory(),
                self.entries
                    .len()
                    .max(4)
                    .checked_mul(2)
                    .ok_or(RuntimeError::Batch)?,
            )?;
            for old in self.entries.as_mut_slice() {
                context.checkpoint()?;
                replacement.push(old.take())?;
            }
            self.entries = replacement;
        }
        self.entries.push(Some(value))?;
        Ok(())
    }
}

/// Copies just the live value backing, so one retained key never reserves a
/// full scheduling batch's variable arenas.
pub(crate) fn own_row<'v, 'm, 'g>(
    values: &[QueryValue<'_>],
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<RowBatch<'v, 'm, 'g>, RuntimeError> {
    fn measure(
        value: QueryValue<'_>,
        capacity: &mut ArenaCapacity,
        context: &mut RuntimeContext<'_, '_, '_>,
    ) -> Result<(), RuntimeError> {
        context.checkpoint()?;
        match value {
            QueryValue::String(text) => {
                capacity.string_bytes = capacity
                    .string_bytes
                    .checked_add(text.len())
                    .ok_or(RuntimeError::Batch)?
            }
            QueryValue::List(list) => {
                capacity.list_cells = capacity
                    .list_cells
                    .checked_add(list.len())
                    .ok_or(RuntimeError::Batch)?;
                // Packed entity lists are copied into their specialized arenas.
                if let Some(ids) = list.node_ids() {
                    capacity.node_ids = capacity
                        .node_ids
                        .checked_add(ids.len())
                        .ok_or(RuntimeError::Batch)?;
                } else if let Some(ids) = list.relationship_ids() {
                    capacity.relationship_ids = capacity
                        .relationship_ids
                        .checked_add(ids.len())
                        .ok_or(RuntimeError::Batch)?;
                } else {
                    for i in 0..list.len() {
                        measure(list.get(i).ok_or(RuntimeError::Batch)?, capacity, context)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut capacity = ArenaCapacity::default();
    for value in values {
        value.validate(context.values())?;
        measure(*value, &mut capacity, context)?;
    }
    let mut row = RowBatch::storage(
        context,
        values.len(),
        1,
        super::super::MAX_QUERY_BYTES,
        capacity,
    )?;
    row.push_row(values, context)?;
    Ok(row)
}

struct Accumulator<'v, 'm, 'g> {
    count: i64,
    sum: QueryValue<'static>,
    extreme: Option<RowBatch<'v, 'm, 'g>>,
    values: Option<Rows<'v, 'm, 'g>>,
    distinct: Option<StreamIndex<'m, 'g>>,
}
/// Retains one input representative and a compact accumulator per group.
pub(crate) struct StreamAggregate<'v, 'm, 'g> {
    representatives: Rows<'v, 'm, 'g>,
    index: StreamIndex<'m, 'g>,
    columns: QueryArena<'m, 'g, usize>,
    operands: QueryArena<'m, 'g, Option<usize>>,
    groups: Growing<'m, 'g, QueryArena<'m, 'g, Accumulator<'v, 'm, 'g>>>,
    capacity: StorageCapacity,
}
impl<'v, 'm, 'g> StreamAggregate<'v, 'm, 'g> {
    pub(crate) fn new(
        input: Rows<'v, 'm, 'g>,
        keys: &[SlotProjection],
        aggregates: &[AggregateColumn],
        capacity: StorageCapacity,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, RuntimeError> {
        let mut columns = QueryArena::new(context.memory(), keys.len())?;
        for key in keys {
            columns.push(input.schema().column(key.source)?)?;
        }
        let mut operands = QueryArena::new(context.memory(), aggregates.len())?;
        for aggregate in aggregates {
            operands.push(match aggregate.operation {
                Aggregate::CountAll => None,
                Aggregate::Count { slot, .. }
                | Aggregate::Collect { slot, .. }
                | Aggregate::Sum { slot, .. }
                | Aggregate::Min { slot }
                | Aggregate::Max { slot } => Some(input.schema().column(slot)?),
            })?;
        }
        let mut result = Self {
            representatives: input,
            index: StreamIndex::new(context)?,
            columns,
            operands,
            groups: Growing::new(context)?,
            capacity,
        };
        if keys.is_empty() {
            result.add_group(aggregates, context)?;
        }
        Ok(result)
    }
    fn add_group(
        &mut self,
        aggregates: &[AggregateColumn],
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let mut group = QueryArena::new(context.memory(), aggregates.len())?;
        for aggregate in aggregates {
            let distinct = matches!(
                aggregate.operation,
                Aggregate::Count { distinct: true, .. }
                    | Aggregate::Collect { distinct: true, .. }
                    | Aggregate::Sum { distinct: true, .. }
            );
            let retain = distinct || matches!(aggregate.operation, Aggregate::Collect { .. });
            group.push(Accumulator {
                count: 0,
                sum: QueryValue::I64(0),
                extreme: None,
                values: if retain {
                    let mut capacity = self.representatives.capacity;
                    if matches!(aggregate.operation, Aggregate::Collect { .. })
                        && capacity.max_rows > capacity.rows
                    {
                        // A collect operand is a list member, not a returned row.
                        capacity.max_rows = super::super::MAX_LIST_ELEMENTS;
                    }
                    Some(Rows::new(context, &[SlotId(0)], capacity)?)
                } else {
                    None
                },
                distinct: if distinct {
                    Some(StreamIndex::new(context)?)
                } else {
                    None
                },
            })?;
        }
        self.groups.push(group, context)
    }
    /// Returns whether this row established the group's representative.
    pub(crate) fn push(
        &mut self,
        values: &[QueryValue<'_>],
        aggregates: &[AggregateColumn],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, RuntimeError> {
        let (group, fresh) = if self.columns.is_empty() {
            let fresh = self.representatives.is_empty();
            if fresh {
                self.representatives.push(values, context)?;
            }
            (0, fresh)
        } else {
            context.charge(WorkKind::GroupKeys, 1)?;
            let (hash, found) = self.index.locate(
                values,
                &self.representatives,
                self.columns.as_slice(),
                context,
            )?;
            match found {
                Some(group) => (group, false),
                None => {
                    let group = self.groups.len();
                    self.representatives.push(values, context)?;
                    self.index.insert(hash, group, context)?;
                    self.add_group(aggregates, context)?;
                    (group, true)
                }
            }
        };
        let group = self.groups.get_mut(group)?;
        for (position, aggregate) in aggregates.iter().enumerate() {
            let accumulator = group
                .as_mut_slice()
                .get_mut(position)
                .ok_or(RuntimeError::Batch)?;
            let operand = self
                .operands
                .as_slice()
                .get(position)
                .ok_or(RuntimeError::Batch)?
                .map(|column| values.get(column).copied().ok_or(RuntimeError::Batch))
                .transpose()?;
            if matches!(operand, Some(QueryValue::Null)) {
                continue;
            }
            if let Some(rows) = &mut accumulator.values {
                let value = operand.ok_or(RuntimeError::Batch)?;
                if let Some(index) = &mut accumulator.distinct {
                    let (hash, found) = index.locate(&[value], rows, &[0], context)?;
                    if found.is_some() {
                        continue;
                    }
                    index.insert(hash, rows.len(), context)?;
                }
                rows.push(&[value], context)?;
            }
            match aggregate.operation {
                Aggregate::Sum { .. } => {
                    let value = accumulator.sum.arithmetic(
                        operand.ok_or(RuntimeError::Batch)?,
                        super::super::Arithmetic::Add,
                    )?;
                    accumulator.sum = match value {
                        QueryValue::I64(v) => QueryValue::I64(v),
                        QueryValue::F64(v) => QueryValue::F64(v),
                        _ => return Err(RuntimeError::Batch),
                    };
                }
                Aggregate::Min { .. } | Aggregate::Max { .. } => {
                    let value = operand.ok_or(RuntimeError::Batch)?;
                    let replace = if let Some(previous) = &accumulator.extreme {
                        let order = value.order(
                            previous.value(0, 0).ok_or(RuntimeError::Batch)?,
                            context.values(),
                        )?;
                        if matches!(aggregate.operation, Aggregate::Min { .. }) {
                            order.is_lt()
                        } else {
                            order.is_gt()
                        }
                    } else {
                        true
                    };
                    if replace {
                        accumulator.extreme = Some(own_row(&[value], context)?);
                    }
                }
                _ => {}
            }
            if matches!(
                aggregate.operation,
                Aggregate::CountAll | Aggregate::Count { .. }
            ) {
                accumulator.count = accumulator
                    .count
                    .checked_add(1)
                    .ok_or(super::super::QueryError::ArithmeticOverflow)?;
            }
        }
        Ok(fresh)
    }
    pub(crate) fn finish(
        self,
        keys: &[SlotProjection],
        aggregates: &[AggregateColumn],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Rows<'v, 'm, 'g>, RuntimeError> {
        let mut slots = QueryArena::new(context.memory(), keys.len() + aggregates.len())?;
        for key in keys {
            slots.push(key.output)?;
        }
        for aggregate in aggregates {
            slots.push(aggregate.output)?;
        }
        let mut output = Rows::new(context, slots.as_slice(), self.capacity)?;
        for group in 0..self.groups.len() {
            let mut values = RowBatch::storage(
                context,
                1,
                aggregates.len(),
                self.capacity.payload_bytes,
                self.capacity.variable,
            )?;
            for accumulator in self.groups.get(group)?.as_slice() {
                if let Some(rows) = &accumulator.values {
                    // Count DISTINCT needs its keys while consuming, not in the result.
                    let position = values.rows();
                    if matches!(
                        aggregates
                            .get(position)
                            .ok_or(RuntimeError::Batch)?
                            .operation,
                        Aggregate::Collect { .. }
                    ) {
                        let mut selected = QueryArena::new(context.memory(), rows.len())?;
                        for i in 0..rows.len() {
                            selected.push(i)?;
                        }
                        super::aggregate::collect(
                            rows,
                            0,
                            selected.as_slice(),
                            &mut values,
                            context,
                        )?;
                        continue;
                    }
                }
                let value = match aggregates
                    .get(values.rows())
                    .ok_or(RuntimeError::Batch)?
                    .operation
                {
                    Aggregate::Sum { .. } => accumulator.sum,
                    Aggregate::Min { .. } | Aggregate::Max { .. } => accumulator
                        .extreme
                        .as_ref()
                        .and_then(|row| row.value(0, 0))
                        .unwrap_or(QueryValue::Null),
                    _ => QueryValue::I64(accumulator.count),
                };
                values.push_row(&[value], context)?;
            }
            output.push_from(
                |column| {
                    if column < keys.len() {
                        self.representatives
                            .value(
                                group,
                                *self
                                    .columns
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
        }
        Ok(output)
    }
}
