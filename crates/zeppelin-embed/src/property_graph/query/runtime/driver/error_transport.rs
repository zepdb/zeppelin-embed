#![allow(clippy::expect_used, clippy::panic)]

use super::*;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use crate::property_graph::query::QueryView;
use crate::property_graph::query::expression::{ExpressionError, ExpressionFailure};
use crate::property_graph::query::plan::{
    ExprId, GraphPlan, NodeFacts, Operator, OperatorKind, PlanBacking, PlanDescription,
    PlanFootprint, RetainedRegion,
};
use crate::property_graph::query::relational::{
    BlockingOperation, BlockingRows, MapRows, RowOperator, Schema, SlotProjection, StorageCapacity,
};
use crate::property_graph::query::resources::{
    QueryInputs, RetainedAllocation, RetentionInventory, RuntimePlan,
};
use crate::property_graph::query::runtime::NativeExecutionError;
use crate::property_graph::query::{QueryError, QueryValue};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::storage::tree::directory::TreeError;
use crate::property_graph::{GraphGeneration, StoreInstanceId};
use std::cell::Cell;

const EXPECTED_EXPRESSION: ExprId = ExprId(37);
const EXPECTED_TREE_DETAIL: &str = "ze-149 retained native tree detail";

#[derive(Debug, Eq, PartialEq)]
enum ObservedDiagnostic {
    ExpressionTree {
        expression: ExprId,
        detail: &'static str,
    },
    Lost,
}

fn expected_expression_failure() -> ExpressionError {
    ExpressionError {
        expression: EXPECTED_EXPRESSION,
        failure: ExpressionFailure::Tree(TreeError::Invalid(EXPECTED_TREE_DETAIL)),
    }
}

fn observe_native_failure(error: NativeExecutionError) -> ObservedDiagnostic {
    match error {
        NativeExecutionError::Expression(ExpressionError {
            expression,
            failure: ExpressionFailure::Tree(TreeError::Invalid(detail)),
        }) => ObservedDiagnostic::ExpressionTree { expression, detail },
        _ => ObservedDiagnostic::Lost,
    }
}

struct TestView {
    token: QueryView,
    lease: SnapshotLease,
}

impl RetainedView for TestView {
    fn query_view(&self) -> &QueryView {
        &self.token
    }

    fn check_active(&self) -> Result<(), crate::property_graph::query::QueryError> {
        self.lease
            .check_active()
            .map_err(|_| crate::property_graph::query::QueryError::ReadCancelled)
    }
}

struct TypedDiagnosticSource {
    expected: Option<ExpressionError>,
    emitted: bool,
}

impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g, NativeExecutionError> for TypedDiagnosticSource {
    fn node(&self) -> crate::property_graph::query::plan::PlanNodeId {
        crate::property_graph::query::plan::PlanNodeId(0)
    }

    fn prepare_search(
        &mut self,
        _: crate::property_graph::query::plan::PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        Err(RuntimeError::Batch.into())
    }

    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeExecutionError> {
        if !self.emitted {
            self.emitted = true;
            output.push_row(&[], context)?;
            return Ok(PullState::More);
        }
        Err(self
            .expected
            .take()
            .expect("one retained diagnostic")
            .into())
    }
}

struct CompletionProbe<'a>(&'a mut bool);

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for CompletionProbe<'_> {
    type Output = ();

    fn complete<'v>(
        &mut self,
        _: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        *self.0 = true;
        FrozenOutput::new((), 0, 0, 0).map_err(Into::into)
    }
}

