//! Relationship binding origins remain in the immutable DAG rather than a
//! lossy per-slot summary. Every traversal/comparison is charged and cancellable.
use super::*;
#[derive(Clone, Copy)]
struct Branch {
    node: PlanNodeId,
    slot: SlotId,
}
struct Origins {
    pending: [Branch; MAX_PLAN_DEPTH],
    length: usize,
}
impl Origins {
    fn new(node: PlanNodeId, slot: SlotId) -> Self {
        Self {
            pending: [Branch { node, slot }; MAX_PLAN_DEPTH],
            length: 1,
        }
    }
    fn push(&mut self, node: PlanNodeId, slot: SlotId) -> Result<(), PlanError> {
        *self.pending.get_mut(self.length).ok_or(PlanError::Limit)? = Branch { node, slot };
        self.length += 1;
        Ok(())
    }
    fn next(
        &mut self,
        description: PlanDescription<'_>,
        facts: &[NodeFacts],
        context: &mut ValueContext<'_>,
    ) -> Result<Option<(PatternId, PlanNodeId)>, PlanError> {
        while self.length != 0 {
            context.step()?;
            self.length -= 1;
            let branch = *self.pending.get(self.length).ok_or(PlanError::Limit)?;
            let op = description
                .operators
                .get(branch.node.0 as usize)
                .ok_or(PlanError::Reference)?;
            match op.kind {
                OperatorKind::Expand {
                    relationship,
                    pattern,
                    ..
                } if relationship == branch.slot => return Ok(Some((pattern, branch.node))),
                OperatorKind::BoundedExpand {
                    relationships,
                    pattern,
                    ..
                } if relationships == branch.slot => return Ok(Some((pattern, branch.node))),
                OperatorKind::Project(items)
                | OperatorKind::With(items)
                | OperatorKind::Aggregate { keys: items, .. } => {
                    for item in items {
                        context.step()?;
                        if item.slot == branch.slot
                            && let Some(Expression::Slot(source)) =
                                description.expressions.get(item.expression.0 as usize)
                        {
                            self.push(*op.inputs.first().ok_or(PlanError::Arity)?, *source)?;
                        }
                    }
                }
                _ => {
                    // A new lookup/create/expand output has no input binding;
                    // inherited slots follow every dependency that contains it.
                    for input in op.inputs {
                        context.step()?;
                        if facts
                            .get(input.0 as usize)
                            .ok_or(PlanError::Reference)?
                            .slot(branch.slot)
                            .is_some()
                        {
                            self.push(*input, branch.slot)?;
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}
pub(super) fn shared(
    description: PlanDescription<'_>,
    facts: &[NodeFacts],
    left: PlanNodeId,
    right: PlanNodeId,
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    let left_scope = facts.get(left.0 as usize).ok_or(PlanError::Reference)?;
    let right_scope = facts.get(right.0 as usize).ok_or(PlanError::Reference)?;
    for slot in left_scope
        .slots
        .get(..left_scope.width)
        .ok_or(PlanError::Limit)?
    {
        context.step()?;
        let id = SlotId(slot.id);
        if right_scope.slot(id).is_none() {
            continue;
        }
        let mut l = Origins::new(left, id);
        while let Some((pattern, origin)) = l.next(description, facts, context)? {
            let mut r = Origins::new(right, id);
            while let Some((other_pattern, other_origin)) = r.next(description, facts, context)? {
                context.step()?;
                if pattern == other_pattern && origin != other_origin {
                    return Err(PlanError::Scope);
                }
            }
        }
    }
    Ok(())
}
