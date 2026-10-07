#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "test fixtures use assertions and checked fixed indices"
)]
//! ZE-53 S3: the structured execution seam, through `Store::execute_graph_query`.
//!
//! Every statement is built inside its admission by the same builders the
//! adversarial probe uses. Each test checks the seam against an oracle that
//! does not read the result collector: the fixture's own values, the
//! statement's arithmetic, the published generation and a fresh read, often
//! after a reopen.

use super::super::{
    CandidateCoverage, CompletedGraphResult, LegState, Outcome, ScorePrecision, SearchKind,
    SearchReport, Value,
};
use super::entry::{Executed, GraphQueryExecutor, NoSearch};
use super::entry_probe::{
    Assign, Backing, Fixture, control, node_values, options, read_p, run_plan, write_p,
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
/// arena smaller than the statement is a limit.
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
    fn search<'s>(
        &mut self,
        _: &'s crate::property_graph::storage::GraphReadView<'s, 'v, 'm, 'g>,
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
                    options: Default::default(),
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

/// One row references each same-low64 entity twice. SET targets one literal
/// full identity while RETURN still observes both sides of its collision.
fn ze202_entities<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
    selected: Option<usize>,
) -> Result<Executed, GraphQueryError> {
    use super::test_support::ze202::{A, B, R, S};
    use crate::property_graph::query::plan::Mutation;
    use crate::property_graph::{GraphName, RelId};
    let p = String::from("p");
    let name = GraphName::new(&p).unwrap();
    let inputs: Vec<Vec<PlanNodeId>> = (0..7).map(|i| vec![PlanNodeId(i)]).collect();
    let mut operators = vec![Operator {
        inputs: &[],
        kind: OperatorKind::Unit,
    }];
    for (slot, kind) in [
        OperatorKind::LookupNode {
            output: SlotId(0),
            id: NodeId::new(A).unwrap(),
        },
        OperatorKind::LookupNode {
            output: SlotId(1),
            id: NodeId::new(B).unwrap(),
        },
        OperatorKind::LookupRelationship {
            output: SlotId(2),
            id: RelId::new(R).unwrap(),
        },
        OperatorKind::LookupRelationship {
            output: SlotId(3),
            id: RelId::new(S).unwrap(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        operators.push(Operator {
            inputs: &inputs[slot],
            kind,
        });
    }
    let mut expressions: Vec<_> = (0..4).map(|i| Expression::Slot(SlotId(i))).collect();
    expressions.extend((0..4).map(|i| Expression::Property {
        entity: ExprId(i),
        name,
    }));
    let mutations = selected
        .map(|slot| {
            vec![Mutation::SetProperty {
                entity: ExprId(slot as u32),
                name,
                value: ExprId(8),
            }]
        })
        .unwrap_or_default();
    if selected.is_some() {
        expressions.push(Expression::Literal(Literal::I64(999)));
        operators.push(Operator {
            inputs: &inputs[4],
            kind: OperatorKind::Eager,
        });
        operators.push(Operator {
            inputs: &inputs[5],
            kind: OperatorKind::Mutate(&mutations),
        });
    }
    let projections: Vec<_> = (0..12)
        .map(|i| Projection {
            slot: SlotId(10 + i),
            expression: ExprId(if i < 8 { i } else { i - 8 }),
        })
        .collect();
    operators.push(Operator {
        inputs: &inputs[operators.len() - 1],
        kind: OperatorKind::Project(&projections),
    });
    let mut backing = Backing::default();
    backing.string(&p)?;
    for input in &inputs {
        backing.vec(input)?;
    }
    backing.vec(&projections)?;
    backing.vec(&mutations)?;
    run_plan(
        runtime,
        executor,
        &operators,
        &expressions,
        &Vec::new(),
        &backing,
        &[
            "a", "b", "r", "s", "ap", "bp", "rp", "sp", "aa", "bb", "rr", "ss",
        ],
    )
}

fn ze202_query(store: &Store, selected: Option<usize>) -> CompletedGraphResult {
    store
        .execute_graph_query(
            &control(),
            &options(16),
            None::<&mut NoSearch>,
            |runtime, executor| ze202_entities(runtime, executor, selected),
        )
        .unwrap()
}

fn ze202_assert_entities(result: &CompletedGraphResult, selected: Option<usize>) {
    use super::test_support::ze202::{A, B, R, S};
    assert_eq!(result.metadata().rows, 1);
    assert_eq!(
        result
            .pools()
            .nodes
            .iter()
            .map(|n| n.id.get())
            .collect::<Vec<_>>(),
        vec![A, B]
    );
    assert_eq!(
        result
            .pools()
            .relationships
            .iter()
            .map(|r| r.id.get())
            .collect::<Vec<_>>(),
        vec![R, S]
    );
    for (i, value) in [101, 202, 111, 212].into_iter().enumerate() {
        assert_eq!(
            result.cell(0, i + 4),
            Some(&Value::I64(if selected == Some(i) { 999 } else { value }))
        );
        assert_eq!(result.cell(0, i), result.cell(0, i + 8));
    }
    assert_eq!(result.cell(0, 0), Some(&Value::Node(0)));
    assert_eq!(result.cell(0, 1), Some(&Value::Node(1)));
    assert_eq!(result.cell(0, 2), Some(&Value::Relationship(0)));
    assert_eq!(result.cell(0, 3), Some(&Value::Relationship(1)));
    let pools = result.pools();
    let spans = pools
        .nodes
        .iter()
        .map(|n| n.properties)
        .chain(pools.relationships.iter().map(|r| r.properties));
    for (i, span) in spans.enumerate() {
        assert_eq!(span.len, 1);
        let property = pools.properties[span.start as usize];
        assert_eq!(
            &pools.bytes
                [property.name.start as usize..(property.name.start + property.name.len) as usize],
            b"p"
        );
        assert_eq!(
            Some(&pools.values[property.value.0 as usize]),
            result.cell(0, i + 4)
        );
    }
    let rels = pools.relationships;
    assert_eq!(rels[0].source.get(), A);
    assert_eq!(rels[0].target.get(), A);
    assert_eq!(rels[1].source.get(), B);
    assert_eq!(rels[1].target.get(), A);
}

#[test]
fn ze202_same_low64_read_results_keep_both_entities() {
    let (store, _dir) = super::test_support::ze202::fixture(None);
    ze202_assert_entities(&ze202_query(&store, None), None);
    store.close().unwrap();
}

#[test]
fn ze202_same_low64_set_changes_only_selected_entity() {
    for selected in [1, 3] {
        let (store, dir) = super::test_support::ze202::fixture(None);
        let before = ze202_query(&store, None);
        let after = ze202_query(&store, Some(selected));
        ze202_assert_entities(&after, Some(selected));
        let metadata = |result: &CompletedGraphResult| {
            result
                .pools()
                .nodes
                .iter()
                .map(|n| (n.revision.get(), n.generation.get()))
                .chain(
                    result
                        .pools()
                        .relationships
                        .iter()
                        .map(|r| (r.revision.get(), r.generation.get())),
                )
                .collect::<Vec<_>>()
        };
        let mut expected = metadata(&before);
        expected[selected] = (2, 7);
        assert_eq!(metadata(&after), expected);
        assert_eq!(after.metadata().outcome, committed_at(7));
        store.close().unwrap();
        drop(store);
        let reopened = Store::open_native_graph(
            dir.path().join("native"),
            super::test_support::ze202::options(),
            None,
        )
        .unwrap();
        let fresh = ze202_query(&reopened, None);
        ze202_assert_entities(&fresh, Some(selected));
        assert_eq!(metadata(&fresh), expected);
        reopened.close().unwrap();
    }
}

#[test]
fn ze192_query_probe_checks_fault_outcomes() {
    let report = super::entry_probe::run_actual_probe(7).unwrap();
    assert_eq!(
        report
            .receipts
            .iter()
            .find(|(name, _)| *name == "incident.fire")
            .map(|(_, count)| *count),
        Some(1),
        "missing measured incident receipt"
    );
    assert_eq!(report.observations, report.expected);
}

/// The marker is durable even when publication fails before acknowledgement.
#[test]
fn ze190_fence_only_commit_marker_recovers_before_ack() {
    for inject in [false, true] {
        let (actual, expected) = super::entry_probe::fence_only_probe(190, 0, inject).unwrap();
        assert_eq!(actual, expected);
        let mut perturbed = expected;
        perturbed[0].0 -= 1;
        assert_ne!(actual, perturbed, "oracle must catch a reused identity");
    }
}
