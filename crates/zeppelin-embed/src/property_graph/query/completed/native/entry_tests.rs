//! ZE-53 S3: the structured execution seam, through `Store::execute_graph_query`.
//!
//! Every statement is built inside its admission by the same builders the
//! adversarial probe uses. Each test checks the seam against an oracle that
//! does not read the result collector: the fixture's own values, the
//! statement's arithmetic, the published generation and a fresh read, often
//! after a reopen.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use super::super::{
    CandidateCoverage, CompletedGraphResult, LegState, Outcome, ScorePrecision, SearchKind,
    SearchReport, Value,
};
use super::entry::{Executed, GraphQueryExecutor, NoSearch};
use super::entry_probe::{
    Assign, Backing, Fixture, control, create_then_delete, node_values, options, read_p, run_plan,
    write_p,
};
use super::error::{GraphQueryCause, GraphQueryError, GraphQueryErrorKind};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeGraphError;
use crate::lifecycle::native_graph::tests::publication::FaultPoint;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::QueryError;
use crate::property_graph::query::pattern::{SearchAdapter, SearchHit, SearchInvocation};
use crate::property_graph::query::plan::{
    ExprId, Expression, Literal, Operator, OperatorKind, PlanError, PlanNodeId, Projection,
    SearchCallId, SearchOutputs, SearchRequest, SlotId,
};
use crate::property_graph::query::resources::QueryArena;
use crate::property_graph::query::runtime::{
    NativeExecutionError, RuntimeContext, RuntimeError, WorkKind,
};
use crate::property_graph::{GraphGeneration, NodeId};
use std::cell::Cell;
use std::path::PathBuf;

fn directory(test: &str) -> PathBuf {
    std::env::temp_dir().join(format!("zeppelin-ze53-s3-{test}-{}", std::process::id()))
}

fn new_fixture(test: &str) -> Fixture {
    Fixture::create(directory(test), 0).expect("ze53 s3 fixture")
}

/// `(node, p)` rows the fixture holds, each `p` moved by `delta`.
fn rows(fixture: &Fixture, delta: i64) -> Vec<(u128, i64)> {
    fixture
        .nodes
        .iter()
        .zip(fixture.values)
        .map(|(node, value)| (node.get(), value + delta))
        .collect()
}

fn committed_at(generation: u64) -> Outcome {
    Outcome::Committed {
        changed: GraphGeneration::new(generation),
    }
}

/// Fails unless `outcome` is a definite rejection of `kind` that left the
/// store's generation and every value exactly as they were.
fn refused_unchanged(
    fixture: &Fixture,
    outcome: Result<CompletedGraphResult, GraphQueryError>,
    kind: GraphQueryErrorKind,
    before: u64,
) -> GraphQueryError {
    let error = match outcome {
        Ok(result) => panic!(
            "expected {kind:?}, published {:?}",
            result.metadata().outcome
        ),
        Err(error) => error,
    };
    assert_eq!(error.kind(), kind, "{error}");
    assert!(error.nothing_committed(), "{error}");
    assert_eq!(fixture.generation().unwrap(), before);
    assert_eq!(
        node_values(&fixture.read().unwrap()).unwrap(),
        rows(fixture, 0)
    );
    error
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// A plan that only reads runs once, under the read admission, and returns
/// the stored values with a `Read` outcome at the current generation.
#[test]
fn ze53_s3_read_plan_runs_once_under_the_read_admission() {
    let fixture = new_fixture("read");
    let before = fixture.generation().unwrap();
    let builds = Cell::new(0);
    let result = fixture
        .store
        .execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            |runtime, executor| {
                builds.set(builds.get() + 1);
                read_p(runtime, executor)
            },
        )
        .expect("read statement");
    assert_eq!(builds.get(), 1);
    assert_eq!(result.metadata().outcome, Outcome::Read);
    assert_eq!(result.metadata().generation.get(), before);
    assert_eq!(node_values(&result).unwrap(), rows(&fixture, 0));
    assert_eq!(fixture.generation().unwrap(), before);
    fixture.remove().unwrap();
}

