//! ZE-53 S1: eager search sources through the real native completion path.
//!
//! A scripted adapter stands in for ranking (ZE-64 binds the real one). It
//! records every invocation, so each test proves the call count, order and
//! eligibility it saw, and returns an independently chosen report that the
//! completed result must carry back exactly.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use super::super::{
    ActualTier, CandidateCoverage, CompletedGraphResult, LegState, ScorePrecision, SearchKind,
    SearchReport, Value,
};
use super::*;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::eligibility::Eligibility;
use crate::property_graph::query::expression::ExpressionCapacity;
use crate::property_graph::query::pattern::{SearchArguments, SearchHit, SearchInvocation};
use crate::property_graph::query::plan::{
    ExprId, Expression, Literal, NodeFacts, Operator, OperatorKind, PlanBacking, PlanDescription,
    PlanFootprint, Projection, RetainedRegion, SearchCallId, SearchMode, SearchOutputs,
    SearchRequest, SlotId, VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{ArenaCapacity, RuntimeLimits, WorkKind};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::tree::directory::TreeError;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphGeneration, GraphRevision, NodeId,
};
use std::mem::size_of;
use std::sync::{Arc, Mutex};

/// What one adapter invocation saw.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    call: u32,
    k: u32,
    /// `None` is `AllIndexed`; `Some` is the exact materialized set.
    eligible: Option<Vec<NodeId>>,
}

#[derive(Clone)]
struct Script {
    hits: Vec<(NodeId, f64)>,
    report: fn(SearchCallId, GraphGeneration, u64) -> SearchReport,
    fail: bool,
}

