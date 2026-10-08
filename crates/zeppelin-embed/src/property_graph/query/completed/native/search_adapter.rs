//! Graph search runs on the document segments pinned by its statement lease.

use super::super::{LegState, ScorePrecision, SearchKind, SearchReport};
use crate::lifecycle::Store;
use crate::property_graph::query::eligibility::Eligibility;
use crate::property_graph::query::pattern::{
    SearchAdapter, SearchArguments, SearchHit, SearchInvocation,
};
use crate::property_graph::query::plan::{SearchMode, SearchOptions};
use crate::property_graph::query::resources::{QueryArena, QueryMemory};
use crate::property_graph::query::runtime::{NativeExecutionError, RuntimeContext, RuntimeError};
use crate::property_graph::query::{QueryError, QueryList, QueryValue};
use crate::property_graph::retrieval::RetrievalError;
use crate::property_graph::storage::GraphReadView;

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

pub(crate) struct NativeSearchAdapter<'a> {
    store: &'a Store,
}
impl<'a> NativeSearchAdapter<'a> {
    pub(crate) const fn new(store: &'a Store) -> Self {
        Self { store }
    }
}

fn lexical(store: &Store, text: &str, prefix: bool) -> crate::fts::query::LexicalQuery {
    let mut terms = store
        .tokenizer
        .analyze(text)
        .into_iter()
        .map(|token| token.term.into_bytes())
        .collect::<Vec<_>>();
    if prefix && let Some(prefix) = terms.pop() {
        crate::fts::query::LexicalQuery::TermsWithPrefix {
            terms,
            prefix,
            fields: crate::fts::search::FieldWeights::flat(&[crate::fts::index::DEFAULT_FIELD]),
        }
    } else {
        crate::fts::query::LexicalQuery::term(crate::fts::search::TermQuery::flat(
            terms,
            &[crate::fts::index::DEFAULT_FIELD],
        ))
    }
}

fn vector_options(
    mode: SearchMode,
    options: SearchOptions,
    window: usize,
) -> Result<crate::lifecycle::SearchOptions, RetrievalError> {
    use crate::lifecycle::{GraphSearchOptions, SearchTier};
    let mut result = crate::lifecycle::SearchOptions::default();
    result = match mode {
        SearchMode::Default => result,
        SearchMode::Auto => result.with_tier(SearchTier::Auto),
        SearchMode::Exact => result.with_tier(SearchTier::Exact),
        SearchMode::Scan => result.with_tier(SearchTier::Scan),
        SearchMode::Graph => {
            let profile = options
                .graph_profile
                .unwrap_or(crate::graph::search::GraphSearchProfile::SiftClass);
            let mut graph = GraphSearchOptions::new(profile).with_seed(options.graph_seed);
            if options.graph_ef != 0 {
                graph = graph.with_ef(options.graph_ef as usize);
            }
            result.with_tier(SearchTier::Graph(graph))
        }
    };
    if mode != SearchMode::Graph
        && (options.graph_profile.is_some() || options.graph_ef != 0 || options.graph_seed != 0)
    {
        return Err(RetrievalError::Control(RuntimeError::Value(
            QueryError::Type,
        )));
    }
    if options.rescore {
        if mode != SearchMode::Scan {
            return Err(RetrievalError::Control(RuntimeError::Value(
                QueryError::Type,
            )));
        }
        result = result.with_scan_rescore(
            crate::lifecycle::ScanRescoreOptions::new(1, window)
                .map_err(|_| RetrievalError::Memory)?,
        );
    }
    Ok(result)
}

