#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use super::super::relational::test_support::execute_relational_plan;
use super::super::{NativePattern, PatternCapacity};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::QueryValue;
use crate::property_graph::query::expression::ExpressionCapacity;
use crate::property_graph::query::plan::{
    ExprId, Expression, Literal, Mutation, NodeFacts, Operator, OperatorKind, PlanBacking,
    PlanDescription, PlanError, PlanFootprint, PlanNodeId, Projection, RetainedRegion, SlotId,
    VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{
    ArenaCapacity, Completion, Execution, ExecutionCapacity, FrozenOutput, NativeExecutionError,
    PreparedRows, PullOperator, PullState, RowBatch, RuntimeContext, RuntimeError, RuntimeFailure,
    RuntimeLimits, WorkKind, execute_in,
};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphProperty,
    GraphRevision, NodeId, PropertyData, PropertyValue,
};
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod d2;

const PATTERN_ROWS: usize = 16;

fn pattern_capacity(rows: usize) -> PatternCapacity {
    PatternCapacity {
        rows: StorageCapacity {
            rows,
            max_rows: rows,
            payload_bytes: 8192,
            variable: ArenaCapacity {
                string_bytes: 4096,
                list_cells: 128,
                node_ids: 64,
                relationship_ids: 64,
            },
        },
        expression: ExpressionCapacity {
            cells: 32,
            string_bytes: 4096,
        },
    }
}

fn execution_capacity(result_rows: usize) -> ExecutionCapacity {
    ExecutionCapacity {
        batch_rows: 1,
        result_rows,
        batch_payload_bytes: 8192,
        result_payload_bytes: 8192,
        batch: ArenaCapacity {
            string_bytes: 4096,
            list_cells: 128,
            node_ids: 64,
            relationship_ids: 64,
        },
        result: ArenaCapacity {
            string_bytes: 4096,
            list_cells: 128,
            node_ids: 64,
            relationship_ids: 64,
        },
    }
}

#[derive(Debug)]
enum EagerExecutionFailure {
    Build(NativeExecutionError),
    Run(Box<RuntimeFailure<NativeExecutionError>>),
}

impl std::fmt::Display for EagerExecutionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(error) => write!(formatter, "build: {error}"),
            Self::Run(error) => write!(formatter, "run: {error}"),
        }
    }
}

/// One execution plus whether every query reservation returned to its
/// pre-execution value once the pattern and its plan owners had dropped.
struct ReleasedExecution<T> {
    result: Result<Execution<T>, EagerExecutionFailure>,
    released: bool,
}

/// A cancelled control is also observed by the store after the consumer
/// returns, so `with_native_read` reports the cancellation instead of the
/// consumer's value. The consumer therefore records what it actually saw.
#[derive(Default)]
struct CancelReport {
    observed: bool,
    cancelled: bool,
    released: bool,
    completed_rows: usize,
}

type NodeValueRows = ([Option<(u128, i64)>; PATTERN_ROWS], usize);
type NodePairRows = ([Option<(u128, u128)>; PATTERN_ROWS], usize);

struct FreezeNodeValueRows;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeNodeValueRows {
    type Output = NodeValueRows;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > PATTERN_ROWS || rows.columns() != 2 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; PATTERN_ROWS];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            let node = match rows.value(row, 0) {
                Some(QueryValue::NodeRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let property = match rows.value(row, 1) {
                Some(QueryValue::I64(value)) => value,
                _ => return Err(RuntimeError::Batch.into()),
            };
            *destination = Some((node.get(), property));
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<(u128, i64)>(),
            0,
        )
        .map_err(Into::into)
    }
}