struct Scripted {
    scripts: Vec<Script>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// Sentinel the adapter fails with, distinct from every executor error.
const ADAPTER_FAILURE: WorkKind = WorkKind::VectorBytes;

impl<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g> for Scripted {
    fn search<'s>(
        &mut self,
        _: &'s crate::property_graph::storage::GraphReadView<'s, 'v, 'm, 'g>,
        invocation: &SearchInvocation<'_, '_, 'v, 'm, 'g>,
        hits: &mut QueryArena<'m, 'g, SearchHit>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<SearchReport, NativeExecutionError> {
        let eligible = match invocation.eligibility {
            Eligibility::AllIndexed => None,
            Eligibility::Set(set) => Some(set.ids_for(context.view()).unwrap().to_vec()),
        };
        match invocation.arguments {
            SearchArguments::Text { query } => assert_eq!(query, "q"),
            SearchArguments::Vector { vector, mode } => {
                assert_eq!(vector.len(), 2);
                assert_eq!(mode, SearchMode::Default);
            }
            SearchArguments::Hybrid { .. } => panic!("no hybrid call is scripted"),
        }
        self.seen.lock().unwrap().push(Seen {
            call: invocation.call.0,
            k: invocation.k,
            eligible,
        });
        let script = self.scripts[invocation.call.0 as usize].clone();
        if script.fail {
            return Err(RuntimeError::Limit(ADAPTER_FAILURE).into());
        }
        for (node, score) in &script.hits {
            hits.push(SearchHit {
                node: *node,
                score: *score,
                vector_distance: None,
                lexical_score: None,
            })
            .unwrap();
        }
        Ok((script.report)(
            invocation.call,
            invocation.generation,
            script.hits.len() as u64,
        ))
    }
}

fn lexical(call: SearchCallId, generation: GraphGeneration, count: u64) -> SearchReport {
    SearchReport {
        call,
        generation,
        kind: SearchKind::Lexical,
        requested_tier: None,
        actual_tier: None,
        precision: ScorePrecision::NotApplicable,
        coverage: CandidateCoverage::Exact,
        vector_leg: LegState::NotRequested,
        lexical_leg: if count == 0 {
            LegState::NoEligibleMembers
        } else {
            LegState::Nonempty
        },
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

/// An approximate graph-route report, which must never come back as Exact.
fn approximate(call: SearchCallId, generation: GraphGeneration, count: u64) -> SearchReport {
    SearchReport {
        kind: SearchKind::Vector,
        actual_tier: Some(ActualTier::Graph),
        precision: ScorePrecision::Original,
        coverage: CandidateCoverage::Approximate,
        vector_leg: LegState::Nonempty,
        lexical_leg: LegState::NotRequested,
        fallback_count: 1,
        ..lexical(call, generation, count)
    }
}

/// ZE-197: a Vector report for an explicit empty eligible set, exactly as
/// the real `rank_vector` (ZE-62, unchanged and closed: see
/// `ze62_absent_restriction_differs_from_explicit_empty_set` and
/// `report_for_exact` in `lifecycle::native_graph::tests::ranking`) reports
/// it: `actual_tier: None`, because membership alone proves there is
/// nothing eligible to rank, which is distinct from an actual route. The
/// completed-result validator (`query::completed::validate::records`) must
/// accept exactly this shape for a `NoEligibleMembers`/`NoIndexedPopulation`
/// vector leg; it still rejects `None` for any other vector leg state.
fn vector_empty(call: SearchCallId, generation: GraphGeneration, count: u64) -> SearchReport {
    SearchReport {
        kind: SearchKind::Vector,
        actual_tier: None,
        precision: ScorePrecision::NotApplicable,
        coverage: CandidateCoverage::Exact,
        vector_leg: LegState::NoEligibleMembers,
        lexical_leg: LegState::NotRequested,
        ..lexical(call, generation, count)
    }
}

#[derive(Clone, Copy)]
enum Shape {
    /// Two independent calls joined, then projected to the two nodes only.
    Cartesian,
    /// One call under `LIMIT 0`.
    LimitZero,
    /// One call restricted to an explicit empty eligibility list.
    EmptyEligible,
}

struct SearchConsumer {
    shape: Shape,
    adapter: Scripted,
}

type Outcome = Result<CompletedGraphResult, RuntimeFailure<NativeResultError>>;

impl NativeReadConsumer<Outcome> for SearchConsumer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Outcome, TreeError> {
        let memory = runtime.memory();
        let query = String::from("q");
        let unit: [PlanNodeId; 1] = [PlanNodeId(0)];
        let second_unit = [PlanNodeId(2)];
        let first_search = [PlanNodeId(1)];
        let join_inputs = [PlanNodeId(1), PlanNodeId(3)];
        let project_join = [PlanNodeId(4)];
        let coordinates = [ExprId(5), ExprId(6)];
        let vector_coords = [ExprId(4), ExprId(5)];
        let no_members: [ExprId; 0] = [];
        let cartesian_projection = [
            Projection {
                slot: SlotId(10),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(11),
                expression: ExprId(4),
            },
        ];
        let single_projection = [Projection {
            slot: SlotId(10),
            expression: ExprId(3),
        }];
        let text = |eligible: Option<ExprId>| OperatorKind::Search {
            call: SearchCallId(0),
            request: SearchRequest::Text {
                query: ExprId(0),
                k: ExprId(1),
                eligible,
            },
            outputs: SearchOutputs {
                node: Some(SlotId(0)),
                score: Some(SlotId(1)),
                ..SearchOutputs::default()
            },
        };
        let (operators, expressions, eager): (Vec<Operator<'_>>, Vec<Expression<'_>>, Vec<_>) =
            match self.shape {
                Shape::Cartesian => (
                    vec![
                        Operator {
                            inputs: &[],
                            kind: OperatorKind::Unit,
                        },
                        Operator {
                            inputs: &unit,
                            kind: text(None),
                        },
                        Operator {
                            inputs: &[],
                            kind: OperatorKind::Unit,
                        },
                        Operator {
                            inputs: &second_unit,
                            kind: OperatorKind::Search {
                                call: SearchCallId(1),
                                request: SearchRequest::Vector {
                                    vector: ExprId(2),
                                    k: ExprId(1),
                                    mode: SearchMode::Default,
                                    eligible: None,
                                },
                                outputs: SearchOutputs {
                                    node: Some(SlotId(2)),
                                    distance: Some(SlotId(3)),
                                    ..SearchOutputs::default()
                                },
                            },
                        },
                        Operator {
                            inputs: &join_inputs,
                            kind: OperatorKind::Join { predicate: None },
                        },
                        Operator {
                            inputs: &project_join,
                            kind: OperatorKind::Project(&cartesian_projection),
                        },
                    ],
                    vec![
                        Expression::Literal(Literal::String(&query)),
                        Expression::Literal(Literal::I64(2)),
                        Expression::List(&coordinates),
                        Expression::Slot(SlotId(0)),
                        Expression::Slot(SlotId(2)),
                        Expression::Literal(Literal::F64(1.0)),
                        Expression::Literal(Literal::F64(0.0)),
                    ],
                    vec![PlanNodeId(1), PlanNodeId(3)],
                ),
                Shape::LimitZero => (
                    vec![
                        Operator {
                            inputs: &[],
                            kind: OperatorKind::Unit,
                        },
                        Operator {
                            inputs: &unit,
                            kind: text(None),
                        },
                        Operator {
                            inputs: &first_search,
                            kind: OperatorKind::OffsetLimit {
                                offset: 0,
                                limit: Some(0),
                            },
                        },
                    ],
                    vec![
                        Expression::Literal(Literal::String(&query)),
                        Expression::Literal(Literal::I64(2)),
                    ],
                    vec![PlanNodeId(1)],
                ),
                // ZE-197: a Vector call (not Text) with an explicit empty
                // eligibility list, so the completed result must carry a
                // Vector/Hybrid report through the validator's actual_tier
                // check, which a Lexical report never exercises.
                Shape::EmptyEligible => (
                    vec![
                        Operator {
                            inputs: &[],
                            kind: OperatorKind::Unit,
                        },
                        Operator {
                            inputs: &unit,
                            kind: OperatorKind::Search {
                                call: SearchCallId(0),
                                request: SearchRequest::Vector {
                                    vector: ExprId(2),
                                    k: ExprId(0),
                                    mode: SearchMode::Default,
                                    eligible: Some(ExprId(1)),
                                },
                                outputs: SearchOutputs {
                                    node: Some(SlotId(0)),
                                    distance: Some(SlotId(1)),
                                    ..SearchOutputs::default()
                                },
                            },
                        },
                        Operator {
                            inputs: &first_search,
                            kind: OperatorKind::Project(&single_projection),
                        },
                    ],
                    vec![
                        Expression::Literal(Literal::I64(2)),
                        Expression::List(&no_members),
                        Expression::List(&vector_coords),
                        Expression::Slot(SlotId(0)),
                        Expression::Literal(Literal::F64(1.0)),
                        Expression::Literal(Literal::F64(0.0)),
                    ],
                    vec![PlanNodeId(1)],
                ),
            };
        let mut facts = QueryArena::new(memory, operators.len()).expect("search fact arena");
        for _ in 0..operators.len() {
            facts.push(NodeFacts::default()).expect("search fact slot");
        }
        let mut regions = vec![
            RetainedRegion::vector(&operators).unwrap(),
            RetainedRegion::vector(&expressions).unwrap(),
            RetainedRegion::vector(&eager).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
            RetainedRegion::declared(query.as_ptr() as usize, query.capacity()).unwrap(),
            RetainedRegion::slice(&unit).unwrap(),
            RetainedRegion::slice(&second_unit).unwrap(),
            RetainedRegion::slice(&first_search).unwrap(),
            RetainedRegion::slice(&join_inputs).unwrap(),
            RetainedRegion::slice(&project_join).unwrap(),
            RetainedRegion::slice(&coordinates).unwrap(),
            RetainedRegion::slice(&vector_coords).unwrap(),
            RetainedRegion::slice(&cartesian_projection).unwrap(),
            RetainedRegion::slice(&single_projection).unwrap(),
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .map(|region| region.end() - region.start())
            .sum::<usize>();
        let mut external = memory
            .reserve_external_capacity()
            .expect("search plan backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("search plan validation backing");
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(u32::try_from(operators.len() - 1).unwrap()),
            eager_searches: &eager,
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                runtime.values(),
            )
            .expect("validate search plan");
        let owners = vec![
            RetainedAllocation::vector(&operators).unwrap(),
            RetainedAllocation::vector(&expressions).unwrap(),
            RetainedAllocation::vector(&eager).unwrap(),
            facts_owner,
            RetainedAllocation::string(&query).unwrap(),
            RetainedAllocation::array(&unit).unwrap(),
            RetainedAllocation::array(&second_unit).unwrap(),
            RetainedAllocation::array(&first_search).unwrap(),
            RetainedAllocation::array(&join_inputs).unwrap(),
            RetainedAllocation::array(&project_join).unwrap(),
            RetainedAllocation::array(&coordinates).unwrap(),
            RetainedAllocation::array(&vector_coords).unwrap(),
            RetainedAllocation::array(&cartesian_projection).unwrap(),
            RetainedAllocation::array(&single_projection).unwrap(),
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            runtime.values(),
        )
        .expect("retain search plan")
        .admit_plan(&plan, runtime.values())
        .expect("admit search plan");
        let columns: Vec<GraphName<'_>> = match self.shape {
            Shape::Cartesian => vec![
                GraphName::new("left").unwrap(),
                GraphName::new("right").unwrap(),
            ],
            Shape::LimitZero => vec![
                GraphName::new("node").unwrap(),
                GraphName::new("score").unwrap(),
            ],
            Shape::EmptyEligible => vec![GraphName::new("node").unwrap()],
        };
        let variable = ArenaCapacity {
            string_bytes: 4096,
            list_cells: 64,
            node_ids: 64,
            relationship_ids: 64,
        };
        Ok(execute_native_search_result(
            view,
            runtime,
            &admitted,
            &[],
            &columns,
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 16,
                    payload_bytes: 8192,
                    variable,
                },
                expression: ExpressionCapacity {
                    cells: 64,
                    string_bytes: 4096,
                },
            },
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 16,
                batch_payload_bytes: 8192,
                result_payload_bytes: 8192,
                batch: variable,
                result: variable,
            },
            &mut self.adapter,
        ))
    }
}

