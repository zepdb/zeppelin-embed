//! ZE-64: the real `SearchAdapter`, composing ZE-62's `rank_vector` and
//! ZE-63's `rank_text`/`rank_hybrid` behind the ZE-53 eager `Search` seam.
//!
//! Every call routes through the invocation's own admitted `view`, so a
//! search and any surrounding graph expansion in the same statement read
//! the same admitted active+published view; the adapter never admits or
//! opens a second one. Reports are built as honest translations of the
//! real ranking producers' own reports, never invented: an unpopulated
//! field (the document/query/tokenizer epochs) stays `None` because no
//! production code anywhere in this crate populates it yet.

use super::super::{LegState, ScorePrecision, SearchKind, SearchReport};
use crate::fts::tokenizer::Analyzer;
use crate::property_graph::query::eligibility::Eligibility;
use crate::property_graph::query::pattern::{
    SearchAdapter, SearchArguments, SearchHit, SearchInvocation,
};
use crate::property_graph::query::plan::SearchBounds;
use crate::property_graph::query::resources::{QueryArena, QueryMemory};
use crate::property_graph::query::runtime::{NativeExecutionError, RuntimeContext, RuntimeError};
use crate::property_graph::query::{QueryError, QueryList, QueryValue};
use crate::property_graph::retrieval::{NativeRetrievalContext, RetrievalError};
use crate::property_graph::storage::GraphReadView;

fn retrieval(error: RetrievalError) -> NativeExecutionError {
    error.into()
}

/// Copies a `QueryList` numeric argument into owned charged `f32` storage.
/// The evaluator only ever produces `F64`/`I64` list elements for a
/// literal or parameter vector; anything else is a plan/argument-type
/// mismatch the caller should have rejected before invoking search.
fn copy_vector<'m, 'g>(
    memory: &'m QueryMemory<'g>,
    list: QueryList<'_>,
) -> Result<QueryArena<'m, 'g, f32>, NativeExecutionError> {
    let mut coordinates = QueryArena::new(memory, list.len()).map_err(RuntimeError::Memory)?;
    for index in 0..list.len() {
        let value = list.get(index).ok_or(RuntimeError::Batch)?;
        let coordinate = match value {
            QueryValue::F64(value) => {
                if !value.is_finite() {
                    return Err(RuntimeError::Value(QueryError::ArithmeticDomain).into());
                }
                let coordinate = value as f32;
                if !coordinate.is_finite() {
                    return Err(RuntimeError::Value(QueryError::ArithmeticOverflow).into());
                }
                coordinate
            }
            QueryValue::I64(value) => value as f32,
            _ => return Err(RuntimeError::Batch.into()),
        };
        coordinates.push(coordinate).map_err(RuntimeError::Memory)?;
    }
    Ok(coordinates)
}

/// Copies `invocation`'s eligibility by value; `Eligibility` borrows an
/// `EligibleNodeSet` reference (itself `Copy`) rather than deriving `Copy`
/// on the enum, so this reconstructs it from a shared reference. The two
/// calls a `Hybrid` invocation makes both copy from the same source field,
/// so `rank_hybrid`'s pointer-identity check on the two prepared legs
/// always sees the same underlying set.
fn eligibility<'e, 'v, 'm, 'g>(
    invocation: &SearchInvocation<'_, 'e, 'v, 'm, 'g>,
) -> Eligibility<'e, 'v, 'm, 'g> {
    match &invocation.eligibility {
        Eligibility::AllIndexed => Eligibility::AllIndexed,
        Eligibility::Set(set) => Eligibility::Set(set),
    }
}

/// The real `SearchAdapter`. It holds only the store's own lexical
/// analyzer, borrowed independently of the view. The caller constructs it
/// before admission and it must satisfy the
/// `for<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g>` bound across every retry
/// attempt, so it cannot capture a view-typed reference in its own type.
pub(crate) struct NativeSearchAdapter<'a> {
    analyzer: &'a Analyzer,
}

impl<'a> NativeSearchAdapter<'a> {
    pub(crate) const fn new(analyzer: &'a Analyzer) -> Self {
        Self { analyzer }
    }
}

