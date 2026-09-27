use super::*;
use std::cmp::Ordering;

/// One explicit ordering key. Descending reverses the full query value order.
#[derive(Clone, Copy)]
pub struct OrderKey {
    /// Already evaluated input key slot, possibly hidden by a later projection.
    pub slot: SlotId,
    /// Reverse supported-type/null/numeric order when true.
    pub descending: bool,
}
#[derive(Clone, Copy)]
struct Bucket {
    hash: u64,
    row: usize,
    value: usize,
}
pub(super) struct Index<'m, 'g> {
    buckets: QueryArena<'m, 'g, Bucket>,
}
impl<'m, 'g> Index<'m, 'g> {
    pub(super) fn new(
        context: &RuntimeContext<'_, 'm, 'g>,
        rows: usize,
    ) -> Result<Self, RuntimeError> {
        let capacity = rows
            .max(1)
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or(RuntimeError::Batch)?;
        let mut buckets = QueryArena::new(context.memory(), capacity)?;
        for _ in 0..capacity {
            context.checkpoint()?;
            buckets.push(Bucket {
                hash: 0,
                row: usize::MAX,
                value: 0,
            })?;
        }
        Ok(Self { buckets })
    }
    pub(super) fn locate(
        &self,
        rows: &Rows<'_, '_, '_>,
        raw: usize,
        columns: &[usize],
        context: &mut RuntimeContext<'_, '_, '_>,
    ) -> Result<(usize, Option<usize>), RuntimeError> {
        let mut hash = 0x9e3779b97f4a7c15_u64;
        for column in columns {
            let value = rows.cell(raw, *column).ok_or(RuntimeError::Batch)?;
            hash = hash.rotate_left(13) ^ value.group_hash(context.values())?;
            hash = hash.wrapping_mul(0x9e3779b185ebca87);
        }
        let mask = self.buckets.len() - 1;
        let mut index = hash as usize & mask;
        for _ in 0..self.buckets.len() {
            context.charge(WorkKind::HashProbes, 1)?;
            let bucket = self
                .buckets
                .as_slice()
                .get(index)
                .ok_or(RuntimeError::Batch)?;
            if bucket.row == usize::MAX {
                return Ok((index, None));
            }
            if bucket.hash == hash {
                let mut equal = true;
                for column in columns {
                    if !rows
                        .cell(raw, *column)
                        .ok_or(RuntimeError::Batch)?
                        .equivalent(
                            rows.cell(bucket.row, *column).ok_or(RuntimeError::Batch)?,
                            context.values(),
                        )?
                    {
                        equal = false;
                        break;
                    }
                }
                if equal {
                    return Ok((index, Some(bucket.value)));
                }
            }
            index = (index + 1) & mask;
        }
        Err(RuntimeError::Batch)
    }
    pub(super) fn insert(
        &mut self,
        bucket: usize,
        rows: &Rows<'_, '_, '_>,
        raw: usize,
        columns: &[usize],
        value: usize,
        context: &mut RuntimeContext<'_, '_, '_>,
    ) -> Result<(), RuntimeError> {
        let mut hash = 0x9e3779b97f4a7c15_u64;
        for column in columns {
            hash = (hash.rotate_left(13)
                ^ rows
                    .cell(raw, *column)
                    .ok_or(RuntimeError::Batch)?
                    .group_hash(context.values())?)
            .wrapping_mul(0x9e3779b185ebca87);
        }
        *self
            .buckets
            .as_mut_slice()
            .get_mut(bucket)
            .ok_or(RuntimeError::Batch)? = Bucket {
            hash,
            row: raw,
            value,
        };
        Ok(())
    }
}

