#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "test fixtures use assertions and checked fixed indices"
)]
//! ZE-64: the real `NativeSearchAdapter` through the ZE-53 seam, ranking
//! against a real store with real ZE-62/63 producers (no mock adapter).

use super::super::{
    ActualTier, CandidateCoverage, CompletedGraphResult, LegState, ScorePrecision, SearchKind,
    Value,
};
use super::*;
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::expression::ExpressionCapacity;
use crate::property_graph::query::pattern::PatternCapacity;
use crate::property_graph::query::plan::{
    ExprId, Expression, Literal, NodeFacts, Operator, OperatorKind, PlanBacking, PlanDescription,
    PlanFootprint, Projection, RetainedRegion, SearchCallId, SearchMode, SearchOutputs,
    SearchRequest, SlotId, VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{ArenaCapacity, RuntimeLimits};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::tree::directory::TreeError;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphName,
    GraphRevision, NodeId,
};
use std::mem::size_of;

fn tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze64-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x06, 0x4a],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}

/// A real store with four nodes: `a`/`b` vector-only, `c` text-only, `d`
/// both (vector at the origin, text `"amber"`), so vector-only, text-only
/// and hybrid calls each have a real, distinct answer.
fn fixture() -> (Store, tempfile::TempDir, [NodeId; 4]) {
    let directory = tempfile::tempdir().expect("ze64 search store");
    let document = tower();
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        Some(document.clone()),
    )
    .expect("create ze64 search store");
    let points: [(Option<[f32; 2]>, Option<&str>); 4] = [
        (Some([1.0, 1.0]), None),
        (Some([5.0, 5.0]), None),
        (None, Some("amber birch")),
        (Some([0.0, 0.0]), Some("amber")),
    ];
    let contents = points
        .iter()
        .map(|(point, text)| {
            let embedding = point
                .as_ref()
                .map(|point| CanonicalEmbedding::new(&document, point).unwrap());
            CanonicalContents::node(&mut [], &mut [], *text, embedding).unwrap()
        })
        .collect::<Vec<_>>();
    let keys =
        ["a", "b", "c", "d"].map(|key| ApplicationKey::new(EntityKind::Node, "ze64", key).unwrap());
    let writes = keys
        .iter()
        .zip(&contents)
        .map(|(key, contents)| StructuredWrite {
            key: *key,
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(contents)),
        })
        .collect::<Vec<_>>();
    let receipts = store
        .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
        .expect("publish ze64 search fixture");
    let nodes = std::array::from_fn(|index| match receipts[index].entity {
        EntityId::Node(node) => node,
        _ => panic!("fixture receipt kind"),
    });
    (store, directory, nodes)
}

type Outcome = Result<CompletedGraphResult, RuntimeFailure<NativeResultError>>;

const VARIABLE: ArenaCapacity = ArenaCapacity {
    string_bytes: 4096,
    list_cells: 64,
    node_ids: 64,
    relationship_ids: 64,
};

const PATTERN_CAPACITY: PatternCapacity = PatternCapacity {
    rows: StorageCapacity {
        rows: 16,
        max_rows: 16,
        payload_bytes: 8192,
        variable: VARIABLE,
    },
    expression: ExpressionCapacity {
        cells: 64,
        string_bytes: 4096,
    },
};

const EXECUTION_CAPACITY: ExecutionCapacity = ExecutionCapacity {
    batch_rows: 16,
    result_rows: 16,
    batch_payload_bytes: 8192,
    result_payload_bytes: 8192,
    batch: VARIABLE,
    result: VARIABLE,
};