fn report(
    invocation: &SearchInvocation<'_, '_, '_, '_, '_>,
    diagnostics: &crate::diag::QueryDiagnostics,
    kind: SearchKind,
) -> SearchReport {
    use super::super::{ActualTier, CandidateCoverage};
    let hybrid = diagnostics.hybrid.as_ref();
    let fusion = diagnostics.fusion.as_ref();
    let vector = kind != SearchKind::Lexical;
    let text = kind != SearchKind::Vector;
    let members = diagnostics
        .plan
        .iter()
        .map(|plan| plan.filter_cardinality)
        .sum::<u64>();
    let lexical_nonempty = hybrid.map_or(diagnostics.returned > 0, |report| {
        report.lexical_returned > 0
    });
    SearchReport {
        call: invocation.call,
        generation: invocation.generation,
        kind,
        requested_tier: None,
        actual_tier: (vector && members > 0).then_some(
            if diagnostics.plan.iter().any(|plan| {
                matches!(
                    plan.branch,
                    crate::planner::SegmentBranch::Graph
                        | crate::planner::SegmentBranch::FilteredGraph
                )
            }) {
                ActualTier::Graph
            } else if diagnostics.plan.iter().any(|plan| {
                matches!(
                    plan.scan_reason,
                    Some(crate::planner::ScanReason::ExplicitTier(
                        crate::planner::ExplicitScanTier::Scan
                    ))
                )
            }) {
                ActualTier::Scan
            } else {
                ActualTier::Exact
            },
        ),
        precision: if !vector {
            ScorePrecision::NotApplicable
        } else if diagnostics.exact_rescore {
            ScorePrecision::Original
        } else {
            ScorePrecision::Quantized
        },
        coverage: if diagnostics.approximate {
            CandidateCoverage::Approximate
        } else {
            CandidateCoverage::Exact
        },
        vector_leg: if !vector {
            LegState::NotRequested
        } else if members == 0 {
            LegState::NoEligibleMembers
        } else {
            LegState::Nonempty
        },
        lexical_leg: if !text {
            LegState::NotRequested
        } else if lexical_nonempty {
            LegState::Nonempty
        } else {
            LegState::NoQueryMatches
        },
        document_epoch: diagnostics.embedding_epoch.map(|epoch| epoch.value()),
        query_epoch: diagnostics.embedding_epoch.map(|epoch| epoch.value()),
        tokenizer_epoch: diagnostics.tokenizer_epoch.map(|epoch| epoch.value()),
        effective_alpha_bits: fusion.map_or(0, |report| report.effective_alpha.to_bits()),
        normalization_version: hybrid
            .map_or(0, |report| u32::from(report.normalization_policy_version)),
        rules_version: fusion.map_or(0, |report| u32::from(report.alpha_policy_version)),
        candidate_count: hybrid.map_or_else(
            || {
                if vector {
                    members
                } else {
                    diagnostics.counters.lexical.docs_evaluated
                }
            },
            |report| report.vector_returned as u64,
        ),
        cross_scored_count: hybrid.map_or(0, |report| {
            if report.provenance.cross_scores_complete {
                report.vector_returned as u64
            } else {
                0
            }
        }),
        fallback_count: diagnostics
            .plan
            .iter()
            .filter(|plan| plan.branch == crate::planner::SegmentBranch::GraphExactFallback)
            .count() as u64,
        cross_score_complete: hybrid.is_some_and(|report| report.provenance.cross_scores_complete),
        work: Default::default(),
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
        let outer_peak = context.reset_candidate_window_peak()?;
        let before = context.counters();
        let result = (|| {
            if invocation.k > invocation.window {
                return Err(RetrievalError::CandidateWindow {
                    required: invocation.k as usize,
                    window: invocation.window as usize,
                }
                .into());
            }
            let binding = view.retrieval_binding(context)?;
            let pin = view.search_documents()?;
            let ids = match &invocation.eligibility {
                Eligibility::AllIndexed => None,
                Eligibility::Set(set) => Some(
                    set.ids_for(context.view())
                        .map_err(RetrievalError::Eligibility)?
                        .iter()
                        .map(|id| crate::ingest::DocId::new(id.get()))
                        .collect::<Vec<_>>(),
                ),
            };
            let filter = ids
                .as_deref()
                .map(|ids| crate::lifecycle::QueryFilter::eligible(self.store.schema(), ids));
            let control = context.values().control().clone();
            let (mut report, counters) = match invocation.arguments {
                SearchArguments::Text { query } => {
                    let query = lexical(self.store, query, invocation.options.last_as_prefix);
                    let (outcome, matching) = self
                        .store
                        .graph_text_in(pin, &query, invocation.k as usize, filter.as_ref(), control)
                        .map_err(RetrievalError::Fusion)?;
                    context.observe_candidate_window(outcome.candidates.len() as u64)?;
                    for hit in &outcome.candidates {
                        hits.push(SearchHit {
                            node: crate::property_graph::NodeId::from(hit.document.doc_id()),
                            score: hit.score,
                            vector_distance: None,
                            lexical_score: None,
                        })
                        .map_err(RuntimeError::Memory)?;
                    }
                    let mut mapped = report(invocation, &outcome.diagnostics, SearchKind::Lexical);
                    mapped.candidate_count = matching;
                    if ids.as_ref().is_some_and(Vec::is_empty) {
                        mapped.lexical_leg = LegState::NoEligibleMembers;
                    }
                    (mapped, outcome.diagnostics.counters)
                }
                SearchArguments::Vector { vector, mode }
                | SearchArguments::Hybrid { vector, mode, .. } => {
                    let tower = binding
                        .interpretation
                        .embedding()
                        .ok_or(RetrievalError::NoVectorSpace)?;
                    let coordinates = copy_vector(context.memory(), vector)?;
                    context.charge(
                        crate::property_graph::query::runtime::WorkKind::VectorCoordinates,
                        coordinates.len() as u64,
                    )?;
                    context.charge(
                        crate::property_graph::query::runtime::WorkKind::VectorBytes,
                        (coordinates.len() * std::mem::size_of::<f32>()) as u64,
                    )?;
                    if coordinates.len() != tower.dimensions() as usize {
                        return Err(RetrievalError::Dimension {
                            expected: tower.dimensions() as usize,
                            actual: coordinates.len(),
                        }
                        .into());
                    }
                    let options =
                        vector_options(mode, invocation.options, invocation.window as usize)?;
                    let request = crate::ingest::SearchRequest::new(coordinates.as_slice())
                        .with_filter(filter.as_ref());
                    let request = if let Some(ids) = ids.as_deref() {
                        request.with_eligible(ids)
                    } else {
                        request
                    };
                    let (mut mapped, counters) = if let SearchArguments::Hybrid { text, .. } =
                        invocation.arguments
                    {
                        let query = lexical(self.store, text, invocation.options.last_as_prefix);
                        let mut hybrid = crate::fusion::HybridQuery::new(invocation.k as usize);
                        hybrid.alpha = invocation.options.alpha;
                        hybrid.rules_enabled = invocation.options.rules_enabled;
                        if let Some(rounds) = invocation.options.max_rounds {
                            hybrid.max_rounds =
                                usize::try_from(rounds).map_err(|_| RuntimeError::Batch)?;
                        }
                        let outcome = self
                            .store
                            .graph_hybrid_in(pin, request, &query, &hybrid, options, control)
                            .map_err(RetrievalError::Fusion)?;
                        context.observe_candidate_window(
                            outcome
                                .diagnostics
                                .hybrid
                                .as_ref()
                                .map_or(outcome.hits.len(), |report| report.window)
                                as u64,
                        )?;
                        for hit in &outcome.hits {
                            hits.push(SearchHit {
                                node: crate::property_graph::NodeId::from(hit.key),
                                score: hit.fused_score,
                                vector_distance: hit.vector_squared_l2,
                                lexical_score: hit.lexical_bm25,
                            })
                            .map_err(RuntimeError::Memory)?;
                        }
                        (
                            report(invocation, &outcome.diagnostics, SearchKind::Hybrid),
                            outcome.diagnostics.counters,
                        )
                    } else {
                        let outcome = self.store.graph_vector_in(pin, request, invocation.k as usize, options, control)
                            .map_err(|error| RetrievalError::Storage(crate::property_graph::storage::tree::directory::TreeError::Control(error)))?;
                        context.observe_candidate_window(outcome.candidates.len() as u64)?;
                        for hit in &outcome.candidates {
                            hits.push(SearchHit {
                                node: crate::property_graph::NodeId::from(
                                    hit.document()
                                        .ok_or(RetrievalError::Invariant(
                                            "vector hit has no document identity",
                                        ))?
                                        .doc_id(),
                                ),
                                score: -(hit.score() as f64),
                                vector_distance: None,
                                lexical_score: None,
                            })
                            .map_err(RuntimeError::Memory)?;
                        }
                        (
                            report(invocation, &outcome.diagnostics, SearchKind::Vector),
                            outcome.diagnostics.counters,
                        )
                    };
                    mapped.requested_tier = options.explicit_tier();
                    (mapped, counters)
                }
            };
            use crate::property_graph::query::runtime::WorkKind;
            context.charge(WorkKind::VectorCoordinates, counters.scan.dims_touched)?;
            context.charge(WorkKind::VectorBytes, counters.scan.bytes_read)?;
            context.charge(WorkKind::LexicalPostings, counters.lexical.postings_decoded)?;
            context.charge(WorkKind::LexicalBlocks, counters.lexical.blocks_decoded)?;
            report.work = context.counters().since(before);
            Ok(report)
        })();
        context.restore_candidate_window_peak(outer_peak)?;
        result
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
