//! Validated borrowed query lists. No nested value is persisted by this module.
use super::context::MAX_QUERY_BYTES;
use super::{QueryError, QueryValue, QueryView, ValueContext};
use crate::property_graph::{NodeId, PropertyData, PropertyValue, RelId};
/// Shared total logical element bound, including nested descendants.
pub const MAX_LIST_ELEMENTS: usize = 524_288;
/// Shared list nesting bound.
pub const MAX_LIST_DEPTH: u8 = 16;

/// Immutable caller-backed list with checked cached geometry.
/// Backing capacity belongs to the caller's reservation; this wrapper allocates
/// nothing. Runtime ownership transfer must charge actual retained capacity.
#[derive(Clone, Copy, Debug)]
pub struct QueryList<'a> {
    values: Backing<'a>,
    view: Option<&'a QueryView>,
    elements: usize,
    depth: u8,
    bytes: usize,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Backing<'a> {
    Values(&'a [QueryValue<'a>]),
    Nodes(&'a [NodeId]),
    Relationships(&'a [RelId]),
    Strings(&'a [&'a str]),
    Bools(&'a [bool]),
    Integers(&'a [i64]),
    Floats(&'a [f64]),
    Arena {
        source: &'a dyn ListArena,
        start: usize,
        len: usize,
    },
}
pub(super) trait ListArena: std::fmt::Debug {
    fn value(&self, index: usize) -> Option<QueryValue<'_>>;
}
impl<'a> QueryList<'a> {
    #[allow(
        dead_code,
        reason = "ZE-145's retained parameter proof is consumed with its later evaluator integration"
    )]
    pub(super) const fn backing(self) -> Backing<'a> {
        self.values
    }

    #[allow(
        dead_code,
        reason = "ZE-145's scratch-list adoption is consumed with its later evaluator integration"
    )]
    pub(super) fn arena_descriptor(
        self,
        expected: &dyn ListArena,
    ) -> Option<(usize, usize, usize, u8, usize, bool)> {
        let Backing::Arena { source, start, len } = self.values else {
            return None;
        };
        if !std::ptr::eq(source, expected) {
            return None;
        }
        Some((
            start,
            len,
            self.elements,
            self.depth,
            self.bytes,
            self.view.is_some(),
        ))
    }
    pub(super) fn arena(
        source: &'a dyn ListArena,
        start: usize,
        len: usize,
        elements: usize,
        depth: u8,
        bytes: usize,
        view: Option<&'a QueryView>,
    ) -> Self {
        Self {
            values: Backing::Arena { source, start, len },
            view,
            elements,
            depth,
            bytes,
        }
    }
    pub(super) fn copied_nodes(view: &'a QueryView, ids: &'a [NodeId]) -> Self {
        Self {
            values: Backing::Nodes(ids),
            view: Some(view),
            elements: ids.len(),
            depth: 1,
            bytes: std::mem::size_of_val(ids),
        }
    }
    pub(super) fn copied_relationships(view: &'a QueryView, ids: &'a [RelId]) -> Self {
        Self {
            values: Backing::Relationships(ids),
            view: Some(view),
            elements: ids.len(),
            depth: 1,
            bytes: std::mem::size_of_val(ids),
        }
    }
    pub(super) const fn node_ids(self) -> Option<&'a [NodeId]> {
        if let Backing::Nodes(v) = self.values {
            Some(v)
        } else {
            None
        }
    }
    pub(super) const fn relationship_ids(self) -> Option<&'a [RelId]> {
        if let Backing::Relationships(v) = self.values {
            Some(v)
        } else {
            None
        }
    }

    /// Checks the entire heterogeneous input before returning a list.
    pub fn new(
        values: &'a [QueryValue<'a>],
        context: &mut ValueContext<'_>,
    ) -> Result<Self, QueryError> {
        let mut view = None;
        let mut elements = values.len();
        let mut depth = 1;
        let mut bytes = std::mem::size_of_val(values);
        if elements > MAX_LIST_ELEMENTS || bytes > MAX_QUERY_BYTES {
            return Err(QueryError::ListLimit);
        }
        context.checkpoint()?;
        for value in values {
            context.step()?;
            value.validate(context)?;
            if let Some(owner) = value.view() {
                view = Some(owner);
            }
            match value {
                QueryValue::List(list) => {
                    elements = elements
                        .checked_add(list.elements)
                        .ok_or(QueryError::ListLimit)?;
                    depth = depth.max(list.depth + 1);
                    bytes = bytes.checked_add(list.bytes).ok_or(QueryError::ListLimit)?;
                }
                QueryValue::String(text) => {
                    bytes = bytes.checked_add(text.len()).ok_or(QueryError::ListLimit)?
                }
                _ => {}
            }
            if elements > MAX_LIST_ELEMENTS || depth > MAX_LIST_DEPTH || bytes > MAX_QUERY_BYTES {
                return Err(QueryError::ListLimit);
            }
        }
        Ok(Self {
            values: Backing::Values(values),
            view,
            elements,
            depth,
            bytes,
        })
    }
    pub(super) fn property(value: PropertyValue<'a>) -> Result<Self, QueryError> {
        let (values, descriptors) = match value.data() {
            PropertyData::EmptyList { count: 0 } => (Backing::Values(&[]), 0),
            PropertyData::Strings(v) => (Backing::Strings(v), std::mem::size_of_val(v)),
            PropertyData::Bools(v) => (Backing::Bools(v), 0),
            PropertyData::Integers(v) => (Backing::Integers(v), 0),
            PropertyData::Floats(v) => (Backing::Floats(v), 0),
            _ => return Err(QueryError::Type),
        };
        let bytes = descriptors
            .checked_add(value.payload_bytes())
            .ok_or(QueryError::ListLimit)?;
        if bytes > MAX_QUERY_BYTES {
            return Err(QueryError::ListLimit);
        }
        Ok(Self {
            values,
            view: None,
            elements: value.list_len().ok_or(QueryError::Type)?,
            depth: 1,
            bytes,
        })
    }
    /// Borrows packed full-width node IDs under one shared view token.
    pub fn nodes(
        view: &'a QueryView,
        ids: &'a [NodeId],
        context: &mut ValueContext<'_>,
    ) -> Result<Self, QueryError> {
        Self::packed(
            view,
            Backing::Nodes(ids),
            ids.len(),
            std::mem::size_of_val(ids),
            context,
        )
    }
    /// Borrows packed relationship IDs without per-element value descriptors.
    pub fn relationships(
        view: &'a QueryView,
        ids: &'a [RelId],
        context: &mut ValueContext<'_>,
    ) -> Result<Self, QueryError> {
        Self::packed(
            view,
            Backing::Relationships(ids),
            ids.len(),
            std::mem::size_of_val(ids),
            context,
        )
    }
    fn packed(
        view: &'a QueryView,
        values: Backing<'a>,
        elements: usize,
        bytes: usize,
        context: &mut ValueContext<'_>,
    ) -> Result<Self, QueryError> {
        context.step()?;
        if !std::ptr::eq(view, context.view) {
            return Err(QueryError::ForeignView);
        }
        if elements > MAX_LIST_ELEMENTS || bytes > MAX_QUERY_BYTES {
            return Err(QueryError::ListLimit);
        }
        Ok(Self {
            values,
            view: Some(view),
            elements,
            depth: 1,
            bytes,
        })
    }
    pub(super) const fn view(self) -> Option<&'a QueryView> {
        self.view
    }
    /// Immediate child count, not the nested element count.
    pub const fn len(self) -> usize {
        match self.values {
            Backing::Values(v) => v.len(),
            Backing::Nodes(v) => v.len(),
            Backing::Relationships(v) => v.len(),
            Backing::Strings(v) => v.len(),
            Backing::Bools(v) => v.len(),
            Backing::Integers(v) => v.len(),
            Backing::Floats(v) => v.len(),
            Backing::Arena { len, .. } => len,
        }
    }
    /// Whether this list has zero immediate children.
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// Total descendants, counting each logical element occurrence.
    pub const fn elements(self) -> usize {
        self.elements
    }
    /// Maximum nesting, with an empty list at depth one.
    pub const fn depth(self) -> u8 {
        self.depth
    }
    /// Logical borrowed descriptor/payload span, not a capacity reservation.
    pub const fn borrowed_bytes(self) -> usize {
        self.bytes
    }
    /// Checked element access preserves the caller-backed lifetime.
    pub fn get(self, index: usize) -> Option<QueryValue<'a>> {
        match self.values {
            Backing::Values(values) => values.get(index).copied(),
            Backing::Strings(values) => values.get(index).map(|value| QueryValue::String(value)),
            Backing::Bools(values) => values.get(index).copied().map(QueryValue::Bool),
            Backing::Integers(values) => values.get(index).copied().map(QueryValue::I64),
            Backing::Floats(values) => values.get(index).copied().map(QueryValue::F64),
            Backing::Nodes(ids) => Some(self.view?.node(*ids.get(index)?)),
            Backing::Relationships(ids) => Some(self.view?.relationship(*ids.get(index)?)),
            Backing::Arena { source, start, len } => {
                if index < len {
                    source.value(start.checked_add(index)?)
                } else {
                    None
                }
            }
        }
    }
}