struct FreezeNodePairRows;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeNodePairRows {
    type Output = NodePairRows;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > PATTERN_ROWS || rows.columns() != 2 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; PATTERN_ROWS];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            let left = match rows.value(row, 0) {
                Some(QueryValue::NodeRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let right = match rows.value(row, 1) {
                Some(QueryValue::NodeRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            *destination = Some((left.get(), right.get()));
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<(u128, u128)>(),
            0,
        )
        .map_err(Into::into)
    }
}

/// Records the cumulative `RowsIn` counter at the first pull that produced a
/// row, so a barrier that drains its whole input first is distinguishable from
/// a streaming pipeline that emits after one input row.
struct RecordFirstEmission<O> {
    source: O,
    rows_in: Arc<AtomicU64>,
    recorded: bool,
}

impl<'v, 'm, 'g, O> PullOperator<'v, 'm, 'g, NativeExecutionError> for RecordFirstEmission<O>
where
    O: PullOperator<'v, 'm, 'g, NativeExecutionError>,
{
    fn node(&self) -> PlanNodeId {
        self.source.node()
    }

    fn prepare_search(
        &mut self,
        node: PlanNodeId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        self.source.prepare_search(node, context)
    }

    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeExecutionError> {
        let state = self.source.pull(context, output)?;
        if !self.recorded && output.rows() != 0 {
            self.rows_in
                .store(context.counters().get(WorkKind::RowsIn), Ordering::SeqCst);
            self.recorded = true;
        }
        Ok(state)
    }
}

/// Cancels the shared token once the named number of rows has been emitted,
/// before delegating the next pull. `after_rows == 0` cancels inside the very
/// first pull, which for an eager plan is inside its drain loop.
struct CancelAfterRows<O> {
    source: O,
    token: CancelToken,
    after_rows: usize,
    seen: usize,
}

impl<'v, 'm, 'g, O> PullOperator<'v, 'm, 'g, NativeExecutionError> for CancelAfterRows<O>
where
    O: PullOperator<'v, 'm, 'g, NativeExecutionError>,
{
    fn node(&self) -> PlanNodeId {
        self.source.node()
    }

    fn prepare_search(
        &mut self,
        node: PlanNodeId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        self.source.prepare_search(node, context)
    }

    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeExecutionError> {
        if self.seen >= self.after_rows {
            self.token.cancel();
        }
        let state = self.source.pull(context, output)?;
        self.seen += output.rows();
        Ok(state)
    }
}

/// Three nodes carrying `p` values 1, 2 and 3, with identities taken from the
/// publication receipts rather than from any later query.
struct EagerFixture {
    _directory: tempfile::TempDir,
    store: Store,
    nodes: Vec<(u128, i64)>,
}

fn eager_fixture(name: &str) -> EagerFixture {
    let directory = tempfile::tempdir().expect("native eager store directory");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native eager store");
    let receipts = crate::property_graph::with_local_refs(|_| {
        let mut first = [GraphProperty::new(
            GraphName::new("p").unwrap(),
            PropertyValue::new(PropertyData::I64(1)).unwrap(),
        )];
        let mut second = [GraphProperty::new(
            GraphName::new("p").unwrap(),
            PropertyValue::new(PropertyData::I64(2)).unwrap(),
        )];
        let mut third = [GraphProperty::new(
            GraphName::new("p").unwrap(),
            PropertyValue::new(PropertyData::I64(3)).unwrap(),
        )];
        let node_one = CanonicalContents::node(&mut [], &mut first, None, None).unwrap();
        let node_two = CanonicalContents::node(&mut [], &mut second, None, None).unwrap();
        let node_three = CanonicalContents::node(&mut [], &mut third, None, None).unwrap();
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, name, "one").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_one)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, name, "two").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_two)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, name, "three").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_three)),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish native eager fixture")
    });
    let nodes = receipts
        .iter()
        .zip([1i64, 2, 3])
        .map(|(receipt, value)| match receipt.entity {
            EntityId::Node(id) => (id.get(), value),
            _ => panic!("eager fixture receipt kind"),
        })
        .collect();
    EagerFixture {
        _directory: directory,
        store,
        nodes,
    }
}

fn first_node(fixture: &EagerFixture) -> NodeId {
    let raw = fixture.nodes.first().expect("first eager fixture node").0;
    NodeId::new(raw).expect("first eager fixture node identity")
}

fn sorted_value_rows(rows: &NodeValueRows) -> Vec<(u128, i64)> {
    let mut observed: Vec<(u128, i64)> = rows.0.iter().take(rows.1).flatten().copied().collect();
    observed.sort_unstable();
    observed
}

