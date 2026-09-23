use super::{NativeResultError, NativeStaging, Sizes};
use crate::property_graph::query::completed::{CompletedError, ListKind, Span, Value, ValueIndex};
use crate::property_graph::query::resources::QueryArena;
use crate::property_graph::query::runtime::{PreparedRows, RuntimeContext, RuntimeError, WorkKind};
use crate::property_graph::query::{QueryList, QueryValue};
use crate::property_graph::storage::stream::{PayloadCursor, PayloadSlice};
use crate::property_graph::storage::tree::directory::{BlockSource, TreeResources};
use crate::property_graph::{NodeId, RelId};

fn limit() -> NativeResultError {
    NativeResultError::Completed(CompletedError::Limit)
}

fn checked_add(target: &mut usize, count: usize) -> Result<(), NativeResultError> {
    *target = target.checked_add(count).ok_or_else(limit)?;
    Ok(())
}

fn measure_value(
    value: QueryValue<'_>,
    sizes: &mut Sizes,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), NativeResultError> {
    context.values().step().map_err(RuntimeError::from)?;
    value
        .validate(context.values())
        .map_err(RuntimeError::from)?;
    match value {
        QueryValue::String(text) => checked_add(&mut sizes.bytes, text.len())?,
        QueryValue::NodeRef(_) => checked_add(&mut sizes.node_occurrences, 1)?,
        QueryValue::RelRef(_) => checked_add(&mut sizes.relationship_occurrences, 1)?,
        QueryValue::List(list) => {
            checked_add(&mut sizes.children, list.len())?;
            for index in 0..list.len() {
                let child = list
                    .get(index)
                    .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
                measure_value(child, sizes, context)?;
            }
            if list.get(list.len()).is_some() {
                return Err(NativeResultError::Completed(CompletedError::Shape));
            }
        }
        QueryValue::Null | QueryValue::Bool(_) | QueryValue::I64(_) | QueryValue::F64(_) => {}
    }
    checked_add(&mut sizes.values, 1)
}

pub(super) fn measure_rows(
    rows: &PreparedRows<'_, '_, '_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<Sizes, NativeResultError> {
    let mut sizes = Sizes {
        cells: rows.rows().checked_mul(rows.columns()).ok_or_else(limit)?,
        ..Sizes::default()
    };
    for row in 0..rows.rows() {
        for column in 0..rows.columns() {
            let value = rows
                .value(row, column)
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?;
            measure_value(value, &mut sizes, context)?;
        }
    }
    Ok(sizes)
}

fn collect_value_ids(
    value: QueryValue<'_>,
    nodes: &mut QueryArena<'_, '_, NodeId>,
    relationships: &mut QueryArena<'_, '_, RelId>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), NativeResultError> {
    context.values().step().map_err(RuntimeError::from)?;
    value
        .validate(context.values())
        .map_err(RuntimeError::from)?;
    match value {
        QueryValue::NodeRef(value) => nodes
            .push(value.id())
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?,
        QueryValue::RelRef(value) => relationships
            .push(value.id())
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?,
        QueryValue::List(list) => {
            for index in 0..list.len() {
                collect_value_ids(
                    list.get(index)
                        .ok_or(NativeResultError::Completed(CompletedError::Shape))?,
                    nodes,
                    relationships,
                    context,
                )?;
            }
        }
        QueryValue::Null
        | QueryValue::Bool(_)
        | QueryValue::I64(_)
        | QueryValue::F64(_)
        | QueryValue::String(_) => {}
    }
    Ok(())
}

pub(super) fn collect_entity_ids<'m, 'g>(
    rows: &PreparedRows<'_, 'm, 'g>,
    sizes: Sizes,
    context: &mut RuntimeContext<'_, 'm, 'g>,
) -> Result<(QueryArena<'m, 'g, NodeId>, QueryArena<'m, 'g, RelId>), NativeResultError> {
    let mut nodes = QueryArena::new(context.memory(), sizes.node_occurrences)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    let mut relationships = QueryArena::new(context.memory(), sizes.relationship_occurrences)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    for row in 0..rows.rows() {
        for column in 0..rows.columns() {
            collect_value_ids(
                rows.value(row, column)
                    .ok_or(NativeResultError::Completed(CompletedError::Shape))?,
                &mut nodes,
                &mut relationships,
                context,
            )?;
        }
    }
    Ok((nodes, relationships))
}

