use super::*;
use crate::property_graph::query::{
    MAX_LIST_DEPTH, MAX_LIST_ELEMENTS, MAX_QUERY_BYTES, QueryError, QueryList, list::ListArena,
};

/// Accepted aggregation over already evaluated input cells.
#[derive(Clone, Copy)]
pub enum Aggregate {
    /// Checked numeric sum; nulls are skipped and empty input is zero.
    Sum {
        /// Already evaluated operand.
        slot: SlotId,
        /// Deduplicate operands.
        distinct: bool,
    },
    /// Least non-null value under query ordering.
    Min {
        /// Already evaluated operand.
        slot: SlotId,
    },
    /// Greatest non-null value under query ordering.
    Max {
        /// Already evaluated operand.
        slot: SlotId,
    },
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
impl<'v, 'm, 'g> Rows<'v, 'm, 'g> {
    /// Reduces materialized input with the same incremental kernel used by
    /// streaming producers. Empty global input produces one aggregate row.
    pub fn aggregate(
        self,
        keys: &[SlotProjection],
        aggregates: &[AggregateColumn],
        capacity: StorageCapacity,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, RuntimeError> {
        if !self.belongs_to(context)
            || keys
                .len()
                .checked_add(aggregates.len())
                .is_none_or(|width| width > 256)
        {
            return Err(RuntimeError::Batch);
        }
        let representatives = Self::new(context, self.schema().slots(), self.capacity)?;
        let mut state =
            streaming::StreamAggregate::new(representatives, keys, aggregates, capacity, context)?;
        let mut values = QueryArena::new(context.memory(), self.schema().slots().len())?;
        for row in 0..self.len() {
            context.charge(WorkKind::OperatorRows, 1)?;
            context.charge(WorkKind::RowsIn, 1)?;
            values.clear();
            for column in 0..self.schema().slots().len() {
                values.push(self.value(row, column).ok_or(RuntimeError::Batch)?)?;
            }
            state.push(values.as_slice(), aggregates, context)?;
        }
        state.finish(keys, aggregates, context)
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
pub(super) fn collect<'v, 'm, 'g>(
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