/// A native store holding three plain nodes, with their full IDs.
fn fixture(directory: &std::path::Path) -> (Store, [NodeId; 3]) {
    let store = Store::create_native_graph(
        directory.join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        None,
    )
    .expect("create native search graph");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("fixture node");
    let keys = ["a", "b", "c"].map(|key| ApplicationKey::new(EntityKind::Node, "s1", key).unwrap());
    let writes = keys.map(|key| StructuredWrite {
        key,
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    });
    let receipts = store
        .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
        .expect("publish native search fixture");
    let ids = [0, 1, 2].map(|index| match receipts[index].entity {
        EntityId::Node(id) => id,
        _ => panic!("fixture receipt kind"),
    });
    (store, ids)
}

fn run(store: &Store, shape: Shape, scripts: Vec<Script>) -> (Outcome, Vec<Seen>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let outcome = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            SearchConsumer {
                shape,
                adapter: Scripted {
                    scripts,
                    seen: Arc::clone(&seen),
                },
            },
        )
        .expect("admit native search read");
    let seen = seen.lock().unwrap().clone();
    (outcome, seen)
}

fn node(result: &CompletedGraphResult, row: usize, column: usize) -> NodeId {
    let Value::Node(index) = *result.cell(row, column).expect("node cell") else {
        panic!("node cell kind");
    };
    result.pools().nodes[index as usize].id
}