/// Runs one already-built `[Unit ->] Search -> Project` plan and returns the
/// completed result. Every byte an expression references (list-index
/// backing, string literals) must have its own registered, sorted, disjoint
/// `RetainedRegion`; a zero-length referenced span needs none.
///
/// A macro, not a function: the pieces below are related by the exact
/// lifetimes `consume`'s own generic parameters name (`'s`, `'lease`, `'m`,
/// `'g`), and a helper function forced those apart into unrelated elided
/// lifetimes that could no longer be proven equal.
macro_rules! run_search_plan {
    (
        $view:expr,
        $runtime:expr,
        $operators:expr,
        $expressions:expr,
        $eager:expr,
        $regions:expr,
        $owners:expr,
        $columns:expr,
        $adapter:expr $(,)?
    ) => {{
        let memory = $runtime.memory();
        let operators = $operators;
        let expressions = $expressions;
        let eager = $eager;
        let mut facts = QueryArena::new(memory, operators.len()).expect("ze64 fact arena");
        for _ in 0..operators.len() {
            facts.push(NodeFacts::default()).expect("ze64 fact slot");
        }
        let mut regions = $regions;
        regions.push(RetainedRegion::vector(&operators).unwrap());
        regions.push(RetainedRegion::vector(&expressions).unwrap());
        regions.push(RetainedRegion::vector(&eager).unwrap());
        regions.push(
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
        );
        regions.sort();
        let retained_bytes = regions
            .iter()
            .map(|region| region.end() - region.start())
            .sum::<usize>();
        let mut external = memory
            .reserve_external_capacity()
            .expect("ze64 plan backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("ze64 plan validation backing");
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
                $runtime.values(),
            )
            .expect("validate ze64 search plan");
        let mut owners = $owners;
        owners.push(RetainedAllocation::vector(&operators).unwrap());
        owners.push(RetainedAllocation::vector(&expressions).unwrap());
        owners.push(RetainedAllocation::vector(&eager).unwrap());
        owners.push(facts_owner);
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            $runtime.values(),
        )
        .expect("retain ze64 search plan")
        .admit_plan(&plan, $runtime.values())
        .expect("admit ze64 search plan");
        let columns: Vec<GraphName<'_>> = $columns
            .iter()
            .map(|name| GraphName::new(name).unwrap())
            .collect();
        execute_native_search_result(
            $view,
            $runtime,
            &admitted,
            &[],
            &columns,
            PATTERN_CAPACITY,
            EXECUTION_CAPACITY,
            $adapter,
        )
    }};
}

fn node_cell(result: &CompletedGraphResult, row: usize, column: usize) -> NodeId {
    let Value::Node(index) = *result.cell(row, column).expect("node cell") else {
        panic!("expected a node cell");
    };
    result.pools().nodes[index as usize].id
}

fn f64_cell(result: &CompletedGraphResult, row: usize, column: usize) -> Option<f64> {
    match *result.cell(row, column).expect("score cell") {
        Value::F64(value) => Some(f64::from_bits(value)),
        Value::Null => None,
        other => panic!("expected an F64 or null cell, got {other:?}"),
    }
}

struct VectorSearch<'a> {
    coords: [f32; 2],
    k: i64,
    mode: SearchMode,
    empty_eligible: bool,
    adapter: NativeSearchAdapter<'a>,
}

impl NativeReadConsumer<Outcome> for VectorSearch<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Outcome, TreeError> {
        let unit = [PlanNodeId(0)];
        let search_input = [PlanNodeId(1)];
        // Every declared expression must be reachable from the root, so the
        // eligible-list expression only exists when the call actually
        // restricts eligibility; the indices after it shift accordingly.
        static EMPTY_LIST: [ExprId; 0] = [];
        let mut expressions: Vec<Expression<'_>> = vec![Expression::Literal(Literal::I64(self.k))];
        let eligible = if self.empty_eligible {
            let id = ExprId(u32::try_from(expressions.len()).unwrap());
            expressions.push(Expression::List(&EMPTY_LIST));
            Some(id)
        } else {
            None
        };
        let vector_id = ExprId(u32::try_from(expressions.len()).unwrap());
        let coord0_id = ExprId(u32::try_from(expressions.len() + 1).unwrap());
        let coord1_id = ExprId(u32::try_from(expressions.len() + 2).unwrap());
        let vector_list = [coord0_id, coord1_id];
        expressions.push(Expression::List(&vector_list));
        expressions.push(Expression::Literal(Literal::F64(f64::from(self.coords[0]))));
        expressions.push(Expression::Literal(Literal::F64(f64::from(self.coords[1]))));
        let node_id = ExprId(u32::try_from(expressions.len()).unwrap());
        expressions.push(Expression::Slot(SlotId(0)));
        let distance_id = ExprId(u32::try_from(expressions.len()).unwrap());
        expressions.push(Expression::Slot(SlotId(1)));