#[test]
fn native_error_transport_late_pull_preserves_expression_and_tree() {
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new()
            .with_max_resident_bytes(1_048_576)
            .with_reader_drain_timeout(std::time::Duration::ZERO),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared accounting");
    let memory = QueryMemory::new(&shared, 131_072).expect("query");
    let operators = [Operator {
        inputs: &[],
        kind: OperatorKind::Unit,
    }];
    let mut facts = vec![NodeFacts::default()];
    let mut regions = vec![
        RetainedRegion::slice(&operators).expect("operators"),
        RetainedRegion::vector(&facts).expect("facts"),
    ];
    regions.sort();
    let token = QueryView::new(
        StoreInstanceId::new(1).expect("identity"),
        GraphGeneration::new(0),
    );
    let control = QueryControl::Cancel(CancelToken::new());
    let mut values =
        crate::property_graph::query::ValueContext::new(&token, &control, 10_000).expect("values");
    let plan = GraphPlan::validate_with_fact_vec(
        PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: crate::property_graph::query::plan::PlanNodeId(0),
            eager_searches: &[],
        },
        &mut facts,
        PlanFootprint::declared(100_000),
        PlanBacking::vector(&regions).expect("proof"),
        &mut values,
    )
    .expect("typed plan");
    let owners = [
        RetainedAllocation::array(&operators).expect("operators"),
        RetainedAllocation::plan_facts(&plan).expect("facts"),
    ];
    let admitted = QueryInputs::reserve(&memory, RetentionInventory::array(&owners), &mut values)
        .expect("owners")
        .admit_plan(&plan, &mut values)
        .expect("runtime plan");
    let view = TestView {
        token,
        lease: store.snapshot().expect("lease"),
    };
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).expect("runtime");
    let mut source = TypedDiagnosticSource {
        expected: Some(expected_expression_failure()),
        emitted: false,
    };
    let mut completion_called = false;
    let failure = match execute_in(
        &mut context,
        &admitted,
        &mut source,
        &mut CompletionProbe(&mut completion_called),
        ExecutionCapacity {
            batch_rows: 1,
            result_rows: 1,
            ..ExecutionCapacity::default()
        },
    ) {
        Ok(_) => panic!("late native failure unexpectedly succeeded"),
        Err(failure) => failure,
    };
    assert!(!completion_called);
    assert_eq!(failure.counters.get(WorkKind::RowsIn), 1);
    assert_eq!(failure.counters.get(WorkKind::CompletedRows), 1);
    assert_eq!(
        observe_native_failure(failure.error),
        ObservedDiagnostic::ExpressionTree {
            expression: EXPECTED_EXPRESSION,
            detail: EXPECTED_TREE_DETAIL,
        }
    );

    assert!(matches!(
        NativeExecutionError::from(ExpressionFailure::Runtime(RuntimeError::Batch)),
        NativeExecutionError::Runtime(RuntimeError::Batch)
    ));
    assert!(matches!(
        NativeExecutionError::from(ExpressionFailure::Plan(
            crate::property_graph::query::plan::PlanError::Type
        )),
        NativeExecutionError::Plan(crate::property_graph::query::plan::PlanError::Type)
    ));
    assert!(matches!(
        NativeExecutionError::from(ExpressionFailure::Tree(TreeError::Invalid(
            EXPECTED_TREE_DETAIL
        ))),
        NativeExecutionError::Tree(TreeError::Invalid(EXPECTED_TREE_DETAIL))
    ));
    let io = std::io::Error::from_raw_os_error(5);
    let io_kind = io.kind();
    assert!(matches!(
        NativeExecutionError::from(TreeError::Io(io)),
        NativeExecutionError::Tree(TreeError::Io(error))
            if error.kind() == io_kind && error.raw_os_error() == Some(5)
    ));
}

struct TypedRows<'m, 'g> {
    schema: Schema<'m, 'g>,
    value: QueryValue<'static>,
    failure: Option<NativeExecutionError>,
    emitted: bool,
}

impl<'v, 'm, 'g> RowOperator<'v, 'm, 'g, NativeExecutionError> for TypedRows<'m, 'g> {
    fn schema(&self) -> &Schema<'m, 'g> {
        &self.schema
    }
}

impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g, NativeExecutionError> for TypedRows<'m, 'g> {
    fn node(&self) -> crate::property_graph::query::plan::PlanNodeId {
        crate::property_graph::query::plan::PlanNodeId(0)
    }

    fn prepare_search(
        &mut self,
        _: crate::property_graph::query::plan::PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        Err(RuntimeError::Batch.into())
    }

    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeExecutionError> {
        if !self.emitted {
            self.emitted = true;
            output.push_row(&[self.value], context)?;
            return Ok(if self.failure.is_some() {
                PullState::More
            } else {
                PullState::Done
            });
        }
        match self.failure.take() {
            Some(error) => Err(error),
            None => Ok(PullState::Done),
        }
    }
}

fn relational_capacity(rows: usize) -> StorageCapacity {
    StorageCapacity {
        rows,
        max_rows: rows,
        payload_bytes: 64,
        variable: ArenaCapacity::default(),
    }
}

fn with_eager_plan<R>(
    operation: impl FnOnce(
        &Store,
        &QueryMemory<'_>,
        &RuntimePlan<'_, '_, '_, '_, '_, '_>,
        &QueryControl,
    ) -> R,
) -> R {
    use crate::property_graph::query::plan::{
        Expression, Literal, PlanNodeId, SearchCallId, SearchOutputs, SearchRequest, SlotId,
    };

    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(1_048_576),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared accounting");
    let memory = QueryMemory::new(&shared, 131_072).expect("query");
    let expressions = [
        Expression::Literal(Literal::String("")),
        Expression::Literal(Literal::I64(1)),
    ];
    let edges = [PlanNodeId(0)];
    let eager = [PlanNodeId(1), PlanNodeId(2)];
    let search = |call| Operator {
        inputs: &edges,
        kind: OperatorKind::Search {
            call: SearchCallId(call),
            request: SearchRequest::Text {
                query: ExprId(0),
                k: ExprId(1),
                eligible: None,
            },
            outputs: SearchOutputs {
                node: Some(SlotId(call * 2)),
                score: Some(SlotId(call * 2 + 1)),
                ..SearchOutputs::default()
            },
        },
    };
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        search(0),
        search(1),
        Operator {
            inputs: &edges,
            kind: OperatorKind::OffsetLimit {
                offset: 0,
                limit: Some(0),
            },
        },
    ];
    let mut facts = vec![NodeFacts::default(); 4];
    let mut regions = vec![
        RetainedRegion::slice(&operators).expect("operators"),
        RetainedRegion::slice(&expressions).expect("expressions"),
        RetainedRegion::slice(&edges).expect("edges"),
        RetainedRegion::slice(&eager).expect("eager"),
        RetainedRegion::vector(&facts).expect("facts"),
    ];
    regions.sort();
    let token = QueryView::new(
        StoreInstanceId::new(1).expect("identity"),
        GraphGeneration::new(0),
    );
    let control = QueryControl::Cancel(CancelToken::new());
    let mut values =
        crate::property_graph::query::ValueContext::new(&token, &control, 10_000).expect("values");
    let plan = GraphPlan::validate_with_fact_vec(
        PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(3),
            eager_searches: &eager,
        },
        &mut facts,
        PlanFootprint::declared(100_000),
        PlanBacking::vector(&regions).expect("proof"),
        &mut values,
    )
    .expect("typed eager plan");
    let owners = [
        RetainedAllocation::array(&operators).expect("operators"),
        RetainedAllocation::array(&expressions).expect("expressions"),
        RetainedAllocation::array(&edges).expect("edges"),
        RetainedAllocation::array(&eager).expect("eager"),
        RetainedAllocation::plan_facts(&plan).expect("facts"),
    ];
    let admitted = QueryInputs::reserve(&memory, RetentionInventory::array(&owners), &mut values)
        .expect("owners")
        .admit_plan(&plan, &mut values)
        .expect("runtime plan");
    operation(&store, &memory, &admitted, &control)
}

fn retained_view(store: &Store) -> TestView {
    TestView {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("lease"),
    }
}

