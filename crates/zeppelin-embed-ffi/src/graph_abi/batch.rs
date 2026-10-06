//! Borrowed batch decoder; no store calls.
// The entry-point consumer lands in slice 4.
#![allow(dead_code)]
use crate::error::FfiError;
use crate::{
    ZeErrorCode, ZeGraphBatchItem, ZeGraphNode, ZeGraphProperty, ZeGraphRange, ZeGraphRelationship,
    ZeGraphValue, ZeGraphValuePool, marshal,
};
use std::ops::Range;
use zeppelin_embed::epoch::EmbeddingTower;
use zeppelin_embed::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use zeppelin_embed::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphDeleteMode,
    GraphName, GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue, RelId,
    with_local_refs,
};
pub(crate) const MAX_BATCH_ITEMS: usize = 16_384;
fn invalid(message: impl Into<String>) -> FfiError {
    FfiError::invalid(message)
}

/// Reads one graph request descriptor, whose `abi_size` must be exactly this
/// version's size: graph descriptors are fixed-stride, never prefix-extended.
fn read_exact<T: Copy>(pointer: *const T, size: fn(&T) -> u32, what: &str) -> Result<T, FfiError> {
    let value =
        marshal::read_struct(pointer).map_err(|error| invalid(format!("{what}: {}", error.0)))?;
    if size(&value) as usize != std::mem::size_of::<T>() {
        return Err(invalid(format!(
            "{what} abi_size must be exactly {}",
            std::mem::size_of::<T>()
        )));
    }
    Ok(value)
}

fn range<'p, T>(items: &'p [T], span: ZeGraphRange, what: &str) -> Result<&'p [T], FfiError> {
    let bounds = span
        .checked_range(items.len())
        .map_err(|_| invalid(format!("{what} range is out of bounds")))?;
    items
        .get(bounds)
        .ok_or_else(|| invalid(format!("{what} range is out of bounds")))
}

fn utf8<'p>(bytes: &'p [u8], what: &str) -> Result<&'p str, FfiError> {
    std::str::from_utf8(bytes).map_err(|_| invalid(format!("{what} is not valid UTF-8")))
}

fn zero_range(span: ZeGraphRange) -> bool {
    span.start == 0 && span.count == 0
}

/// A validated borrowed request pool.
#[derive(Clone, Copy)]
pub(crate) struct Pool<'p> {
    values: &'p [ZeGraphValue],
    children: &'p [u32],
    bytes: &'p [u8],
    nodes: &'p [ZeGraphNode],
    relationships: &'p [ZeGraphRelationship],
    properties: &'p [ZeGraphProperty],
    names: &'p [ZeGraphRange],
    vectors: &'p [f32],
}

impl<'p> Pool<'p> {
    pub(super) fn bytes(&self) -> &[u8] {
        self.bytes
    }

    pub(super) fn with_bytes<'a>(&'a self, bytes: &'a [u8]) -> Pool<'a> {
        Pool { bytes, ..*self }
    }

    pub(crate) fn read(pointer: *const ZeGraphValuePool, what: &str) -> Result<Self, FfiError> {
        let pool = read_exact(pointer, |pool| pool.abi_size, what)?;
        pool.validate_header()
            .map_err(|_| invalid(format!("{what} has an invalid descriptor")))?;
        let counts = [
            pool.value_count,
            pool.child_count,
            pool.node_count,
            pool.relationship_count,
            pool.property_count,
            pool.name_count,
            pool.vector_count,
        ];
        if counts.iter().any(|count| *count > 524_288)
            || pool.byte_count > zeppelin_embed::property_graph::MAX_GRAPH_INPUT_BYTES
        {
            return Err(invalid("graph pool exceeds bounded input arenas"));
        }
        let slice = |error: marshal::MarshalError, field: &str| {
            invalid(format!("{what}.{field}: {}", error.0))
        };
        Ok(Self {
            values: marshal::read_slice(pool.values, pool.value_count)
                .map_err(|error| slice(error, "values"))?,
            children: marshal::read_slice(pool.children, pool.child_count)
                .map_err(|error| slice(error, "children"))?,
            bytes: marshal::read_slice(pool.bytes, pool.byte_count)
                .map_err(|error| slice(error, "bytes"))?,
            nodes: marshal::read_slice(pool.nodes, pool.node_count)
                .map_err(|error| slice(error, "nodes"))?,
            relationships: marshal::read_slice(pool.relationships, pool.relationship_count)
                .map_err(|error| slice(error, "relationships"))?,
            properties: marshal::read_slice(pool.properties, pool.property_count)
                .map_err(|error| slice(error, "properties"))?,
            names: marshal::read_slice(pool.names, pool.name_count)
                .map_err(|error| slice(error, "names"))?,
            vectors: marshal::read_slice(pool.vectors, pool.vector_count)
                .map_err(|error| slice(error, "vectors"))?,
        })
    }