        let projection = [
            Projection {
                slot: SlotId(10),
                expression: node_id,
            },
            Projection {
                slot: SlotId(11),
                expression: distance_id,
            },
        ];
        let operators = vec![
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &unit,
                kind: OperatorKind::Search {
                    call: SearchCallId(0),
                    request: SearchRequest::Vector {
                        vector: vector_id,
                        k: ExprId(0),
                        mode: self.mode,
                        eligible,
                        options: Default::default(),
                    },
                    outputs: SearchOutputs {
                        node: Some(SlotId(0)),
                        distance: Some(SlotId(1)),
                        ..SearchOutputs::default()
                    },
                },
            },
            Operator {
                inputs: &search_input,
                kind: OperatorKind::Project(&projection),
            },
        ];
        let eager = vec![PlanNodeId(1)];
        let regions = vec![
            RetainedRegion::slice(&unit).unwrap(),
            RetainedRegion::slice(&search_input).unwrap(),
            RetainedRegion::slice(&vector_list).unwrap(),
            RetainedRegion::slice(&projection).unwrap(),
        ];
        let owners = vec![
            RetainedAllocation::array(&unit).unwrap(),
            RetainedAllocation::array(&search_input).unwrap(),
            RetainedAllocation::array(&vector_list).unwrap(),
            RetainedAllocation::array(&projection).unwrap(),
        ];
        Ok(run_search_plan!(
            view,
            runtime,
            &operators,
            &expressions,
            &eager,
            regions,
            owners,
            &["node", "score"],
            &mut self.adapter,
        ))
    }
}

struct TextSearch<S> {
    query: String,
    k: i64,
    adapter: S,
}

impl<S: for<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g>> NativeReadConsumer<Outcome> for TextSearch<S> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Outcome, TreeError> {
        let unit = [PlanNodeId(0)];
        let search_input = [PlanNodeId(1)];
        let projection = [
            Projection {
                slot: SlotId(10),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(11),
                expression: ExprId(3),
            },
        ];
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
                        query: ExprId(1),
                        k: ExprId(0),
                        eligible: None,
                        options: Default::default(),
                    },
                    outputs: SearchOutputs {
                        node: Some(SlotId(0)),
                        score: Some(SlotId(1)),
                        ..SearchOutputs::default()
                    },
                },
            },
            Operator {
                inputs: &search_input,
                kind: OperatorKind::Project(&projection),
            },
        ];
        let expressions = vec![
            Expression::Literal(Literal::I64(self.k)),
            Expression::Literal(Literal::String(&self.query)),
            Expression::Slot(SlotId(0)),
            Expression::Slot(SlotId(1)),
        ];
        let eager = vec![PlanNodeId(1)];
        let regions = vec![
            RetainedRegion::slice(&unit).unwrap(),
            RetainedRegion::slice(&search_input).unwrap(),
            RetainedRegion::declared(self.query.as_ptr() as usize, self.query.capacity()).unwrap(),
            RetainedRegion::slice(&projection).unwrap(),
        ];
        let owners = vec![
            RetainedAllocation::array(&unit).unwrap(),
            RetainedAllocation::array(&search_input).unwrap(),
            RetainedAllocation::string(&self.query).unwrap(),
            RetainedAllocation::array(&projection).unwrap(),
        ];
        Ok(run_search_plan!(
            view,
            runtime,
            &operators,
            &expressions,
            &eager,
            regions,
            owners,
            &["node", "score"],
            &mut self.adapter,
        ))
    }
}

struct HybridSearch<'a> {
    coords: [f32; 2],
    query: String,
    k: i64,
    mode: SearchMode,
    adapter: NativeSearchAdapter<'a>,
}

