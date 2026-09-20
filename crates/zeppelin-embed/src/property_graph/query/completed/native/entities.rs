use super::{NativeResultError, NativeStaging, Sizes};
use crate::property_graph::catalog::Symbol;
use crate::property_graph::query::completed::{
    CompletedError, Key, Node, Property, Relationship, SourceError, Span,
};
use crate::property_graph::query::resources::QueryArena;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeError};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::records::RecordShape;
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{
    BlockSource, NativeReadEvent, TreeResources,
};
use crate::property_graph::{EntityId, GraphName, NodeId, RelId};

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

pub(super) fn measure_entities(
    view: &GraphReadView<'_, '_, '_, '_>,
    node_ids: &[NodeId],
    relationship_ids: &[RelId],
    sizes: &mut Sizes,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), NativeResultError> {
    sizes.nodes = node_ids.len();
    sizes.relationships = relationship_ids.len();
    let mut resources = TreeResources::for_query(context)
        .map_err(NativeExecutionError::from)
        .map_err(NativeResultError::Native)?;
    for id in node_ids {
        let node = view
            .lookup_node(*id, &mut resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?
            .ok_or(NativeResultError::Completed(CompletedError::Source(
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
    }
    for id in relationship_ids {
        let relationship = view
            .lookup_relationship(*id, &mut resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?
            .ok_or(NativeResultError::Completed(CompletedError::Source(
                SourceError::Missing(EntityId::Relationship(*id)),
            )))?;
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
    let start = u32::try_from(staging.bytes.len()).map_err(|_| limit())?;
    for chunk in name.as_str().as_bytes().chunks(65536) {
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
        u32::try_from(name.as_str().len()).map_err(|_| limit())?,
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

pub(super) fn fill_entities(
    view: &GraphReadView<'_, '_, '_, '_>,
    node_ids: &[NodeId],
    relationship_ids: &[RelId],
    name_scratch: usize,
    staging: &mut NativeStaging<'_, '_, '_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), NativeResultError> {
    let mut scratch = QueryArena::new(context.memory(), name_scratch)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    let mut resources = TreeResources::for_query(context)
        .map_err(NativeExecutionError::from)
        .map_err(NativeResultError::Native)?;
    for id in node_ids {
        let node = view
            .lookup_node(*id, &mut resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?
            .ok_or(NativeResultError::Completed(CompletedError::Source(
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
        staging
            .nodes
            .push(Node {
                id: *id,
                revision: record.revision(),
                generation: record.provenance().original_generation(),
                key,
                labels: Span::new(label_start, labels),
                properties,
                text: None,
                vector: None,
            })
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    for id in relationship_ids {
        let relationship = view
            .lookup_relationship(*id, &mut resources)
            .map_err(NativeExecutionError::from)
            .map_err(NativeResultError::Native)?
            .ok_or(NativeResultError::Completed(CompletedError::Source(
                SourceError::Missing(EntityId::Relationship(*id)),
            )))?;
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

use crate::property_graph::query::runtime::NativeExecutionError;
