use super::{NativeResultError, NativeStaging, Sizes};
use crate::lifecycle::QueryControl;
use crate::property_graph::catalog::Symbol;
use crate::property_graph::query::completed::{
    CompletedError, Key, ListKind, Node, Property, Relationship, SourceError, Span, Value,
    ValueIndex,
};
use crate::property_graph::query::resources::QueryArena;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeError};
use crate::property_graph::staging::{
    BatchEntityRef, GraphBatchReadView, StageError, WriteImage, WritePhase,
};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::records::RecordShape;
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{
    BlockSource, NativeReadEvent, TreeResources,
};
use crate::property_graph::{
    EntityId, GraphName, GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData,
    PropertyValue, RelId, RelRef,
};

#[derive(Clone, Copy)]
struct NameItem {
    symbol: Symbol,
    index: u64,
}

fn limit() -> NativeResultError {
    NativeResultError::Completed(CompletedError::Limit)
}

fn sift_down<T: Copy + Ord>(
    values: &mut [T],
    start: usize,
    end: usize,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), NativeResultError> {
    let mut root = start;
    loop {
        let child = root
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(limit)?;
        if child >= end {
            return Ok(());
        }
        context.values().step().map_err(RuntimeError::from)?;
        let mut selected = child;
        if child + 1 < end {
            context.values().step().map_err(RuntimeError::from)?;
            if values
                .get(child)
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?
                < values
                    .get(child + 1)
                    .ok_or(NativeResultError::Completed(CompletedError::Shape))?
            {
                selected = child + 1;
            }
        }
        if values
            .get(root)
            .ok_or(NativeResultError::Completed(CompletedError::Shape))?
            >= values
                .get(selected)
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?
        {
            return Ok(());
        }
        values.swap(root, selected);
        root = selected;
    }
}

pub(super) fn sort_unique<'m, 'g, T: Copy + Ord>(
    mut occurrences: QueryArena<'m, 'g, T>,
    context: &mut RuntimeContext<'_, 'm, 'g>,
) -> Result<QueryArena<'m, 'g, T>, NativeResultError> {
    let len = occurrences.len();
    for start in (0..len / 2).rev() {
        sift_down(occurrences.as_mut_slice(), start, len, context)?;
    }
    for end in (1..len).rev() {
        occurrences.as_mut_slice().swap(0, end);
        sift_down(occurrences.as_mut_slice(), 0, end, context)?;
    }
    let mut count = 0usize;
    let mut previous = None;
    for value in occurrences.as_slice().iter().copied() {
        context.values().step().map_err(RuntimeError::from)?;
        if previous != Some(value) {
            count = count.checked_add(1).ok_or_else(limit)?;
            previous = Some(value);
        }
    }
    let mut unique = QueryArena::new(context.memory(), count)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    previous = None;
    for value in occurrences.as_slice().iter().copied() {
        context.values().step().map_err(RuntimeError::from)?;
        if previous != Some(value) {
            unique
                .push(value)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
            previous = Some(value);
        }
    }
    Ok(unique)
}

