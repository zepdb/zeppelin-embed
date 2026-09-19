use super::expression::expression;
use super::*;
#[derive(Clone, Copy)]
struct Frame {
    id: PlanNodeId,
    next: usize,
}
pub(super) fn validate(
    description: PlanDescription<'_>,
    facts: &mut [NodeFacts],
    footprint: PlanFootprint,
    backing: PlanBacking<'_>,
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    preflight(description, facts, footprint, backing, context)?;
    for fact in facts.iter_mut() {
        context.step()?;
        *fact = NodeFacts::default();
    }
    let mut seen = [false; MAX_PLAN_NODES];
    for root in std::iter::once(&description.root).chain(description.eager_searches.iter()) {
        if fact(facts, *root)?.state == 2 {
            continue;
        }
        let mut stack = [Frame { id: *root, next: 0 }; MAX_PLAN_DEPTH];
        let mut depth = 1usize;
        fact_mut(facts, *root)?.state = 1;
        while depth != 0 {
            context.step()?;
            let frame = *stack.get(depth - 1).ok_or(PlanError::Limit)?;
            let operator = description
                .operators
                .get(frame.id.0 as usize)
                .ok_or(PlanError::Reference)?;
            if let Some(child) = operator.inputs.get(frame.next).copied() {
                stack.get_mut(depth - 1).ok_or(PlanError::Limit)?.next += 1;
                match fact(facts, child)?.state {
                    1 => return Err(PlanError::Cycle),
                    2 => {}
                    _ => {
                        *stack.get_mut(depth).ok_or(PlanError::Limit)? =
                            Frame { id: child, next: 0 };
                        depth += 1;
                        fact_mut(facts, child)?.state = 1;
                    }
                }
            } else {
                let result = derive(description, *operator, facts, &mut seen, context)?;
                *fact_mut(facts, frame.id)? = result;
                depth -= 1;
            }
        }
    }
    if facts
        .get(..description.operators.len())
        .ok_or(PlanError::Reference)?
        .iter()
        .any(|f| f.state != 2)
        || seen
            .get(..description.expressions.len())
            .ok_or(PlanError::Reference)?
            .iter()
            .any(|s| !s)
    {
        return Err(PlanError::Unreachable);
    }
    let classification = facts
        .get(..description.operators.len())
        .ok_or(PlanError::Reference)?
        .iter()
        .fold(0, |bits, fact| bits | fact.classification.0);
    if classification & 6 == 6 {
        return Err(PlanError::ReadWriteSearch);
    }
    Ok(())
}
pub(super) fn preflight(
    description: PlanDescription<'_>,
    facts: &[NodeFacts],
    footprint: PlanFootprint,
    backing: PlanBacking<'_>,
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    if description.operators.is_empty()
        || description.operators.len() > MAX_PLAN_NODES
        || description.expressions.len() > MAX_PLAN_NODES
        || description.parameters.len() > MAX_COLUMNS
        || description.eager_searches.len() > 8
    {
        return Err(PlanError::Limit);
    }
    if facts.len() < description.operators.len() {
        return Err(PlanError::Reference);
    }
    super::search::inventory(description, context)?;
    let accounting = super::accounting::Accounting::new(backing, footprint, context)?;
    accounting.span(description.operators, context)?;
    accounting.span(description.expressions, context)?;
    accounting.span(description.parameters, context)?;
    accounting.span(facts, context)?;
    accounting.span(description.eager_searches, context)?;
    for op in description.operators {
        context.step()?;
        accounting.span(op.inputs, context)?;
        match op.kind {
            OperatorKind::LookupKey { namespace, .. } => {
                accounting.span(namespace.as_str().as_bytes(), context)?
            }
            OperatorKind::Sort(keys) => accounting.span(keys, context)?,
            OperatorKind::Mutate(items) => {
                accounting.span(items, context)?;
                for item in items {
                    context.step()?;
                    match item {
                        Mutation::SetProperty { name, .. }
                        | Mutation::RemoveProperty { name, .. }
                        | Mutation::SetLabel { label: name, .. }
                        | Mutation::CreateRelationship {
                            relationship_type: name,
                            ..
                        } => accounting.span(name.as_str().as_bytes(), context)?,
                        Mutation::CreateNode { labels, .. } => {
                            accounting.span(labels, context)?;
                            for label in *labels {
                                context.step()?;
                                accounting.span(label.as_str().as_bytes(), context)?;
                            }
                        }
                        Mutation::Delete { .. } => {}
                    }
                }
            }
            OperatorKind::ScanNodes {
                label: Some(name), ..
            } => accounting.span(name.as_str().as_bytes(), context)?,
            OperatorKind::Aggregate { keys, aggregates } => {
                if keys.len() + aggregates.len() > MAX_COLUMNS {
                    return Err(PlanError::Limit);
                }
                accounting.span(keys, context)?;
                accounting.span(aggregates, context)?;
            }
            OperatorKind::Project(items) | OperatorKind::With(items) => {
                if items.len() > MAX_COLUMNS {
                    return Err(PlanError::Limit);
                }
                accounting.span(items, context)?;
            }
            OperatorKind::Expand {
                relationship_type: Some(name),
                ..
            }
            | OperatorKind::BoundedExpand {
                relationship_type: Some(name),
                ..
            } => accounting.span(name.as_str().as_bytes(), context)?,
            _ => {}
        }
    }
    for expr in description.expressions {
        context.step()?;
        match expr {
            Expression::Literal(Literal::String(text)) => {
                accounting.span(text.as_bytes(), context)?
            }
            Expression::Property { name, .. } | Expression::HasLabel { label: name, .. } => {
                accounting.span(name.as_str().as_bytes(), context)?
            }
            Expression::List(items) => {
                if items.len() > super::super::MAX_LIST_ELEMENTS {
                    return Err(PlanError::Limit);
                }
                accounting.span(items, context)?;
            }
            _ => {}
        }
    }
    for (index, parameter) in description.parameters.iter().enumerate() {
        context.step()?;
        accounting.span(parameter.name.as_bytes(), context)?;
        if parameter.name.is_empty()
            || parameter.kinds == ValueKinds::default()
            || parameter
                .kinds
                .overlaps(ValueKinds::NODE.union(ValueKinds::REL))
        {
            return Err(PlanError::Parameter);
        }
        for previous in description
            .parameters
            .get(..index)
            .ok_or(PlanError::Reference)?
        {
            context.step()?;
            if super::super::value::compare_bytes(
                previous.name.as_bytes(),
                parameter.name.as_bytes(),
                context,
            )? == std::cmp::Ordering::Equal
            {
                return Err(PlanError::Parameter);
            }
        }
    }
    Ok(())
}
fn derive(
    description: PlanDescription<'_>,
    op: Operator<'_>,
    facts: &[NodeFacts],
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
) -> Result<NodeFacts, PlanError> {
    let arity = match op.kind {
        OperatorKind::Unit => 0,
        OperatorKind::OptionalApply { .. } | OperatorKind::Join { .. } => 2,
        _ => 1,
    };
    if op.inputs.len() != arity {
        return Err(PlanError::Arity);
    }
    let input = match op.inputs.first() {
        Some(id) => fact(facts, *id)?.clone(),
        None => NodeFacts::default(),
    };
    let mut output = input.clone();
    output.state = 2;
    output.depth = input.depth + 1;
    if output.depth > MAX_PLAN_DEPTH {
        return Err(PlanError::Limit);
    }
    if input.classification.writes()
        && matches!(
            op.kind,
            OperatorKind::LookupNode { .. }
                | OperatorKind::LookupRelationship { .. }
                | OperatorKind::LookupKey { .. }
                | OperatorKind::ScanNodes { .. }
                | OperatorKind::Expand { .. }
                | OperatorKind::BoundedExpand { .. }
                | OperatorKind::OptionalApply { .. }
                | OperatorKind::Join { .. }
        )
    {
        return Err(PlanError::ReadAfterWrite);
    }
    match op.kind {
        OperatorKind::Sort(keys) => {
            if keys.is_empty() {
                return Err(PlanError::Arity);
            }
            for key in keys {
                expression(description, key.expression, &input, seen, context)?;
            }
            output.ordered = true;
            output.barriers.0 |= 16;
        }
        OperatorKind::Distinct => {
            output.barriers.0 |= 8;
            output.ordered = false;
        }
        OperatorKind::Eager => {
            output.barriers.0 |= 64;
        }
        OperatorKind::Mutate(items) => {
            if !matches!(
                op.inputs
                    .first()
                    .and_then(|id| description.operators.get(id.0 as usize))
                    .map(|op| op.kind),
                Some(OperatorKind::Eager)
            ) {
                return Err(PlanError::Barrier);
            }
            if items.is_empty() {
                return Err(PlanError::Arity);
            }
            super::mutation::validate(description, items, &mut output, seen, context)?;
            output.classification.0 |= 2;
        }
        OperatorKind::Unit => {
            output.singleton = true;
        }
        OperatorKind::Collect => {}
        OperatorKind::OffsetLimit { offset, limit } => {
            output.barriers.0 |= 32;
            if offset != 0 || limit == Some(0) {
                output.singleton = false;
            }
        }
        OperatorKind::Aggregate { keys, aggregates } => {
            output.width = 0;
            for item in keys {
                let kinds = expression(description, item.expression, &input, seen, context)?;
                add_slot(
                    &mut output,
                    Slot {
                        id: item.slot.0,
                        kinds,
                    },
                )?;
            }
            for item in aggregates {
                let kinds = super::expression::aggregate(
                    description,
                    item.expression,
                    &input,
                    seen,
                    context,
                )?;
                add_slot(
                    &mut output,
                    Slot {
                        id: item.slot.0,
                        kinds,
                    },
                )?;
            }
            output.singleton = keys.is_empty();
            output.ordered = false;
            output.barriers.0 |= 4;
        }
        OperatorKind::Search {
            call,
            request,
            node,
            score,
        } => {
            super::search::validate(description, call, request, &input, seen, context)?;
            add_slot(
                &mut output,
                Slot {
                    id: node.0,
                    kinds: ValueKinds::NODE,
                },
            )?;
            add_slot(
                &mut output,
                Slot {
                    id: score.0,
                    kinds: ValueKinds::F64,
                },
            )?;
            output.classification.0 |= 5;
            output.barriers.0 |= 128;
            output.search_calls |= 1u8.checked_shl(call.0).ok_or(PlanError::Search)?;
            output.singleton = false;
            output.ordered = false;
        }
        OperatorKind::LookupNode { output: slot, .. }
        | OperatorKind::ScanNodes { output: slot, .. } => {
            add_slot(
                &mut output,
                Slot {
                    id: slot.0,
                    kinds: ValueKinds::NODE,
                },
            )?;
            output.classification.0 |= 1;
            output.singleton = false;
            output.ordered = false;
        }
        OperatorKind::LookupRelationship { output: slot, .. } => {
            add_slot(
                &mut output,
                Slot {
                    id: slot.0,
                    kinds: ValueKinds::REL,
                },
            )?;
            output.classification.0 |= 1;
            output.singleton = false;
            output.ordered = false;
        }
        OperatorKind::LookupKey {
            output: slot,
            namespace,
            key,
            kind,
        } => {
            if !expression(description, key, &input, seen, context)?
                .overlaps(ValueKinds::STRING.union(ValueKinds::NULL))
            {
                return Err(PlanError::Type);
            }
            if let Some(Expression::Literal(Literal::String(text))) =
                description.expressions.get(key.0 as usize)
            {
                crate::property_graph::ApplicationKey::new(kind, namespace.as_str(), text)
                    .map_err(|_| PlanError::Limit)?;
            }
            let kinds = match kind {
                EntityKind::Node => ValueKinds::NODE,
                EntityKind::Relationship => ValueKinds::REL,
            };
            add_slot(&mut output, Slot { id: slot.0, kinds })?;
            output.classification.0 |= 1;
            output.singleton = false;
            output.ordered = false;
        }
        OperatorKind::Expand {
            source,
            node,
            relationship,
            ..
        } => {
            expand(&mut output, source, node, relationship, ValueKinds::REL)?;
        }
        OperatorKind::BoundedExpand {
            source,
            node,
            relationships,
            min,
            max,
            ..
        } => {
            if min > max || max > 16 {
                return Err(PlanError::PathBound);
            }
            expand(&mut output, source, node, relationships, ValueKinds::LIST)?;
        }
        OperatorKind::OptionalApply { predicate } | OperatorKind::Join { predicate } => {
            let right = fact(facts, *op.inputs.get(1).ok_or(PlanError::Arity)?)?;
            if right.classification.writes() {
                return Err(PlanError::ReadAfterWrite);
            }
            super::lineage::shared(
                description,
                facts,
                *op.inputs.first().ok_or(PlanError::Arity)?,
                *op.inputs.get(1).ok_or(PlanError::Arity)?,
                context,
            )?;
            let mut combined = input.clone();
            merge_scope(&mut combined, right, false, context)?;
            if let Some(id) = predicate {
                boolean(description, id, &combined, seen, context)?;
            }
            let optional = matches!(op.kind, OperatorKind::OptionalApply { .. });
            merge_scope(&mut output, right, optional, context)?;
            output.classification.0 |= right.classification.0;
            output.search_calls |= right.search_calls;
            output.singleton = false;
            output.ordered = false;
            output.barriers.0 |= right.barriers.0 | u16::from(optional);
            output.depth = output.depth.max(right.depth + 1);
            if output.depth > MAX_PLAN_DEPTH {
                return Err(PlanError::Limit);
            }
        }
        OperatorKind::Project(items) | OperatorKind::With(items) => {
            if matches!(op.kind, OperatorKind::With(_)) {
                output.barriers.0 |= 2;
            }
            output.width = 0;
            for item in items {
                context.step()?;
                if output.slot(item.slot).is_some() {
                    return Err(PlanError::Scope);
                }
                let kinds = expression(description, item.expression, &input, seen, context)?;
                *output.slots.get_mut(output.width).ok_or(PlanError::Limit)? = Slot {
                    id: item.slot.0,
                    kinds,
                };
                output.width += 1;
            }
        }
        OperatorKind::Filter(id) => {
            output.singleton = false;
            boolean(description, id, &input, seen, context)?;
        }
    }
    Ok(output)
}
fn fact(facts: &[NodeFacts], id: PlanNodeId) -> Result<&NodeFacts, PlanError> {
    facts.get(id.0 as usize).ok_or(PlanError::Reference)
}
fn fact_mut(facts: &mut [NodeFacts], id: PlanNodeId) -> Result<&mut NodeFacts, PlanError> {
    facts.get_mut(id.0 as usize).ok_or(PlanError::Reference)
}

