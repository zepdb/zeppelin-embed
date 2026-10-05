//! Eager once-per-query search sources inside the native pattern engine.
//!
//! A `Search` operator is a source, never a per-row callback. The driver calls
//! `prepare_search` once for every syntactic call, in source order, before any
//! row is pulled, so LIMIT 0 or an empty join cannot skip a call or its report.
//! Preparation drains the call's own singleton input subtree, evaluates the
//! arguments against that one row, builds the execution-owned eligible set,
//! and invokes the typed `SearchAdapter` exactly once. The hits and the report
//! are retained per call. The report is recorded into the caller's
//! `SearchReports` sink at that moment, so a later projection that drops the
//! score column cannot lose it, and it is recorded exactly as the adapter
//! returned it: approximate coverage or precision is never upgraded here.
//!
//! Every `Search` occurrence in the row tree only replays the retained hits,
//! each extended with the singleton input row. A reset, for example the inner
//! side of a nested-loop join, rewinds the replay; it never searches again.
//! Independent calls therefore combine through the ordinary `Join` operator
//! as a Cartesian bag, and are never fused.
//!
//! The adapter is the seam ZE-64 binds to real ranking. This module owns only
//! argument evaluation, eligibility, invocation count, retention and replay.

#![allow(
    clippy::result_large_err,
    reason = "native typed causes remain unboxed and allocation-free"
)]

use std::cell::Cell;

use super::super::completed::SearchReport;
use super::super::eligibility::Eligibility;
use super::super::plan::{SearchBounds, SearchCallId, SearchMode, SearchOutputs, SearchRequest};
use super::relational::eligibility::eligible_set;
use super::*;
use crate::property_graph::GraphGeneration;
use crate::property_graph::storage::GraphReadView;

/// Plan validation admits at most this many syntactic calls per statement.
pub(crate) const MAX_SEARCH_CALLS: usize = 8;

/// One ranked node. `score` is the call's primary column: the squared-L2
/// distance for a vector call, the BM25 or fused score otherwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SearchHit {
    pub(crate) node: NodeId,
    pub(crate) score: f64,
    /// Hybrid vector component; `None` projects as null.
    pub(crate) vector_distance: Option<f64>,
    /// Hybrid lexical component; `None` projects as null.
    pub(crate) lexical_score: Option<f64>,
}

/// Evaluated request arguments, borrowed from the call's retained cells.
/// Vector contents and provenance are validated by the adapter's retrieval.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SearchArguments<'a> {
    Vector {
        vector: QueryList<'a>,
        mode: SearchMode,
    },
    Text {
        query: &'a str,
    },
    Hybrid {
        vector: QueryList<'a>,
        text: &'a str,
        mode: SearchMode,
    },
}

/// One complete invocation. `k` has already passed `SearchBounds`; choosing a
/// retained candidate window is the adapter's ranking decision.
pub(crate) struct SearchInvocation<'a, 'e, 'v, 'm, 'g> {
    pub(crate) call: SearchCallId,
    /// The admitted view's generation; the report must carry exactly this.
    pub(crate) generation: GraphGeneration,
    pub(crate) arguments: SearchArguments<'a>,
    pub(crate) k: u32,
    /// Absent restriction is `AllIndexed`; an explicit empty list is `Set`.
    pub(crate) eligibility: Eligibility<'e, 'v, 'm, 'g>,
}

/// Typed search seam. An implementation ranks once, pushes at most `k` hits
/// into the charged `hits` arena and returns the invocation's report. Any
/// error fails the whole statement with no rows and no result.
///
/// `view` is the same admitted view the rest of the statement reads through;
/// an adapter must never admit or open a second, independently generationed
/// view. It is a method-generic parameter, not part of the trait's own
/// lifetimes, because the adapter value is chosen by the caller before any
/// view exists and must stay valid across every retry attempt.
pub(crate) trait SearchAdapter<'v, 'm, 'g> {
    fn search<'s>(
        &mut self,
        view: &'s GraphReadView<'s, 'v, 'm, 'g>,
        invocation: &SearchInvocation<'_, '_, 'v, 'm, 'g>,
        hits: &mut QueryArena<'m, 'g, SearchHit>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<SearchReport, NativeExecutionError>;
}