#[test]
fn ze53_s1_independent_calls_keep_cartesian_bag_and_reports() {
    let directory = tempfile::tempdir().expect("search store");
    let (store, [a, b, c]) = fixture(directory.path());
    let (outcome, seen) = run(
        &store,
        Shape::Cartesian,
        vec![
            Script {
                hits: vec![(a, 0.9), (b, 0.5)],
                report: lexical,
                fail: false,
            },
            Script {
                hits: vec![(a, 0.1), (c, 0.4)],
                report: approximate,
                fail: false,
            },
        ],
    );
    let result = outcome.expect("cartesian search result");
    store.close().expect("close search store");
    drop(store);

    // Each syntactic call ran exactly once, in call order, although the
    // nested-loop join replays the right-hand call for every left row.
    let unrestricted = |call| Seen {
        call,
        k: 2,
        eligible: None,
    };
    assert_eq!(seen, vec![unrestricted(0), unrestricted(1)]);

    // The complete Cartesian bag, with the shared node `a` kept on both sides.
    let mut rows: Vec<(NodeId, NodeId)> = (0..result.metadata().rows as usize)
        .map(|row| (node(&result, row, 0), node(&result, row, 1)))
        .collect();
    rows.sort();
    let mut expected = vec![(a, a), (a, c), (b, a), (b, c)];
    expected.sort();
    assert_eq!(rows, expected);

    // Both reports survive a projection that dropped every score column,
    // and the approximate one is carried back unchanged, not upgraded.
    let generation = result.metadata().generation;
    assert_eq!(
        result.pools().reports,
        [
            lexical(SearchCallId(0), generation, 2),
            approximate(SearchCallId(1), generation, 2),
        ]
    );
    assert_eq!(
        result.metadata().counters.get(WorkKind::SearchInvocations),
        2
    );
}