fn sorted_pair_rows(rows: &NodePairRows) -> Vec<(u128, u128)> {
    let mut observed: Vec<(u128, u128)> = rows.0.iter().take(rows.1).flatten().copied().collect();
    observed.sort_unstable();
    observed
}

/// `Unit -> ScanNodes -> [Eager] -> Project(n, n.p) -> Collect`.
struct ScanProjectConsumer {
    eager: bool,
    pattern_rows: usize,
    rows_in: Option<Arc<AtomicU64>>,
    cancel: Option<(CancelToken, usize)>,
    report: Option<Arc<Mutex<CancelReport>>>,
}

impl NativeReadConsumer<ReleasedExecution<NodeValueRows>> for ScanProjectConsumer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        ReleasedExecution<NodeValueRows>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let scan_inputs = [PlanNodeId(0)];
        let barrier_inputs = [PlanNodeId(1)];
        let project_inputs = [PlanNodeId(2)];
        let collect_inputs = [PlanNodeId(3)];
        let projections = [
            Projection {
                slot: SlotId(100),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(101),
                expression: ExprId(1),
            },
        ];
        let property_name = String::from("p");
        let mut expressions: Vec<Expression<'_>> = vec![
            Expression::Slot(SlotId(0)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&property_name).unwrap(),
            },
        ];
        // The control replaces the barrier with an always-true Filter, so the
        // two plans differ only in that one operator's semantics.
        let barrier = if self.eager {
            OperatorKind::Eager
        } else {
            expressions.push(Expression::Literal(Literal::Bool(true)));
            OperatorKind::Filter(ExprId(2))
        };
        let operators: Vec<Operator<'_>> = vec![
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &scan_inputs,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: None,
                },
            },
            Operator {
                inputs: &barrier_inputs,
                kind: barrier,
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        let rows_in = self.rows_in.clone();
        let cancel = self.cancel.clone();
        // The statement keeps its page memo after this operator is dropped.
        let baseline = runtime.memory().reserved_bytes() - view.retained_validation_bytes();
        let result = execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_inputs).unwrap(),
                RetainedRegion::slice(&barrier_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
                RetainedRegion::declared(
                    property_name.as_ptr() as usize,
                    property_name.capacity(),
                )
                .unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_inputs).unwrap(),
                RetainedAllocation::array(&barrier_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
                RetainedAllocation::string(&property_name).unwrap(),
            ],
            &mut FreezeNodeValueRows,
            pattern_capacity(self.pattern_rows),
            execution_capacity(PATTERN_ROWS),
            EagerExecutionFailure::Build,
            |error| EagerExecutionFailure::Run(Box::new(error)),
            move |source| {
                let source = RecordFirstEmission {
                    source,
                    rows_in: rows_in.unwrap_or_else(|| Arc::new(AtomicU64::new(0))),
                    recorded: false,
                };
                let (token, after_rows) =
                    cancel.unwrap_or_else(|| (CancelToken::new(), usize::MAX));
                CancelAfterRows {
                    source,
                    token,
                    after_rows,
                    seen: 0,
                }
            },
            vector
        );
        let released =
            runtime.memory().reserved_bytes() - view.retained_validation_bytes() == baseline;
        if let Some(report) = &self.report
            && let Ok(mut report) = report.lock()
        {
            report.observed = true;
            report.released = released;
            match &result {
                Ok(execution) => report.completed_rows = execution.output.1,
                Err(EagerExecutionFailure::Run(failure)) => {
                    report.cancelled = matches!(
                        failure.error,
                        NativeExecutionError::Runtime(RuntimeError::Value(
                            crate::property_graph::query::QueryError::Cancelled
                        ))
                    );
                }
                Err(EagerExecutionFailure::Build(_)) => {}
            }
        }
        Ok(ReleasedExecution { result, released })
    }
}

/// `Join(left, Unit -> ScanNodes -> Eager)` over disjoint scopes.
struct JoinEagerConsumer {
    left_scan: bool,
    source: NodeId,
}