/// A plan whose classification writes is built under the read admission,
/// not run there, and built again under the writer admission, where it
/// commits. The result carries the staged values and the committed outcome,
/// each returned node carries the published generation, and a reopened
/// store reads the same values.
#[test]
fn ze53_s3_write_plan_rebuilds_under_the_writer_and_commits() {
    let fixture = new_fixture("write");
    let before = fixture.generation().unwrap();
    let builds = Cell::new(0);
    let result = fixture
        .store
        .execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            |runtime, executor| {
                builds.set(builds.get() + 1);
                write_p(runtime, executor, Assign::Increment)
            },
        )
        .expect("write statement");
    assert_eq!(builds.get(), 2, "one classifying build, one writing build");
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    assert_eq!(node_values(&result).unwrap(), rows(&fixture, 1));
    for node in result.pools().nodes {
        assert_eq!(node.generation.get(), before + 1);
        assert_eq!(node.revision.get(), 2);
    }
    assert_eq!(fixture.generation().unwrap(), before + 1);

    let expected = rows(&fixture, 1);
    let Fixture {
        directory,
        store,
        vfs,
        nodes,
        values,
    } = fixture;
    store.close().unwrap();
    let reopened = Fixture {
        store: Store::open_native_graph(
            directory.join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
        )
        .expect("reopen"),
        directory,
        vfs,
        nodes,
        values,
    };
    assert_eq!(node_values(&reopened.read().unwrap()).unwrap(), expected);
    reopened.remove().unwrap();
}

/// A statement that fails at its second row, after its first row staged a
/// value, is refused as an expression error that names its operator and its
/// work so far, and publishes nothing.
#[test]
fn ze53_s3_mid_drain_failure_keeps_its_group_operator_and_counters() {
    let fixture = new_fixture("mid-drain");
    let before = fixture.generation().unwrap();
    let error = refused_unchanged(
        &fixture,
        fixture.write(&control(), 16, Assign::DivideAround(fixture.values[1])),
        GraphQueryErrorKind::Expression,
        before,
    );
    assert!(error.operator().is_some(), "{error}");
    let counters = error.counters().expect("driver counters");
    assert!(counters.get(WorkKind::Expressions) > 0, "{error}");
    match error.cause() {
        GraphQueryCause::Execution(NativeExecutionError::Expression(expression)) => {
            assert!(format!("{expression}").contains("division"), "{expression}");
        }
        other => panic!("expected the expression's own cause, got {other:?}"),
    }
    fixture.remove().unwrap();
}

/// The write path's other definite refusals keep their own causes: an image
/// arena smaller than the statement is a limit, and a statement that only
/// consumes an identity is an unsupported plan.
#[test]
fn ze53_s3_write_refusals_are_typed_and_publish_nothing() {
    let fixture = new_fixture("refusals");
    let before = fixture.generation().unwrap();
    let limit = refused_unchanged(
        &fixture,
        fixture.write(&control(), 2, Assign::Increment),
        GraphQueryErrorKind::Limit,
        before,
    );
    assert!(
        matches!(
            limit.cause(),
            GraphQueryCause::Execution(NativeExecutionError::Stage(_))
        ),
        "{limit}"
    );
    let fence = refused_unchanged(
        &fixture,
        fixture.store.execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            create_then_delete,
        ),
        GraphQueryErrorKind::InvalidPlan,
        before,
    );
    assert!(
        matches!(
            fence.cause(),
            GraphQueryCause::Graph(NativeGraphError::FenceOnlyStatement)
        ),
        "{fence}"
    );
    fixture.remove().unwrap();
}

/// A builder that returns without running its plan breaks the seam's
/// contract and is refused, under either admission.
#[test]
fn ze53_s3_builder_that_skips_its_executor_is_refused() {
    let fixture = new_fixture("contract");
    let before = fixture.generation().unwrap();
    let error = fixture
        .store
        .execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            |_: &mut RuntimeContext<'_, '_, '_>, _: GraphQueryExecutor<'_, '_, '_, '_, '_, '_>| {
                Err(GraphQueryError::contract("builder refused"))
            },
        )
        .map(|_| ())
        .expect_err("a builder error is the statement's error");
    assert!(matches!(
        error.cause(),
        GraphQueryCause::Contract("builder refused")
    ));
    // The first build writes, so the second runs under the writer; that one
    // then returns a plan that no longer writes.
    let builds = Cell::new(0);
    let error = fixture
        .store
        .execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            |runtime, executor| {
                builds.set(builds.get() + 1);
                if builds.get() == 1 {
                    write_p(runtime, executor, Assign::Increment)
                } else {
                    read_p(runtime, executor)
                }
            },
        )
        .map(|_| ())
        .expect_err("an inconsistent builder is refused");
    assert_eq!(error.kind(), GraphQueryErrorKind::InvalidPlan);
    assert!(
        matches!(error.cause(), GraphQueryCause::Contract(_)),
        "{error}"
    );
    assert!(error.nothing_committed());
    assert_eq!(fixture.generation().unwrap(), before);
    fixture.remove().unwrap();
}