#[test]
fn native_error_transport_relational_wrappers_preserve_child_failure() {
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(1_048_576),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared accounting");
    let memory = QueryMemory::new(&shared, 131_072).expect("query");
    let control = QueryControl::Cancel(CancelToken::new());
    let view = TestView {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("lease"),
    };
    let baseline = memory.reserved_bytes();
    {
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .expect("runtime");
        let source = TypedRows {
            schema: Schema::new(&context, &[crate::property_graph::query::plan::SlotId(7)])
                .expect("schema"),
            value: QueryValue::Bool(true),
            failure: Some(TreeError::Invalid("map child detail").into()),
            emitted: false,
        };
        let mut map = MapRows::<_, NativeExecutionError>::new(
            &context,
            crate::property_graph::query::plan::PlanNodeId(0),
            source,
            &[SlotProjection {
                source: crate::property_graph::query::plan::SlotId(7),
                output: crate::property_graph::query::plan::SlotId(9),
            }],
            None,
            0,
            None,
            relational_capacity(1),
        )
        .expect("map");
        let mut output =
            RowBatch::with_arenas(&context, 1, 1, 64, ArenaCapacity::default()).expect("output");
        assert_eq!(
            map.pull(&mut context, &mut output).expect("first row"),
            PullState::More
        );
        assert_eq!(output.rows(), 1);
        output.clear();
        assert!(matches!(
            map.pull(&mut context, &mut output),
            Err(NativeExecutionError::Tree(TreeError::Invalid(
                "map child detail"
            )))
        ));
    }
    assert_eq!(memory.reserved_bytes(), baseline);
    {
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .expect("runtime");
        let source = TypedRows {
            schema: Schema::new(&context, &[crate::property_graph::query::plan::SlotId(7)])
                .expect("schema"),
            value: QueryValue::Bool(true),
            failure: Some(TreeError::Invalid("blocking child detail").into()),
            emitted: false,
        };
        let mut blocking = BlockingRows::<_, NativeExecutionError>::new(
            &context,
            crate::property_graph::query::plan::PlanNodeId(0),
            source,
            BlockingOperation::Distinct,
            relational_capacity(1),
            relational_capacity(2),
            relational_capacity(2),
        )
        .expect("blocking");
        let mut output =
            RowBatch::with_arenas(&context, 1, 1, 64, ArenaCapacity::default()).expect("output");
        assert!(matches!(
            blocking.pull(&mut context, &mut output),
            Err(NativeExecutionError::Tree(TreeError::Invalid(
                "blocking child detail"
            )))
        ));
        assert_eq!(output.rows(), 0);
    }
    assert_eq!(memory.reserved_bytes(), baseline);
    {
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .expect("runtime");
        let source = TypedRows {
            schema: Schema::new(&context, &[crate::property_graph::query::plan::SlotId(7)])
                .expect("schema"),
            value: QueryValue::I64(1),
            failure: None,
            emitted: false,
        };
        let mut map = MapRows::<_, NativeExecutionError>::new(
            &context,
            crate::property_graph::query::plan::PlanNodeId(0),
            source,
            &[SlotProjection {
                source: crate::property_graph::query::plan::SlotId(7),
                output: crate::property_graph::query::plan::SlotId(9),
            }],
            Some(crate::property_graph::query::plan::SlotId(7)),
            0,
            None,
            relational_capacity(1),
        )
        .expect("map");
        let mut output =
            RowBatch::with_arenas(&context, 1, 1, 64, ArenaCapacity::default()).expect("output");
        assert!(matches!(
            map.pull(&mut context, &mut output),
            Err(NativeExecutionError::Runtime(RuntimeError::Value(
                QueryError::Type
            )))
        ));
    }
    assert_eq!(memory.reserved_bytes(), baseline);
}

struct NeverSource;

impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g, NativeExecutionError> for NeverSource {
    fn node(&self) -> crate::property_graph::query::plan::PlanNodeId {
        crate::property_graph::query::plan::PlanNodeId(3)
    }

    fn prepare_search(
        &mut self,
        _: crate::property_graph::query::plan::PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        Err(RuntimeError::Batch.into())
    }

    fn pull(
        &mut self,
        _: &mut RuntimeContext<'v, 'm, 'g>,
        _: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeExecutionError> {
        Ok(PullState::Done)
    }
}

struct FailingFactory;

impl<'m, 'g: 'm> OperatorFactory<'m, 'g, NativeExecutionError> for FailingFactory {
    type Operator<'v> = NeverSource;

    fn build<'v>(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self::Operator<'v>, NativeExecutionError> {
        let _retained = RowBatch::with_arenas(context, 0, 1, 64, ArenaCapacity::default())?;
        Err(TreeError::Invalid("factory retained detail").into())
    }
}

struct EagerSource<'a> {
    calls: &'a Cell<usize>,
    pulls: &'a Cell<usize>,
    fail_first: bool,
}

impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g, NativeExecutionError> for EagerSource<'_> {
    fn node(&self) -> crate::property_graph::query::plan::PlanNodeId {
        crate::property_graph::query::plan::PlanNodeId(3)
    }

    fn prepare_search(
        &mut self,
        node: crate::property_graph::query::plan::PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        let call = self.calls.get();
        assert_eq!(node.0 as usize, call + 1);
        self.calls.set(call + 1);
        if self.fail_first {
            return Err(TreeError::Invalid("eager retained detail").into());
        }
        Ok(())
    }

    fn pull(
        &mut self,
        _: &mut RuntimeContext<'v, 'm, 'g>,
        _: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeExecutionError> {
        self.pulls.set(self.pulls.get() + 1);
        Ok(PullState::Done)
    }
}

struct RejectCompletion<'a>(&'a Cell<usize>);

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for RejectCompletion<'_> {
    type Output = ();

    fn complete<'v>(
        &mut self,
        _: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        self.0.set(self.0.get() + 1);
        Err(ExpressionError {
            expression: ExprId(91),
            failure: ExpressionFailure::Plan(crate::property_graph::query::plan::PlanError::Scope),
        }
        .into())
    }
}

#[test]
fn native_error_transport_factory_eager_and_completion_preserve_failures() {
    with_eager_plan(|store, memory, plan, control| {
        let baseline = memory.reserved_bytes();
        let completion_calls = Cell::new(0);
        let factory_failure = execute_factory(
            retained_view(store),
            control,
            memory,
            plan,
            &mut FailingFactory,
            &mut RejectCompletion(&completion_calls),
            ExecutionCapacity {
                result_rows: 0,
                ..ExecutionCapacity::default()
            },
            RuntimeLimits::default(),
        );
        let factory_failure = match factory_failure {
            Ok(_) => panic!("factory failure exposed output"),
            Err(failure) => failure,
        };
        assert_eq!(factory_failure.operator.0, 3);
        assert!(matches!(
            factory_failure.error,
            NativeExecutionError::Tree(TreeError::Invalid("factory retained detail"))
        ));
        assert_eq!(completion_calls.get(), 0);
        assert_eq!(memory.reserved_bytes(), baseline);

        let eager_calls = Cell::new(0);
        let pulls = Cell::new(0);
        let mut source = EagerSource {
            calls: &eager_calls,
            pulls: &pulls,
            fail_first: true,
        };
        let eager_failure = execute(
            retained_view(store),
            control,
            memory,
            plan,
            &mut source,
            &mut RejectCompletion(&completion_calls),
            ExecutionCapacity {
                result_rows: 0,
                ..ExecutionCapacity::default()
            },
            RuntimeLimits::default(),
        );
        let eager_failure = match eager_failure {
            Ok(_) => panic!("eager failure exposed output"),
            Err(failure) => failure,
        };
        assert_eq!(eager_failure.operator.0, 1);
        assert!(matches!(
            eager_failure.error,
            NativeExecutionError::Tree(TreeError::Invalid("eager retained detail"))
        ));
        assert_eq!(eager_failure.counters.get(WorkKind::SearchInvocations), 1);
        assert_eq!(eager_calls.get(), 1, "later eager source was not invoked");
        assert_eq!(pulls.get(), 0);
        assert_eq!(completion_calls.get(), 0);
        assert_eq!(memory.reserved_bytes(), baseline);

        let eager_calls = Cell::new(0);
        let pulls = Cell::new(0);
        let mut source = EagerSource {
            calls: &eager_calls,
            pulls: &pulls,
            fail_first: false,
        };
        let completion_failure = execute(
            retained_view(store),
            control,
            memory,
            plan,
            &mut source,
            &mut RejectCompletion(&completion_calls),
            ExecutionCapacity {
                result_rows: 0,
                ..ExecutionCapacity::default()
            },
            RuntimeLimits::default(),
        );
        let completion_failure = match completion_failure {
            Ok(_) => panic!("completion failure exposed output"),
            Err(failure) => failure,
        };
        assert_eq!(completion_failure.operator.0, 3);
        assert!(matches!(
            completion_failure.error,
            NativeExecutionError::Expression(ExpressionError {
                expression: ExprId(91),
                failure: ExpressionFailure::Plan(
                    crate::property_graph::query::plan::PlanError::Scope
                )
            })
        ));
        assert_eq!(
            completion_failure.counters.get(WorkKind::SearchInvocations),
            2
        );
        assert_eq!(eager_calls.get(), 2);
        assert_eq!(pulls.get(), 1);
        assert_eq!(completion_calls.get(), 1);
        assert_eq!(memory.reserved_bytes(), baseline);
    });
}