impl NativeReadConsumer<ReleasedExecution<NodePairRows>> for JoinEagerConsumer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        ReleasedExecution<NodePairRows>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let left_inputs = [PlanNodeId(0)];
        let right_scan_inputs = [PlanNodeId(2)];
        let eager_inputs = [PlanNodeId(3)];
        let join_inputs = [PlanNodeId(1), PlanNodeId(4)];
        let project_inputs = [PlanNodeId(5)];
        let collect_inputs = [PlanNodeId(6)];
        let projections = [
            Projection {
                slot: SlotId(200),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(201),
                expression: ExprId(1),
            },
        ];
        let expressions: Vec<Expression<'_>> =
            vec![Expression::Slot(SlotId(0)), Expression::Slot(SlotId(1))];
        let left = if self.left_scan {
            OperatorKind::ScanNodes {
                output: SlotId(0),
                label: None,
            }
        } else {
            OperatorKind::LookupNode {
                output: SlotId(0),
                id: self.source,
            }
        };
        let operators: Vec<Operator<'_>> = vec![
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &left_inputs,
                kind: left,
            },
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &right_scan_inputs,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(1),
                    label: None,
                },
            },
            Operator {
                inputs: &eager_inputs,
                kind: OperatorKind::Eager,
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join { predicate: None },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        // The statement keeps its page memo after this operator is dropped.
        let baseline = runtime.memory().reserved_bytes() - view.retained_validation_bytes();
        let result = execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&left_inputs).unwrap(),
                RetainedRegion::slice(&right_scan_inputs).unwrap(),
                RetainedRegion::slice(&eager_inputs).unwrap(),
                RetainedRegion::slice(&join_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&left_inputs).unwrap(),
                RetainedAllocation::array(&right_scan_inputs).unwrap(),
                RetainedAllocation::array(&eager_inputs).unwrap(),
                RetainedAllocation::array(&join_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
            ],
            &mut FreezeNodePairRows,
            pattern_capacity(PATTERN_ROWS),
            execution_capacity(PATTERN_ROWS),
            EagerExecutionFailure::Build,
            |error| EagerExecutionFailure::Run(Box::new(error)),
            |source| source,
            vector
        );
        let released =
            runtime.memory().reserved_bytes() - view.retained_validation_bytes() == baseline;
        Ok(ReleasedExecution { result, released })
    }
}

/// `Unit -> Eager -> Mutate([CreateNode])`: the mutation executor is a later
/// slice, so building this occurrence tree must still be refused.
struct EagerMutateConsumer;

impl NativeReadConsumer<ReleasedExecution<NodeValueRows>> for EagerMutateConsumer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        ReleasedExecution<NodeValueRows>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let eager_inputs = [PlanNodeId(0)];
        let mutate_inputs = [PlanNodeId(1)];
        let property_name = String::from("p");
        let mutations = [
            Mutation::CreateNode {
                output: SlotId(0),
                labels: &[],
            },
            Mutation::SetProperty {
                entity: ExprId(0),
                name: GraphName::new(&property_name).unwrap(),
                value: ExprId(1),
            },
        ];
        let expressions: Vec<Expression<'_>> = vec![
            Expression::Slot(SlotId(0)),
            Expression::Literal(Literal::I64(1)),
        ];
        let operators = vec![
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &eager_inputs,
                kind: OperatorKind::Eager,
            },
            Operator {
                inputs: &mutate_inputs,
                kind: OperatorKind::Mutate(&mutations),
            },
        ];
        // The statement keeps its page memo after this operator is dropped.
        let baseline = runtime.memory().reserved_bytes() - view.retained_validation_bytes();
        let result = execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&eager_inputs).unwrap(),
                RetainedRegion::slice(&mutate_inputs).unwrap(),
                RetainedRegion::slice(&mutations).unwrap(),
                RetainedRegion::declared(
                    property_name.as_ptr() as usize,
                    property_name.capacity(),
                )
                .unwrap(),
            ],
            vec![
                RetainedAllocation::array(&eager_inputs).unwrap(),
                RetainedAllocation::array(&mutate_inputs).unwrap(),
                RetainedAllocation::array(&mutations).unwrap(),
                RetainedAllocation::string(&property_name).unwrap(),
            ],
            &mut FreezeNodeValueRows,
            pattern_capacity(PATTERN_ROWS),
            execution_capacity(PATTERN_ROWS),
            EagerExecutionFailure::Build,
            |error| EagerExecutionFailure::Run(Box::new(error)),
            |source| source,
            vector
        );
        let released =
            runtime.memory().reserved_bytes() - view.retained_validation_bytes() == baseline;
        Ok(ReleasedExecution { result, released })
    }
}