impl NativeReadConsumer<Outcome> for HybridSearch<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Outcome, TreeError> {
        let unit = [PlanNodeId(0)];
        let search_input = [PlanNodeId(1)];
        let vector_list = [ExprId(5), ExprId(6)];
        let projection = [
            Projection {
                slot: SlotId(10),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(11),
                expression: ExprId(4),
            },
            Projection {
                slot: SlotId(12),
                expression: ExprId(7),
            },
            Projection {
                slot: SlotId(13),
                expression: ExprId(8),
            },
        ];
        let operators = vec![
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &unit,
                kind: OperatorKind::Search {
                    call: SearchCallId(0),
                    request: SearchRequest::Hybrid {
                        vector: ExprId(2),
                        text: ExprId(1),
                        k: ExprId(0),
                        mode: self.mode,
                        eligible: None,
                        options: Default::default(),
                    },
                    outputs: SearchOutputs {
                        node: Some(SlotId(0)),
                        score: Some(SlotId(1)),
                        vector_distance: Some(SlotId(2)),
                        lexical_score: Some(SlotId(3)),
                        ..SearchOutputs::default()
                    },
                },
            },
            Operator {
                inputs: &search_input,
                kind: OperatorKind::Project(&projection),
            },
        ];
        let expressions = vec![
            Expression::Literal(Literal::I64(self.k)),
            Expression::Literal(Literal::String(&self.query)),
            Expression::List(&vector_list),
            Expression::Slot(SlotId(0)),
            Expression::Slot(SlotId(1)),
            Expression::Literal(Literal::F64(f64::from(self.coords[0]))),
            Expression::Literal(Literal::F64(f64::from(self.coords[1]))),
            Expression::Slot(SlotId(2)),
            Expression::Slot(SlotId(3)),
        ];
        let eager = vec![PlanNodeId(1)];
        let regions = vec![
            RetainedRegion::slice(&unit).unwrap(),
            RetainedRegion::slice(&search_input).unwrap(),
            RetainedRegion::declared(self.query.as_ptr() as usize, self.query.capacity()).unwrap(),
            RetainedRegion::slice(&vector_list).unwrap(),
            RetainedRegion::slice(&projection).unwrap(),
        ];
        let owners = vec![
            RetainedAllocation::array(&unit).unwrap(),
            RetainedAllocation::array(&search_input).unwrap(),
            RetainedAllocation::string(&self.query).unwrap(),
            RetainedAllocation::array(&vector_list).unwrap(),
            RetainedAllocation::array(&projection).unwrap(),
        ];
        Ok(run_search_plan!(
            view,
            runtime,
            &operators,
            &expressions,
            &eager,
            regions,
            owners,
            &["node", "score", "vector_distance", "lexical_score"],
            &mut self.adapter,
        ))
    }
}

/// Two independent, distinctly typed calls joined with no predicate: the
/// real-adapter analogue of `search_tests::ze53_s1_independent_calls_keep_cartesian_bag_and_reports`.
struct Cartesian<'a> {
    adapter: NativeSearchAdapter<'a>,
}

impl NativeReadConsumer<Outcome> for Cartesian<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Outcome, TreeError> {
        let unit: [PlanNodeId; 1] = [PlanNodeId(0)];
        let second_unit = [PlanNodeId(2)];
        let join_inputs = [PlanNodeId(1), PlanNodeId(3)];
        let project_join = [PlanNodeId(4)];
        let coordinates = [ExprId(5), ExprId(6)];
        let projection = [
            Projection {
                slot: SlotId(10),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(11),
                expression: ExprId(4),
            },
        ];
        let query = String::from("amber");
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
                        score: Some(SlotId(1)),
                        ..SearchOutputs::default()
                    },
                },
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
                        mode: SearchMode::Exact,
                        eligible: None,
                        options: Default::default(),
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
                kind: OperatorKind::Project(&projection),
            },
        ];
        let expressions = vec![
            Expression::Literal(Literal::String(&query)),
            Expression::Literal(Literal::I64(2)),
            Expression::List(&coordinates),
            Expression::Slot(SlotId(0)),
            Expression::Slot(SlotId(2)),
            Expression::Literal(Literal::F64(0.0)),
            Expression::Literal(Literal::F64(0.0)),
        ];
        let eager = vec![PlanNodeId(1), PlanNodeId(3)];
        let regions = vec![
            RetainedRegion::declared(query.as_ptr() as usize, query.capacity()).unwrap(),
            RetainedRegion::slice(&unit).unwrap(),
            RetainedRegion::slice(&second_unit).unwrap(),
            RetainedRegion::slice(&join_inputs).unwrap(),
            RetainedRegion::slice(&project_join).unwrap(),
            RetainedRegion::slice(&coordinates).unwrap(),
            RetainedRegion::slice(&projection).unwrap(),
        ];
        let owners = vec![
            RetainedAllocation::string(&query).unwrap(),
            RetainedAllocation::array(&unit).unwrap(),
            RetainedAllocation::array(&second_unit).unwrap(),
            RetainedAllocation::array(&join_inputs).unwrap(),
            RetainedAllocation::array(&project_join).unwrap(),
            RetainedAllocation::array(&coordinates).unwrap(),
            RetainedAllocation::array(&projection).unwrap(),
        ];
        Ok(run_search_plan!(
            view,
            runtime,
            &operators,
            &expressions,
            &eager,
            regions,
            owners,
            &["text_node", "vector_node"],
            &mut self.adapter,
        ))
    }
}

