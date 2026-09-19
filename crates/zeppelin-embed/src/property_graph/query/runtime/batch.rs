use super::{RuntimeContext, RuntimeError, WorkKind};
use crate::property_graph::query::resources::{QueryArena, QueryReservation};
use crate::property_graph::query::{QueryList, QueryValue, QueryView, list::ListArena};
use crate::property_graph::{NodeId, RelId};

/// Complete fixed variable-arena capacities (or initialized usage when reported).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArenaCapacity {
    /// UTF-8 string byte storage.
    pub string_bytes: usize,
    /// Heterogeneous/nested list descriptors, separate from root row cells.
    pub list_cells: usize,
    /// Packed 16-byte NodeIds, with one arena-owned view token.
    pub node_ids: usize,
    /// Packed 16-byte RelIds, with one arena-owned view token.
    pub relationship_ids: usize,
}
#[derive(Clone, Copy)]
enum Cell {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Node(NodeId),
    Relationship(RelId),
    String {
        start: usize,
        len: usize,
    },
    List {
        start: usize,
        len: usize,
        elements: usize,
        depth: u8,
        bytes: usize,
        entities: bool,
    },
    Nodes {
        start: usize,
        len: usize,
    },
    Relationships {
        start: usize,
        len: usize,
    },
}
impl Cell {
    fn value<'a>(self, arena: &'a VariableArena<'_, '_, '_>) -> Option<QueryValue<'a>> {
        Some(match self {
            Self::Null => QueryValue::Null,
            Self::Bool(v) => QueryValue::Bool(v),
            Self::I64(v) => QueryValue::I64(v),
            Self::F64(v) => QueryValue::F64(v),
            Self::Node(id) => arena.view.node(id),
            Self::Relationship(id) => arena.view.relationship(id),
            Self::String { start, len } => {
                let bytes = arena.bytes.as_slice().get(start..start.checked_add(len)?)?;
                // SAFETY: only copy_value creates private String cells, after
                // copying every byte of an existing valid &str contiguously.
                // No mutable byte slice escapes VariableArena; failed copies
                // discard their cells, and borrowing this value prevents arena
                // mutation. The exact checked range therefore remains UTF-8.
                // Revalidating here would scan an unbounded string on access.
                QueryValue::String(unsafe { std::str::from_utf8_unchecked(bytes) })
            }
            Self::List {
                start,
                len,
                elements,
                depth,
                bytes,
                entities,
            } => QueryValue::List(QueryList::arena(
                arena,
                start,
                len,
                elements,
                depth,
                bytes,
                if entities { Some(arena.view) } else { None },
            )),
            Self::Nodes { start, len } => QueryValue::List(QueryList::copied_nodes(
                arena.view,
                arena.nodes.as_slice().get(start..start.checked_add(len)?)?,
            )),
            Self::Relationships { start, len } => {
                QueryValue::List(QueryList::copied_relationships(
                    arena.view,
                    arena
                        .relationships
                        .as_slice()
                        .get(start..start.checked_add(len)?)?,
                ))
            }
        })
    }
}
struct VariableArena<'v, 'm, 'g> {
    view: &'v QueryView,
    children: QueryArena<'m, 'g, Cell>,
    bytes: QueryArena<'m, 'g, u8>,
    nodes: QueryArena<'m, 'g, NodeId>,
    relationships: QueryArena<'m, 'g, RelId>,
}
impl std::fmt::Debug for VariableArena<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryVariableArena").finish_non_exhaustive()
    }
}
impl ListArena for VariableArena<'_, '_, '_> {
    fn value(&self, index: usize) -> Option<QueryValue<'_>> {
        self.children.as_slice().get(index).copied()?.value(self)
    }
}
impl<'v, 'm, 'g> VariableArena<'v, 'm, 'g> {
    fn usage(&self) -> ArenaCapacity {
        ArenaCapacity {
            string_bytes: self.bytes.len(),
            list_cells: self.children.len(),
            node_ids: self.nodes.len(),
            relationship_ids: self.relationships.len(),
        }
    }
    fn truncate(&mut self, old: ArenaCapacity) {
        self.bytes.truncate(old.string_bytes);
        self.children.truncate(old.list_cells);
        self.nodes.truncate(old.node_ids);
        self.relationships.truncate(old.relationship_ids);
    }
    fn copy_value(
        &mut self,
        value: QueryValue<'_>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        payload: &mut usize,
        limit: usize,
        depth: u8,
    ) -> Result<Cell, RuntimeError> {
        context.values().step()?;
        value.validate(context.values())?;
        let scalar = match value {
            QueryValue::Null => Some((Cell::Null, 0)),
            QueryValue::Bool(v) => Some((Cell::Bool(v), 1)),
            QueryValue::I64(v) => Some((Cell::I64(v), 8)),
            QueryValue::F64(v) => Some((Cell::F64(v), 8)),
            QueryValue::NodeRef(v) => Some((Cell::Node(v.id()), 16)),
            QueryValue::RelRef(v) => Some((Cell::Relationship(v.id()), 16)),
            _ => None,
        };
        if let Some((cell, bytes)) = scalar {
            copied(context, payload, limit, bytes)?;
            return Ok(cell);
        }
        match value {
            QueryValue::String(text) => {
                if self
                    .bytes
                    .len()
                    .checked_add(text.len())
                    .is_none_or(|n| n > self.bytes.capacity())
                {
                    return Err(RuntimeError::Batch);
                }
                if payload.checked_add(text.len()).is_none_or(|n| n > limit) {
                    return Err(RuntimeError::Batch);
                }
                let start = self.bytes.len();
                for chunk in text.as_bytes().chunks(65536) {
                    context.checkpoint()?;
                    copied(context, payload, limit, chunk.len())?;
                    self.bytes.extend_copy(chunk)?;
                }
                Ok(Cell::String {
                    start,
                    len: text.len(),
                })
            }
            QueryValue::List(list) => {
                if depth >= super::super::MAX_LIST_DEPTH {
                    return Err(RuntimeError::Batch);
                }
                if let Some(ids) = list.node_ids() {
                    if self
                        .nodes
                        .len()
                        .checked_add(ids.len())
                        .is_none_or(|n| n > self.nodes.capacity())
                    {
                        return Err(RuntimeError::Batch);
                    }
                    let start = self.nodes.len();
                    for id in ids {
                        context.values().step()?;
                        copied(context, payload, limit, 16)?;
                        self.nodes.push(*id)?;
                    }
                    return Ok(Cell::Nodes {
                        start,
                        len: ids.len(),
                    });
                }
                if let Some(ids) = list.relationship_ids() {
                    if self
                        .relationships
                        .len()
                        .checked_add(ids.len())
                        .is_none_or(|n| n > self.relationships.capacity())
                    {
                        return Err(RuntimeError::Batch);
                    }
                    let start = self.relationships.len();
                    for id in ids {
                        context.values().step()?;
                        copied(context, payload, limit, 16)?;
                        self.relationships.push(*id)?;
                    }
                    return Ok(Cell::Relationships {
                        start,
                        len: ids.len(),
                    });
                }
                if self
                    .children
                    .len()
                    .checked_add(list.len())
                    .is_none_or(|n| n > self.children.capacity())
                {
                    return Err(RuntimeError::Batch);
                }
                let before = self.usage();
                let start = self.children.len();
                for _ in 0..list.len() {
                    context.values().step()?;
                    self.children.push(Cell::Null)?;
                }
                for index in 0..list.len() {
                    let child = self.copy_value(
                        list.get(index).ok_or(RuntimeError::Batch)?,
                        context,
                        payload,
                        limit,
                        depth + 1,
                    )?;
                    *self
                        .children
                        .as_mut_slice()
                        .get_mut(start + index)
                        .ok_or(RuntimeError::Batch)? = child;
                }
                let after = self.usage();
                let bytes = (after.list_cells - before.list_cells)
                    .checked_mul(std::mem::size_of::<Cell>())
                    .and_then(|n| n.checked_add(after.string_bytes - before.string_bytes))
                    .and_then(|n| n.checked_add((after.node_ids - before.node_ids) * 16))
                    .and_then(|n| {
                        n.checked_add((after.relationship_ids - before.relationship_ids) * 16)
                    })
                    .ok_or(RuntimeError::Batch)?;
                Ok(Cell::List {
                    start,
                    len: list.len(),
                    elements: list.elements(),
                    depth: list.depth(),
                    bytes,
                    entities: list.view().is_some(),
                })
            }
            _ => Err(RuntimeError::Batch),
        }
    }
}
fn copied(
    context: &mut RuntimeContext<'_, '_, '_>,
    payload: &mut usize,
    limit: usize,
    bytes: usize,
) -> Result<(), RuntimeError> {
    let next = payload.checked_add(bytes).ok_or(RuntimeError::Batch)?;
    if next > limit {
        return Err(RuntimeError::Batch);
    }
    context.charge(WorkKind::CopiedBytes, bytes as u64)?;
    *payload = next;
    Ok(())
}