/// Caller-owned report sink, filled at search time in call order.
pub(crate) struct SearchReports {
    reports: Cell<[Option<SearchReport>; MAX_SEARCH_CALLS]>,
}

impl SearchReports {
    pub(crate) const fn new() -> Self {
        Self {
            reports: Cell::new([None; MAX_SEARCH_CALLS]),
        }
    }

    fn record(&self, report: SearchReport) -> Result<(), RuntimeError> {
        let mut reports = self.reports.get();
        let slot = reports
            .get_mut(report.call.0 as usize)
            .ok_or(RuntimeError::Batch)?;
        if slot.is_some() {
            return Err(RuntimeError::Batch);
        }
        *slot = Some(report);
        self.reports.set(reports);
        Ok(())
    }

    /// Copies exactly `calls` reports, in call order. A missing report, or
    /// one beyond `calls`, is a broken once-per-query obligation.
    pub(crate) fn copy_into<'m, 'g>(
        &self,
        calls: usize,
        output: &mut QueryArena<'m, 'g, SearchReport>,
    ) -> Result<(), RuntimeError> {
        for (index, report) in self.reports.get().iter().enumerate() {
            match (index < calls, report) {
                (true, Some(report)) => output.push(*report).map_err(RuntimeError::Memory)?,
                (false, None) => {}
                _ => return Err(RuntimeError::Batch),
            }
        }
        Ok(())
    }
}

/// The adapter and report sink one read statement's searches use.
pub(crate) struct SearchScope<'i, 'v, 'm, 'g> {
    adapter: &'i mut dyn SearchAdapter<'v, 'm, 'g>,
    reports: &'i SearchReports,
}

impl<'i, 'v, 'm, 'g> SearchScope<'i, 'v, 'm, 'g> {
    pub(crate) fn new(
        adapter: &'i mut dyn SearchAdapter<'v, 'm, 'g>,
        reports: &'i SearchReports,
    ) -> Self {
        Self { adapter, reports }
    }
}

/// Per-call retained state. `input` is the call's private singleton subtree.
pub(super) struct PreparedSearch<'v, 'm, 'g> {
    input: usize,
    outputs: SearchOutputs,
    prepared: Option<Retained<'v, 'm, 'g>>,
}

struct Retained<'v, 'm, 'g> {
    schema: Schema<'m, 'g>,
    row: RowBatch<'v, 'm, 'g>,
    hits: QueryArena<'m, 'g, SearchHit>,
}

impl<'v, 'm, 'g> PreparedSearch<'v, 'm, 'g> {
    pub(super) const fn new(input: usize, outputs: SearchOutputs) -> Self {
        Self {
            input,
            outputs,
            prepared: None,
        }
    }
}

/// Returns the eager call index for a `Search` plan node.
pub(super) fn call_index(
    description: super::super::plan::PlanDescription<'_>,
    node: PlanNodeId,
) -> Result<usize, PlanError> {
    match description
        .operators
        .get(node.0 as usize)
        .map(|operator| operator.kind)
    {
        Some(OperatorKind::Search { call, .. })
            if description.eager_searches.get(call.0 as usize) == Some(&node) =>
        {
            Ok(call.0 as usize)
        }
        _ => Err(PlanError::Search),
    }
}