fn run_vector(
    store: &Store,
    coords: [f32; 2],
    k: i64,
    mode: SearchMode,
    empty_eligible: bool,
) -> Outcome {
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            VectorSearch {
                coords,
                k,
                mode,
                empty_eligible,
                adapter: NativeSearchAdapter::new(&store.tokenizer),
            },
        )
        .expect("admit ze64 vector read")
}

fn run_text(store: &Store, query: &str, k: i64) -> Outcome {
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            TextSearch {
                query: query.to_owned(),
                k,
                adapter: NativeSearchAdapter::new(&store.tokenizer),
            },
        )
        .expect("admit ze64 text read")
}

fn run_hybrid(store: &Store, coords: [f32; 2], query: &str, k: i64, mode: SearchMode) -> Outcome {
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            HybridSearch {
                coords,
                query: query.to_owned(),
                k,
                mode,
                adapter: NativeSearchAdapter::new(&store.tokenizer),
            },
        )
        .expect("admit ze64 hybrid read")
}

#[test]
fn ze64_vector_only_call_ranks_by_real_squared_l2() {
    let (store, _directory, [a, b, _c, d]) = fixture();
    let result =
        run_vector(&store, [0.0, 0.0], 3, SearchMode::Exact, false).expect("vector-only result");
    assert_eq!(result.metadata().rows, 3);
    // d (origin, distance 0) < a (distance 2) < b (distance 50).
    assert_eq!(node_cell(&result, 0, 0), d);
    assert_eq!(f64_cell(&result, 0, 1), Some(0.0));
    assert_eq!(node_cell(&result, 1, 0), a);
    assert_eq!(f64_cell(&result, 1, 1), Some(2.0));
    assert_eq!(node_cell(&result, 2, 0), b);
    assert_eq!(f64_cell(&result, 2, 1), Some(50.0));
    let report = result.pools().reports[0];
    use crate::property_graph::query::runtime::WorkKind;
    assert_eq!(
        result
            .metadata()
            .counters
            .get(WorkKind::CandidateWindowPeak),
        3
    );
    assert_eq!(report.work.get(WorkKind::CandidateWindowPeak), 3);
    // Two query-validation coordinates plus three real two-coordinate scores.
    assert_eq!(report.work.get(WorkKind::VectorCoordinates), 8);
    assert_eq!(report.work.get(WorkKind::VectorBytes), 32);
    assert_eq!(report.kind, SearchKind::Vector);
    assert_eq!(report.actual_tier, Some(ActualTier::Exact));
    assert_eq!(report.precision, ScorePrecision::Original);
    assert_eq!(report.coverage, CandidateCoverage::Exact);
    assert_eq!(report.vector_leg, LegState::Nonempty);
    assert_eq!(report.lexical_leg, LegState::NotRequested);
}

