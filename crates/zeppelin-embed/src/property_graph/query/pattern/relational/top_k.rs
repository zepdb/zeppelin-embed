use super::super::super::relational::streaming::{Growing, own_row};
use super::*;
use std::cmp::Ordering;

struct Kept<'v, 'm, 'g> {
    visible: RowBatch<'v, 'm, 'g>,
    keys: RowBatch<'v, 'm, 'g>,
    uses: QueryArena<'m, 'g, RelationshipUse>,
    ordinal: u64,
}
pub(super) struct TopRows<'v, 'm, 'g> {
    rows: Growing<'m, 'g, Kept<'v, 'm, 'g>>,
    limit: usize,
    ordinal: u64,
}
impl<'v, 'm, 'g> TopRows<'v, 'm, 'g> {
    pub(super) fn new(
        limit: usize,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            rows: Growing::new(context)?,
            limit,
            ordinal: 0,
        })
    }
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
    fn compare(
        &self,
        left: usize,
        right: usize,
        order: &[OrderKey],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Ordering, RuntimeError> {
        let left = self.rows.get(left)?;
        let right = self.rows.get(right)?;
        for (column, key) in order.iter().enumerate() {
            let cmp = left
                .keys
                .value(0, column)
                .ok_or(RuntimeError::Batch)?
                .order(
                    right.keys.value(0, column).ok_or(RuntimeError::Batch)?,
                    context.values(),
                )?;
            let cmp = if key.descending { cmp.reverse() } else { cmp };
            if cmp != Ordering::Equal {
                return Ok(cmp);
            }
        }
        Ok(left.ordinal.cmp(&right.ordinal))
    }
    pub(super) fn offer(
        &mut self,
        visible: &[QueryValue<'_>],
        keys: &[QueryValue<'_>],
        uses: &[RelationshipUse],
        order: &[OrderKey],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let ordinal = self.ordinal;
        self.ordinal = self.ordinal.checked_add(1).ok_or(RuntimeError::Batch)?;
        if self.limit == 0 {
            return Ok(());
        }
        if self.rows.len() == self.limit {
            let worst = self.rows.get(0)?;
            let mut comparison = Ordering::Equal;
            for (column, key) in order.iter().enumerate() {
                comparison = keys.get(column).ok_or(RuntimeError::Batch)?.order(
                    worst.keys.value(0, column).ok_or(RuntimeError::Batch)?,
                    context.values(),
                )?;
                if key.descending {
                    comparison = comparison.reverse();
                }
                if comparison != Ordering::Equal {
                    break;
                }
            }
            // On ties the earlier row wins, including its relationship sidecar.
            if comparison != Ordering::Less {
                return Ok(());
            }
        }
        let mut owned_uses = QueryArena::new(context.memory(), uses.len())?;
        owned_uses.extend_copy(uses)?;
        let row = Kept {
            visible: own_row(visible, context)?,
            keys: own_row(keys, context)?,
            uses: owned_uses,
            ordinal,
        };
        if self.rows.len() < self.limit {
            self.rows.push(row, context)?;
            let mut child = self.rows.len() - 1;
            while child > 0 {
                let parent = (child - 1) / 2;
                if self.compare(child, parent, order, context)? != Ordering::Greater {
                    break;
                }
                self.rows.swap(child, parent)?;
                child = parent;
            }
        } else {
            *self.rows.get_mut(0)? = row;
            self.sift(self.rows.len(), order, context)?;
        }
        Ok(())
    }
    fn sift(
        &mut self,
        len: usize,
        order: &[OrderKey],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let mut root = 0;
        loop {
            let mut child = root * 2 + 1;
            if child >= len {
                return Ok(());
            }
            if child + 1 < len
                && self.compare(child + 1, child, order, context)? == Ordering::Greater
            {
                child += 1;
            }
            if self.compare(child, root, order, context)? != Ordering::Greater {
                return Ok(());
            }
            self.rows.swap(child, root)?;
            root = child;
        }
    }
    pub(super) fn sort(
        &mut self,
        order: &[OrderKey],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        for end in (1..self.rows.len()).rev() {
            self.rows.swap(0, end)?;
            self.sift(end, order, context)?;
        }
        Ok(())
    }
    pub(super) fn emit(
        &self,
        row: usize,
        output: &mut RowBatch<'v, 'm, 'g>,
        uses: &mut QueryArena<'m, 'g, RelationshipUse>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let kept = self.rows.get(row)?;
        output.push_from(
            |column| kept.visible.value(0, column).ok_or(RuntimeError::Batch),
            context,
        )?;
        copy_relationship_uses_slice(kept.uses.as_slice(), uses)
    }
    pub(super) fn reset(
        &mut self,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        self.rows = Growing::new(context)?;
        self.ordinal = 0;
        Ok(())
    }
}
