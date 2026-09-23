//! ZE-66 S3: typed `GraphStore::get_nodes`/`get_relationships`.
//!
//! Both calls admit exactly one native read lease (`Store::with_native_read`)
//! for the whole batch of requested identities, so every returned entity was
//! read against the same generation: this is the "one admitted generation for
//! the whole call" contract, not N independent point reads. The copy shape
//! mirrors ZE-53's `query::completed::{Node, Relationship, Property, Value}`
//! pooled representation (label/type names, key, properties, and for nodes,
//! explicitly selected text/vector) rather than inventing a new one; unlike a
//! query row this is a flat `Option<_>` per requested id, so there is no
//! column/cell/search apparatus to copy.
//!
//! Labels and properties are always copied; a node's stored text and vector
//! are opt-in through [`GraphGetOptions`] because reading them copies payload
//! bytes callers may not want. Copying preserves the exact absent-vs-empty
//! text distinction and the typed-empty-list-vs-`EmptyList`-vs-absent-property
//! distinction the stored canonical encoding already carries.

use super::{GraphStore, GraphStoreError};
use crate::lifecycle::QueryControl;
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::property_graph::catalog::Symbol;
use crate::property_graph::query::completed::{
    Key, ListKind, Node, Property, Relationship, Span, Value, ValueIndex,
};
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::records::{RecordShape, RecordView, StoredKey, StoredVector};
use crate::property_graph::storage::stream::{PayloadCursor, PayloadSlice};
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError, TreeResources};
use crate::property_graph::{GraphGeneration, MAX_GRAPH_CHANGES, NodeId, RelId};

/// Which optional node fields [`GraphStore::get_nodes`] copies. Labels, the
/// application key and properties are always copied; text and vector are
/// opt-in because reading them copies payload bytes, not just index rows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GraphGetOptions {
    /// Copy stored text, preserving absent versus present-empty.
    pub text: bool,
    /// Copy the stored vector's original finite f32 bits, when present.
    pub vector: bool,
}

/// Growable owned backing pools shared by one `get_nodes`/`get_relationships`
/// call. Unlike the internal query engine's `QueryArena`, these are ordinary
/// heap `Vec`s: a typed get is a bounded caller-sized batch, not an operator
/// in the admitted query budget, and the same trade-off already applies to
/// `GraphWriteResult`'s receipts (ZE-66 S1).
#[derive(Default)]
struct EntityPool {
    bytes: Vec<u8>,
    names: Vec<Span>,
    properties: Vec<Property>,
    values: Vec<Value>,
    children: Vec<ValueIndex>,
    vectors: Vec<u32>,
}

fn range(span: Span) -> std::ops::Range<usize> {
    let start = span.start as usize;
    start..start.saturating_add(span.len as usize)
}

fn span_slice<T>(pool: &[T], span: Span) -> &[T] {
    pool.get(range(span)).unwrap_or(&[])
}

/// Owned result of [`GraphStore::get_nodes`]. Nothing here borrows the store,
/// its admitted lease, or its generation; values stay valid after close.
pub struct GraphNodesResult {
    generation: GraphGeneration,
    nodes: Vec<Option<Node>>,
    pool: EntityPool,
}