pub(super) fn add_slot(scope: &mut NodeFacts, slot: Slot) -> Result<(), PlanError> {
    if scope.slot(SlotId(slot.id)).is_some() {
        return Err(PlanError::Scope);
    }
    *scope.slots.get_mut(scope.width).ok_or(PlanError::Limit)? = slot;
    scope.width += 1;
    Ok(())
}
fn expand(
    output: &mut NodeFacts,
    source: SlotId,
    node: SlotId,
    rel: SlotId,
    kind: ValueKinds,
) -> Result<(), PlanError> {
    if !output
        .slot(source)
        .ok_or(PlanError::Scope)?
        .overlaps(ValueKinds::NODE.union(ValueKinds::NULL))
    {
        return Err(PlanError::Type);
    }
    add_slot(
        output,
        Slot {
            id: node.0,
            kinds: ValueKinds::NODE,
        },
    )?;
    add_slot(
        output,
        Slot {
            id: rel.0,
            kinds: kind,
        },
    )?;
    output.classification.0 |= 1;
    output.singleton = false;
    output.ordered = false;
    Ok(())
}
fn merge_scope(
    output: &mut NodeFacts,
    right: &NodeFacts,
    nullable: bool,
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    for slot in right.slots.get(..right.width).ok_or(PlanError::Limit)? {
        context.step()?;
        if let Some(existing) = output
            .slots
            .get_mut(..output.width)
            .and_then(|slots| slots.iter_mut().find(|other| other.id == slot.id))
        {
            let narrowed = compatible(existing.kinds, slot.kinds)?;
            if !nullable {
                existing.kinds = narrowed;
            }
        } else {
            let mut slot = *slot;
            if nullable {
                slot.kinds = slot.kinds.union(ValueKinds::NULL);
            }
            add_slot(output, slot)?;
        }
    }
    Ok(())
}
fn boolean(
    description: PlanDescription<'_>,
    id: ExprId,
    scope: &NodeFacts,
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    if !expression(description, id, scope, seen, context)?
        .overlaps(ValueKinds::BOOL.union(ValueKinds::NULL))
    {
        return Err(PlanError::Type);
    }
    Ok(())
}

fn compatible(left: ValueKinds, right: ValueKinds) -> Result<ValueKinds, PlanError> {
    let l = left.0 & !ValueKinds::NULL.0;
    let r = right.0 & !ValueKinds::NULL.0;
    if l == 0 || r == 0 {
        return Ok(ValueKinds::NULL);
    }
    let numeric = ValueKinds::I64.0 | ValueKinds::F64.0;
    let kinds = (l & r) | if r & numeric != 0 { l & numeric } else { 0 };
    if kinds == 0 {
        return Err(PlanError::Scope);
    }
    Ok(ValueKinds(kinds))
}