    pub(crate) fn text(&self, span: ZeGraphRange, what: &str) -> Result<&'p str, FfiError> {
        utf8(range(self.bytes, span, what)?, what)
    }

    pub(crate) fn children(&self, span: ZeGraphRange) -> Result<&'p [u32], FfiError> {
        range(self.children, span, "list children")
    }

    pub(crate) fn names(&self, span: ZeGraphRange) -> Result<&'p [ZeGraphRange], FfiError> {
        range(self.names, span, "plan names")
    }

    pub(crate) fn value(&self, index: u32, what: &str) -> Result<&'p ZeGraphValue, FfiError> {
        let value = self
            .values
            .get(index as usize)
            .ok_or_else(|| invalid(format!("{what} value index {index} is out of bounds")))?;
        value
            .validate_shape()
            .map_err(|_| invalid(format!("{what} value {index} has an invalid descriptor")))?;
        Ok(value)
    }
}

/// Where one decoded property value's elements live until images are built.
#[derive(Clone)]
enum PropertySource<'p> {
    Scalar(PropertyData<'p>),
    Strings(Range<usize>),
    Bools(Range<usize>),
    Integers(Range<usize>),
    Floats(Range<usize>),
}

/// Typed list element backing for every property in one request.
#[derive(Default)]
struct ListBacking<'p> {
    strings: Vec<&'p str>,
    bools: Vec<bool>,
    integers: Vec<i64>,
    floats: Vec<f64>,
}

fn stored_value<'p>(
    pool: &Pool<'p>,
    index: u32,
    backing: &mut ListBacking<'p>,
    what: &str,
) -> Result<PropertySource<'p>, FfiError> {
    use crate::{ZeGraphListKind as L, ZeGraphValueTag as V};
    let value = pool.value(index, what)?;
    value.validate_stored_shape().map_err(|_| {
        invalid(format!(
            "{what} must be a bool, i64, f64, string or typed list; null is absence"
        ))
    })?;
    let data = match value.tag {
        tag if tag == V::ZeGraphValueBool as u32 => PropertyData::Bool(value.boolean == 1),
        tag if tag == V::ZeGraphValueI64 as u32 => PropertyData::I64(value.integer),
        tag if tag == V::ZeGraphValueF64 as u32 => PropertyData::F64(value.floating),
        tag if tag == V::ZeGraphValueString as u32 => {
            PropertyData::String(pool.text(value.range, what)?)
        }
        _ if value.list_kind == L::ZeGraphListEmpty as u32 => PropertyData::EmptyList { count: 0 },
        _ => {
            let (element, start) = match value.list_kind {
                kind if kind == L::ZeGraphListBool as u32 => {
                    (V::ZeGraphValueBool, backing.bools.len())
                }
                kind if kind == L::ZeGraphListI64 as u32 => {
                    (V::ZeGraphValueI64, backing.integers.len())
                }
                kind if kind == L::ZeGraphListF64 as u32 => {
                    (V::ZeGraphValueF64, backing.floats.len())
                }
                _ => (V::ZeGraphValueString, backing.strings.len()),
            };
            for &child in range(pool.children, value.range, what)? {
                let item = pool.value(child, what)?;
                if item.tag != element as u32 {
                    return Err(invalid(format!(
                        "{what} list element {child} does not match the list kind"
                    )));
                }
                match element {
                    V::ZeGraphValueBool => backing.bools.push(item.boolean == 1),
                    V::ZeGraphValueI64 => backing.integers.push(item.integer),
                    V::ZeGraphValueF64 => backing.floats.push(item.floating),
                    _ => backing.strings.push(pool.text(item.range, what)?),
                }
            }
            return Ok(match element {
                V::ZeGraphValueBool => PropertySource::Bools(start..backing.bools.len()),
                V::ZeGraphValueI64 => PropertySource::Integers(start..backing.integers.len()),
                V::ZeGraphValueF64 => PropertySource::Floats(start..backing.floats.len()),
                _ => PropertySource::Strings(start..backing.strings.len()),
            });
        }
    };
    Ok(PropertySource::Scalar(data))
}