fn find<T: Ord>(
    values: &[T],
    wanted: &T,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<Option<u32>, NativeResultError> {
    let (mut low, mut high) = (0usize, values.len());
    while low < high {
        context.values().step().map_err(RuntimeError::from)?;
        let mid = low + (high - low) / 2;
        match values
            .get(mid)
            .ok_or(NativeResultError::Completed(CompletedError::Shape))?
            .cmp(wanted)
        {
            std::cmp::Ordering::Equal => {
                return u32::try_from(mid).map(Some).map_err(|_| limit());
            }
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
        }
    }
    Ok(None)
}

fn append_bytes(
    bytes: &[u8],
    staging: &mut NativeStaging<'_, '_, '_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<Span, NativeResultError> {
    let start = u32::try_from(staging.bytes.len()).map_err(|_| limit())?;
    staging
        .bytes
        .extend_copy(bytes)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    context.charge(WorkKind::CopiedBytes, bytes.len() as u64)?;
    Ok(Span::new(
        start,
        u32::try_from(bytes.len()).map_err(|_| limit())?,
    ))
}

fn push_query_value(
    value: QueryValue<'_>,
    node_ids: &[NodeId],
    relationship_ids: &[RelId],
    staging: &mut NativeStaging<'_, '_, '_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<ValueIndex, NativeResultError> {
    context.values().step().map_err(RuntimeError::from)?;
    value
        .validate(context.values())
        .map_err(RuntimeError::from)?;
    let value = match value {
        QueryValue::Null => Value::Null,
        QueryValue::Bool(value) => Value::Bool(value),
        QueryValue::I64(value) => Value::I64(value),
        QueryValue::F64(value) => Value::F64(value.to_bits()),
        QueryValue::String(value) => {
            Value::String(append_bytes(value.as_bytes(), staging, context)?)
        }
        QueryValue::NodeRef(value) => Value::Node(find(node_ids, &value.id(), context)?.ok_or(
            NativeResultError::Completed(CompletedError::Source(
                crate::property_graph::query::completed::SourceError::Missing(
                    crate::property_graph::EntityId::Node(value.id()),
                ),
            )),
        )?),
        QueryValue::RelRef(value) => {
            Value::Relationship(find(relationship_ids, &value.id(), context)?.ok_or(
                NativeResultError::Completed(CompletedError::Source(
                    crate::property_graph::query::completed::SourceError::Missing(
                        crate::property_graph::EntityId::Relationship(value.id()),
                    ),
                )),
            )?)
        }
        QueryValue::List(list) => {
            push_query_list(list, node_ids, relationship_ids, staging, context)?
        }
    };
    let index = ValueIndex(u32::try_from(staging.values.len()).map_err(|_| limit())?);
    staging
        .values
        .push(value)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    Ok(index)
}

fn push_query_list(
    list: QueryList<'_>,
    node_ids: &[NodeId],
    relationship_ids: &[RelId],
    staging: &mut NativeStaging<'_, '_, '_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<Value, NativeResultError> {
    let mut immediate = QueryArena::new(context.memory(), list.len())
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    for index in 0..list.len() {
        let child = push_query_value(
            list.get(index)
                .ok_or(NativeResultError::Completed(CompletedError::Shape))?,
            node_ids,
            relationship_ids,
            staging,
            context,
        )?;
        immediate
            .push(child)
            .map_err(CompletedError::from)
            .map_err(NativeResultError::Completed)?;
    }
    let start = u32::try_from(staging.children.len()).map_err(|_| limit())?;
    staging
        .children
        .extend_copy(immediate.as_slice())
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    Ok(Value::List {
        children: Span::new(start, u32::try_from(list.len()).map_err(|_| limit())?),
        element: ListKind::Query,
    })
}

pub(super) fn fill_rows(
    rows: &PreparedRows<'_, '_, '_>,
    node_ids: &[NodeId],
    relationship_ids: &[RelId],
    staging: &mut NativeStaging<'_, '_, '_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), NativeResultError> {
    for row in 0..rows.rows() {
        for column in 0..rows.columns() {
            let index = push_query_value(
                rows.value(row, column)
                    .ok_or(NativeResultError::Completed(CompletedError::Shape))?,
                node_ids,
                relationship_ids,
                staging,
                context,
            )?;
            staging
                .cells
                .push(index)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
        }
    }
    Ok(())
}

pub(super) fn append_column_bytes(
    value: &[u8],
    staging: &mut NativeStaging<'_, '_, '_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<Span, NativeResultError> {
    append_bytes(value, staging, context)
}

fn stored_byte<S: BlockSource>(
    cursor: &mut PayloadCursor<'_, '_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<u8, NativeResultError> {
    Ok(u8::from_le_bytes(
        cursor
            .read_array(resources)
            .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
            .map_err(NativeResultError::Native)?,
    ))
}

fn stored_count<S: BlockSource>(
    cursor: &mut PayloadCursor<'_, '_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<u64, NativeResultError> {
    Ok(u64::from_le_bytes(
        cursor
            .read_array(resources)
            .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
            .map_err(NativeResultError::Native)?,
    ))
}

pub(super) fn measure_stored<S: BlockSource>(
    payload: PayloadSlice<'_, S>,
    sizes: &mut Sizes,
    resources: &mut TreeResources<'_>,
) -> Result<(), NativeResultError> {
    let mut cursor = PayloadCursor::new(payload);
    let tag = stored_byte(&mut cursor, resources)?;
    match tag {
        1 => {
            let text = cursor
                .blob(resources)
                .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
                .map_err(NativeResultError::Native)?;
            checked_add(
                &mut sizes.bytes,
                usize::try_from(text.len()).map_err(|_| limit())?,
            )?;
            checked_add(&mut sizes.values, 1)?;
        }
        2 => {
            if stored_byte(&mut cursor, resources)? > 1 {
                return Err(NativeResultError::Completed(CompletedError::Shape));
            }
            checked_add(&mut sizes.values, 1)?;
        }
        3 | 4 => {
            cursor
                .read_array::<8>(resources)
                .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
                .map_err(NativeResultError::Native)?;
            checked_add(&mut sizes.values, 1)?;
        }
        5..=9 => {
            let count =
                usize::try_from(stored_count(&mut cursor, resources)?).map_err(|_| limit())?;
            if tag == 5 && count != 0 {
                return Err(NativeResultError::Completed(CompletedError::Shape));
            }
            checked_add(&mut sizes.children, count)?;
            checked_add(&mut sizes.values, count.checked_add(1).ok_or_else(limit)?)?;
            for _ in 0..count {
                match tag {
                    6 => {
                        let text = cursor
                            .blob(resources)
                            .map_err(
                                crate::property_graph::query::runtime::NativeExecutionError::from,
                            )
                            .map_err(NativeResultError::Native)?;
                        checked_add(
                            &mut sizes.bytes,
                            usize::try_from(text.len()).map_err(|_| limit())?,
                        )?;
                    }
                    7 => {
                        if stored_byte(&mut cursor, resources)? > 1 {
                            return Err(NativeResultError::Completed(CompletedError::Shape));
                        }
                    }
                    8 | 9 => {
                        cursor
                            .read_array::<8>(resources)
                            .map_err(
                                crate::property_graph::query::runtime::NativeExecutionError::from,
                            )
                            .map_err(NativeResultError::Native)?;
                    }
                    _ => return Err(NativeResultError::Completed(CompletedError::Shape)),
                }
            }
        }
        _ => return Err(NativeResultError::Completed(CompletedError::Shape)),
    }
    cursor
        .finish(resources)
        .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
        .map_err(NativeResultError::Native)
}

pub(super) fn push_stored_scalar(
    value: Value,
    staging: &mut NativeStaging<'_, '_, '_>,
) -> Result<ValueIndex, NativeResultError> {
    let index = ValueIndex(u32::try_from(staging.values.len()).map_err(|_| limit())?);
    staging
        .values
        .push(value)
        .map_err(CompletedError::from)
        .map_err(NativeResultError::Completed)?;
    Ok(index)
}

pub(super) fn fill_stored<S: BlockSource>(
    payload: PayloadSlice<'_, S>,
    staging: &mut NativeStaging<'_, '_, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<ValueIndex, NativeResultError> {
    let mut cursor = PayloadCursor::new(payload);
    let tag = stored_byte(&mut cursor, resources)?;
    let value = match tag {
        1 => {
            let text = cursor
                .blob(resources)
                .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
                .map_err(NativeResultError::Native)?;
            let span = super::entities::append_payload(text, staging, resources)?;
            push_stored_scalar(Value::String(span), staging)?
        }
        2 => {
            let value = stored_byte(&mut cursor, resources)?;
            if value > 1 {
                return Err(NativeResultError::Completed(CompletedError::Shape));
            }
            push_stored_scalar(Value::Bool(value == 1), staging)?
        }
        3 => push_stored_scalar(
            Value::I64(i64::from_le_bytes(
                cursor
                    .read_array(resources)
                    .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
                    .map_err(NativeResultError::Native)?,
            )),
            staging,
        )?,
        4 => push_stored_scalar(
            Value::F64(u64::from_le_bytes(
                cursor
                    .read_array(resources)
                    .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
                    .map_err(NativeResultError::Native)?,
            )),
            staging,
        )?,
        5..=9 => {
            let count =
                usize::try_from(stored_count(&mut cursor, resources)?).map_err(|_| limit())?;
            if tag == 5 && count != 0 {
                return Err(NativeResultError::Completed(CompletedError::Shape));
            }
            let start = u32::try_from(staging.children.len()).map_err(|_| limit())?;
            for _ in 0..count {
                let child = match tag {
                    6 => {
                        let text = cursor
                            .blob(resources)
                            .map_err(
                                crate::property_graph::query::runtime::NativeExecutionError::from,
                            )
                            .map_err(NativeResultError::Native)?;
                        let span = super::entities::append_payload(text, staging, resources)?;
                        push_stored_scalar(Value::String(span), staging)?
                    }
                    7 => {
                        let value = stored_byte(&mut cursor, resources)?;
                        if value > 1 {
                            return Err(NativeResultError::Completed(CompletedError::Shape));
                        }
                        push_stored_scalar(Value::Bool(value == 1), staging)?
                    }
                    8 => push_stored_scalar(
                        Value::I64(i64::from_le_bytes(
                            cursor
                                .read_array(resources)
                                .map_err(
                                    crate::property_graph::query::runtime::NativeExecutionError::from,
                                )
                                .map_err(NativeResultError::Native)?,
                        )),
                        staging,
                    )?,
                    9 => push_stored_scalar(
                        Value::F64(u64::from_le_bytes(
                            cursor
                                .read_array(resources)
                                .map_err(
                                    crate::property_graph::query::runtime::NativeExecutionError::from,
                                )
                                .map_err(NativeResultError::Native)?,
                        )),
                        staging,
                    )?,
                    _ => return Err(NativeResultError::Completed(CompletedError::Shape)),
                };
                staging
                    .children
                    .push(child)
                    .map_err(CompletedError::from)
                    .map_err(NativeResultError::Completed)?;
            }
            push_stored_scalar(
                Value::List {
                    children: Span::new(start, u32::try_from(count).map_err(|_| limit())?),
                    element: match tag {
                        5 => ListKind::Empty,
                        6 => ListKind::String,
                        7 => ListKind::Bool,
                        8 => ListKind::I64,
                        9 => ListKind::F64,
                        _ => return Err(NativeResultError::Completed(CompletedError::Shape)),
                    },
                },
                staging,
            )?
        }
        _ => return Err(NativeResultError::Completed(CompletedError::Shape)),
    };
    cursor
        .finish(resources)
        .map_err(crate::property_graph::query::runtime::NativeExecutionError::from)
        .map_err(NativeResultError::Native)?;
    Ok(value)
}