impl GraphNodesResult {
    /// The single generation this call's one read admission observed. Every
    /// returned node was read against this same generation.
    #[must_use]
    pub const fn generation(&self) -> GraphGeneration {
        self.generation
    }
    /// One entry per requested id, in request order. `None` is a missing or
    /// deleted node, indistinguishable at this seam.
    #[must_use]
    pub fn nodes(&self) -> &[Option<Node>] {
        &self.nodes
    }
    /// A node's label names, in ascending UTF-8 byte order.
    #[must_use]
    pub fn labels(&self, node: &Node) -> &[Span] {
        span_slice(&self.pool.names, node.labels)
    }
    /// A node's or relationship's properties, in ascending name order.
    #[must_use]
    pub fn properties(&self, span: Span) -> &[Property] {
        span_slice(&self.pool.properties, span)
    }
    /// A copied scalar value, or a list's own descriptor.
    #[must_use]
    pub fn value(&self, index: ValueIndex) -> Option<&Value> {
        self.pool.values.get(index.0 as usize)
    }
    /// A list value's ordered child value indices.
    #[must_use]
    pub fn children(&self, span: Span) -> &[ValueIndex] {
        span_slice(&self.pool.children, span)
    }
    /// Exact owned UTF-8 bytes at `span`, including embedded NUL; a present
    /// empty span returns `Some("")`.
    #[must_use]
    pub fn string(&self, span: Span) -> Option<&str> {
        std::str::from_utf8(span_slice(&self.pool.bytes, span)).ok()
    }
    /// Original finite f32 coordinate bits at `span`, one `u32` per dimension.
    #[must_use]
    pub fn vector(&self, span: Span) -> &[u32] {
        span_slice(&self.pool.vectors, span)
    }
}

/// Owned result of [`GraphStore::get_relationships`]. See
/// [`GraphNodesResult`]; relationships never carry text or a vector.
pub struct GraphRelationshipsResult {
    generation: GraphGeneration,
    relationships: Vec<Option<Relationship>>,
    pool: EntityPool,
}

impl GraphRelationshipsResult {
    /// The single generation this call's one read admission observed.
    #[must_use]
    pub const fn generation(&self) -> GraphGeneration {
        self.generation
    }
    /// One entry per requested id, in request order. `None` is a missing or
    /// deleted relationship, indistinguishable at this seam.
    #[must_use]
    pub fn relationships(&self) -> &[Option<Relationship>] {
        &self.relationships
    }
    /// A relationship's properties, in ascending name order.
    #[must_use]
    pub fn properties(&self, span: Span) -> &[Property] {
        span_slice(&self.pool.properties, span)
    }
    /// A copied scalar value, or a list's own descriptor.
    #[must_use]
    pub fn value(&self, index: ValueIndex) -> Option<&Value> {
        self.pool.values.get(index.0 as usize)
    }
    /// A list value's ordered child value indices.
    #[must_use]
    pub fn children(&self, span: Span) -> &[ValueIndex] {
        span_slice(&self.pool.children, span)
    }
    /// Exact owned UTF-8 bytes at `span`, including embedded NUL.
    #[must_use]
    pub fn string(&self, span: Span) -> Option<&str> {
        std::str::from_utf8(span_slice(&self.pool.bytes, span)).ok()
    }
}

fn append_text(text: &str, bytes: &mut Vec<u8>) -> Result<Span, TreeError> {
    let start = u32::try_from(bytes.len()).map_err(|_| TreeError::Memory)?;
    bytes.extend_from_slice(text.as_bytes());
    Ok(Span::new(
        start,
        u32::try_from(text.len()).map_err(|_| TreeError::Memory)?,
    ))
}

fn append_payload<S: BlockSource>(
    payload: PayloadSlice<'_, S>,
    bytes: &mut Vec<u8>,
    resources: &mut TreeResources<'_>,
) -> Result<Span, TreeError> {
    let start = u32::try_from(bytes.len()).map_err(|_| TreeError::Memory)?;
    let len = usize::try_from(payload.len()).map_err(|_| TreeError::Memory)?;
    let base = bytes.len();
    bytes.resize(base.checked_add(len).ok_or(TreeError::Memory)?, 0);
    let target = bytes.get_mut(base..).ok_or(TreeError::Memory)?;
    let written = payload.read_at(0, target, resources)?;
    if written != len {
        return Err(TreeError::Invalid("typed get: short payload copy"));
    }
    Ok(Span::new(
        start,
        u32::try_from(len).map_err(|_| TreeError::Memory)?,
    ))
}