#[test]
fn ze53_s1_limit_zero_still_invokes_and_reports() {
    let directory = tempfile::tempdir().expect("search store");
    let (store, [a, _, _]) = fixture(directory.path());
    let (outcome, seen) = run(
        &store,
        Shape::LimitZero,
        vec![Script {
            hits: vec![(a, 0.9)],
            report: lexical,
            fail: false,
        }],
    );
    let result = outcome.expect("limit-zero search result");
    assert_eq!(seen.len(), 1);
    assert_eq!(result.metadata().rows, 0);
    assert_eq!(
        result.pools().reports,
        [lexical(SearchCallId(0), result.metadata().generation, 1)]
    );
}

#[test]
fn ze53_s1_empty_eligible_vector_set_yields_zero_rows_and_one_report() {
    let directory = tempfile::tempdir().expect("search store");
    let (store, _) = fixture(directory.path());
    let (outcome, seen) = run(
        &store,
        Shape::EmptyEligible,
        vec![Script {
            hits: vec![],
            report: vector_empty,
            fail: false,
        }],
    );
    let result = outcome.expect("empty-eligible search result");
    // An explicit empty list is a restriction, distinct from none at all.
    assert_eq!(
        seen,
        vec![Seen {
            call: 0,
            k: 2,
            eligible: Some(vec![]),
        }]
    );
    assert_eq!(result.metadata().rows, 0);
    assert_eq!(
        result.pools().reports,
        [vector_empty(
            SearchCallId(0),
            result.metadata().generation,
            0
        )]
    );
}

/// ZE-197 discrimination: `actual_tier: None` is accepted only for the two
/// leg states that mean "nothing eligible to rank". A `None` tier paired
/// with any other vector leg (here, `Nonempty`, which a real producer would
/// never emit) is still a `CompletedError::Shape` violation, so the ZE-197
/// fix narrowly targets the empty-eligible gap and does not accept every
/// missing tier.
#[test]
fn ze197_missing_tier_still_rejected_for_a_nonempty_vector_leg() {
    fn broken_nonempty(
        call: SearchCallId,
        generation: GraphGeneration,
        count: u64,
    ) -> SearchReport {
        SearchReport {
            actual_tier: None,
            precision: ScorePrecision::Original,
            vector_leg: LegState::Nonempty,
            ..vector_empty(call, generation, count)
        }
    }
    let directory = tempfile::tempdir().expect("search store");
    let (store, _) = fixture(directory.path());
    let (outcome, _) = run(
        &store,
        Shape::EmptyEligible,
        vec![Script {
            hits: vec![],
            report: broken_nonempty,
            fail: false,
        }],
    );
    let Err(failure) = outcome else {
        panic!("a Vector report with a Nonempty leg and no actual_tier must not produce a result");
    };
    assert!(matches!(
        failure.error,
        NativeResultError::Completed(CompletedError::Shape)
    ));
}

#[test]
fn ze53_s1_adapter_error_returns_no_result() {
    let directory = tempfile::tempdir().expect("search store");
    let (store, [a, _, _]) = fixture(directory.path());
    let (outcome, seen) = run(
        &store,
        Shape::LimitZero,
        vec![Script {
            hits: vec![(a, 0.9)],
            report: lexical,
            fail: true,
        }],
    );
    let Err(failure) = outcome else {
        panic!("a failed search must not produce a result");
    };
    assert_eq!(seen.len(), 1);
    assert_eq!(failure.operator, PlanNodeId(1));
    assert!(matches!(
        failure.error,
        NativeResultError::Native(NativeExecutionError::Runtime(RuntimeError::Limit(
            ADAPTER_FAILURE
        )))
    ));
}