#[test]
fn ze64_text_only_call_ranks_by_real_bm25() {
    let (store, _directory, [_a, _b, c, d]) = fixture();
    let result = run_text(&store, "amber", 4).expect("text-only result");
    let rows = result.metadata().rows;
    // Both "amber birch" (c) and "amber" (d) match; nothing else does.
    assert_eq!(rows, 2);
    let mut seen = std::collections::BTreeSet::new();
    let mut previous = f64::INFINITY;
    for row in 0..rows as usize {
        let node = node_cell(&result, row, 0);
        assert!(node == c || node == d, "unexpected lexical hit {node:?}");
        seen.insert(node);
        let score = f64_cell(&result, row, 1).expect("bm25 score");
        assert!(
            score > 0.0,
            "a matching hit must have a positive BM25 score"
        );
        assert!(score <= previous, "hits must be ordered by descending BM25");
        previous = score;
    }
    assert_eq!(
        seen.len(),
        2,
        "both lexical matches must appear exactly once"
    );
    let report = result.pools().reports[0];
    assert_eq!(report.kind, SearchKind::Lexical);
    assert_eq!(report.lexical_leg, LegState::Nonempty);
    assert_eq!(report.vector_leg, LegState::NotRequested);
    assert_eq!(report.requested_tier, None);
    assert_eq!(report.actual_tier, None);
}

#[test]
fn ze64_hybrid_call_cross_scores_the_node_with_both_components() {
    let (store, _directory, [_a, _b, _c, d]) = fixture();
    let result =
        run_hybrid(&store, [0.0, 0.0], "amber", 4, SearchMode::Exact).expect("hybrid result");
    assert!(result.metadata().rows >= 1);
    let mut found_d = false;
    for row in 0..result.metadata().rows as usize {
        if node_cell(&result, row, 0) == d {
            found_d = true;
            assert_eq!(
                f64_cell(&result, row, 2),
                Some(0.0),
                "d's exact vector distance"
            );
            let lexical = f64_cell(&result, row, 3).expect("d has an indexed lexical match");
            assert!(lexical > 0.0);
        }
    }
    assert!(
        found_d,
        "the node with both a vector and a lexical match must be ranked"
    );
    let report = result.pools().reports[0];
    assert_eq!(report.kind, SearchKind::Hybrid);
    assert_eq!(report.vector_leg, LegState::Nonempty);
    assert_eq!(report.lexical_leg, LegState::Nonempty);
    assert!(report.cross_score_complete);
    assert_eq!(report.cross_scored_count, report.candidate_count);
    assert_ne!(report.normalization_version, 0);
    assert_ne!(report.rules_version, 0);
}

#[test]
fn ze64_two_distinct_search_calls_in_one_statement_keep_separate_reports() {
    let (store, _directory, [a, b, c, d]) = fixture();
    let result = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            Cartesian {
                adapter: NativeSearchAdapter::new(&store.tokenizer),
            },
        )
        .expect("admit ze64 cartesian read")
        .expect("ze64 cartesian search result");
    // Text("amber") hits {c, d} (both match, k=2 covers both); Vector
    // ([0,0], Exact, k=2) keeps only its real top 2 by distance: d (0) and
    // a (2), never b (50). 2 * 2 rows.
    assert_eq!(result.metadata().rows, 4);
    let reports = &result.pools().reports;
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].call.0, 0);
    assert_eq!(reports[0].kind, SearchKind::Lexical);
    assert_eq!(reports[1].call.0, 1);
    assert_eq!(reports[1].kind, SearchKind::Vector);
    assert_eq!(reports[0].generation, reports[1].generation);
    let mut text_nodes = std::collections::BTreeSet::new();
    let mut vector_nodes = std::collections::BTreeSet::new();
    for row in 0..result.metadata().rows as usize {
        text_nodes.insert(node_cell(&result, row, 0));
        vector_nodes.insert(node_cell(&result, row, 1));
    }
    assert_eq!(text_nodes, std::collections::BTreeSet::from([c, d]));
    assert_eq!(vector_nodes, std::collections::BTreeSet::from([a, d]));
    assert!(
        !vector_nodes.contains(&b),
        "b (distance 50) must not survive real top-2 ranking"
    );
}

#[test]
fn ze64_empty_eligible_vector_call_differs_from_absent_restriction_with_the_real_adapter() {
    let (store, _directory, _nodes) = fixture();
    let all = run_vector(&store, [0.0, 0.0], 3, SearchMode::Exact, false)
        .expect("absent-restriction result");
    assert_eq!(all.metadata().rows, 3);

    let none = run_vector(&store, [0.0, 0.0], 3, SearchMode::Exact, true)
        .expect("explicit-empty-eligibility result");
    assert_eq!(none.metadata().rows, 0);
    let report = none.pools().reports[0];
    assert_eq!(report.kind, SearchKind::Vector);
    // ZE-197, exercised end to end with the real adapter and the real
    // (unmodified, closed) `rank_vector`: membership alone proves nothing
    // is eligible, so `actual_tier` is `None`, and the relaxed validator
    // must still accept this real shape.
    assert_eq!(report.actual_tier, None);
    assert_eq!(report.vector_leg, LegState::NoEligibleMembers);
    assert_eq!(report.coverage, CandidateCoverage::Exact);
}