impl<'v, 'm, 'g> Rows<'v, 'm, 'g> {
    /// Hash DISTINCT over every output column with mandatory collision equality.
    /// Consumes the owner: a failure cannot return a partially reduced bag.
    pub fn distinct(
        mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, RuntimeError> {
        if !self.belongs_to(context) {
            return Err(RuntimeError::Batch);
        }
        let mut columns = QueryArena::new(context.memory(), self.schema.slots().len())?;
        for column in 0..self.schema.slots().len() {
            columns.push(column)?;
        }
        let mut index = Index::new(context, self.len())?;
        let mut kept = 0;
        for row in 0..self.len() {
            context.charge(WorkKind::OperatorRows, 1)?;
            context.charge(WorkKind::RowsIn, 1)?;
            let raw = *self.order.as_slice().get(row).ok_or(RuntimeError::Batch)?;
            let (bucket, existing) = index.locate(&self, raw, columns.as_slice(), context)?;
            if existing.is_none() {
                index.insert(bucket, &self, raw, columns.as_slice(), kept, context)?;
                context.charge(WorkKind::CopiedBytes, std::mem::size_of::<usize>() as u64)?;
                *self
                    .order
                    .as_mut_slice()
                    .get_mut(kept)
                    .ok_or(RuntimeError::Batch)? = raw;
                kept += 1;
            }
        }
        self.order.truncate(kept);
        Ok(self)
    }
    /// Stable bounded merge sort. Scratch, old order and payload capacities
    /// remain simultaneously charged; comparisons and copies are cancellable.
    pub fn sort(
        mut self,
        keys: &[OrderKey],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, RuntimeError> {
        context.checkpoint()?;
        if !self.belongs_to(context) || keys.len() > 256 {
            return Err(RuntimeError::Batch);
        }
        let mut columns = QueryArena::new(context.memory(), keys.len())?;
        for key in keys {
            context.checkpoint()?;
            columns.push((self.schema.column(key.slot)?, key.descending))?;
        }
        let mut scratch = QueryArena::new(context.memory(), self.len())?;
        for raw in self.order.as_slice() {
            context.charge(WorkKind::OperatorRows, 1)?;
            context.charge(WorkKind::RowsIn, 1)?;
            context.charge(WorkKind::CopiedBytes, std::mem::size_of::<usize>() as u64)?;
            scratch.push(*raw)?;
        }
        let length = self.len();
        let mut width = 1usize;
        while width < length {
            let mut start = 0;
            while start < length {
                let mid = start.saturating_add(width).min(length);
                let end = mid.saturating_add(width).min(length);
                let (mut left, mut right) = (start, mid);
                for output in start..end {
                    context.checkpoint()?;
                    let choose_left = right == end
                        || (left < mid
                            && compare(
                                &self,
                                *self.order.as_slice().get(left).ok_or(RuntimeError::Batch)?,
                                *self
                                    .order
                                    .as_slice()
                                    .get(right)
                                    .ok_or(RuntimeError::Batch)?,
                                columns.as_slice(),
                                context,
                            )? != Ordering::Greater);
                    let from = if choose_left {
                        let old = left;
                        left += 1;
                        old
                    } else {
                        let old = right;
                        right += 1;
                        old
                    };
                    context.charge(WorkKind::CopiedBytes, std::mem::size_of::<usize>() as u64)?;
                    *scratch
                        .as_mut_slice()
                        .get_mut(output)
                        .ok_or(RuntimeError::Batch)? =
                        *self.order.as_slice().get(from).ok_or(RuntimeError::Batch)?;
                }
                start = end;
            }
            std::mem::swap(&mut self.order, &mut scratch);
            width = width.saturating_mul(2);
        }
        Ok(self)
    }
}
fn compare(
    data: &Rows<'_, '_, '_>,
    left: usize,
    right: usize,
    columns: &[(usize, bool)],
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<Ordering, RuntimeError> {
    for (column, descending) in columns {
        let order = data.cell(left, *column).ok_or(RuntimeError::Batch)?.order(
            data.cell(right, *column).ok_or(RuntimeError::Batch)?,
            context.values(),
        )?;
        let order = if *descending { order.reverse() } else { order };
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(Ordering::Equal)
}