// ---------------------------------------------------------------------------
// Search dispatch
// ---------------------------------------------------------------------------

/// Returns `hits` for every call, or fails the call when `fail` is set.
pub(super) struct FixedHits {
    pub(super) hits: Vec<NodeId>,
    pub(super) calls: usize,
    pub(super) fail: bool,
}

pub(super) fn lexical(call: SearchCallId, generation: GraphGeneration, count: u64) -> SearchReport {
    SearchReport {
        call,
        generation,
        kind: SearchKind::Lexical,
        requested_tier: None,
        actual_tier: None,
        precision: ScorePrecision::NotApplicable,
        coverage: CandidateCoverage::Exact,
        vector_leg: LegState::NotRequested,
        lexical_leg: LegState::Nonempty,
        document_epoch: None,
        query_epoch: None,
        tokenizer_epoch: None,
        effective_alpha_bits: 0,
        normalization_version: 0,
        rules_version: 0,
        candidate_count: count,
        cross_scored_count: 0,
        fallback_count: 0,
        cross_score_complete: false,
        work: Default::default(),
    }
}

impl<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g> for FixedHits {
    fn search(
        &mut self,
        invocation: &SearchInvocation<'_, '_, 'v, 'm, 'g>,
        hits: &mut QueryArena<'m, 'g, SearchHit>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<SearchReport, NativeExecutionError> {
        self.calls += 1;
        if self.fail {
            return Err(RuntimeError::Limit(WorkKind::VectorBytes).into());
        }
        for node in &self.hits {
            hits.push(SearchHit {
                node: *node,
                score: 1.0,
                vector_distance: None,
                lexical_score: None,
            })
            .map_err(RuntimeError::Memory)?;
        }
        Ok(lexical(
            invocation.call,
            invocation.generation,
            self.hits.len() as u64,
        ))
    }
}

/// `CALL text_search('q', 3) YIELD node RETURN node`.
pub(super) fn search_nodes<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
) -> Result<Executed, GraphQueryError> {
    let query = String::from("q");
    let unit = vec![PlanNodeId(0)];
    let search = vec![PlanNodeId(1)];
    let projections = vec![Projection {
        slot: SlotId(10),
        expression: ExprId(2),
    }];
    let operators = vec![
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &unit,
            kind: OperatorKind::Search {
                call: SearchCallId(0),
                request: SearchRequest::Text {
                    query: ExprId(0),
                    k: ExprId(1),
                    eligible: None,
                },
                outputs: SearchOutputs {
                    node: Some(SlotId(0)),
                    ..SearchOutputs::default()
                },
            },
        },
        Operator {
            inputs: &search,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let expressions = vec![
        Expression::Literal(Literal::String(&query)),
        Expression::Literal(Literal::I64(3)),
        Expression::Slot(SlotId(0)),
    ];
    let eager = vec![PlanNodeId(1)];
    let mut backing = Backing::default();
    backing.string(&query)?;
    backing.vec(&unit)?;
    backing.vec(&search)?;
    backing.vec(&projections)?;
    run_plan(
        runtime,
        executor,
        &operators,
        &expressions,
        &eager,
        &backing,
        &["node"],
    )
}

/// A searching read plan reaches its adapter exactly once and returns the
/// adapter's hits with its report unchanged. Without an adapter the same
/// plan is an invalid plan, refused before any search.
#[test]
fn ze53_s3_search_plan_reaches_its_adapter_once() {
    let fixture = new_fixture("search");
    let before = fixture.generation().unwrap();
    let mut adapter = FixedHits {
        hits: vec![fixture.nodes[2], fixture.nodes[0]],
        calls: 0,
        fail: false,
    };
    let result = fixture
        .store
        .execute_graph_query(&control(), &options(16), Some(&mut adapter), search_nodes)
        .expect("search statement");
    assert_eq!(adapter.calls, 1);
    assert_eq!(result.metadata().outcome, Outcome::Read);
    let returned: Vec<u128> = (0..result.metadata().rows as usize)
        .map(|row| match result.cell(row, 0) {
            Some(Value::Node(index)) => result.pools().nodes[*index as usize].id.get(),
            other => panic!("row {row} is not a node: {other:?}"),
        })
        .collect();
    assert_eq!(
        returned,
        vec![fixture.nodes[2].get(), fixture.nodes[0].get()]
    );
    assert_eq!(
        result.pools().reports,
        &[lexical(SearchCallId(0), GraphGeneration::new(before), 2)]
    );

    let error = fixture
        .store
        .execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            search_nodes,
        )
        .map(|_| ())
        .expect_err("a searching plan needs an adapter");
    assert_eq!(error.kind(), GraphQueryErrorKind::InvalidPlan);
    assert!(
        matches!(
            error.cause(),
            GraphQueryCause::Execution(NativeExecutionError::Plan(PlanError::Search))
        ),
        "{error}"
    );
    fixture.remove().unwrap();
}

