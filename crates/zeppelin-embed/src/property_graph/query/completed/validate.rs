use super::*;
use crate::property_graph::query::{MAX_LIST_DEPTH, MAX_LIST_ELEMENTS};

#[derive(Clone, Copy, Default)]
struct Geometry {
    depth: u8,
    descendants: usize,
}

pub(super) fn span<T>(values: &[T], range: Span) -> Result<&[T], CompletedError> {
    values
        .get(range.range().ok_or(CompletedError::Shape)?)
        .ok_or(CompletedError::Shape)
}

// Check at most 64KiB per step; a partial UTF-8 suffix is checked with the
// following chunk. Neither a long string nor a multibyte boundary skips control.
fn utf8(
    pools: Pools<'_>,
    range: Span,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), CompletedError> {
    let mut bytes = span(pools.bytes, range)?;
    while !bytes.is_empty() {
        context.values().step()?;
        let length = bytes.len().min(65536);
        let chunk = bytes.get(..length).ok_or(CompletedError::Shape)?;
        let consumed = match std::str::from_utf8(chunk) {
            Ok(_) => length,
            Err(error) if error.error_len().is_none() && length < bytes.len() => {
                error.valid_up_to()
            }
            Err(_) => return Err(CompletedError::Utf8),
        };
        if consumed == 0 {
            return Err(CompletedError::Utf8);
        }
        bytes = bytes.get(consumed..).ok_or(CompletedError::Shape)?;
    }
    Ok(())
}

fn kind(value: Value) -> ValueKinds {
    match value {
        Value::Null => ValueKinds::NULL,
        Value::Bool(_) => ValueKinds::BOOL,
        Value::I64(_) => ValueKinds::I64,
        Value::F64(_) => ValueKinds::F64,
        Value::String(_) => ValueKinds::STRING,
        Value::Node(_) => ValueKinds::NODE,
        Value::Relationship(_) => ValueKinds::REL,
        Value::List { .. } => ValueKinds::LIST,
    }
}
fn list_accepts(element: ListKind, value: Value) -> bool {
    matches!(
        (element, value),
        (ListKind::Query, _)
            | (ListKind::Bool, Value::Bool(_))
            | (ListKind::I64, Value::I64(_))
            | (ListKind::F64, Value::F64(_))
            | (ListKind::String, Value::String(_))
    )
}