/// Sizes every entity the rows name. With a write statement's `overlay`,
/// each entity's staged image is measured in place of its admitted record;
/// without one this is exactly the read path.
pub(super) fn measure_entities(
    view: &GraphReadView<'_, '_, '_, '_>,
    mut overlay: Option<&mut GraphBatchReadView<'_, 'static>>,
    node_ids: &[NodeId],
    relationship_ids: &[RelId],
    sizes: &mut Sizes,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), NativeResultError> {
    sizes.nodes = node_ids.len();
    sizes.relationships = relationship_ids.len();
    let control = context.values().control();
    let mut resources = TreeResources::for_query(context)
        .map_err(NativeExecutionError::from)
        .map_err(NativeResultError::Native)?;
    for id in node_ids {
        let pending = match overlay.as_deref_mut() {
            Some(overlay) => staged(overlay, EntityId::Node(*id), control)?,
            None => None,
        };
        let node = view
            .lookup_node(*id, &mut resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        if let Some(image) = pending {
            if let Some(node) = &node {
                let record = node.record();
                if !matches!(record.shape(), RecordShape::Node { id: actual, .. } if actual == *id)
                {
                    return Err(NativeResultError::Completed(CompletedError::Shape));
                }
                measure_key(record.provenance().key(), sizes)?;
            }
            let WriteImage::Node(image) = image else {
                return Err(NativeResultError::Completed(CompletedError::Shape));
            };
            let (labels, properties, _, _) = image
                .staging_node_parts()
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
            sizes.name_scratch = sizes.name_scratch.max(labels.len());
            for label in labels {
                step(&mut resources, 1)?;
                add_bytes(sizes, label.as_str().len())?;
                sizes.names = sizes.names.checked_add(1).ok_or_else(limit)?;
            }
            measure_staged_properties(properties, sizes, &mut resources)?;
            if view
                .document_version(*id)
                .map_err(NativeExecutionError::from)
                .map_err(NativeResultError::Native)?
                .is_some()
            {
                measure_document_node(view, *id, sizes, &mut resources)?;
            }
            continue;
        }
        if node.is_none()
            && view
                .document_version(*id)
                .map_err(NativeExecutionError::from)
                .map_err(NativeResultError::Native)?
                .is_some()
        {
            measure_document_node(view, *id, sizes, &mut resources)?;
            continue;
        }
        let node = node.ok_or(NativeResultError::Completed(CompletedError::Source(
            SourceError::Missing(EntityId::Node(*id)),
        )))?;
        let record = node.record();
        let RecordShape::Node { id: actual, labels } = record.shape() else {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        };
        if actual != *id {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        }
        measure_key(record.provenance().key(), sizes)?;
        sizes.name_scratch = sizes.name_scratch.max(labels as usize);
        for index in 0..labels {
            let label = record
                .label(index, &mut resources)
                .map_err(NativeExecutionError::from)
                .map_err(NativeResultError::Native)?;
            let name = required_name(view, Symbol::Label(label), &mut resources)?;
            add_bytes(sizes, name.as_str().len())?;
            sizes.names = sizes.names.checked_add(1).ok_or_else(limit)?;
        }
        measure_properties(view, record, sizes, &mut resources)?;
        if record.document_version().is_some() {
            measure_document_node(view, *id, sizes, &mut resources)?;
        }
    }
    for id in relationship_ids {
        let pending = match overlay.as_deref_mut() {
            Some(overlay) => staged(overlay, EntityId::Relationship(*id), control)?,
            None => None,
        };
        let relationship = view
            .lookup_relationship(*id, &mut resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        if let Some(image) = pending {
            let (relationship_type, properties, _) =
                staged_relationship(image, relationship.as_ref().map(|r| r.record()), *id)?;
            if let Some(relationship) = &relationship {
                measure_key(relationship.record().provenance().key(), sizes)?;
            }
            add_bytes(sizes, relationship_type.as_str().len())?;
            measure_staged_properties(properties, sizes, &mut resources)?;
            continue;
        }
        let relationship = relationship.ok_or(NativeResultError::Completed(
            CompletedError::Source(SourceError::Missing(EntityId::Relationship(*id))),
        ))?;
        let record = relationship.record();
        let RecordShape::Relationship {
            id: actual,
            relationship_type,
            ..
        } = record.shape()
        else {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        };
        if actual != *id {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        }
        measure_key(record.provenance().key(), sizes)?;
        let name = required_name(
            view,
            Symbol::RelationshipType(relationship_type),
            &mut resources,
        )?;
        add_bytes(sizes, name.as_str().len())?;
        measure_properties(view, record, sizes, &mut resources)?;
    }
    Ok(())
}

fn add_bytes(sizes: &mut Sizes, count: usize) -> Result<(), NativeResultError> {
    sizes.bytes = sizes.bytes.checked_add(count).ok_or_else(limit)?;
    Ok(())
}

fn measure_key<S: BlockSource>(
    key: Option<crate::property_graph::storage::records::StoredKey<'_, S>>,
    sizes: &mut Sizes,
) -> Result<(), NativeResultError> {
    if let Some(key) = key {
        add_bytes(
            sizes,
            usize::try_from(key.namespace().len()).map_err(|_| limit())?,
        )?;
        add_bytes(
            sizes,
            usize::try_from(key.key().len()).map_err(|_| limit())?,
        )?;
    }
    Ok(())
}

fn required_name<'a>(
    view: &GraphReadView<'a, '_, '_, '_>,
    symbol: Symbol,
    resources: &mut TreeResources<'_>,
) -> Result<GraphName<'a>, NativeResultError> {
    view.expression_symbol_name(symbol, resources)
        .map_err(NativeExecutionError::from)
        .map_err(NativeResultError::Native)?
        .ok_or_else(|| {
            NativeResultError::Native(NativeExecutionError::from(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result symbol is missing",
                ),
            ))
        })
}