fn property_data<'b>(
    source: &PropertySource<'b>,
    backing: &'b ListBacking<'b>,
) -> Option<PropertyData<'b>> {
    Some(match source {
        PropertySource::Scalar(data) => *data,
        PropertySource::Strings(span) => PropertyData::Strings(backing.strings.get(span.clone())?),
        PropertySource::Bools(span) => PropertyData::Bools(backing.bools.get(span.clone())?),
        PropertySource::Integers(span) => {
            PropertyData::Integers(backing.integers.get(span.clone())?)
        }
        PropertySource::Floats(span) => PropertyData::Floats(backing.floats.get(span.clone())?),
    })
}

#[derive(Clone, Copy)]
enum Endpoint {
    Node(NodeId),
    Local(u32),
}

enum ImageSource<'p> {
    None,
    Node {
        labels: usize,
        properties: usize,
        text: Option<&'p str>,
        vector: Option<&'p [f32]>,
    },
    Relationship {
        relationship_type: GraphName<'p>,
        properties: usize,
        source: Endpoint,
        target: Endpoint,
    },
}

struct ItemSource<'p> {
    key: ApplicationKey<'p>,
    revision: GraphRevision,
    operation: StructuredOperation,
    image: ImageSource<'p>,
}

enum Image<'a> {
    None,
    Node(CanonicalContents<'a>),
    Relationship {
        relationship_type: GraphName<'a>,
        properties: &'a [GraphProperty<'a>],
        source: Endpoint,
        target: Endpoint,
    },
}

fn node_id(id: crate::ZeNodeId, what: &str) -> Result<NodeId, FfiError> {
    NodeId::try_from(id).map_err(|_| invalid(format!("{what} must be a nonzero node id")))
}

fn endpoint(
    value: crate::ZeGraphEndpoint,
    items: &[ZeGraphBatchItem],
    what: &str,
) -> Result<Endpoint, FfiError> {
    use crate::ZeGraphEndpointKind as K;
    if value.kind == K::ZeGraphEndpointNode as u32 {
        return Ok(Endpoint::Node(node_id(value.node, what)?));
    }
    // A local endpoint names a node this same batch creates or replays.
    let target = items.get(value.local_item as usize).ok_or_else(|| {
        invalid(format!(
            "{what} names batch item {} which does not exist",
            value.local_item
        ))
    })?;
    if target.entity_kind != crate::ZeGraphEntityKind::ZeGraphEntityNode as u32
        || target.operation == crate::ZeGraphBatchOperation::ZeGraphBatchDelete as u32
    {
        return Err(invalid(format!(
            "{what} names batch item {} which does not write a node",
            value.local_item
        )));
    }
    Ok(Endpoint::Local(value.local_item))
}

fn decode_properties<'p>(
    pool: &Pool<'p>,
    span: ZeGraphRange,
    names: &mut Vec<GraphName<'p>>,
    sources: &mut Vec<PropertySource<'p>>,
    backing: &mut ListBacking<'p>,
    what: &str,
) -> Result<usize, FfiError> {
    let entries = range(pool.properties, span, &format!("{what} properties"))?;
    for (position, entry) in entries.iter().enumerate() {
        let field = format!("{what} property {position}");
        if entry.validate_header().is_err() || entry.reserved != 0 {
            return Err(invalid(format!("{field} has an invalid descriptor")));
        }
        let name = pool.text(entry.name, &format!("{field} name"))?;
        names.push(
            GraphName::new(name).map_err(|error| {
                FfiError::new(error.into(), format!("{field} name is too large"))
            })?,
        );
        sources.push(stored_value(pool, entry.value, backing, &field)?);
    }
    Ok(entries.len())
}

fn operation(item: &ZeGraphBatchItem, what: &str) -> Result<StructuredOperation, FfiError> {
    use crate::ZeGraphBatchOperation as O;
    let node = item.entity_kind == crate::ZeGraphEntityKind::ZeGraphEntityNode as u32;
    let expected = || -> Result<EntityId, FfiError> {
        if node {
            Ok(EntityId::Node(node_id(
                item.expected_node,
                &format!("{what} expected_node"),
            )?))
        } else {
            RelId::try_from(item.expected_relationship)
                .map(EntityId::Relationship)
                .map_err(|_| invalid(format!("{what} expected_relationship must be nonzero")))
        }
    };
    Ok(match item.operation {
        op if op == O::ZeGraphBatchCreate as u32 => StructuredOperation::Create,
        op if op == O::ZeGraphBatchPut as u32 => StructuredOperation::Put(expected()?),
        op if op == O::ZeGraphBatchDelete as u32 => StructuredOperation::Delete(
            expected()?,
            if item.delete_mode == 1 {
                GraphDeleteMode::Detach
            } else {
                GraphDeleteMode::Restrict
            },
        ),
        _ => StructuredOperation::Recreate(
            GraphRevision::new(item.expected_deletion_revision).map_err(|_| {
                invalid(format!(
                    "{what} expected_deletion_revision must be positive"
                ))
            })?,
        ),
    })
}