impl<'s, 'r, 'plan, 'v, 'm, 'g, 'i, 'q> NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i, 'q> {
    /// Runs one call's complete eager obligation. See the module comment.
    pub(super) fn run_search(
        &mut self,
        node: PlanNodeId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        let index = call_index(self.description, node)?;
        let Some(OperatorKind::Search { call, request, .. }) = self
            .description
            .operators
            .get(node.0 as usize)
            .map(|operator| operator.kind)
        else {
            return Err(PlanError::Search.into());
        };
        let input = {
            let state = self
                .searches
                .as_slice()
                .get(index)
                .ok_or(PlanError::Search)?;
            if state.prepared.is_some() {
                return Err(RuntimeError::Batch.into());
            }
            state.input
        };
        let (schema, row) = self.singleton_input(input, context)?;
        let (first, second, k, eligible) = match request {
            SearchRequest::Vector {
                vector,
                k,
                eligible,
                ..
            } => (vector, None, k, eligible),
            SearchRequest::Text { query, k, eligible } => (query, None, k, eligible),
            SearchRequest::Hybrid {
                vector,
                text,
                k,
                eligible,
                ..
            } => (vector, Some(text), k, eligible),
        };
        let k = match self.evaluate_search_argument(k, &schema, &row, context)? {
            QueryValue::I64(value) => SearchBounds::new(value, 0)?.k(),
            _ => return Err(expression_type(k)),
        };
        let set = match eligible {
            Some(expression) => {
                let value = self.evaluate_search_argument(expression, &schema, &row, context)?;
                Some(eligible_set(value, expression, context)?)
            }
            None => None,
        };
        // Each argument is copied into its own one-cell batch, because an
        // evaluated value borrows the single evaluator until it is copied.
        let mut cells = [None, None];
        for (cell, expression) in cells.iter_mut().zip([Some(first), second]) {
            let Some(expression) = expression else {
                continue;
            };
            let mut batch = RowBatch::storage(
                context,
                1,
                1,
                self.capacity.rows.payload_bytes,
                self.capacity.rows.variable,
            )?;
            let value = self.evaluate_search_argument(expression, &schema, &row, context)?;
            batch.push_row(&[value], context)?;
            *cell = Some((expression, batch));
        }
        let arguments = search_arguments(request, &cells)?;
        let generation = context.view().generation();
        let invocation = SearchInvocation {
            call,
            generation,
            arguments,
            k,
            eligibility: set
                .as_ref()
                .map_or(Eligibility::AllIndexed, Eligibility::Set),
        };
        let mut hits =
            QueryArena::new(context.memory(), k as usize).map_err(RuntimeError::Memory)?;
        let view = self.view;
        let scope = self.search.as_mut().ok_or(PlanError::Search)?;
        let report = scope
            .adapter
            .search(view, &invocation, &mut hits, context)?;
        if report.call != call || report.generation != generation || hits.len() > k as usize {
            return Err(RuntimeError::Batch.into());
        }
        // Retrieval only borrowed the arguments and the eligible set.
        drop(cells);
        drop(set);
        scope.reports.record(report)?;
        let state = self
            .searches
            .as_mut_slice()
            .get_mut(index)
            .ok_or(PlanError::Search)?;
        state.prepared = Some(Retained { schema, row, hits });
        context.checkpoint()?;
        Ok(())
    }

    /// Drains the call's private input subtree, which must yield exactly one
    /// row, into a retained batch with its own schema.
    fn singleton_input(
        &mut self,
        input: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(Schema<'m, 'g>, RowBatch<'v, 'm, 'g>), NativeExecutionError> {
        if !self.next_occurrence(input, context)? {
            return Err(RuntimeError::Batch.into());
        }
        let occurrence = self.occurrence(input)?;
        let schema = Schema::new(context, occurrence.schema.slots())?;
        let mut row = RowBatch::storage(
            context,
            schema.slots().len(),
            1,
            self.capacity.rows.payload_bytes,
            self.capacity.rows.variable,
        )?;
        row.push_from(
            |column| {
                occurrence
                    .output
                    .value(0, column)
                    .ok_or(RuntimeError::Batch)
            },
            context,
        )?;
        if self.next_occurrence(input, context)? {
            return Err(RuntimeError::Batch.into());
        }
        Ok((schema, row))
    }

    fn evaluate_search_argument(
        &mut self,
        expression: ExprId,
        schema: &Schema<'m, 'g>,
        row: &RowBatch<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryValue<'_>, NativeExecutionError> {
        Ok(evaluate_at(
            &mut self.evaluator,
            self.mutation.as_mut(),
            expression,
            schema,
            row,
            0,
            self.view,
            context,
        )?)
    }

    /// Replays the next retained hit of call `call` into occurrence `index`.
    pub(super) fn next_search(
        &mut self,
        index: usize,
        call: usize,
        next: &mut usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        let query_view = self.query_view;
        let state = self
            .searches
            .as_slice()
            .get(call)
            .ok_or(PlanError::Search)?;
        let retained = state.prepared.as_ref().ok_or(RuntimeError::Batch)?;
        let Some(hit) = retained.hits.as_slice().get(*next).copied() else {
            return Ok(false);
        };
        context.charge(WorkKind::OperatorRows, 1)?;
        let outputs = state.outputs;
        let occurrence = self
            .occurrences
            .as_mut_slice()
            .get_mut(index)
            .ok_or(RuntimeError::Batch)?;
        occurrence.output.push_from(
            |column| {
                let slot = occurrence
                    .schema
                    .slots()
                    .get(column)
                    .copied()
                    .ok_or(RuntimeError::Batch)?;
                if let Ok(source) = retained.schema.column(slot) {
                    return retained.row.value(0, source).ok_or(RuntimeError::Batch);
                }
                let slot = Some(slot);
                if slot == outputs.node {
                    Ok(query_view.node(hit.node))
                } else if slot == outputs.distance || slot == outputs.score {
                    Ok(QueryValue::F64(hit.score))
                } else if slot == outputs.vector_distance {
                    Ok(hit
                        .vector_distance
                        .map_or(QueryValue::Null, QueryValue::F64))
                } else if slot == outputs.lexical_score {
                    Ok(hit.lexical_score.map_or(QueryValue::Null, QueryValue::F64))
                } else {
                    Err(RuntimeError::Batch)
                }
            },
            context,
        )?;
        *next += 1;
        Ok(true)
    }
}

