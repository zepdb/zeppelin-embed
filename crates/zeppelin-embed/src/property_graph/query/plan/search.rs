use super::expression::expression;
use super::*;
pub(super) fn inventory(
    description: PlanDescription<'_>,
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    let mut count = 0;
    for (index, op) in description.operators.iter().enumerate() {
        context.step()?;
        if let OperatorKind::Search { call, .. } = op.kind {
            if description
                .eager_searches
                .get(call.0 as usize)
                .map(|id| id.0 as usize)
                != Some(index)
            {
                return Err(PlanError::Search);
            }
            count += 1;
        }
    }
    if count != description.eager_searches.len() {
        return Err(PlanError::Search);
    }
    Ok(())
}
pub(super) fn validate(
    description: PlanDescription<'_>,
    call: SearchCallId,
    request: SearchRequest,
    input: &NodeFacts,
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    if !input.singleton
        || call.0 >= 8
        || u32::from(input.search_calls) >= 1u32.checked_shl(call.0).ok_or(PlanError::Search)?
    {
        return Err(PlanError::Search);
    }

    let (k, eligible) = match request {
        SearchRequest::Text { query, k, eligible } => {
            argument(description, query, ValueKinds::STRING, input, seen, context)?;
            (k, eligible)
        }
        SearchRequest::Vector {
            vector,
            k,
            eligible,
            ..
        } => {
            argument(description, vector, ValueKinds::LIST, input, seen, context)?;
            (k, eligible)
        }
        SearchRequest::Hybrid {
            vector,
            text,
            k,
            eligible,
            ..
        } => {
            argument(description, vector, ValueKinds::LIST, input, seen, context)?;
            argument(description, text, ValueKinds::STRING, input, seen, context)?;
            (k, eligible)
        }
    };
    argument(description, k, ValueKinds::I64, input, seen, context)?;
    if let Some(Expression::Literal(Literal::I64(value))) =
        description.expressions.get(k.0 as usize)
    {
        SearchBounds::new(*value, 0)?;
    }
    if let Some(id) = eligible {
        argument(description, id, ValueKinds::LIST, input, seen, context)?;
    }
    Ok(())
}
fn argument(
    description: PlanDescription<'_>,
    id: ExprId,
    kinds: ValueKinds,
    input: &NodeFacts,
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    if !expression(description, id, input, seen, context)?.overlaps(kinds) {
        return Err(PlanError::Type);
    }
    Ok(())
}
/// Explicit vector candidate coverage mode; runtime provenance remains required.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchMode {
    /// Exhaustive streaming ranking.
    Exact,
    /// Approximate candidate coverage, never relabeled exact.
    Approximate,
}
/// Shared checked limits for evaluated requests, before retrieval invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchBounds {
    k: u32,
    candidate_window: u32,
}
impl SearchBounds {
    /// Rejects invalid/over-limit values without clamping. A zero retained
    /// window is a tightened resource allowance, not permission to omit work:
    /// runtime must fail if the selected method needs any retained candidates.
    pub fn new(k: i64, candidate_window: u64) -> Result<Self, PlanError> {
        if !(1..=4096).contains(&k) || candidate_window > 65_536 {
            return Err(PlanError::Search);
        }
        Ok(Self {
            k: u32::try_from(k).map_err(|_| PlanError::Search)?,
            candidate_window: u32::try_from(candidate_window).map_err(|_| PlanError::Search)?,
        })
    }
    /// Requested result count.
    pub const fn k(self) -> u32 {
        self.k
    }
    /// Explicit maximum retained candidate capacity; not a scan/visit cap.
    pub const fn candidate_window(self) -> u32 {
        self.candidate_window
    }
}
