//! ZE-66 S2: [`GraphStore::query`], the public wrapper over ZE-53's
//! structured execution seam (`Store::execute_graph_statement`).
//!
//! That seam already picks read or write admission from a plan's own
//! classification, and reruns its builder under the writer admission when
//! the plan writes; it is not the release API (ZE-66 owns that, per the
//! doc comment on `execute_graph_statement`). What it does not do is remove
//! the caller's own setup: a raw builder gets a `RuntimeContext` and a
//! `GraphQueryExecutor` with six free lifetimes, and inside that admission
//! has to build a `NodeFacts` arena from the runtime's own `QueryArena`,
//! retain every allocation the plan borrows as a `RetainedRegion` /
//! `RetainedAllocation` pair, compute the plan's footprint and reserve it,
//! and only then construct and run a `GraphQuery`. ZE-53 S3's own test
//! fixture (`entry_probe::run_plan`) needs about 80 lines of that per plan,
//! and it cannot be reused here: it is `#[cfg(any(test, feature =
//! "test-seams"))]` only.
//!
//! [`GraphStore::query`] does that work once. A caller supplies only its
//! own plan data: the operator, expression and eager-search arenas ZE-48's
//! `PlanDescription` already borrows, a [`GraphPlanBacking`] naming every
//! other allocation the plan touches (input-edge lists, `Project`/`Mutate`
//! payloads, `Sort` keys, and any text a `Literal`, `Property` or
//! `HasLabel` expression names), the plan's declared parameters and root
//! operator, its parameter bindings and its output column names. No
//! `RuntimeContext`, `QueryArena` or builder callback is exposed.
//!
//! Typed search sources run through the same statement admission as graph
//! expansion and copied results, using the store's real ranking adapter.

#![allow(
    clippy::result_large_err,
    reason = "the typed graph cause stays unboxed and allocation-free, as in GraphQueryError"
)]

use super::{GraphStore, GraphStoreError};
use crate::lifecycle::QueryControl;
use crate::property_graph::GraphName;
use crate::property_graph::query::completed::{
    CompletedGraphResult, Executed, GraphBoundary, GraphQuery, GraphQueryError, GraphQueryExecutor,
    GraphQueryOptions,
};
use crate::property_graph::query::plan::{
    Expression, NodeFacts, Operator, Parameter, ParameterBinding, PlanBacking, PlanDescription,
    PlanFootprint, PlanNodeId, RetainedRegion, VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::resources::{QueryArena, RetainedAllocation, RetentionInventory};
use crate::property_graph::query::runtime::RuntimeContext;
use std::mem::size_of;

/// Every allocation a [`GraphQueryPlan`] borrows besides its operator,
/// expression and eager-search arenas: input-edge lists, `Sort`/`Project`/
/// `Mutate` payload lists, and any text a `Literal`, `Property` or
/// `HasLabel` expression names. Each must be named once, in any order; an
/// allocation the plan does not actually reference is harmless to include.
///
/// An empty backing ([`GraphPlanBacking::default`]) is correct for a plan
/// whose operators and expressions borrow nothing beyond those three
/// arenas, such as `MATCH (n) RETURN n`.
#[derive(Default)]
pub struct GraphPlanBacking<'a> {
    regions: Vec<RetainedRegion>,
    owners: Vec<RetainedAllocation<'a>>,
}

impl<'a> GraphPlanBacking<'a> {
    /// Names one caller-owned `Vec`'s complete allocation.
    ///
    /// # Errors
    ///
    /// A byte-count overflow past the plan's declarable capacity.
    pub fn vec<T>(&mut self, value: &'a Vec<T>) -> Result<(), GraphStoreError> {
        if value.capacity() != 0 {
            self.regions
                .push(RetainedRegion::vector(value).map_err(GraphQueryError::from)?);
            self.owners
                .push(RetainedAllocation::vector(value).map_err(GraphQueryError::from)?);
        }
        Ok(())
    }