// ---------------------------------------------------------------------------
// ZE-193: cancellation around the commit attempt
// ---------------------------------------------------------------------------

/// Cancellation that arrives after a write statement's commit attempt has
/// appended its WAL envelope, while the Full sync is held, cannot claim a
/// rollback: the statement reports its commit, and a reopened store holds
/// it. The same cancellation before the commit tail's point of no return is
/// a definite `Cancelled` that published nothing, and a failed append is
/// indeterminate rather than cancelled or committed.
#[test]
fn ze193_cancel_after_commit_attempt_cannot_claim_rollback() {
    let fixture = new_fixture("ze193");
    let before = fixture.generation().unwrap();

    // Before the point of no return: the first private artifact exists, the
    // WAL is untouched.
    let token = CancelToken::new();
    let cancel = token.clone();
    fixture.vfs.after_next_create(move || cancel.cancel());
    let error = refused_unchanged(
        &fixture,
        fixture.write(&QueryControl::Cancel(token), 16, Assign::Increment),
        GraphQueryErrorKind::Cancelled,
        before,
    );
    assert!(
        !fixture.vfs.after_create_is_armed(),
        "the cancel hook fired"
    );
    assert!(
        !matches!(
            error.cause(),
            GraphQueryCause::Graph(NativeGraphError::CommitIndeterminate { .. })
        ),
        "{error}"
    );

    // After the WAL append: cancelled while the Full sync is held.
    let token = CancelToken::new();
    let (entered, release) = fixture.vfs.arm_wal_full_sync();
    let committed = std::thread::scope(|scope| {
        let writer = scope
            .spawn(|| fixture.write(&QueryControl::Cancel(token.clone()), 16, Assign::Increment));
        entered.wait();
        token.cancel();
        release.wait();
        writer.join().expect("writer thread")
    })
    .unwrap_or_else(|error| panic!("a post-commit-attempt cancel claimed rollback: {error}"));
    assert!(token.is_cancelled());
    assert_eq!(committed.metadata().outcome, committed_at(before + 1));
    assert_eq!(node_values(&committed).unwrap(), rows(&fixture, 1));
    assert_eq!(fixture.generation().unwrap(), before + 1);
    let expected = rows(&fixture, 1);
    let directory = fixture.directory.clone();
    fixture.store.close().unwrap();
    let reopened = Store::open_native_graph(
        directory.join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("reopen after the cancelled commit");
    let read = reopened
        .execute_graph_query(&control(), &options(16), None::<&mut NoSearch>, read_p)
        .unwrap();
    assert_eq!(node_values(&read).unwrap(), expected);
    reopened.close().unwrap();
    std::fs::remove_dir_all(&directory).unwrap();

    // A failed append: the outcome is unknown, never "nothing happened".
    let fixture = new_fixture("ze193-append");
    fixture.vfs.arm_fault(FaultPoint::Append);
    let error = fixture
        .write(&control(), 16, Assign::Increment)
        .map(|_| ())
        .expect_err("a failed WAL append cannot report a commit");
    fixture.vfs.assert_fired_once();
    assert_eq!(
        error.kind(),
        GraphQueryErrorKind::WriteIndeterminate,
        "{error}"
    );
    assert!(!error.nothing_committed());
    fixture.remove().unwrap();
}

/// The seam maps the lifecycle's closing and cancelled controls to their
/// own groups rather than to a generic failure.
#[test]
fn ze53_s3_control_errors_keep_their_groups() {
    let cancelled = GraphQueryError::from(NativeExecutionError::Runtime(RuntimeError::Value(
        QueryError::Cancelled,
    )));
    assert_eq!(cancelled.kind(), GraphQueryErrorKind::Cancelled);
    let closed = GraphQueryError::from(NativeExecutionError::Runtime(RuntimeError::Value(
        QueryError::ReadCancelled,
    )));
    assert_eq!(closed.kind(), GraphQueryErrorKind::Closed);
    let stopped = GraphQueryError::from(NativeGraphError::WritesStopped);
    assert_eq!(stopped.kind(), GraphQueryErrorKind::Unavailable);
    assert!(stopped.nothing_committed());
}