fn ordered_names(
    pools: Pools<'_>,
    names: impl Iterator<Item = Span>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), CompletedError> {
    let mut previous: Option<&[u8]> = None;
    for name in names {
        context.values().step()?;
        let bytes = span(pools.bytes, name)?;
        if let Some(old) = previous {
            let mut order = std::cmp::Ordering::Equal;
            for (left, right) in old.chunks(65536).zip(bytes.chunks(65536)) {
                context.values().step()?;
                order = left.cmp(right);
                if order != std::cmp::Ordering::Equal {
                    break;
                }
            }
            if order == std::cmp::Ordering::Equal {
                order = old.len().cmp(&bytes.len());
            }
            if order != std::cmp::Ordering::Less {
                return Err(CompletedError::Shape);
            }
        }
        previous = Some(bytes);
    }
    Ok(())
}
fn key(
    pools: Pools<'_>,
    key: Option<Key>,
    kind: crate::property_graph::EntityKind,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), CompletedError> {
    if let Some(key) = key {
        if key.kind != kind {
            return Err(CompletedError::Shape);
        }
        utf8(pools, key.namespace, context)?;
        utf8(pools, key.value, context)?;
    }
    Ok(())
}
fn properties(
    pools: Pools<'_>,
    range: Span,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), CompletedError> {
    ordered_names(
        pools,
        span(pools.properties, range)?.iter().map(|p| p.name),
        context,
    )
}
fn records(
    pools: Pools<'_>,
    outcome: Outcome,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), CompletedError> {
    use crate::property_graph::EntityKind;
    let admitted = context.view().generation();
    let maximum = match outcome {
        Outcome::Committed { changed } => {
            if admitted.get().checked_add(1) != Some(changed.get()) {
                return Err(CompletedError::Shape);
            }
            changed
        }
        _ => admitted,
    };
    for name in pools.names {
        utf8(pools, *name, context)?;
    }
    for child in pools.children {
        context.values().step()?;
        pools
            .values
            .get(child.0 as usize)
            .ok_or(CompletedError::Shape)?;
    }
    for property in pools.properties {
        context.values().step()?;
        utf8(pools, property.name, context)?;
        let value = pools
            .values
            .get(property.value.0 as usize)
            .ok_or(CompletedError::Shape)?;
        if !matches!(
            value,
            Value::Bool(_)
                | Value::I64(_)
                | Value::F64(_)
                | Value::String(_)
                | Value::List {
                    element: ListKind::Empty
                        | ListKind::Bool
                        | ListKind::I64
                        | ListKind::F64
                        | ListKind::String,
                    ..
                }
        ) {
            return Err(CompletedError::Shape);
        }
    }
    for bits in pools.vectors {
        context.values().step()?;
        if !f32::from_bits(*bits).is_finite() {
            return Err(CompletedError::Shape);
        }
    }
    let mut previous_node = None;
    for node in pools.nodes {
        context.values().step()?;
        if node.generation > maximum || previous_node.is_some_and(|id| id >= node.id) {
            return Err(CompletedError::Shape);
        }
        previous_node = Some(node.id);
        key(pools, node.key, EntityKind::Node, context)?;
        ordered_names(
            pools,
            span(pools.names, node.labels)?.iter().copied(),
            context,
        )?;
        properties(pools, node.properties, context)?;
        if let Some(range) = node.text {
            utf8(pools, range, context)?;
        }
        if let Some(range) = node.vector
            && span(pools.vectors, range)?.is_empty()
        {
            return Err(CompletedError::Shape);
        }
    }
    let mut previous_rel = None;
    for rel in pools.relationships {
        context.values().step()?;
        if rel.generation > maximum || previous_rel.is_some_and(|id| id >= rel.id) {
            return Err(CompletedError::Shape);
        }
        previous_rel = Some(rel.id);
        key(pools, rel.key, EntityKind::Relationship, context)?;
        utf8(pools, rel.relationship_type, context)?;
        properties(pools, rel.properties, context)?;
    }
    for (ordinal, item) in pools.receipts.iter().enumerate() {
        context.values().step()?;
        if item.item_index as usize != ordinal {
            return Err(CompletedError::Shape);
        }
        let receipt = &item.receipt;
        if receipt.generation > maximum
            || matches!(outcome, Outcome::Read | Outcome::NoOp)
            || (matches!(outcome, Outcome::Replayed) && !receipt.replayed)
            || (!receipt.replayed && receipt.generation != maximum)
        {
            return Err(CompletedError::Shape);
        }
    }
    if pools.reports.len() > 8 {
        return Err(CompletedError::Limit);
    }
    for (ordinal, report) in pools.reports.iter().enumerate() {
        context.values().step()?;
        let alpha = f64::from_bits(report.effective_alpha_bits);
        if report.call.0 as usize != ordinal
            || report.generation != admitted
            || !alpha.is_finite()
            || !(0.0..=1.0).contains(&alpha)
            || report.cross_scored_count > report.candidate_count
        {
            return Err(CompletedError::Shape);
        }
        if report.kind == SearchKind::Lexical {
            if report.lexical_leg == LegState::NotRequested {
                return Err(CompletedError::Shape);
            }
        } else if report.actual_tier.is_none()
            || report.vector_leg == LegState::NoQueryMatches
            || (report.vector_leg == LegState::Nonempty
                && report.precision == ScorePrecision::NotApplicable)
        {
            return Err(CompletedError::Shape);
        }
        match report.kind {
            SearchKind::Lexical
                if report.requested_tier.is_some()
                    || report.actual_tier.is_some()
                    || report.precision != ScorePrecision::NotApplicable
                    || report.vector_leg != LegState::NotRequested =>
            {
                return Err(CompletedError::Shape);
            }
            SearchKind::Vector
                if report.lexical_leg != LegState::NotRequested
                    || report.vector_leg == LegState::NotRequested =>
            {
                return Err(CompletedError::Shape);
            }
            SearchKind::Hybrid
                if report.vector_leg == LegState::NotRequested
                    || report.lexical_leg == LegState::NotRequested
                    || !report.cross_score_complete
                    || report.cross_scored_count != report.candidate_count
                    || report.normalization_version == 0
                    || report.rules_version == 0 =>
            {
                return Err(CompletedError::Shape);
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn validate(
    pools: Pools<'_>,
    outcome: Outcome,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<(), CompletedError> {
    records(pools, outcome, context)?;
    // Values are postorder: each list child precedes its parent. This proves a
    // DAG in one bounded pass and permits repeated children without deduplication.
    let mut shapes = QueryArena::<Geometry>::new(context.memory(), pools.values.len())?;
    for column in pools.columns {
        context.values().step()?;
        utf8(pools, column.name, context)?;
        if column.kinds == ValueKinds::default() {
            return Err(CompletedError::Shape);
        }
    }
    for (index, value) in pools.values.iter().copied().enumerate() {
        context.values().step()?;
        let mut geometry = Geometry::default();
        match value {
            Value::String(range) => utf8(pools, range, context)?,
            Value::Node(index) => {
                pools
                    .nodes
                    .get(index as usize)
                    .ok_or(CompletedError::Shape)?;
            }
            Value::Relationship(index) => {
                pools
                    .relationships
                    .get(index as usize)
                    .ok_or(CompletedError::Shape)?;
            }
            Value::List { children, element } => {
                let children = span(pools.children, children)?;
                if element == ListKind::Empty && !children.is_empty() {
                    return Err(CompletedError::Shape);
                }
                geometry.depth = 1;
                geometry.descendants = children.len();
                for child in children {
                    context.values().step()?;
                    if child.0 as usize >= index {
                        return Err(CompletedError::Shape);
                    }
                    let shape = shapes
                        .as_slice()
                        .get(child.0 as usize)
                        .ok_or(CompletedError::Shape)?;
                    let value = *pools
                        .values
                        .get(child.0 as usize)
                        .ok_or(CompletedError::Shape)?;
                    if !list_accepts(element, value) {
                        return Err(CompletedError::Shape);
                    }
                    geometry.depth = geometry
                        .depth
                        .max(shape.depth.checked_add(1).ok_or(CompletedError::Limit)?);
                    geometry.descendants = geometry
                        .descendants
                        .checked_add(shape.descendants)
                        .ok_or(CompletedError::Limit)?;
                    if geometry.depth > MAX_LIST_DEPTH || geometry.descendants > MAX_LIST_ELEMENTS {
                        return Err(CompletedError::Limit);
                    }
                }
            }
            _ => {}
        }
        shapes.push(geometry)?;
    }
    for (index, cell) in pools.cells.iter().enumerate() {
        context.values().step()?;
        let value = *pools
            .values
            .get(cell.0 as usize)
            .ok_or(CompletedError::Shape)?;
        let ordinal = index
            .checked_rem(pools.columns.len())
            .ok_or(CompletedError::Shape)?;
        if !pools
            .columns
            .get(ordinal)
            .ok_or(CompletedError::Shape)?
            .kinds
            .contains(kind(value))
        {
            return Err(CompletedError::Shape);
        }
    }
    Ok(())
}