// Keep the WIP decoder's separate backing buffers and borrowed lifetimes.
#[allow(clippy::too_many_arguments)]
fn node_image<'p>(
    pool: &Pool<'p>,
    item: &ZeGraphBatchItem,
    document: Option<&EmbeddingTower>,
    labels: &mut Vec<GraphName<'p>>,
    names: &mut Vec<GraphName<'p>>,
    sources: &mut Vec<PropertySource<'p>>,
    backing: &mut ListBacking<'p>,
    what: &str,
) -> Result<ImageSource<'p>, FfiError> {
    let node = pool
        .nodes
        .get(item.image as usize)
        .ok_or_else(|| invalid(format!("{what} image {} is out of bounds", item.image)))?;
    let zero_id = node.id.high == 0 && node.id.low == 0;
    if node.validate_header().is_err()
        || !zero_id
        || node.has_key != 0
        || !zero_range(node.namespace_name)
        || !zero_range(node.key)
        || node.revision != 0
        || node.last_change_generation != 0
        || node.reserved != 0
        || node.has_text > 1
        || node.has_vector > 1
        || (node.has_text == 0 && !zero_range(node.text))
        || (node.has_vector == 0 && !zero_range(node.vector))
    {
        return Err(invalid(format!(
            "{what} node image must carry zero identity, key and revision fields and 0/1 presence flags"
        )));
    }
    let label_spans = range(pool.names, node.labels, &format!("{what} labels"))?;
    for (position, span) in label_spans.iter().enumerate() {
        let label = pool.text(*span, &format!("{what} label {position}"))?;
        labels.push(GraphName::new(label).map_err(|error| {
            FfiError::new(
                error.into(),
                format!("{what} label {position} is too large"),
            )
        })?);
    }
    let properties = decode_properties(pool, node.properties, names, sources, backing, what)?;
    let text = if node.has_text == 1 {
        Some(pool.text(node.text, &format!("{what} text"))?)
    } else {
        None
    };
    let vector = if node.has_vector == 1 {
        if document.is_none() {
            return Err(FfiError::new(
                ZeErrorCode::ZeErrNoVectorSpace,
                format!(
                    "{what} carries a vector but the store was opened without a document tower"
                ),
            ));
        }
        Some(range(pool.vectors, node.vector, &format!("{what} vector"))?)
    } else {
        None
    };
    Ok(ImageSource::Node {
        labels: label_spans.len(),
        properties,
        text,
        vector,
    })
}

fn relationship_image<'p>(
    pool: &Pool<'p>,
    item: &ZeGraphBatchItem,
    items: &[ZeGraphBatchItem],
    names: &mut Vec<GraphName<'p>>,
    sources: &mut Vec<PropertySource<'p>>,
    backing: &mut ListBacking<'p>,
    what: &str,
) -> Result<ImageSource<'p>, FfiError> {
    let relationship = pool
        .relationships
        .get(item.image as usize)
        .ok_or_else(|| invalid(format!("{what} image {} is out of bounds", item.image)))?;
    let zero = |high: u64, low: u64| high == 0 && low == 0;
    if relationship.validate_header().is_err()
        || !zero(relationship.id.high, relationship.id.low)
        || !zero(relationship.source.high, relationship.source.low)
        || !zero(relationship.target.high, relationship.target.low)
        || relationship.has_key != 0
        || !zero_range(relationship.namespace_name)
        || !zero_range(relationship.key)
        || relationship.revision != 0
        || relationship.last_change_generation != 0
        || relationship.reserved != 0
    {
        return Err(invalid(format!(
            "{what} relationship image must carry zero identity, endpoint, key and revision fields"
        )));
    }
    let relationship_type = pool.text(
        relationship.relationship_type,
        &format!("{what} relationship type"),
    )?;
    let relationship_type = GraphName::new(relationship_type).map_err(|error| {
        FfiError::new(
            error.into(),
            format!("{what} relationship type is too large"),
        )
    })?;
    let properties =
        decode_properties(pool, relationship.properties, names, sources, backing, what)?;
    Ok(ImageSource::Relationship {
        relationship_type,
        properties,
        source: endpoint(item.source, items, &format!("{what} source"))?,
        target: endpoint(item.target, items, &format!("{what} target"))?,
    })
}

