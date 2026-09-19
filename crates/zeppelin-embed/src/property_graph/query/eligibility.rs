//! Execution owns eligibility once; retrieval borrows a checked same-view set.
use super::resources::{MemoryError, QueryArena, QueryReservation};
use super::runtime::{RuntimeContext, RuntimeError, WorkKind};
use super::{MAX_LIST_ELEMENTS, QueryError, QueryValue, QueryView};
use crate::property_graph::NodeId;

/// Absence of restriction is distinct from an explicitly empty materialized set.
pub enum Eligibility<'a, 'v, 'm, 'g> {
    /// Stream all indexed nodes; no implicit complete ID array is allocated.
    AllIndexed,
    /// Restrict to this immutable execution-owned set, even when empty.
    Set(&'a EligibleNodeSet<'v, 'm, 'g>),
}
/// One packed NodeId owner and its exact admitted view identity. Fields are
/// private; values cannot be reused merely because generation metadata matches.
pub struct EligibleNodeSet<'v, 'm, 'g> {
    view: &'v QueryView,
    ids: QueryArena<'m, 'g, NodeId>,
    _control: QueryReservation<'m, 'g>,
}
impl<'v, 'm, 'g> EligibleNodeSet<'v, 'm, 'g> {
    /// Builds the complete set or fails without returning a successful prefix.
    /// Capacity bounds materialized unique IDs. Every examined member is checked
    /// and charged, including duplicates after the arena becomes full. Producer
    /// list values separately retain their accepted per-value geometry limits.
    pub fn build<'a>(
        context: &mut RuntimeContext<'v, 'm, 'g>,
        capacity: usize,
        values: impl IntoIterator<Item = QueryValue<'a>>,
    ) -> Result<Self, RuntimeError> {
        context.checkpoint()?;
        if capacity > MAX_LIST_ELEMENTS {
            return Err(QueryError::ListLimit.into());
        }
        let control = context.memory().reserve(
            std::mem::size_of::<Self>() - std::mem::size_of::<QueryArena<'_, '_, NodeId>>(),
        )?;
        let mut ids = QueryArena::<NodeId>::new(context.memory(), capacity)?;
        let mut ordered = true;
        let table_capacity = capacity
            .max(1)
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or(MemoryError::Limit)?;
        let mut table = QueryArena::new(context.memory(), table_capacity)?;
        for _ in 0..table_capacity {
            context.checkpoint()?;
            table.push(usize::MAX)?;
        }
        for value in values {
            context.charge(WorkKind::OperatorRows, 1)?;
            context.charge(WorkKind::EligibilityEntries, 1)?;
            value.validate(context.values())?;
            let QueryValue::NodeRef(node) = value else {
                return Err(QueryError::Type.into());
            };
            let id = node.id();
            let mixed = xxhash_rust::xxh3::xxh3_64(&id.get().to_le_bytes());
            let mut bucket = mixed as usize & (table_capacity - 1);
            loop {
                context.charge(WorkKind::HashProbes, 1)?;
                let entry = *table.as_slice().get(bucket).ok_or(RuntimeError::Batch)?;
                if entry == usize::MAX {
                    if ids.len() == capacity {
                        return Err(MemoryError::Limit.into());
                    }
                    if let Some(previous) = ids.as_slice().last() {
                        context.values().step()?;
                        ordered &= *previous < id;
                    }
                    context.charge(WorkKind::CopiedBytes, 16)?;
                    *table
                        .as_mut_slice()
                        .get_mut(bucket)
                        .ok_or(RuntimeError::Batch)? = ids.len();
                    ids.push(id)?;
                    break;
                }
                if ids.as_slice().get(entry) == Some(&id) {
                    break;
                }
                bucket = (bucket + 1) & (table_capacity - 1);
            }
        }
        drop(table);
        // In-place heapsort needs no second ID arena; every comparison and move
        // remains cancellable and counted under the same execution.
        let length = ids.len();
        if !ordered {
            for root in (0..length / 2).rev() {
                sift(ids.as_mut_slice(), root, length, context)?;
            }
            for end in (1..length).rev() {
                swap(ids.as_mut_slice(), 0, end, context)?;
                sift(ids.as_mut_slice(), 0, end, context)?;
            }
        }
        context.checkpoint()?;
        Ok(Self {
            view: context.view(),
            ids,
            _control: control,
        })
    }
    /// Borrow sorted unique full IDs only for the exact originating view.
    pub fn ids_for(&self, view: &QueryView) -> Result<&[NodeId], QueryError> {
        if !std::ptr::eq(self.view, view) {
            return Err(QueryError::ForeignView);
        }
        Ok(self.ids.as_slice())
    }
    /// Actual retained packed-ID capacity, including unused slots.
    pub fn capacity(&self) -> usize {
        self.ids.capacity()
    }
}
fn sift(
    ids: &mut [NodeId],
    mut root: usize,
    length: usize,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), RuntimeError> {
    loop {
        let left = root
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(RuntimeError::Batch)?;
        if left >= length {
            break;
        }
        let mut child = left;
        if left + 1 < length {
            context.values().step()?;
            if ids.get(left).ok_or(RuntimeError::Batch)?
                < ids.get(left + 1).ok_or(RuntimeError::Batch)?
            {
                child = left + 1;
            }
        }
        context.values().step()?;
        if ids.get(root).ok_or(RuntimeError::Batch)? >= ids.get(child).ok_or(RuntimeError::Batch)? {
            break;
        }
        swap(ids, root, child, context)?;
        root = child;
    }
    Ok(())
}
fn swap(
    ids: &mut [NodeId],
    a: usize,
    b: usize,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), RuntimeError> {
    let av = *ids.get(a).ok_or(RuntimeError::Batch)?;
    let bv = *ids.get(b).ok_or(RuntimeError::Batch)?;
    context.charge(WorkKind::CopiedBytes, 32)?;
    *ids.get_mut(a).ok_or(RuntimeError::Batch)? = bv;
    *ids.get_mut(b).ok_or(RuntimeError::Batch)? = av;
    Ok(())
}