    /// Names one caller-owned `String`'s complete allocation.
    ///
    /// # Errors
    ///
    /// A byte-count overflow past the plan's declarable capacity.
    pub fn string(&mut self, value: &'a String) -> Result<(), GraphStoreError> {
        if value.capacity() != 0 {
            self.regions.push(
                RetainedRegion::declared(value.as_ptr() as usize, value.capacity())
                    .map_err(GraphQueryError::from)?,
            );
            self.owners
                .push(RetainedAllocation::string(value).map_err(GraphQueryError::from)?);
        }
        Ok(())
    }
}

/// Caller-owned plan data for [`GraphStore::query`]. Every field borrows the
/// caller's own arena; nothing here is copied or retained past the call.
pub struct GraphQueryPlan<'a> {
    /// Operator arena, in dependency order.
    pub operators: &'a Vec<Operator<'a>>,
    /// Expression arena.
    pub expressions: &'a Vec<Expression<'a>>,
    /// Exact declared parameters; empty when the plan binds none.
    pub parameters: &'a Vec<Parameter<'a>>,
    /// Eager obligations in source order; empty for a plan with none.
    pub eager_searches: &'a Vec<PlanNodeId>,
    /// The row-producing root operator.
    pub root: PlanNodeId,
    /// Every other allocation the plan's operators and expressions borrow.
    pub backing: &'a GraphPlanBacking<'a>,
    /// Parameter values by name, checked against `parameters` on every run.
    pub bindings: &'a [ParameterBinding<'a>],
    /// One name per root output column, in column order.
    pub columns: &'a [&'a str],
}

impl GraphStore {
    /// Runs one caller-built plan and returns its complete owned result, or
    /// one typed rejection with no partial result.
    ///
    /// Read or write admission is chosen from `plan`'s own classification.
    /// A write plan is compiled again under the writer admission (and once
    /// more per writer checkpoint retry); `plan`'s data is immutable and
    /// borrowed for the whole call, so it is safe to reread from scratch.
    /// The returned [`CompletedGraphResult`] is an owned copy: it stays
    /// valid after this store closes.
    ///
    /// `options` is typically [`GraphQueryOptions::default`]; its fields
    /// stay closed to outside construction beyond that for now.
    ///
    /// # Errors
    ///
    /// A classified rejection. Unless
    /// [`nothing_committed`](GraphStoreError::nothing_committed) is false,
    /// the store did not change. A write statement submitted to a
    /// read-only-opened store is refused, but folds to
    /// [`GraphStoreErrorKind`](super::GraphStoreErrorKind)`::Unavailable`
    /// rather than the finer `ReadOnly` [`GraphStore::apply_batch`] reports;
    /// see [`GraphStoreError::kind`] for why.
    pub fn query(
        &self,
        control: &QueryControl,
        options: &GraphQueryOptions,
        plan: &GraphQueryPlan<'_>,
    ) -> Result<CompletedGraphResult, GraphStoreError> {
        Ok(self
            .store
            .execute_graph_statement(control, options, |runtime, executor| {
                run_query_plan(runtime, executor, plan, options.slot_column_names)
            })?)
    }
    /// Runs the same validated plan with binding preparation before commit.
    #[doc(hidden)]
    pub fn query_with_boundary(
        &self,
        control: &QueryControl,
        options: &GraphQueryOptions,
        plan: &GraphQueryPlan<'_>,
        boundary: &dyn GraphBoundary,
    ) -> Result<CompletedGraphResult, GraphStoreError> {
        Ok(self.store.execute_graph_statement_with_boundary(
            control,
            options,
            |runtime, executor| run_query_plan(runtime, executor, plan, options.slot_column_names),
            Some(boundary),
        )?)
    }
}