fn append_vector<S: BlockSource>(
    vector: StoredVector<'_, S>,
    vectors: &mut Vec<u32>,
    resources: &mut TreeResources<'_>,
) -> Result<Span, TreeError> {
    let start = u32::try_from(vectors.len()).map_err(|_| TreeError::Memory)?;
    for index in 0..vector.dimensions() {
        vectors.push(vector.coordinate(index, resources)?.to_bits());
    }
    Ok(Span::new(start, vector.dimensions()))
}

fn copy_key<S: BlockSource>(
    key: Option<StoredKey<'_, S>>,
    bytes: &mut Vec<u8>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<Key>, TreeError> {
    key.map(|key| {
        Ok(Key {
            kind: key.kind(),
            namespace: append_payload(key.namespace(), bytes, resources)?,
            value: append_payload(key.key(), bytes, resources)?,
        })
    })
    .transpose()
}

fn push_value(pool: &mut EntityPool, value: Value) -> Result<ValueIndex, TreeError> {
    let index = ValueIndex(u32::try_from(pool.values.len()).map_err(|_| TreeError::Memory)?);
    pool.values.push(value);
    Ok(index)
}

/// Decodes one ZGCIv1 stored property value: tag 1 string, 2 bool, 3 i64, 4
/// f64, 5 the untyped `EmptyList` sentinel (count must be zero), 6..9 typed
/// lists. This mirrors `query::completed::native::values::fill_stored`'s
/// decode exactly, over an owned `Vec` pool instead of a charged `QueryArena`.
fn decode_value<S: BlockSource>(
    payload: PayloadSlice<'_, S>,
    pool: &mut EntityPool,
    resources: &mut TreeResources<'_>,
) -> Result<ValueIndex, TreeError> {
    let mut cursor = PayloadCursor::new(payload);
    let tag = u8::from_le_bytes(cursor.read_array(resources)?);
    let value = match tag {
        1 => {
            let text = cursor.blob(resources)?;
            Value::String(append_payload(text, &mut pool.bytes, resources)?)
        }
        2 => {
            let raw = u8::from_le_bytes(cursor.read_array(resources)?);
            if raw > 1 {
                return Err(TreeError::Invalid("typed get: stored bool tag"));
            }
            Value::Bool(raw == 1)
        }
        3 => Value::I64(i64::from_le_bytes(cursor.read_array(resources)?)),
        4 => Value::F64(u64::from_le_bytes(cursor.read_array(resources)?)),
        5..=9 => {
            let count = usize::try_from(u64::from_le_bytes(cursor.read_array(resources)?))
                .map_err(|_| TreeError::Memory)?;
            if tag == 5 && count != 0 {
                return Err(TreeError::Invalid("typed get: nonempty untyped list"));
            }
            let start = u32::try_from(pool.children.len()).map_err(|_| TreeError::Memory)?;
            for _ in 0..count {
                let child = match tag {
                    6 => {
                        let text = cursor.blob(resources)?;
                        let span = append_payload(text, &mut pool.bytes, resources)?;
                        push_value(pool, Value::String(span))?
                    }
                    7 => {
                        let raw = u8::from_le_bytes(cursor.read_array(resources)?);
                        if raw > 1 {
                            return Err(TreeError::Invalid("typed get: stored list bool tag"));
                        }
                        push_value(pool, Value::Bool(raw == 1))?
                    }
                    8 => push_value(
                        pool,
                        Value::I64(i64::from_le_bytes(cursor.read_array(resources)?)),
                    )?,
                    9 => push_value(
                        pool,
                        Value::F64(u64::from_le_bytes(cursor.read_array(resources)?)),
                    )?,
                    _ => return Err(TreeError::Invalid("typed get: stored list tag")),
                };
                pool.children.push(child);
            }
            let element = match tag {
                5 => ListKind::Empty,
                6 => ListKind::String,
                7 => ListKind::Bool,
                8 => ListKind::I64,
                9 => ListKind::F64,
                _ => return Err(TreeError::Invalid("typed get: stored list element tag")),
            };
            Value::List {
                children: Span::new(start, u32::try_from(count).map_err(|_| TreeError::Memory)?),
                element,
            }
        }
        _ => return Err(TreeError::Invalid("typed get: stored property tag")),
    };
    cursor.finish(resources)?;
    push_value(pool, value)
}

fn copy_properties<S: BlockSource>(
    view: &GraphReadView<'_, '_, '_, '_>,
    record: &RecordView<'_, S>,
    pool: &mut EntityPool,
    resources: &mut TreeResources<'_>,
) -> Result<Span, TreeError> {
    let count =
        usize::try_from(record.canonical().property_count()).map_err(|_| TreeError::Memory)?;
    let mut items = Vec::with_capacity(count);
    for index in 0..count as u64 {
        let (key, _) = record.property_at(index, resources)?;
        let name = view
            .expression_symbol_name(Symbol::Property(key), resources)?
            .ok_or(TreeError::Invalid("typed get: property symbol is missing"))?;
        items.push((name.as_str(), index));
    }
    items.sort_by(|left, right| left.0.cmp(right.0));
    let start = u32::try_from(pool.properties.len()).map_err(|_| TreeError::Memory)?;
    for (name, index) in items {
        let (_, payload) = record.property_at(index, resources)?;
        let name_span = append_text(name, &mut pool.bytes)?;
        let value = decode_value(payload, pool, resources)?;
        pool.properties.push(Property {
            name: name_span,
            value,
        });
    }
    Ok(Span::new(
        start,
        u32::try_from(count).map_err(|_| TreeError::Memory)?,
    ))
}

fn copy_node(
    view: &GraphReadView<'_, '_, '_, '_>,
    id: NodeId,
    options: GraphGetOptions,
    pool: &mut EntityPool,
    resources: &mut TreeResources<'_>,
) -> Result<Option<Node>, TreeError> {
    let Some(node) = view.lookup_node(id, resources)? else {
        return Ok(None);
    };
    let record = node.record();
    let RecordShape::Node {
        labels: label_count,
        ..
    } = record.shape()
    else {
        return Err(TreeError::Invalid("typed get: node directory role"));
    };
    let mut labels = Vec::with_capacity(label_count as usize);
    for index in 0..label_count {
        let label = record.label(index, resources)?;
        let name = view
            .expression_symbol_name(Symbol::Label(label), resources)?
            .ok_or(TreeError::Invalid("typed get: label symbol is missing"))?;
        labels.push(name.as_str());
    }
    labels.sort_unstable();
    let label_start = u32::try_from(pool.names.len()).map_err(|_| TreeError::Memory)?;
    for name in labels {
        let span = append_text(name, &mut pool.bytes)?;
        pool.names.push(span);
    }
    let labels_span = Span::new(label_start, label_count);
    let properties_span = copy_properties(view, record, pool, resources)?;
    let key = copy_key(record.provenance().key(), &mut pool.bytes, resources)?;
    let text = if options.text {
        record
            .canonical()
            .stored_text()
            .map(|payload| append_payload(payload, &mut pool.bytes, resources))
            .transpose()?
    } else {
        None
    };
    let vector = if options.vector {
        record
            .canonical()
            .stored_vector()
            .map(|vector| append_vector(vector, &mut pool.vectors, resources))
            .transpose()?
    } else {
        None
    };
    Ok(Some(Node {
        id,
        revision: record.revision(),
        generation: record.provenance().original_generation(),
        key,
        labels: labels_span,
        properties: properties_span,
        text,
        vector,
    }))
}

fn copy_relationship(
    view: &GraphReadView<'_, '_, '_, '_>,
    id: RelId,
    pool: &mut EntityPool,
    resources: &mut TreeResources<'_>,
) -> Result<Option<Relationship>, TreeError> {
    let Some(relationship) = view.lookup_relationship(id, resources)? else {
        return Ok(None);
    };
    let record = relationship.record();
    let RecordShape::Relationship {
        source,
        target,
        relationship_type,
        ..
    } = record.shape()
    else {
        return Err(TreeError::Invalid("typed get: relationship directory role"));
    };
    let type_name = view
        .expression_symbol_name(Symbol::RelationshipType(relationship_type), resources)?
        .ok_or(TreeError::Invalid(
            "typed get: relationship type symbol is missing",
        ))?;
    let relationship_type_span = append_text(type_name.as_str(), &mut pool.bytes)?;
    let properties_span = copy_properties(view, record, pool, resources)?;
    let key = copy_key(record.provenance().key(), &mut pool.bytes, resources)?;
    Ok(Some(Relationship {
        id,
        revision: record.revision(),
        generation: record.provenance().original_generation(),
        key,
        source,
        target,
        relationship_type: relationship_type_span,
        properties: properties_span,
    }))
}

struct GetNodes<'a> {
    ids: &'a [NodeId],
    options: GraphGetOptions,
}