fn canonical_error(error: zeppelin_embed::property_graph::CanonicalError, what: &str) -> FfiError {
    let message = format!("{what}: {error}");
    FfiError::new(error.into(), message)
}

fn take<'s, T>(rest: &mut &'s mut [T], count: usize) -> Result<&'s mut [T], FfiError> {
    let whole = std::mem::take(rest);
    let (head, tail) = whole.split_at_mut_checked(count).ok_or_else(|| {
        FfiError::new(
            ZeErrorCode::ZeErrInternal,
            "graph batch descriptor split overran",
        )
    })?;
    *rest = tail;
    Ok(head)
}

/// Validates the whole batch into borrowed engine inputs, then runs `run`
/// with them inside one local-reference scope. Nothing reaches the store
/// unless every item validated.
pub(crate) fn with_batch<R>(
    pool: &Pool<'_>,
    items: &[ZeGraphBatchItem],
    document: Option<&EmbeddingTower>,
    run: impl for<'batch> FnOnce(&[StructuredWrite<'_, 'batch>]) -> R,
) -> Result<R, FfiError> {
    let mut sources = Vec::new();
    let mut labels = Vec::new();
    let mut names = Vec::new();
    let mut values = Vec::new();
    let mut backing = ListBacking::default();
    for (index, item) in items.iter().enumerate() {
        let what = format!("batch item {index}");
        item.validate_shape().map_err(|_| {
            invalid(format!(
                "{what} has an invalid entity kind, operation, revision, precondition, image or endpoint shape"
            ))
        })?;
        let kind = if item.entity_kind == crate::ZeGraphEntityKind::ZeGraphEntityNode as u32 {
            EntityKind::Node
        } else {
            EntityKind::Relationship
        };
        let namespace = pool.text(item.namespace_name, &format!("{what} namespace"))?;
        let key = pool.text(item.key, &format!("{what} key"))?;
        let key = ApplicationKey::new(kind, namespace, key)
            .map_err(|error| FfiError::new(error.into(), format!("{what} key is too large")))?;
        let revision = GraphRevision::new(item.revision)
            .map_err(|_| invalid(format!("{what} revision must be positive")))?;
        let operation = operation(item, &what)?;
        let image = match (item.has_image, kind) {
            (0, _) => ImageSource::None,
            (_, EntityKind::Node) => node_image(
                pool,
                item,
                document,
                &mut labels,
                &mut names,
                &mut values,
                &mut backing,
                &what,
            )?,
            (_, EntityKind::Relationship) => relationship_image(
                pool,
                item,
                items,
                &mut names,
                &mut values,
                &mut backing,
                &what,
            )?,
        };
        sources.push(ItemSource {
            key,
            revision,
            operation,
            image,
        });
    }

    let mut properties = Vec::new();
    properties.try_reserve_exact(names.len()).map_err(|_| {
        FfiError::new(
            ZeErrorCode::ZeErrOutOfMemory,
            "graph property allocation failed",
        )
    })?;
    for (position, (name, source)) in names.iter().zip(&values).enumerate() {
        let data = property_data(source, &backing).ok_or_else(|| {
            FfiError::new(ZeErrorCode::ZeErrInternal, "graph list backing overran")
        })?;
        let value = PropertyValue::new(data).map_err(|error| {
            FfiError::new(
                error.into(),
                format!("property {position} value is out of bounds"),
            )
        })?;
        properties.push(GraphProperty::new(*name, value));
    }

    let mut label_rest = labels.as_mut_slice();
    let mut property_rest = properties.as_mut_slice();
    let mut images = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let what = format!("batch item {index}");
        images.push(match &source.image {
            ImageSource::None => Image::None,
            ImageSource::Node {
                labels,
                properties,
                text,
                vector,
            } => {
                let embedding = match (vector, document) {
                    (Some(coordinates), Some(tower)) => Some(
                        CanonicalEmbedding::new(tower, coordinates)
                            .map_err(|error| canonical_error(error, &format!("{what} vector")))?,
                    ),
                    _ => None,
                };
                Image::Node(
                    CanonicalContents::node(
                        take(&mut label_rest, *labels)?,
                        take(&mut property_rest, *properties)?,
                        *text,
                        embedding,
                    )
                    .map_err(|error| canonical_error(error, &what))?,
                )
            }
            ImageSource::Relationship {
                relationship_type,
                properties,
                source,
                target,
            } => Image::Relationship {
                relationship_type: *relationship_type,
                properties: take(&mut property_rest, *properties)?,
                source: *source,
                target: *target,
            },
        });
    }

    with_local_refs(|refs| {
        let resolve = |value: Endpoint| -> Result<NodeRef<'_>, FfiError> {
            Ok(match value {
                Endpoint::Node(id) => NodeRef::Existing(id),
                Endpoint::Local(index) => {
                    NodeRef::Local(refs.node(index as usize).map_err(|error| {
                        FfiError::new(error.into(), "local endpoint is out of range")
                    })?)
                }
            })
        };
        let mut writes = Vec::new();
        writes.try_reserve_exact(sources.len()).map_err(|_| {
            FfiError::new(
                ZeErrorCode::ZeErrOutOfMemory,
                "graph batch allocation failed",
            )
        })?;
        for (source, image) in sources.iter().zip(&images) {
            let image = match image {
                Image::None => None,
                Image::Node(contents) => Some(WriteImage::Node(contents)),
                Image::Relationship {
                    relationship_type,
                    properties,
                    source,
                    target,
                } => Some(WriteImage::Relationship {
                    source: resolve(*source)?,
                    target: resolve(*target)?,
                    relationship_type: *relationship_type,
                    properties,
                }),
            };
            writes.push(StructuredWrite {
                key: source.key,
                revision: source.revision,
                operation: source.operation,
                image,
            });
        }
        Ok(run(&writes))
    })
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::*;
    use std::cell::Cell;

    // All these C descriptors contain only scalars and raw pointers; zero is valid.
    macro_rules! descriptor {
        ($ty:ty) => {{
            let mut value: $ty = unsafe { std::mem::zeroed() };
            value.abi_size = std::mem::size_of::<$ty>() as u32;
            value
        }};
    }
    #[derive(Default)]
    struct Fixture {
        bytes: Vec<u8>,
        values: Vec<ZeGraphValue>,
        children: Vec<u32>,
        nodes: Vec<ZeGraphNode>,
        relationships: Vec<ZeGraphRelationship>,
        properties: Vec<ZeGraphProperty>,
        names: Vec<ZeGraphRange>,
        vectors: Vec<f32>,
        items: Vec<ZeGraphBatchItem>,
    }
    impl Fixture {
        fn text(&mut self, text: &str) -> ZeGraphRange {
            let span = ZeGraphRange {
                start: self.bytes.len() as u32,
                count: text.len() as u32,
            };
            self.bytes.extend_from_slice(text.as_bytes());
            span
        }
        fn pool(&self) -> ZeGraphValuePool {
            ZeGraphValuePool {
                abi_size: std::mem::size_of::<ZeGraphValuePool>() as u32,
                abi_reserved: 0,
                values: self.values.as_ptr(),
                value_count: self.values.len(),
                children: self.children.as_ptr(),
                child_count: self.children.len(),
                bytes: self.bytes.as_ptr(),
                byte_count: self.bytes.len(),
                nodes: self.nodes.as_ptr(),
                node_count: self.nodes.len(),
                relationships: self.relationships.as_ptr(),
                relationship_count: self.relationships.len(),
                properties: self.properties.as_ptr(),
                property_count: self.properties.len(),
                names: self.names.as_ptr(),
                name_count: self.names.len(),
                vectors: self.vectors.as_ptr(),
                vector_count: self.vectors.len(),
            }
        }
        fn new() -> Self {
            let mut f = Self::default();
            let mut item = descriptor!(ZeGraphBatchItem);
            item.source = descriptor!(ZeGraphEndpoint);
            item.target = descriptor!(ZeGraphEndpoint);
            item.namespace_name = f.text("docs");
            item.key = f.text("alpha");
            item.revision = 1;
            item.has_image = 1;
            f.items.push(item);
            let mut node = descriptor!(ZeGraphNode);
            node.has_text = 1;
            node.labels.count = 1;
            node.properties.count = 1;
            f.nodes.push(node);
            let label = f.text("Doc");
            f.names.push(label);
            let mut value = descriptor!(ZeGraphValue);
            value.tag = ZeGraphValueTag::ZeGraphValueString as u32;
            value.range = item.key;
            f.values.push(value);
            let mut property = descriptor!(ZeGraphProperty);
            property.name = f.text("title");
            f.properties.push(property);
            item.entity_kind = 1;
            item.namespace_name = f.text("edges");
            item.key = f.text("alpha-beta");
            item.source.kind = 2;
            item.target.kind = 1;
            item.target.node.low = 9;
            f.items.push(item);
            let mut rel = descriptor!(ZeGraphRelationship);
            rel.relationship_type = f.text("LINKS");
            f.relationships.push(rel);
            f
        }
    }
    #[test]
    fn batch_decodes_a_node_and_a_relationship_with_a_local_endpoint() {
        let f = Fixture::new();
        let raw = f.pool();
        let pool = Pool::read(&raw, "pool").unwrap();
        with_batch(&pool, &f.items, None, |writes| {
            assert_eq!(writes.len(), 2);
            assert_eq!(writes[0].key.namespace().as_str(), "docs");
            assert_eq!(writes[0].key.key().as_str(), "alpha");
            assert_eq!(writes[0].revision.get(), 1);
            let Some(WriteImage::Node(actual)) = writes[0].image else {
                panic!("node image")
            };
            let mut labels = [GraphName::new("Doc").unwrap()];
            let mut props = [GraphProperty::new(
                GraphName::new("title").unwrap(),
                PropertyValue::new(PropertyData::String("alpha")).unwrap(),
            )];
            let expected =
                CanonicalContents::node(&mut labels, &mut props, Some(""), None).unwrap();
            let mut control = || Ok(());
            let mut a = Vec::new();
            let mut b = Vec::new();
            actual.write_to(&mut a, &mut control).unwrap();
            expected.write_to(&mut b, &mut control).unwrap();
            assert_eq!(a, b);
            match writes[1].image {
                Some(WriteImage::Relationship {
                    source: NodeRef::Local(local),
                    target: NodeRef::Existing(id),
                    relationship_type,
                    ..
                }) => {
                    assert_eq!(local.index(), 0);
                    assert_eq!(id.get(), 9);
                    assert_eq!(relationship_type.as_str(), "LINKS");
                }
                _ => panic!("relationship image"),
            }
        })
        .unwrap();
    }
    #[test]
    fn batch_decodes_every_operation_and_delete_mode() {
        let mut f = Fixture::new();
        let base = f.items[0];
        f.items.clear();
        for (op, mode) in [(1, 0), (2, 0), (2, 1), (3, 0)] {
            let mut item = base;
            item.operation = op;
            item.delete_mode = mode;
            if op == 1 || op == 2 {
                item.expected_node.low = 7;
            }
            if op == 2 {
                item.has_image = 0;
            }
            if op == 3 {
                item.expected_deletion_revision = 3;
            }
            f.items.push(item);
        }
        let raw = f.pool();
        let pool = Pool::read(&raw, "pool").unwrap();
        with_batch(&pool,&f.items,None,|writes| {
            assert_eq!(writes.len(),4);
            assert!(matches!(writes[0].operation, StructuredOperation::Put(EntityId::Node(id)) if id.get()==7));
            assert!(matches!(writes[1].operation, StructuredOperation::Delete(EntityId::Node(id),GraphDeleteMode::Restrict) if id.get()==7));
            assert!(matches!(writes[2].operation, StructuredOperation::Delete(EntityId::Node(id),GraphDeleteMode::Detach) if id.get()==7));
            assert!(writes[1].image.is_none() && writes[2].image.is_none());
            assert!(matches!(writes[3].operation, StructuredOperation::Recreate(rev) if rev.get()==3));
        }).unwrap();
    }
    #[test]
    fn batch_rejects_each_malformed_item_before_producing_any_write() {
        for case in 0..18 {
            let mut f = Fixture::new();
            match case {
                0 => f.bytes[4] = 255,
                1 => f.items[0].revision = 0,
                2 => f.nodes[0].id.low = 1,
                3 => f.nodes[0].has_text = 2,
                4 => {
                    f.nodes[0].has_text = 0;
                    f.nodes[0].text.count = 1;
                }
                5 => f.nodes[0].labels.count = 2,
                6 => f.properties[0].name.count = u32::MAX,
                7 => f.values[0] = descriptor!(ZeGraphValue),
                8 => f.properties[0].value = 99,
                9 | 10 => {
                    let mut v = descriptor!(ZeGraphValue);
                    v.tag = ZeGraphValueTag::ZeGraphValueList as u32;
                    v.list_kind = if case == 9 {
                        ZeGraphListKind::ZeGraphListBool as u32
                    } else {
                        ZeGraphListKind::ZeGraphListEmpty as u32
                    };
                    v.range.count = 1;
                    f.values.push(v);
                    f.properties[0].value = 1;
                    f.children.push(0);
                }
                11 => f.items[1].source.local_item = 1,
                12 => {
                    f.items[0].operation = 2;
                    f.items[0].has_image = 0;
                    f.items[0].expected_node.low = 7;
                }
                13 => f.items[1].source.local_item = 2,
                14 => f.relationships[0].source.low = 1,
                15 => f.nodes[0].has_vector = 1,
                16 | 17 => {}
                _ => unreachable!(),
            }
            let mut raw = f.pool();
            if case == 16 {
                raw.abi_size += 8;
            }
            if case == 17 {
                raw.abi_reserved = 1;
            }
            let called = Cell::new(false);
            let result = Pool::read(&raw, "pool")
                .and_then(|pool| with_batch(&pool, &f.items, None, |_| called.set(true)));
            assert!(!called.get(), "case {case} called run");
            assert_eq!(
                result.unwrap_err().code,
                if case == 15 {
                    ZeErrorCode::ZeErrNoVectorSpace
                } else {
                    ZeErrorCode::ZeErrInvalidArgument
                },
                "case {case}"
            );
        }
    }
    #[test]
    fn batch_typed_lists_and_the_empty_list_sentinel_round_trip() {
        let mut f = Fixture::new();
        f.nodes[0].properties.count = 0;
        f.properties.clear();
        f.values.clear();
        let nan = 0x7ff8_0000_0000_0042;
        for (name, kind) in [
            ("bools", 1),
            ("ints", 2),
            ("floats", 3),
            ("strings", 4),
            ("empty", 5),
        ] {
            let mut list = descriptor!(ZeGraphValue);
            list.tag = ZeGraphValueTag::ZeGraphValueList as u32;
            list.list_kind = kind;
            list.range.start = f.children.len() as u32;
            for n in 0..if kind == 5 {
                0
            } else if kind == 3 {
                1
            } else {
                2
            } {
                let mut v = descriptor!(ZeGraphValue);
                v.tag = kind;
                match kind {
                    1 => v.boolean = u32::from(n == 0),
                    2 => v.integer = n + 1,
                    3 => v.floating = f64::from_bits(nan),
                    4 => v.range = f.text(if n == 0 { "" } else { "x" }),
                    _ => {}
                }
                f.children.push(f.values.len() as u32);
                f.values.push(v);
                list.range.count += 1;
            }
            let mut prop = descriptor!(ZeGraphProperty);
            prop.name = f.text(name);
            prop.value = f.values.len() as u32;
            f.values.push(list);
            f.properties.push(prop);
        }
        f.relationships[0].properties.count = 5;
        let raw = f.pool();
        let pool = Pool::read(&raw, "pool").unwrap();
        with_batch(&pool, &f.items, None, |writes| {
            assert_eq!(writes.len(), 2);
            let Some(WriteImage::Relationship { properties, .. }) = writes[1].image else {
                panic!("relationship")
            };
            assert_eq!(properties.len(), 5);
            for p in properties {
                match (p.name().as_str(), p.value().data()) {
                    ("bools", PropertyData::Bools(v)) => assert_eq!(v, &[true, false]),
                    ("ints", PropertyData::Integers(v)) => assert_eq!(v, &[1, 2]),
                    ("floats", PropertyData::Floats(v)) => {
                        assert_eq!(v.len(), 1);
                        assert_eq!(v[0].to_bits(), nan);
                    }
                    ("strings", PropertyData::Strings(v)) => assert_eq!(v, &["", "x"]),
                    ("empty", PropertyData::EmptyList { count }) => assert_eq!(count, 0),
                    _ => panic!("wrong property"),
                }
            }
        })
        .unwrap();
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod ze241_tests {
    use super::*;
    #[test]
    fn ze241_raw_pool_refuses_overflow_utf8_and_misalignment() {
        let mut raw: ZeGraphValuePool = unsafe { std::mem::zeroed() };
        raw.abi_size = std::mem::size_of::<ZeGraphValuePool>() as u32;
        raw.value_count = usize::MAX;
        assert!(Pool::read(&raw, "overflow").is_err());
        raw.value_count = 0;
        raw.bytes = [255u8].as_ptr();
        raw.byte_count = 1;
        let bytes = [255u8];
        raw.bytes = bytes.as_ptr();
        let pool = match Pool::read(&raw, "UTF-8") {
            Ok(pool) => pool,
            Err(error) => panic!("{error:?}"),
        };
        assert!(
            pool.text(ZeGraphRange { start: 0, count: 1 }, "text")
                .is_err()
        );
        let allocated = [0u64; 64];
        let pointer = allocated
            .as_ptr()
            .cast::<u8>()
            .wrapping_add(1)
            .cast::<ZeGraphValuePool>();
        assert!(Pool::read(pointer, "misaligned").is_err());
    }
}