/// Reads the evaluated argument cells back with the request's own shape.
fn search_arguments<'a>(
    request: SearchRequest,
    cells: &'a [Option<(ExprId, RowBatch<'_, '_, '_>)>; 2],
) -> Result<SearchArguments<'a>, NativeExecutionError> {
    let [first, second] = cells;
    let first = first.as_ref().ok_or(RuntimeError::Batch)?;
    let first_value = first.1.value(0, 0).ok_or(RuntimeError::Batch)?;
    Ok(match request {
        SearchRequest::Vector { mode, .. } => SearchArguments::Vector {
            vector: list(first.0, first_value)?,
            mode,
        },
        SearchRequest::Text { .. } => SearchArguments::Text {
            query: string(first.0, first_value)?,
        },
        SearchRequest::Hybrid { mode, .. } => {
            let second = second.as_ref().ok_or(RuntimeError::Batch)?;
            SearchArguments::Hybrid {
                vector: list(first.0, first_value)?,
                text: string(second.0, second.1.value(0, 0).ok_or(RuntimeError::Batch)?)?,
                mode,
            }
        }
    })
}

fn list(expression: ExprId, value: QueryValue<'_>) -> Result<QueryList<'_>, NativeExecutionError> {
    match value {
        QueryValue::List(list) => Ok(list),
        _ => Err(expression_type(expression)),
    }
}

fn string(expression: ExprId, value: QueryValue<'_>) -> Result<&str, NativeExecutionError> {
    match value {
        QueryValue::String(text) => Ok(text),
        _ => Err(expression_type(expression)),
    }
}

fn expression_type(expression: ExprId) -> NativeExecutionError {
    super::super::expression::ExpressionError {
        expression,
        failure: super::super::expression::ExpressionFailure::Runtime(QueryError::Type.into()),
    }
    .into()
}