/// The control token carried by the admitted read is the same token the
/// observing `PullOperator` cancels, so a cancellation is actually observed.
fn run_scan_project(
    store: &Store,
    consumer: ScanProjectConsumer,
) -> ReleasedExecution<NodeValueRows> {
    let control = QueryControl::Cancel(
        consumer
            .cancel
            .as_ref()
            .map_or_else(CancelToken::new, |(token, _)| token.clone()),
    );
    store
        .with_native_read(
            &control,
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            consumer,
        )
        .expect("admit native eager view")
}

#[test]
fn native_eager_bag_matches_unbarriered_plan() {
    let fixture = eager_fixture("eager-bag");
    let barriered = run_scan_project(
        &fixture.store,
        ScanProjectConsumer {
            eager: true,
            pattern_rows: PATTERN_ROWS,
            rows_in: None,
            cancel: None,
            report: None,
        },
    );
    let direct = run_scan_project(
        &fixture.store,
        ScanProjectConsumer {
            eager: false,
            pattern_rows: PATTERN_ROWS,
            rows_in: None,
            cancel: None,
            report: None,
        },
    );
    let mutate = fixture
        .store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            EagerMutateConsumer,
        )
        .expect("admit native eager mutate view");
    fixture.store.close().expect("close native eager store");

    let barriered = barriered
        .result
        .unwrap_or_else(|error| panic!("eager plan must execute: {error}"));
    let direct = direct
        .result
        .unwrap_or_else(|error| panic!("unbarriered plan must execute: {error}"));
    let mut expected = fixture.nodes.clone();
    expected.sort_unstable();
    assert_eq!(expected.len(), 3);
    assert_eq!(barriered.output.1, 3);
    assert_eq!(direct.output.1, 3);
    assert_eq!(sorted_value_rows(&barriered.output), expected);
    assert_eq!(sorted_value_rows(&direct.output), expected);

    match mutate.result {
        Err(EagerExecutionFailure::Build(NativeExecutionError::Plan(PlanError::Reference))) => {}
        Err(other) => panic!("Mutate must still be refused by reference: {other}"),
        Ok(_) => panic!("Mutate must not build a native occurrence tree"),
    }
}

#[test]
fn native_eager_drains_input_before_first_emission() {
    let fixture = eager_fixture("eager-drain");
    let barriered_rows_in = Arc::new(AtomicU64::new(0));
    let direct_rows_in = Arc::new(AtomicU64::new(0));
    let barriered = run_scan_project(
        &fixture.store,
        ScanProjectConsumer {
            eager: true,
            pattern_rows: PATTERN_ROWS,
            rows_in: Some(Arc::clone(&barriered_rows_in)),
            cancel: None,
            report: None,
        },
    );
    let direct = run_scan_project(
        &fixture.store,
        ScanProjectConsumer {
            eager: false,
            pattern_rows: PATTERN_ROWS,
            rows_in: Some(Arc::clone(&direct_rows_in)),
            cancel: None,
            report: None,
        },
    );
    fixture.store.close().expect("close native eager store");

    let barriered = barriered
        .result
        .unwrap_or_else(|error| panic!("eager plan must execute: {error}"));
    let direct = direct
        .result
        .unwrap_or_else(|error| panic!("unbarriered plan must execute: {error}"));
    assert_eq!(barriered.output.1, 3);
    assert_eq!(direct.output.1, 3);
    assert!(
        barriered_rows_in.load(Ordering::SeqCst) >= 3,
        "Eager must drain all three input rows before its first emission, saw {}",
        barriered_rows_in.load(Ordering::SeqCst)
    );
    assert!(
        direct_rows_in.load(Ordering::SeqCst) < 3,
        "the unbarriered control must emit before draining, saw {}",
        direct_rows_in.load(Ordering::SeqCst)
    );
}

