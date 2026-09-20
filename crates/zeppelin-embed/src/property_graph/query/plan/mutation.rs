use super::expression::expression;
use super::validate::add_slot;
use super::*;
pub(super) fn validate(
    description: PlanDescription<'_>,
    items: &[Mutation<'_>],
    output: &mut NodeFacts,
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    for item in items {
        context.step()?;
        match *item {
            Mutation::CreateNode { output: slot, .. } => add_slot(
                output,
                Slot {
                    id: slot.0,
                    kinds: ValueKinds::NODE,
                },
            )?,
            Mutation::CreateRelationship {
                output: slot,
                source,
                target,
                ..
            } => {
                entity(description, source, output, seen, context, ValueKinds::NODE)?;
                entity(description, target, output, seen, context, ValueKinds::NODE)?;
                add_slot(
                    output,
                    Slot {
                        id: slot.0,
                        kinds: ValueKinds::REL,
                    },
                )?;
            }
            Mutation::SetProperty {
                entity: receiver,
                value,
                ..
            } => {
                entity(
                    description,
                    receiver,
                    output,
                    seen,
                    context,
                    ValueKinds::NODE
                        .union(ValueKinds::REL)
                        .union(ValueKinds::NULL),
                )?;
                let value = expression(description, value, output, seen, context)?;
                if !value.overlaps(
                    ValueKinds::NULL
                        .union(ValueKinds::BOOL)
                        .union(ValueKinds::I64)
                        .union(ValueKinds::F64)
                        .union(ValueKinds::STRING)
                        .union(ValueKinds::LIST),
                ) {
                    return Err(PlanError::Type);
                }
            }
            Mutation::RemoveProperty {
                entity: receiver, ..
            } => entity(
                description,
                receiver,
                output,
                seen,
                context,
                ValueKinds::NODE
                    .union(ValueKinds::REL)
                    .union(ValueKinds::NULL),
            )?,
            Mutation::SetLabel {
                entity: receiver, ..
            } => entity(
                description,
                receiver,
                output,
                seen,
                context,
                ValueKinds::NODE.union(ValueKinds::NULL),
            )?,
            Mutation::Delete {
                entity: receiver, ..
            } => entity(
                description,
                receiver,
                output,
                seen,
                context,
                ValueKinds::NODE
                    .union(ValueKinds::REL)
                    .union(ValueKinds::NULL),
            )?,
        }
    }
    Ok(())
}
fn entity(
    description: PlanDescription<'_>,
    id: ExprId,
    scope: &NodeFacts,
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
    kinds: ValueKinds,
) -> Result<(), PlanError> {
    if !expression(description, id, scope, seen, context)?.overlaps(kinds) {
        return Err(PlanError::Type);
    }
    Ok(())
}