struct ClosedView(QueryView);

impl RetainedView for ClosedView {
    fn query_view(&self) -> &QueryView {
        &self.0
    }

    fn check_active(&self) -> Result<(), QueryError> {
        Err(QueryError::ReadCancelled)
    }
}

#[test]
fn native_error_transport_default_runtime_and_close_first_are_unchanged() {
    fn default_runtime_type(_: RuntimeFailure) {}
    default_runtime_type(RuntimeFailure {
        operator: crate::property_graph::query::plan::PlanNodeId(3),
        error: RuntimeError::Batch,
        counters: WorkCounters::default(),
    });

    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(1_048_576),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared accounting");
    let memory = QueryMemory::new(&shared, 131_072).expect("query");
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let control = QueryControl::Cancel(cancelled);
    let view = ClosedView(QueryView::new(
        StoreInstanceId::new(1).expect("identity"),
        GraphGeneration::new(0),
    ));
    let default = match RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()) {
        Ok(_) => panic!("closed view unexpectedly admitted"),
        Err(error) => error,
    };
    assert!(matches!(
        default,
        RuntimeError::Value(QueryError::ReadCancelled)
    ));
    let outer = match RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()) {
        Ok(_) => panic!("closed view unexpectedly admitted"),
        Err(error) => NativeExecutionError::from(error),
    };
    assert!(matches!(
        outer,
        NativeExecutionError::Runtime(RuntimeError::Value(QueryError::ReadCancelled))
    ));

    let live_view = TestView {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("lease"),
    };
    let live_control = QueryControl::Cancel(CancelToken::new());
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::OperatorRows, 1)
        .expect("tighten");
    let mut context =
        RuntimeContext::new(&live_view, &live_control, &memory, limits).expect("runtime");
    context
        .charge(WorkKind::OperatorRows, 1)
        .expect("exact boundary");
    let failure = context
        .charge(WorkKind::OperatorRows, 1)
        .map_err(NativeExecutionError::from)
        .expect_err("cumulative limit");
    assert!(matches!(
        failure,
        NativeExecutionError::Runtime(RuntimeError::Limit(WorkKind::OperatorRows))
    ));
    assert_eq!(context.counters().get(WorkKind::OperatorRows), 1);
}