impl<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g> for NativeSearchAdapter<'_> {
    fn search<'s>(
        &mut self,
        view: &'s GraphReadView<'s, 'v, 'm, 'g>,
        invocation: &SearchInvocation<'_, '_, 'v, 'm, 'g>,
        hits: &mut QueryArena<'m, 'g, SearchHit>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<SearchReport, NativeExecutionError> {
        let before = context.counters();
        let ctx = NativeRetrievalContext::new(view, context).map_err(retrieval)?;
        let bounds = SearchBounds::new(i64::from(invocation.k), u64::from(invocation.window))?;
        match invocation.arguments {
            SearchArguments::Vector { vector, mode } => {
                let coordinates = copy_vector(context.memory(), vector)?;
                let prepared = ctx
                    .prepare_vector(
                        coordinates.as_slice(),
                        mode,
                        eligibility(invocation),
                        context,
                    )
                    .map_err(retrieval)?
                    .with_options(invocation.options);
                let ranked = ctx
                    .rank_vector(&prepared, bounds, context)
                    .map_err(retrieval)?;
                let report = ranked.report();
                for hit in ranked.hits() {
                    hits.push(SearchHit {
                        node: hit.node,
                        score: hit.distance,
                        vector_distance: None,
                        lexical_score: None,
                    })
                    .map_err(RuntimeError::Memory)?;
                }
                Ok(SearchReport {
                    call: invocation.call,
                    generation: invocation.generation,
                    kind: SearchKind::Vector,
                    requested_tier: report.requested_tier,
                    actual_tier: report.actual_tier,
                    precision: report.precision,
                    coverage: report.coverage,
                    vector_leg: report.leg,
                    lexical_leg: LegState::NotRequested,
                    document_epoch: None,
                    query_epoch: None,
                    tokenizer_epoch: None,
                    effective_alpha_bits: 0,
                    normalization_version: 0,
                    rules_version: 0,
                    candidate_count: report.eligible_members,
                    cross_scored_count: 0,
                    fallback_count: report.fallback_count,
                    cross_score_complete: false,
                    work: context.counters().since(before),
                })
            }
            SearchArguments::Text { query } => {
                let prepared = ctx
                    .prepare_text_with_options(
                        self.analyzer,
                        query,
                        eligibility(invocation),
                        invocation.options,
                        context,
                    )
                    .map_err(retrieval)?;
                let ranked = ctx
                    .rank_text(&prepared, bounds, context)
                    .map_err(retrieval)?;
                let report = ranked.report();
                for hit in ranked.hits() {
                    hits.push(SearchHit {
                        node: hit.node,
                        score: hit.bm25,
                        vector_distance: None,
                        lexical_score: None,
                    })
                    .map_err(RuntimeError::Memory)?;
                }
                Ok(SearchReport {
                    call: invocation.call,
                    generation: invocation.generation,
                    kind: SearchKind::Lexical,
                    requested_tier: None,
                    actual_tier: None,
                    precision: ScorePrecision::NotApplicable,
                    coverage: report.coverage,
                    vector_leg: LegState::NotRequested,
                    lexical_leg: report.domain.leg,
                    document_epoch: None,
                    query_epoch: None,
                    tokenizer_epoch: None,
                    effective_alpha_bits: 0,
                    normalization_version: 0,
                    rules_version: 0,
                    candidate_count: report.domain.eligible_matches,
                    cross_scored_count: 0,
                    fallback_count: 0,
                    cross_score_complete: false,
                    work: context.counters().since(before),
                })
            }
            SearchArguments::Hybrid { vector, text, mode } => {
                let coordinates = copy_vector(context.memory(), vector)?;
                let prepared_vector = ctx
                    .prepare_vector(
                        coordinates.as_slice(),
                        mode,
                        eligibility(invocation),
                        context,
                    )
                    .map_err(retrieval)?
                    .with_options(invocation.options);
                let prepared_text = ctx
                    .prepare_text_with_options(
                        self.analyzer,
                        text,
                        eligibility(invocation),
                        invocation.options,
                        context,
                    )
                    .map_err(retrieval)?;
                let ranked = ctx
                    .rank_hybrid(&prepared_vector, &prepared_text, bounds, context)
                    .map_err(retrieval)?;
                let report = ranked.report();
                for hit in ranked.hits() {
                    hits.push(SearchHit {
                        node: hit.node,
                        score: hit.fused,
                        vector_distance: hit.vector,
                        lexical_score: hit.lexical,
                    })
                    .map_err(RuntimeError::Memory)?;
                }
                Ok(SearchReport {
                    call: invocation.call,
                    generation: invocation.generation,
                    kind: SearchKind::Hybrid,
                    requested_tier: report.requested_tier,
                    actual_tier: report.actual_tier,
                    precision: report.precision,
                    coverage: report.coverage,
                    vector_leg: report.vector_leg,
                    lexical_leg: report.lexical_leg,
                    document_epoch: None,
                    query_epoch: None,
                    tokenizer_epoch: None,
                    effective_alpha_bits: report.effective_alpha.to_bits(),
                    normalization_version: u32::from(report.normalization_version),
                    rules_version: u32::from(report.rules_version),
                    candidate_count: report.candidate_count,
                    cross_scored_count: report.cross_scored_count,
                    fallback_count: report.fallback_count,
                    cross_score_complete: report.cross_score_complete,
                    work: context.counters().since(before),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
    use crate::property_graph::query::{QueryError, QueryView, ValueContext};
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::{GraphGeneration, StoreInstanceId};

    #[test]
    #[allow(clippy::expect_used)]
    fn ze64_copy_vector_checks_finite_f32_conversion() {
        let root = tempfile::tempdir().expect("fixture");
        let store = Store::open(
            root.path(),
            OpenOptions::new().with_max_resident_bytes(1_048_576),
        )
        .expect("store");
        let shared = GraphResources::from_store(&store).expect("resources");
        let memory = QueryMemory::new(&shared, 4096).expect("memory");
        let view = QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        );
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context = ValueContext::new(&view, &control, 1000).expect("context");
        let values = [
            QueryValue::F64(1.25),
            QueryValue::F64(-2.5),
            QueryValue::I64(i64::MIN),
            QueryValue::I64(i64::MAX),
        ];
        let list = QueryList::new(&values, &mut context).expect("list");
        let copied = copy_vector(&memory, list).expect("finite vector");
        assert_eq!(
            copied.as_slice(),
            &[1.25, -2.5, i64::MIN as f32, i64::MAX as f32]
        );
        for (value, expected) in [
            (f64::NAN, QueryError::ArithmeticDomain),
            (f64::INFINITY, QueryError::ArithmeticDomain),
            (f64::NEG_INFINITY, QueryError::ArithmeticDomain),
            (1e39, QueryError::ArithmeticOverflow),
            (-1e39, QueryError::ArithmeticOverflow),
        ] {
            let values = [QueryValue::F64(value)];
            let list = QueryList::new(&values, &mut context).expect("list");
            assert!(
                matches!(
                    copy_vector(&memory, list),
                    Err(NativeExecutionError::Runtime(RuntimeError::Value(error))) if error == expected
                ),
                "expected {expected:?} for {value}"
            );
        }
    }
}