#[test]
fn ze202_same_low64_search_and_eligibility_keep_selected_id() {
    use super::entry_probe::{Backing, control, options, run_plan};
    use super::test_support::ze202::{A, B};
    use crate::property_graph::query::plan::{AggregateExpression, BinaryExpression};
    let (store, _dir) = super::test_support::ze202::fixture(Some(tower()));
    for kind in [SearchKind::Vector, SearchKind::Lexical, SearchKind::Hybrid] {
        for restriction in [None, Some(vec![0]), Some(vec![1]), Some(vec![1, 0, 1])] {
            let mut adapter = NativeSearchAdapter::new(&store.tokenizer);
            let result = store
                .execute_graph_query(
                    &control(),
                    &options(16),
                    Some(&mut adapter),
                    |runtime, executor| {
                        let inputs: Vec<Vec<_>> = (0..5).map(|i| vec![PlanNodeId(i)]).collect();
                        let vector = vec![ExprId(4), ExprId(5)];
                        let eligible_list: Vec<_> = restriction
                            .as_ref()
                            .map(|slots| slots.iter().map(|slot| ExprId(*slot)).collect())
                            .unwrap_or_default();
                        let text = String::from("amber");
                        let mut expressions = vec![
                            Expression::Binary {
                                operation: BinaryExpression::Index,
                                left: ExprId(9),
                                right: ExprId(11),
                            },
                            Expression::Binary {
                                operation: BinaryExpression::Index,
                                left: ExprId(10),
                                right: ExprId(11),
                            },
                            Expression::Literal(Literal::I64(2)),
                        ];
                        let eligible = restriction.as_ref().map(|_| ExprId(3));
                        if eligible.is_some() {
                            expressions.push(Expression::List(&eligible_list));
                        } else {
                            expressions.push(Expression::Literal(Literal::F64(0.0)));
                        }
                        expressions.push(Expression::Literal(Literal::F64(0.0)));
                        expressions.push(Expression::Literal(Literal::F64(0.0)));
                        expressions.push(Expression::List(&vector));
                        expressions.push(Expression::Literal(Literal::String(&text)));
                        expressions.push(Expression::Slot(SlotId(2)));
                        expressions.extend([
                            Expression::Slot(SlotId(3)),
                            Expression::Slot(SlotId(4)),
                            Expression::Literal(Literal::I64(0)),
                            Expression::Slot(SlotId(0)),
                            Expression::Slot(SlotId(1)),
                            Expression::Aggregate {
                                operation: AggregateExpression::Collect { distinct: false },
                                operand: Some(ExprId(12)),
                            },
                            Expression::Aggregate {
                                operation: AggregateExpression::Collect { distinct: false },
                                operand: Some(ExprId(13)),
                            },
                        ]);
                        let aggregates = vec![
                            Projection {
                                slot: SlotId(3),
                                expression: ExprId(14),
                            },
                            Projection {
                                slot: SlotId(4),
                                expression: ExprId(15),
                            },
                        ];
                        let request = match kind {
                            SearchKind::Vector => SearchRequest::Vector {
                                vector: ExprId(6),
                                k: ExprId(2),
                                mode: SearchMode::Exact,
                                eligible,
                                options: Default::default(),
                            },
                            SearchKind::Lexical => SearchRequest::Text {
                                query: ExprId(7),
                                k: ExprId(2),
                                eligible,
                                options: Default::default(),
                            },
                            SearchKind::Hybrid => SearchRequest::Hybrid {
                                vector: ExprId(6),
                                text: ExprId(7),
                                k: ExprId(2),
                                mode: SearchMode::Exact,
                                eligible,
                                options: Default::default(),
                            },
                        };
                        // Project the common inputs too: every declared expression
                        // must be reachable, regardless of the requested search mode.
                        let projections: Vec<_> = (0..9)
                            .map(|i| Projection {
                                slot: SlotId(10 + i),
                                expression: ExprId(i),
                            })
                            .collect();
                        let operators = vec![
                            Operator {
                                inputs: &[],
                                kind: OperatorKind::Unit,
                            },
                            Operator {
                                inputs: &inputs[0],
                                kind: OperatorKind::LookupNode {
                                    output: SlotId(0),
                                    id: NodeId::new(A).unwrap(),
                                },
                            },
                            Operator {
                                inputs: &inputs[1],
                                kind: OperatorKind::LookupNode {
                                    output: SlotId(1),
                                    id: NodeId::new(B).unwrap(),
                                },
                            },
                            Operator {
                                inputs: &inputs[2],
                                kind: OperatorKind::Aggregate {
                                    keys: &[],
                                    aggregates: &aggregates,
                                },
                            },
                            Operator {
                                inputs: &inputs[3],
                                kind: OperatorKind::Search {
                                    call: SearchCallId(0),
                                    request,
                                    outputs: SearchOutputs {
                                        node: Some(SlotId(2)),
                                        ..SearchOutputs::default()
                                    },
                                },
                            },
                            Operator {
                                inputs: &inputs[4],
                                kind: OperatorKind::Project(&projections),
                            },
                        ];
                        let eager = vec![PlanNodeId(4)];
                        let mut backing = Backing::default();
                        for input in &inputs {
                            backing.vec(input)?;
                        }
                        backing.vec(&vector)?;
                        backing.vec(&eligible_list)?;
                        backing.vec(&projections)?;
                        backing.vec(&aggregates)?;
                        backing.string(&text)?;
                        run_plan(
                            runtime,
                            executor,
                            &operators,
                            &expressions,
                            &eager,
                            &backing,
                            &["a", "b", "k", "eligible", "x", "y", "vector", "text", "hit"],
                        )
                    },
                )
                .unwrap();
            let expected = match restriction.as_deref() {
                Some([0]) => vec![A],
                Some([1]) => vec![B],
                _ => vec![A, B],
            };
            let mut hits: Vec<_> = (0..result.metadata().rows as usize)
                .map(|row| node_cell(&result, row, 8).get())
                .collect();
            hits.sort();
            assert_eq!(hits, expected);
            assert_eq!(result.pools().reports.len(), 1);
            let report = result.pools().reports[0];
            assert_eq!(report.call, SearchCallId(0));
            assert_eq!(report.generation.get(), 6);
            assert_eq!(report.kind, kind);
            assert_eq!(report.candidate_count, expected.len() as u64);
            assert_eq!(
                report.cross_scored_count,
                if kind == SearchKind::Hybrid {
                    expected.len() as u64
                } else {
                    0
                }
            );
        }
    }
    store.close().unwrap();
}