#[test]
fn native_eager_resets_under_nested_loop_join() {
    let fixture = eager_fixture("eager-join");
    let source = first_node(&fixture);
    let single = fixture
        .store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            JoinEagerConsumer {
                left_scan: false,
                source,
            },
        )
        .expect("admit single-left eager join view");
    let repeated = fixture
        .store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            JoinEagerConsumer {
                left_scan: true,
                source,
            },
        )
        .expect("admit repeated-left eager join view");
    fixture.store.close().expect("close native eager store");

    let single = single
        .result
        .unwrap_or_else(|error| panic!("single-left eager join must execute: {error}"));
    let repeated = repeated
        .result
        .unwrap_or_else(|error| panic!("repeated-left eager join must execute: {error}"));
    assert_eq!(single.output.1, 3);
    assert_eq!(repeated.output.1, 9);
    let right: Vec<u128> = fixture.nodes.iter().map(|(id, _)| *id).collect();
    let mut expected_single: Vec<(u128, u128)> =
        right.iter().map(|target| (source.get(), *target)).collect();
    expected_single.sort_unstable();
    assert_eq!(sorted_pair_rows(&single.output), expected_single);
    let mut expected_repeated: Vec<(u128, u128)> = right
        .iter()
        .flat_map(|left| right.iter().map(move |target| (*left, *target)))
        .collect();
    expected_repeated.sort_unstable();
    assert_eq!(sorted_pair_rows(&repeated.output), expected_repeated);
}

#[test]
fn native_eager_capacity_limit_rejects_without_partial_rows() {
    let fixture = eager_fixture("eager-capacity");
    let barriered = run_scan_project(
        &fixture.store,
        ScanProjectConsumer {
            eager: true,
            pattern_rows: 2,
            rows_in: None,
            cancel: None,
            report: None,
        },
    );
    let direct = run_scan_project(
        &fixture.store,
        ScanProjectConsumer {
            eager: false,
            pattern_rows: 2,
            rows_in: None,
            cancel: None,
            report: None,
        },
    );
    fixture.store.close().expect("close native eager store");

    assert!(
        barriered.released,
        "a rejected eager bag must release every operator reservation"
    );
    match barriered.result {
        Err(EagerExecutionFailure::Run(failure)) => match failure.error {
            NativeExecutionError::Runtime(RuntimeError::BatchCapacity) => {}
            other => panic!("eager row capacity must reject as BatchCapacity: {other:?}"),
        },
        Err(other) => panic!("a two-row eager bag must reject as BatchCapacity: {other}"),
        Ok(_) => panic!("a two-row eager bag must reject three input rows"),
    }
    let direct = direct
        .result
        .unwrap_or_else(|error| panic!("the streaming control must still execute: {error}"));
    assert_eq!(direct.output.1, 3);
}

#[test]
fn native_eager_cancel_during_drain_releases() {
    let fixture = eager_fixture("eager-cancel");
    let mut reports = Vec::new();
    // `after_rows == 0` cancels at the top of the first pull, so the drain loop
    // itself observes it; `after_rows == 1` cancels once the bag has been
    // frozen and one row emitted, proving the frozen bag is not handed back.
    for (after_rows, label) in [(0usize, "during drain"), (1usize, "after freeze")] {
        let token = CancelToken::new();
        let report = Arc::new(Mutex::new(CancelReport::default()));
        let outcome = fixture.store.with_native_read(
            &QueryControl::Cancel(token.clone()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ScanProjectConsumer {
                eager: true,
                pattern_rows: PATTERN_ROWS,
                rows_in: None,
                cancel: Some((token, after_rows)),
                report: Some(Arc::clone(&report)),
            },
        );
        assert!(
            outcome.is_err(),
            "{label}: the store must surface the cancelled control"
        );
        reports.push((label, report));
    }
    fixture.store.close().expect("close native eager store");

    for (label, report) in reports {
        let report = report.lock().expect("cancel report");
        assert!(report.observed, "{label}: the consumer never ran");
        assert!(
            report.cancelled,
            "{label}: expected a typed cancellation from the eager occurrence"
        );
        assert!(
            report.released,
            "{label}: cancellation must release every operator reservation"
        );
        assert_eq!(
            report.completed_rows, 0,
            "{label}: cancellation must return no rows at all"
        );
    }
}