#[derive(Clone, Copy)]
struct RowOffset {
    start: usize,
    payload_bytes: usize,
}

/// Private-execution flat rows. Public query success is owned by the drain driver,
/// not by reading an intermediate batch. Duplicate rows retain bag multiplicity.
pub struct RowBatch<'v, 'm, 'g> {
    view: &'v QueryView,
    cells: QueryArena<'m, 'g, Cell>,
    variable: VariableArena<'v, 'm, 'g>,
    offsets: QueryArena<'m, 'g, RowOffset>,
    columns: usize,
    max_rows: usize,
    payload_limit: usize,
    payload_bytes: usize,
    _control: QueryReservation<'m, 'g>,
}
impl<'v, 'm, 'g> RowBatch<'v, 'm, 'g> {
    /// Admits fixed cell/offset capacity and a separate initialized payload cap.
    /// Descriptor overhead is charged to query memory, not subtracted from payload.
    pub fn new(
        context: &RuntimeContext<'v, 'm, 'g>,
        columns: usize,
        max_rows: usize,
        payload_limit: usize,
    ) -> Result<Self, RuntimeError> {
        Self::with_arenas(
            context,
            columns,
            max_rows,
            payload_limit,
            ArenaCapacity::default(),
        )
    }
    /// Admits all variable backing at actual fixed capacities before pulling.
    pub fn with_arenas(
        context: &RuntimeContext<'v, 'm, 'g>,
        columns: usize,
        max_rows: usize,
        payload_limit: usize,
        capacity: ArenaCapacity,
    ) -> Result<Self, RuntimeError> {
        if max_rows == 0 || max_rows > 256 {
            return Err(RuntimeError::Batch);
        }
        Self::storage(context, columns, max_rows, payload_limit, capacity)
    }
    pub(super) fn storage(
        context: &RuntimeContext<'v, 'm, 'g>,
        columns: usize,
        max_rows: usize,
        payload_limit: usize,
        capacity: ArenaCapacity,
    ) -> Result<Self, RuntimeError> {
        context.checkpoint()?;
        if columns > 256 || max_rows > 65_536 || payload_limit > super::super::MAX_QUERY_BYTES {
            return Err(RuntimeError::Batch);
        }
        let bytes = std::mem::size_of::<Self>()
            - 2 * std::mem::size_of::<QueryArena<'_, '_, Cell>>()
            - std::mem::size_of::<QueryArena<'_, '_, RowOffset>>()
            - std::mem::size_of::<QueryArena<'_, '_, u8>>()
            - std::mem::size_of::<QueryArena<'_, '_, NodeId>>()
            - std::mem::size_of::<QueryArena<'_, '_, RelId>>();
        let control = context.memory().reserve(bytes)?;
        Ok(Self {
            view: context.view(),
            variable: VariableArena {
                view: context.view(),
                children: QueryArena::new(context.memory(), capacity.list_cells)?,
                bytes: QueryArena::new(context.memory(), capacity.string_bytes)?,
                nodes: QueryArena::new(context.memory(), capacity.node_ids)?,
                relationships: QueryArena::new(context.memory(), capacity.relationship_ids)?,
            },
            cells: QueryArena::new(context.memory(), columns * max_rows)?,
            offsets: QueryArena::new(context.memory(), max_rows)?,
            columns,
            max_rows,
            payload_limit,
            payload_bytes: 0,
            _control: control,
        })
    }
    /// Complete initialized rows only; partial row cells are never exposed.
    pub fn rows(&self) -> usize {
        self.offsets.len()
    }
    /// Fixed width of each flat row, including zero-column Unit bindings.
    pub const fn columns(&self) -> usize {
        self.columns
    }
    /// Exact currently initialized scalar/string/list payload, excluding controls.
    pub const fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }
    /// Returns a complete row cell under this execution's original view token.
    pub fn value(&self, row: usize, column: usize) -> Option<QueryValue<'_>> {
        if column >= self.columns {
            return None;
        }
        let start = self.offsets.as_slice().get(row)?.start;
        self.cells
            .as_slice()
            .get(start.checked_add(column)?)
            .copied()
            .and_then(|cell| cell.value(&self.variable))
    }
    /// Copies a complete row, preserving bags and full typed IDs. Failure rolls
    /// back its cells; the drain driver discards all earlier rows on query error.
    pub fn push_row(
        &mut self,
        row: &[QueryValue<'_>],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        context.checkpoint()?;
        if !std::ptr::eq(self.view, context.view())
            || !std::ptr::eq(self._control.owner(), context.memory())
            || row.len() != self.columns
            || self.rows() >= self.max_rows
        {
            return Err(RuntimeError::Batch);
        }
        let start = self.cells.len();
        let prior_bytes = self.payload_bytes;
        let prior_variable = self.variable.usage();
        let result = (|| {
            for value in row {
                let cell = self.variable.copy_value(
                    *value,
                    context,
                    &mut self.payload_bytes,
                    self.payload_limit,
                    0,
                )?;
                self.cells.push(cell)?;
            }
            context.checkpoint()?;
            context.charge(WorkKind::RowsOut, 1)?;
            self.offsets.push(RowOffset {
                start,
                payload_bytes: self.payload_bytes - prior_bytes,
            })?;
            Ok(())
        })();
        if result.is_err() {
            self.cells.truncate(start);
            self.payload_bytes = prior_bytes;
            self.variable.truncate(prior_variable);
        }
        result
    }
    pub(super) fn row_payload_bytes(&self, row: usize) -> Result<usize, RuntimeError> {
        Ok(self
            .offsets
            .as_slice()
            .get(row)
            .ok_or(RuntimeError::Batch)?
            .payload_bytes)
    }
    pub(super) fn copy_row(
        &mut self,
        source: &Self,
        row: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        context.checkpoint()?;
        if !std::ptr::eq(self.view, source.view)
            || self.columns != source.columns
            || row >= source.rows()
            || self.rows() >= self.max_rows
        {
            return Err(RuntimeError::Batch);
        }
        let start = self.cells.len();
        let prior_bytes = self.payload_bytes;
        for column in 0..self.columns {
            let value = source.value(row, column).ok_or(RuntimeError::Batch)?;
            let cell = self.variable.copy_value(
                value,
                context,
                &mut self.payload_bytes,
                self.payload_limit,
                0,
            )?;
            self.cells.push(cell)?;
        }
        self.offsets.push(RowOffset {
            start,
            payload_bytes: self.payload_bytes - prior_bytes,
        })?;
        Ok(())
    }
    /// Initialized variable-arena counts, distinct from their retained capacities.
    pub fn arena_usage(&self) -> ArenaCapacity {
        self.variable.usage()
    }
    /// Reuses admitted capacities after dropping all initialized rows.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.offsets.clear();
        self.payload_bytes = 0;
        self.variable.truncate(ArenaCapacity::default());
    }
}