struct MeasuredSearch<'a>(NativeSearchAdapter<'a>);
impl<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g> for MeasuredSearch<'_> {
    fn search<'s>(
        &mut self,
        view: &'s GraphReadView<'s, 'v, 'm, 'g>,
        invocation: &crate::property_graph::query::pattern::SearchInvocation<'_, '_, 'v, 'm, 'g>,
        hits: &mut QueryArena<'m, 'g, crate::property_graph::query::pattern::SearchHit>,
        runtime: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<super::super::SearchReport, NativeExecutionError> {
        let before = runtime.counters();
        let report = self.0.search(view, invocation, hits, runtime)?;
        assert_eq!(
            report.work,
            runtime.counters().since(before),
            "complete invocation work includes preparation"
        );
        Ok(report)
    }
}
#[test]
fn ze64_call_work_includes_preparation() {
    let (store, _directory, _) = fixture();
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            TextSearch {
                query: "amber".into(),
                k: 1,
                adapter: MeasuredSearch(NativeSearchAdapter::new(&store.tokenizer)),
            },
        )
        .unwrap()
        .unwrap();
}

#[test]
fn ze64_seed_expand_and_copy_share_admitted_generation() {
    super::search_probe::same_view(64);
}
#[test]
fn ze64_approximation_report_survives_projection_and_aggregation() {
    super::search_probe::approximation(64);
}