/// Builds `plan` inside the admitted query memory, validates it and hands
/// it to the seam's executor. This is [`Store::execute_graph_statement`]'s
/// builder, generalized from ZE-53 S3's `entry_probe::run_plan` to take a
/// caller-owned [`GraphQueryPlan`] instead of one fixture's literal arenas.
fn run_query_plan<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
    plan: &GraphQueryPlan<'_>,
    slot_column_names: bool,
) -> Result<Executed, GraphQueryError> {
    let memory = runtime.memory();
    let operators = plan.operators;
    let expressions = plan.expressions;
    let eager = plan.eager_searches;

    let mut facts = QueryArena::new(memory, operators.len())?;
    for _ in 0..operators.len() {
        facts.push(NodeFacts::default())?;
    }

    let mut regions = Vec::new();
    regions.push(RetainedRegion::vector(operators)?);
    if expressions.capacity() != 0 {
        regions.push(RetainedRegion::vector(expressions)?);
    }
    if eager.capacity() != 0 {
        regions.push(RetainedRegion::vector(eager)?);
    }
    regions.push(RetainedRegion::declared(
        facts.as_slice().as_ptr() as usize,
        facts.heap_bytes(),
    )?);
    regions.extend(plan.backing.regions.iter().copied());
    regions.sort();
    let retained = regions
        .iter()
        .try_fold(0usize, |total, region| {
            total.checked_add(region.end() - region.start())
        })
        .ok_or(GraphQueryError::contract("retained plan bytes overflow"))?;

    let mut external = memory.reserve_external_capacity()?;
    external.reserve_additional(
        retained
            + VALIDATION_SCRATCH_BYTES
            + regions.capacity() * size_of::<RetainedRegion>()
            + size_of::<PlanDescription<'_>>(),
    )?;

    let description = PlanDescription {
        operators,
        expressions,
        parameters: plan.parameters,
        root: plan.root,
        eager_searches: eager,
    };
    let (validated, facts_owner) = facts.validate_plan(
        description,
        PlanFootprint::declared(memory.reserved_bytes()),
        PlanBacking::vector(&regions)?,
        runtime.values(),
    )?;

    let mut owners = vec![RetainedAllocation::vector(operators)?, facts_owner];
    if expressions.capacity() != 0 {
        owners.push(RetainedAllocation::vector(expressions)?);
    }
    if eager.capacity() != 0 {
        owners.push(RetainedAllocation::vector(eager)?);
    }
    owners.extend(plan.backing.owners.iter().copied());

    let no_return = matches!(
        plan.operators.get(plan.root.0 as usize).map(|o| o.kind),
        Some(crate::property_graph::query::plan::OperatorKind::Mutate(_))
    );
    let slot_names = if slot_column_names && !no_return {
        let root = validated
            .facts(plan.root)
            .ok_or(GraphQueryError::contract("missing root facts"))?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(root.width())
            .map_err(|_| GraphQueryError::contract("column name capacity"))?;
        for ordinal in 0..root.width() {
            let (slot, _) = root
                .slot_at(ordinal)
                .ok_or(GraphQueryError::contract("missing root slot"))?;
            result.push(format!("slot_{}", slot.0));
        }
        result
    } else {
        Vec::new()
    };
    let slot_refs = slot_names.iter().map(String::as_str).collect::<Vec<_>>();
    let columns = if slot_column_names {
        slot_refs.as_slice()
    } else {
        plan.columns
    };
    let names = columns
        .iter()
        .map(|column| GraphName::new(column))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| GraphQueryError::contract("invalid column name"))?;

    let generated_bytes = slot_names
        .iter()
        .try_fold(0usize, |total, name| total.checked_add(name.capacity()))
        .and_then(|bytes| bytes.checked_add(slot_names.capacity() * size_of::<String>()))
        .and_then(|bytes| bytes.checked_add(slot_refs.capacity() * size_of::<&str>()))
        .and_then(|bytes| bytes.checked_add(names.capacity() * size_of::<GraphName<'_>>()))
        .and_then(|bytes| {
            bytes.checked_add(owners.capacity() * size_of::<RetainedAllocation<'_>>())
        })
        .ok_or(GraphQueryError::contract("column capacity overflow"))?;
    external.reserve_additional(generated_bytes)?;
    executor.run(
        runtime,
        GraphQuery {
            plan: &validated,
            inventory: RetentionInventory::vector(&owners)?,
            bindings: plan.bindings,
            columns: &names,
        },
    )
}

#[cfg(test)]
mod tests;