fn measure_properties<S: BlockSource>(
    view: &GraphReadView<'_, '_, '_, '_>,
    record: &crate::property_graph::storage::records::RecordView<'_, S>,
    sizes: &mut Sizes,
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    let count = usize::try_from(record.canonical().property_count()).map_err(|_| limit())?;
    sizes.name_scratch = sizes.name_scratch.max(count);
    sizes.properties = sizes.properties.checked_add(count).ok_or_else(limit)?;
    for index in 0..count {
        let (key, payload) = record
            .property_at(index as u64, resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        let name = required_name(view, Symbol::Property(key), resources)?;
        add_bytes(sizes, name.as_str().len())?;
        super::values::measure_stored(payload, sizes, resources)?;
    }
    Ok(())
}

pub(super) fn append_payload<S: BlockSource>(
    payload: PayloadSlice<'_, S>,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<Span, NativeResultError> {
    let start = u32::try_from(staging.bytes.len()).map_err(|_| limit())?;
    let mut offset = 0u64;
    while offset < payload.len() {
        let bytes = payload
            .span_at(offset, resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        if bytes.is_empty() {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        }
        let count = bytes
            .len()
            .min(usize::try_from(payload.len() - offset).map_err(|_| limit())?);
        let bytes = bytes
            .get(..count)
            .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
        resources
            .step(count as u64)
            .and_then(|()| resources.read_event(NativeReadEvent::CopiedBytes(count as u64)))
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        staging
            .bytes
            .extend_copy(bytes)
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
        offset = offset.checked_add(count as u64).ok_or_else(limit)?;
    }
    Ok(Span::new(
        start,
        u32::try_from(payload.len()).map_err(|_| limit())?,
    ))
}

fn append_catalog_name(
    name: GraphName<'_>,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<Span, NativeResultError> {
    append_text(name.as_str(), staging, resources)
}

/// Copies exact UTF-8 bytes in bounded chunks, charging each copied chunk.
fn append_text(
    text: &str,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<Span, NativeResultError> {
    let start = u32::try_from(staging.bytes.len()).map_err(|_| limit())?;
    for chunk in text.as_bytes().chunks(65536) {
        resources
            .step(chunk.len() as u64)
            .and_then(|()| resources.read_event(NativeReadEvent::CopiedBytes(chunk.len() as u64)))
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        staging
            .bytes
            .extend_copy(chunk)
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    Ok(Span::new(
        start,
        u32::try_from(text.len()).map_err(|_| limit())?,
    ))
}

fn name_order(
    view: &GraphReadView<'_, '_, '_, '_>,
    left: NameItem,
    right: NameItem,
    resources: &mut TreeResources<'_>,
) -> Result<std::cmp::Ordering, NativeResultError> {
    let left = required_name(view, left.symbol, resources)?;
    let right = required_name(view, right.symbol, resources)?;
    resources
        .step((left.as_str().len().min(right.as_str().len()) / 65536 + 1) as u64)
        .map_err(NativeExecutionError::from)
        .map_err(NativeResultError::Native)?;
    Ok(left.as_str().as_bytes().cmp(right.as_str().as_bytes()))
}

fn sift_names(
    view: &GraphReadView<'_, '_, '_, '_>,
    values: &mut [NameItem],
    start: usize,
    end: usize,
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    let mut root = start;
    loop {
        let child = root
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(limit)?;
        if child >= end {
            return Ok(());
        }
        let mut selected = child;
        if child + 1 < end
            && name_order(
                view,
                *values
                    .get(child)
                    .ok_or(NativeResultError::Completed(CompletedError::Shape))?,
                *values
                    .get(child + 1)
                    .ok_or(NativeResultError::Completed(CompletedError::Shape))?,
                resources,
            )? == std::cmp::Ordering::Less
        {
            selected = child + 1;
        }
        if name_order(
            view,
            *values
                .get(root)
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?,
            *values
                .get(selected)
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?,
            resources,
        )? != std::cmp::Ordering::Less
        {
            return Ok(());
        }
        values.swap(root, selected);
        root = selected;
    }
}

fn sort_names(
    view: &GraphReadView<'_, '_, '_, '_>,
    values: &mut [NameItem],
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    let len = values.len();
    for start in (0..len / 2).rev() {
        sift_names(view, values, start, len, resources)?;
    }
    for end in (1..len).rev() {
        values.swap(0, end);
        sift_names(view, values, 0, end, resources)?;
    }
    Ok(())
}

fn fill_key<S: BlockSource>(
    key: Option<crate::property_graph::storage::records::StoredKey<'_, S>>,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<Key>, NativeResultError> {
    key.map(|key| {
        Ok(Key {
            kind: key.kind(),
            namespace: append_payload(key.namespace(), staging, resources)?,
            value: append_payload(key.key(), staging, resources)?,
        })
    })
    .transpose()
}

fn fill_properties<S: BlockSource>(
    view: &GraphReadView<'_, '_, '_, '_>,
    record: &crate::property_graph::storage::records::RecordView<'_, S>,
    scratch: &mut QueryArena<'_, '_, NameItem>,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<Span, NativeResultError> {
    scratch.clear();
    let count = usize::try_from(record.canonical().property_count()).map_err(|_| limit())?;
    for index in 0..count {
        let (key, _) = record
            .property_at(index as u64, resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        scratch
            .push(NameItem {
                symbol: Symbol::Property(key),
                index: index as u64,
            })
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    sort_names(view, scratch.as_mut_slice(), resources)?;
    let start = u32::try_from(staging.properties.len()).map_err(|_| limit())?;
    for item in scratch.as_slice().iter().copied() {
        let (key, payload) = record
            .property_at(item.index, resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        if item.symbol != Symbol::Property(key) {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        }
        let name = append_catalog_name(
            required_name(view, item.symbol, resources)?,
            staging,
            resources,
        )?;
        let value = super::values::fill_stored(payload, staging, resources)?;
        staging
            .properties
            .push(Property { name, value })
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    Ok(Span::new(start, u32::try_from(count).map_err(|_| limit())?))
}

/// Copies every entity the rows name. With a write statement's `overlay`,
/// each entity is copied from its staged image when one exists, and from its
/// admitted record otherwise; an entity this statement deleted is refused.
/// Without one this is exactly the read path.
///
/// A staged entity keeps its admitted key, revision and generation, and a
/// created one is copied at revision one and the admitted generation: the
/// commit tail settles both once it has decided what the statement changed.
pub(super) fn fill_entities(
    view: &GraphReadView<'_, '_, '_, '_>,
    mut overlay: Option<&mut GraphBatchReadView<'_, 'static>>,
    node_ids: &[NodeId],
    relationship_ids: &[RelId],
    name_scratch: usize,
    staging: &mut NativeStaging<'_, '_, '_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), NativeResultError> {
    let mut scratch = QueryArena::new(context.memory(), name_scratch)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    // Staged names are ordered by their own bytes, not by catalog symbol.
    let mut order = match overlay {
        Some(_) => Some(
            QueryArena::new(context.memory(), name_scratch)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
        ),
        None => None,
    };
    let admitted = context.view().generation();
    let control = context.values().control();
    let mut resources = TreeResources::for_query(context)
        .map_err(NativeExecutionError::from)
        .map_err(NativeResultError::Native)?;
    for id in node_ids {
        let pending = match overlay.as_deref_mut() {
            Some(overlay) => staged(overlay, EntityId::Node(*id), control)?,
            None => None,
        };
        let node = view
            .lookup_node(*id, &mut resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        if let Some(image) = pending {
            let order = order
                .as_mut()
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
            let WriteImage::Node(image) = image else {
                return Err(NativeResultError::Completed(CompletedError::Shape));
            };
            let (labels, properties, _, _) = image
                .staging_node_parts()
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
            let (key, revision, generation) = match &node {
                Some(node) => (
                    fill_key(node.record().provenance().key(), staging, &mut resources)?,
                    node.record().revision(),
                    node.record().provenance().original_generation(),
                ),
                None => (None, created_revision()?, admitted),
            };
            fill_order(order, labels.len())?;
            sort_staged(
                order.as_mut_slice(),
                |index| label_bytes(labels, index),
                &mut resources,
            )?;
            let label_start = u32::try_from(staging.names.len()).map_err(|_| limit())?;
            for index in order.as_slice().iter().copied() {
                let label = usize::try_from(index)
                    .ok()
                    .and_then(|index| labels.get(index))
                    .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
                let span = append_text(label.as_str(), staging, &mut resources)?;
                staging
                    .names
                    .push(span)
                    .map_err(CompletedError::from)
                    .map_err(NativeResultError::Completed)?;
            }
            let properties = fill_staged_properties(properties, order, staging, &mut resources)?;
            let (labels, properties) = merge_document_node(
                view,
                *id,
                label_start,
                properties.start,
                staging,
                &mut resources,
            )?;
            staging
                .nodes
                .push(Node {
                    id: *id,
                    revision,
                    generation,
                    key,
                    labels,
                    properties,
                    text: None,
                    vector: None,
                })
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
            continue;
        }
        if node.is_none()
            && view
                .document_version(*id)
                .map_err(NativeExecutionError::from)
                .map_err(NativeResultError::Native)?
                .is_some()
        {
            fill_document_node(view, *id, admitted, staging, &mut resources)?;
            continue;
        }
        let node = node.ok_or(NativeResultError::Completed(CompletedError::Source(
            SourceError::Missing(EntityId::Node(*id)),
        )))?;
        let record = node.record();
        let RecordShape::Node { labels, .. } = record.shape() else {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        };
        let key = fill_key(record.provenance().key(), staging, &mut resources)?;
        scratch.clear();
        for index in 0..labels {
            let label = record
                .label(index, &mut resources)
                .map_err(NativeExecutionError::from)
                .map_err(NativeResultError::Native)?;
            scratch
                .push(NameItem {
                    symbol: Symbol::Label(label),
                    index: index as u64,
                })
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
        }
        sort_names(view, scratch.as_mut_slice(), &mut resources)?;
        let label_start = u32::try_from(staging.names.len()).map_err(|_| limit())?;
        for item in scratch.as_slice().iter().copied() {
            let span = append_catalog_name(
                required_name(view, item.symbol, &mut resources)?,
                staging,
                &mut resources,
            )?;
            staging
                .names
                .push(span)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
        }
        let properties = fill_properties(view, record, &mut scratch, staging, &mut resources)?;
        let (labels, properties) = merge_document_node(
            view,
            *id,
            label_start,
            properties.start,
            staging,
            &mut resources,
        )?;
        staging
            .nodes
            .push(Node {
                id: *id,
                revision: record.revision(),
                generation: record.provenance().original_generation(),
                key,
                labels,
                properties,
                text: None,
                vector: None,
            })
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    for id in relationship_ids {
        let pending = match overlay.as_deref_mut() {
            Some(overlay) => staged(overlay, EntityId::Relationship(*id), control)?,
            None => None,
        };
        let relationship = view
            .lookup_relationship(*id, &mut resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?;
        if let Some(image) = pending {
            let order = order
                .as_mut()
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
            let (relationship_type, properties, (source, target)) =
                staged_relationship(image, relationship.as_ref().map(|r| r.record()), *id)?;
            let (key, revision, generation) = match &relationship {
                Some(relationship) => (
                    fill_key(
                        relationship.record().provenance().key(),
                        staging,
                        &mut resources,
                    )?,
                    relationship.record().revision(),
                    relationship.record().provenance().original_generation(),
                ),
                None => (None, created_revision()?, admitted),
            };
            let relationship_type =
                append_text(relationship_type.as_str(), staging, &mut resources)?;
            let properties = fill_staged_properties(properties, order, staging, &mut resources)?;
            staging
                .relationships
                .push(Relationship {
                    id: *id,
                    revision,
                    generation,
                    key,
                    source,
                    target,
                    relationship_type,
                    properties,
                })
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
            continue;
        }
        let relationship = relationship.ok_or(NativeResultError::Completed(
            CompletedError::Source(SourceError::Missing(EntityId::Relationship(*id))),
        ))?;
        let record = relationship.record();
        let RecordShape::Relationship {
            source,
            target,
            relationship_type,
            ..
        } = record.shape()
        else {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        };
        let key = fill_key(record.provenance().key(), staging, &mut resources)?;
        let relationship_type = append_catalog_name(
            required_name(
                view,
                Symbol::RelationshipType(relationship_type),
                &mut resources,
            )?,
            staging,
            &mut resources,
        )?;
        let properties = fill_properties(view, record, &mut scratch, staging, &mut resources)?;
        staging
            .relationships
            .push(Relationship {
                id: *id,
                revision: record.revision(),
                generation: record.provenance().original_generation(),
                key,
                source,
                target,
                relationship_type,
                properties,
            })
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    Ok(())
}

/// The image this write statement staged for `entity`, or `None` when it
/// staged nothing and the admitted record is current. An entity the statement
/// deleted has no contents to copy and is refused, typed.
fn staged<'w>(
    overlay: &mut GraphBatchReadView<'w, 'static>,
    entity: EntityId,
    control: &QueryControl,
) -> Result<Option<WriteImage<'w, 'static>>, NativeResultError> {
    let mut write_control = |_: WritePhase| control.checkpoint().map_err(|_| StageError::Cancelled);
    let target = match entity {
        EntityId::Node(id) => BatchEntityRef::Node(NodeRef::Existing(id)),
        EntityId::Relationship(id) => BatchEntityRef::Relationship(RelRef::Existing(id)),
    };
    overlay
        .pending_image(target, &mut write_control)
        .map_err(|error| match error {
            StageError::DeletedEntity => {
                NativeResultError::Completed(CompletedError::Source(SourceError::Deleted(entity)))
            }
            error => NativeResultError::Native(NativeExecutionError::Stage(error)),
        })
}

/// A staged relationship's type, properties and endpoints. Its endpoints are
/// bound identities, and they must be the admitted record's when it has one:
/// a statement never changes a relationship's topology.
#[allow(clippy::type_complexity)]
fn staged_relationship<'a, S: BlockSource>(
    image: WriteImage<'a, 'static>,
    record: Option<&crate::property_graph::storage::records::RecordView<'_, S>>,
    id: RelId,
) -> Result<(GraphName<'a>, &'a [GraphProperty<'a>], (NodeId, NodeId)), NativeResultError> {
    let WriteImage::Relationship {
        source: NodeRef::Existing(source),
        target: NodeRef::Existing(target),
        relationship_type,
        properties,
    } = image
    else {
        return Err(NativeResultError::Completed(CompletedError::Shape));
    };
    if let Some(record) = record
        && !matches!(
            record.shape(),
            RecordShape::Relationship { id: actual, source: s, target: t, .. }
                if actual == id && s == source && t == target
        )
    {
        return Err(NativeResultError::Completed(CompletedError::Shape));
    }
    Ok((relationship_type, properties, (source, target)))
}

/// The revision a created entity is installed at.
fn created_revision() -> Result<GraphRevision, NativeResultError> {
    GraphRevision::new(1).map_err(|_| NativeResultError::Completed(CompletedError::Shape))
}

fn step(resources: &mut TreeResources<'_>, units: u64) -> Result<(), NativeResultError> {
    resources
        .step(units)
        .map_err(NativeExecutionError::from)
        .map_err(NativeResultError::Native)
}

fn label_bytes<'n>(labels: &'n [GraphName<'_>], index: u64) -> Option<&'n [u8]> {
    labels
        .get(usize::try_from(index).ok()?)
        .map(|label| label.as_str().as_bytes())
}

fn property_at<'n, 'a>(
    properties: &'n [GraphProperty<'a>],
    index: u64,
) -> Option<&'n GraphProperty<'a>> {
    properties.get(usize::try_from(index).ok()?)
}

fn measure_staged_properties(
    properties: &[GraphProperty<'_>],
    sizes: &mut Sizes,
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    sizes.name_scratch = sizes.name_scratch.max(properties.len());
    sizes.properties = sizes
        .properties
        .checked_add(properties.len())
        .ok_or_else(limit)?;
    for property in properties {
        step(resources, 1)?;
        add_bytes(sizes, property.name().as_str().len())?;
        let (children, bytes) = match property.value().data() {
            PropertyData::String(text) => (None, text.len()),
            PropertyData::Bool(_) | PropertyData::I64(_) | PropertyData::F64(_) => (None, 0),
            PropertyData::EmptyList { count: 0 } => (Some(0), 0),
            PropertyData::EmptyList { .. } => {
                return Err(NativeResultError::Completed(CompletedError::Shape));
            }
            PropertyData::Strings(values) => {
                let mut bytes = 0usize;
                for value in values {
                    step(resources, 1)?;
                    bytes = bytes.checked_add(value.len()).ok_or_else(limit)?;
                }
                (Some(values.len()), bytes)
            }
            PropertyData::Bools(values) => (Some(values.len()), 0),
            PropertyData::Integers(values) => (Some(values.len()), 0),
            PropertyData::Floats(values) => (Some(values.len()), 0),
        };
        add_bytes(sizes, bytes)?;
        let values = match children {
            Some(count) => {
                sizes.children = sizes.children.checked_add(count).ok_or_else(limit)?;
                count.checked_add(1).ok_or_else(limit)?
            }
            None => 1,
        };
        sizes.values = sizes.values.checked_add(values).ok_or_else(limit)?;
    }
    Ok(())
}

fn fill_order(order: &mut QueryArena<'_, '_, u64>, count: usize) -> Result<(), NativeResultError> {
    order.clear();
    for index in 0..count {
        order
            .push(index as u64)
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    Ok(())
}

/// Heap-sorts positions by the exact bytes of the names they index, charging
/// each comparison exactly as catalog-name ordering does.
fn sort_staged<'n>(
    values: &mut [u64],
    name: impl Fn(u64) -> Option<&'n [u8]>,
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    let less = |left: u64, right: u64, resources: &mut TreeResources<'_>| {
        let left = name(left).ok_or(NativeResultError::Completed(CompletedError::Shape))?;
        let right = name(right).ok_or(NativeResultError::Completed(CompletedError::Shape))?;
        step(resources, (left.len().min(right.len()) / 65536 + 1) as u64)?;
        Ok::<_, NativeResultError>(left < right)
    };
    let sift = |values: &mut [u64], start: usize, end: usize, resources: &mut TreeResources<'_>| {
        let mut root = start;
        loop {
            let child = root
                .checked_mul(2)
                .and_then(|n| n.checked_add(1))
                .ok_or_else(limit)?;
            if child >= end {
                return Ok::<_, NativeResultError>(());
            }
            let at = |index: usize| {
                values
                    .get(index)
                    .copied()
                    .ok_or(NativeResultError::Completed(CompletedError::Shape))
            };
            let mut selected = child;
            if child + 1 < end && less(at(child)?, at(child + 1)?, resources)? {
                selected = child + 1;
            }
            if !less(at(root)?, at(selected)?, resources)? {
                return Ok(());
            }
            values.swap(root, selected);
            root = selected;
        }
    };
    let len = values.len();
    for start in (0..len / 2).rev() {
        sift(values, start, len, resources)?;
    }
    for end in (1..len).rev() {
        values.swap(0, end);
        sift(values, 0, end, resources)?;
    }
    Ok(())
}

/// Copies staged properties in exact name-byte order, preserving each typed
/// value, list element type and IEEE bit pattern.
fn fill_staged_properties(
    properties: &[GraphProperty<'_>],
    order: &mut QueryArena<'_, '_, u64>,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<Span, NativeResultError> {
    fill_order(order, properties.len())?;
    sort_staged(
        order.as_mut_slice(),
        |index| property_at(properties, index).map(|p| p.name().as_str().as_bytes()),
        resources,
    )?;
    let start = u32::try_from(staging.properties.len()).map_err(|_| limit())?;
    for index in order.as_slice().iter().copied() {
        let property = *property_at(properties, index)
            .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
        let name = append_text(property.name().as_str(), staging, resources)?;
        let value = fill_staged_value(property.value(), staging, resources)?;
        staging
            .properties
            .push(Property { name, value })
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    Ok(Span::new(
        start,
        u32::try_from(properties.len()).map_err(|_| limit())?,
    ))
}

fn fill_staged_value(
    value: PropertyValue<'_>,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<ValueIndex, NativeResultError> {
    use super::values::push_stored_scalar as push;
    let list = |element: ListKind,
                count: usize,
                staging: &mut NativeStaging<'_, '_, '_>,
                resources: &mut TreeResources<'_>,
                child: &mut dyn FnMut(
        usize,
        &mut NativeStaging<'_, '_, '_>,
        &mut TreeResources<'_>,
    ) -> Result<Value, NativeResultError>| {
        let start = u32::try_from(staging.children.len()).map_err(|_| limit())?;
        for index in 0..count {
            step(resources, 1)?;
            let value = child(index, staging, resources)?;
            let child = push(value, staging)?;
            staging
                .children
                .push(child)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
        }
        push(
            Value::List {
                children: Span::new(start, u32::try_from(count).map_err(|_| limit())?),
                element,
            },
            staging,
        )
    };
    let shape = || NativeResultError::Completed(CompletedError::Shape);
    match value.data() {
        PropertyData::String(text) => {
            let span = append_text(text, staging, resources)?;
            push(Value::String(span), staging)
        }
        PropertyData::Bool(value) => push(Value::Bool(value), staging),
        PropertyData::I64(value) => push(Value::I64(value), staging),
        PropertyData::F64(value) => push(Value::F64(value.to_bits()), staging),
        PropertyData::EmptyList { count: 0 } => {
            list(ListKind::Empty, 0, staging, resources, &mut |_, _, _| {
                Err(shape())
            })
        }
        PropertyData::EmptyList { .. } => Err(shape()),
        PropertyData::Strings(values) => list(
            ListKind::String,
            values.len(),
            staging,
            resources,
            &mut |index, staging, resources| {
                let text = values.get(index).ok_or_else(shape)?;
                Ok(Value::String(append_text(text, staging, resources)?))
            },
        ),
        PropertyData::Bools(values) => list(
            ListKind::Bool,
            values.len(),
            staging,
            resources,
            &mut |index, _, _| Ok(Value::Bool(*values.get(index).ok_or_else(shape)?)),
        ),
        PropertyData::Integers(values) => list(
            ListKind::I64,
            values.len(),
            staging,
            resources,
            &mut |index, _, _| Ok(Value::I64(*values.get(index).ok_or_else(shape)?)),
        ),
        PropertyData::Floats(values) => list(
            ListKind::F64,
            values.len(),
            staging,
            resources,
            &mut |index, _, _| Ok(Value::F64(values.get(index).ok_or_else(shape)?.to_bits())),
        ),
    }
}

use crate::property_graph::query::runtime::NativeExecutionError;

fn document_value(
    value: &crate::meta::PredicateValue,
) -> Result<PropertyValue<'_>, crate::property_graph::storage::tree::directory::TreeError> {
    use crate::meta::PredicateValue as V;
    use crate::property_graph::storage::tree::directory::TreeError;
    let data = match value {
        V::I64(value) => PropertyData::I64(*value),
        V::U64(value) => PropertyData::I64(
            i64::try_from(*value)
                .map_err(|_| TreeError::Invalid("document integer exceeds Cypher i64"))?,
        ),
        V::F64(value) => PropertyData::F64(*value),
        V::Bool(value) => PropertyData::Bool(*value),
        V::String(value) => PropertyData::String(value),
        V::Id128(_) => {
            return Err(TreeError::Invalid(
                "document Id128 has no Cypher scalar representation",
            ));
        }
    };
    PropertyValue::new(data).map_err(|_| TreeError::Invalid("document scalar"))
}
fn measure_document_node(
    view: &GraphReadView<'_, '_, '_, '_>,
    id: NodeId,
    sizes: &mut Sizes,
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    sizes.names = sizes.names.checked_add(1).ok_or_else(limit)?;
    add_bytes(sizes, "Document".len())?;
    let mut failure = None;
    view.visit_document_properties(id, |name, value| {
        let result = (|| {
            let property = GraphProperty::new(
                GraphName::new(name)
                    .map_err(|_| NativeResultError::Completed(CompletedError::Shape))?,
                document_value(value)
                    .map_err(NativeExecutionError::from)
                    .map_err(NativeResultError::Native)?,
            );
            measure_staged_properties(&[property], sizes, resources)
        })();
        if let Err(error) = result {
            failure = Some(error);
            return Err(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "document projection measurement",
                ),
            );
        }
        Ok(())
    })
    .map_err(|error| failure.unwrap_or_else(|| NativeResultError::Native(error.into())))?;
    Ok(())
}
fn fill_document_node(
    view: &GraphReadView<'_, '_, '_, '_>,
    id: NodeId,
    admitted: crate::property_graph::GraphGeneration,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    let label_start = u32::try_from(staging.names.len()).map_err(|_| limit())?;
    let label = append_text("Document", staging, resources)?;
    staging
        .names
        .push(label)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    let property_start = u32::try_from(staging.properties.len()).map_err(|_| limit())?;
    let mut failure = None;
    view.visit_document_properties(id, |name, value| {
        let result = (|| {
            let name = append_text(name, staging, resources)?;
            let value = fill_staged_value(
                document_value(value)
                    .map_err(NativeExecutionError::from)
                    .map_err(NativeResultError::Native)?,
                staging,
                resources,
            )?;
            staging
                .properties
                .push(Property { name, value })
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)
        })();
        if let Err(error) = result {
            failure = Some(error);
            return Err(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "document projection copy",
                ),
            );
        }
        Ok(())
    })
    .map_err(|error| failure.unwrap_or_else(|| NativeResultError::Native(error.into())))?;
    sort_copied_names(
        staging
            .properties
            .as_mut_slice()
            .get_mut(property_start as usize..)
            .ok_or_else(limit)?,
        staging.bytes.as_slice(),
        |property| property.name,
        resources,
    )?;
    let count = u32::try_from(staging.properties.len())
        .map_err(|_| limit())?
        .checked_sub(property_start)
        .ok_or_else(limit)?;
    staging
        .nodes
        .push(Node {
            id,
            revision: created_revision()?,
            generation: admitted,
            key: None,
            labels: Span::new(label_start, 1),
            properties: Span::new(property_start, count),
            text: None,
            vector: None,
        })
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)
}

// The existing pools own the merged label/property description. Document columns
// replace same-named graph properties; sorted pools retain their public contract.
fn merge_document_node(
    view: &GraphReadView<'_, '_, '_, '_>,
    id: NodeId,
    label_start: u32,
    property_start: u32,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<(Span, Span), NativeResultError> {
    if view
        .document_version(id)
        .map_err(NativeExecutionError::from)
        .map_err(NativeResultError::Native)?
        .is_some()
    {
        let mut has_document = false;
        for name in staging.names.as_slice().iter().skip(label_start as usize) {
            step(resources, 1)?;
            if copied_name(staging.bytes.as_slice(), *name)? == b"Document" {
                has_document = true;
            }
        }
        if !has_document {
            let name = append_text("Document", staging, resources)?;
            staging
                .names
                .push(name)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
        }
        let mut failure = None;
        view.visit_document_properties(id, |name, value| {
            let result = (|| {
                let mut previous = None;
                for (index, property) in staging
                    .properties
                    .as_slice()
                    .iter()
                    .enumerate()
                    .skip(property_start as usize)
                {
                    step(resources, 1)?;
                    if copied_name(staging.bytes.as_slice(), property.name)? == name.as_bytes() {
                        previous = Some(index);
                        break;
                    }
                }
                let value = fill_staged_value(
                    document_value(value)
                        .map_err(NativeExecutionError::from)
                        .map_err(NativeResultError::Native)?,
                    staging,
                    resources,
                )?;
                if let Some(index) = previous {
                    staging
                        .properties
                        .as_mut_slice()
                        .get_mut(index)
                        .ok_or_else(limit)?
                        .value = value;
                } else {
                    let name = append_text(name, staging, resources)?;
                    staging
                        .properties
                        .push(Property { name, value })
                        .map_err(CompletedError::from)
                        .map_err(NativeResultError::Completed)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                failure = Some(error);
                return Err(
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "document projection merge",
                    ),
                );
            }
            Ok(())
        })
        .map_err(|error| failure.unwrap_or_else(|| NativeResultError::Native(error.into())))?;
        sort_copied_names(
            staging
                .names
                .as_mut_slice()
                .get_mut(label_start as usize..)
                .ok_or_else(limit)?,
            staging.bytes.as_slice(),
            |span| span,
            resources,
        )?;
        sort_copied_names(
            staging
                .properties
                .as_mut_slice()
                .get_mut(property_start as usize..)
                .ok_or_else(limit)?,
            staging.bytes.as_slice(),
            |property| property.name,
            resources,
        )?;
    }
    let labels = u32::try_from(staging.names.len())
        .map_err(|_| limit())?
        .checked_sub(label_start)
        .ok_or_else(limit)?;
    let properties = u32::try_from(staging.properties.len())
        .map_err(|_| limit())?
        .checked_sub(property_start)
        .ok_or_else(limit)?;
    Ok((
        Span::new(label_start, labels),
        Span::new(property_start, properties),
    ))
}
fn copied_name(bytes: &[u8], span: Span) -> Result<&[u8], NativeResultError> {
    bytes
        .get(
            span.start as usize
                ..(span.start as usize)
                    .checked_add(span.len as usize)
                    .ok_or_else(limit)?,
        )
        .ok_or_else(limit)
}
fn sort_copied_names<T: Copy>(
    values: &mut [T],
    bytes: &[u8],
    name: impl Fn(T) -> Span,
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    for index in 1..values.len() {
        let mut cursor = index;
        while cursor > 0 {
            step(resources, 1)?;
            let left = copied_name(bytes, name(*values.get(cursor - 1).ok_or_else(limit)?))?;
            let right = copied_name(bytes, name(*values.get(cursor).ok_or_else(limit)?))?;
            if left <= right {
                break;
            }
            values.swap(cursor - 1, cursor);
            cursor -= 1;
        }
    }
    Ok(())
}