impl NativeReadConsumer<GraphNodesResult> for GetNodes<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<GraphNodesResult, TreeError> {
        let generation = view.generation();
        let mut pool = EntityPool::default();
        let mut nodes = Vec::with_capacity(self.ids.len());
        let mut resources = TreeResources::for_query(runtime)?;
        for id in self.ids {
            nodes.push(copy_node(
                view,
                *id,
                self.options,
                &mut pool,
                &mut resources,
            )?);
        }
        Ok(GraphNodesResult {
            generation,
            nodes,
            pool,
        })
    }
}

struct GetRelationships<'a> {
    ids: &'a [RelId],
}

impl NativeReadConsumer<GraphRelationshipsResult> for GetRelationships<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<GraphRelationshipsResult, TreeError> {
        let generation = view.generation();
        let mut pool = EntityPool::default();
        let mut relationships = Vec::with_capacity(self.ids.len());
        let mut resources = TreeResources::for_query(runtime)?;
        for id in self.ids {
            relationships.push(copy_relationship(view, *id, &mut pool, &mut resources)?);
        }
        Ok(GraphRelationshipsResult {
            generation,
            relationships,
            pool,
        })
    }
}

impl GraphStore {
    /// Reads a batch of nodes by identity under one admitted read generation.
    ///
    /// Returns one `Option<Node>` per requested id, in request order: `None`
    /// for an id that is missing or deleted. Labels, the application key and
    /// properties are always copied; `options` additionally selects stored
    /// text and the stored vector, each `None` unless explicitly requested,
    /// so absent text stays distinct from present empty text, and an absent
    /// vector stays distinct from a requested-but-unset one.
    ///
    /// # Errors
    ///
    /// The classified read rejection; nothing is partially returned.
    pub fn get_nodes(
        &self,
        ids: &[NodeId],
        options: GraphGetOptions,
        control: &QueryControl,
    ) -> Result<GraphNodesResult, GraphStoreError> {
        if ids.len() > MAX_GRAPH_CHANGES {
            return Err(GraphStoreError::limit(
                "requested id count exceeds MAX_GRAPH_CHANGES",
            ));
        }
        Ok(self.store.with_native_read(
            control,
            RuntimeLimits::default(),
            24 * 1024 * 1024,
            64,
            GetNodes { ids, options },
        )?)
    }

    /// Reads a batch of relationships by identity under one admitted read
    /// generation. See [`get_nodes`](Self::get_nodes); relationships never
    /// carry text or a vector, so there is no field selection to make.
    ///
    /// # Errors
    ///
    /// The classified read rejection; nothing is partially returned.
    pub fn get_relationships(
        &self,
        ids: &[RelId],
        control: &QueryControl,
    ) -> Result<GraphRelationshipsResult, GraphStoreError> {
        if ids.len() > MAX_GRAPH_CHANGES {
            return Err(GraphStoreError::limit(
                "requested id count exceeds MAX_GRAPH_CHANGES",
            ));
        }
        Ok(self.store.with_native_read(
            control,
            RuntimeLimits::default(),
            24 * 1024 * 1024,
            64,
            GetRelationships { ids },
        )?)
    }
}

#[cfg(test)]
mod tests;
