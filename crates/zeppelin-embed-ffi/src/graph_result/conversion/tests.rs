#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use super::*;
use std::cell::{Cell, RefCell};
use std::mem::{size_of, size_of_val};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::query::completed::{
    Column, Key, Node, Property, Receipt, Relationship, ResultInput, SourceError, ValueIndex,
};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::query::resources::*;
use zeppelin_embed::property_graph::query::runtime::*;
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{
    EntityKind, GraphGeneration, GraphRevision, NodeId, RelId, StoreInstanceId,
};

static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(64);

struct CheckpointView {
    token: QueryView,
    lease: SnapshotLease,
    cancellation: CancelToken,
    cancel_at: Cell<usize>,
    close_at: Cell<usize>,
    polls: Cell<usize>,
}
impl RetainedView for CheckpointView {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)?;
        let next = self.polls.get() + 1;
        self.polls.set(next);
        if next == self.cancel_at.get() {
            self.cancellation.cancel();
        }
        if next == self.close_at.get() {
            return Err(QueryError::ReadCancelled);
        }
        Ok(())
    }
}

fn with_checkpoint_context<T>(
    cancel_at: usize,
    test: impl FnOnce(&mut RuntimeContext<'_, '_, '_>, &CheckpointView) -> T,
) -> T {
    with_checkpoint_memory(24 * 1024 * 1024, cancel_at, test)
}

fn with_checkpoint_memory<T>(
    memory_limit: usize,
    cancel_at: usize,
    test: impl FnOnce(&mut RuntimeContext<'_, '_, '_>, &CheckpointView) -> T,
) -> T {
    with_checkpoint_limits(memory_limit, cancel_at, RuntimeLimits::default(), test)
}

fn with_checkpoint_limits<T>(
    memory_limit: usize,
    cancel_at: usize,
    limits: RuntimeLimits,
    test: impl FnOnce(&mut RuntimeContext<'_, '_, '_>, &CheckpointView) -> T,
) -> T {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&resources, memory_limit).unwrap();
    let cancellation = CancelToken::new();
    let view = CheckpointView {
        token: QueryView::new(StoreInstanceId::new(141).unwrap(), GraphGeneration::new(0)),
        lease: store.snapshot().unwrap(),
        cancellation: cancellation.clone(),
        cancel_at: Cell::new(0),
        close_at: Cell::new(0),
        polls: Cell::new(0),
    };
    let control = QueryControl::Cancel(cancellation);
    let mut context = RuntimeContext::new(&view, &control, &memory, limits).unwrap();
    view.polls.set(0);
    view.cancel_at.set(cancel_at);
    test(&mut context, &view)
}

struct NativeSource<'a> {
    input: ResultInput<'a>,
    calls: Cell<usize>,
}

struct ErrorSource(SourceError);
impl ResultSource for ErrorSource {
    fn result_input(&self) -> Result<ResultInput<'_>, SourceError> {
        Err(self.0)
    }
}
impl ResultSource for NativeSource<'_> {
    fn result_input(&self) -> Result<ResultInput<'_>, SourceError> {
        self.calls.set(self.calls.get() + 1);
        Ok(self.input)
    }
}

struct Rows {
    count: usize,
    done: bool,
}
impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for Rows {
    fn node(&self) -> PlanNodeId {
        PlanNodeId(0)
    }
    fn prepare_search(
        &mut self,
        _: PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Batch)
    }
    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, RuntimeError> {
        if self.done {
            return Err(RuntimeError::Batch);
        }
        self.done = true;
        for _ in 0..self.count {
            output.push_row(&[], context)?;
        }
        Ok(PullState::Done)
    }
}

struct Convert<'a, S> {
    source: &'a S,
    error: &'a RefCell<Option<ConversionError>>,
}
impl<'m, 'g: 'm, S: ResultSource> Completion<'m, 'g> for Convert<'_, S> {
    type Output = PreparedNativeResponse<'m, 'g>;
    fn complete<'v>(
        &mut self,
        _: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
        prepare_native(&REGISTRY, self.source, context).map_err(|error| {
            *self.error.borrow_mut() = Some(error);
            RuntimeError::Batch
        })
    }
}

fn with_plan<'v, 'm, 'g, T>(
    context: &mut RuntimeContext<'v, 'm, 'g>,
    run: impl FnOnce(&RuntimePlan<'_, '_, '_, '_, '_, '_>, &mut RuntimeContext<'v, 'm, 'g>) -> T,
) -> T {
    let memory = context.memory();
    let mut scratch = memory.reserve_external_capacity().unwrap();
    scratch
        .reserve_additional(
            VALIDATION_SCRATCH_BYTES
                + size_of::<[RetainedRegion; 2]>()
                + size_of::<PlanDescription<'_>>(),
        )
        .unwrap();
    let mut operators = QueryArena::new(memory, 1).unwrap();
    operators
        .push(Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        })
        .unwrap();
    let mut facts = QueryArena::new(memory, 16).unwrap();
    facts.push(NodeFacts::default()).unwrap();
    let fact_bytes = facts.heap_bytes();
    let mut regions = [
        RetainedRegion::declared(
            operators.as_slice().as_ptr() as usize,
            operators.heap_bytes(),
        )
        .unwrap(),
        RetainedRegion::declared(facts.as_slice().as_ptr() as usize, fact_bytes).unwrap(),
    ];
    regions.sort();
    let (plan, facts_owner) = facts
        .validate_plan(
            PlanDescription {
                operators: operators.as_slice(),
                expressions: &[],
                parameters: &[],
                root: PlanNodeId(0),
                eager_searches: &[],
            },
            PlanFootprint::declared(memory.reserved_bytes()),
            PlanBacking::new(&regions, size_of_val(&regions)).unwrap(),
            context.values(),
        )
        .unwrap();
    let owners = [RetainedAllocation::arena(&operators).unwrap(), facts_owner];
    let inputs =
        QueryInputs::reserve(memory, RetentionInventory::array(&owners), context.values()).unwrap();
    let admitted = inputs.admit_plan(&plan, context.values()).unwrap();
    run(&admitted, context)
}

fn execute_conversion<'m, 'g: 'm, S: ResultSource>(
    context: &mut RuntimeContext<'_, 'm, 'g>,
    source: &S,
    rows: usize,
) -> Execution<PreparedNativeResponse<'m, 'g>> {
    with_plan(context, |plan, context| {
        let mut pull = Rows {
            count: rows,
            done: false,
        };
        let error = RefCell::new(None);
        let mut completion = Convert {
            source,
            error: &error,
        };
        execute_in(
            context,
            plan,
            &mut pull,
            &mut completion,
            ExecutionCapacity {
                batch_rows: rows.max(1),
                result_rows: rows.max(1),
                ..ExecutionCapacity::default()
            },
        )
        .unwrap_or_else(|failure| {
            panic!("conversion failed: {failure}; native={:?}", error.borrow())
        })
    })
}

fn convert<S: ResultSource>(
    context: &mut RuntimeContext<'_, '_, '_>,
    source: &S,
    rows: usize,
) -> ZeGraphResponse {
    let finalized = finalize_native(execute_conversion(context, source, rows));
    let (owner, outcome) = finalized.into_parts();
    owner.expose(outcome)
}

unsafe fn output_slice<'a, T>(pointer: *const T, count: usize) -> &'a [T] {
    if count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(pointer, count) }
    }
}

#[test]
fn graph_result_native_all_pools_match_literal_field_oracle() {
    super::super::tests::with_context(|context| {
        let bytes = b"a\0btext\0\xce\xbbkeynsreltypecolcol";
        let children = [
            ValueIndex(1),
            ValueIndex(2),
            ValueIndex(3),
            ValueIndex(4),
            ValueIndex(5),
            ValueIndex(0),
            ValueIndex(8),
            ValueIndex(9),
            ValueIndex(9),
        ];
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::I64(-17),
            Value::F64(0x8000_0000_0000_0000),
            Value::String(Span::new(7, 3)),
            Value::Node(1),
            Value::Relationship(0),
            Value::List {
                children: Span::new(0, 0),
                element: ListKind::Empty,
            },
            Value::List {
                children: Span::new(0, 2),
                element: ListKind::Bool,
            },
            Value::List {
                children: Span::new(2, 1),
                element: ListKind::I64,
            },
            Value::List {
                children: Span::new(3, 1),
                element: ListKind::F64,
            },
            Value::List {
                children: Span::new(4, 1),
                element: ListKind::String,
            },
            Value::List {
                children: Span::new(5, 4),
                element: ListKind::Query,
            },
        ];
        let properties = [
            Property {
                name: Span::new(0, 1),
                value: ValueIndex(8),
            },
            Property {
                name: Span::new(2, 1),
                value: ValueIndex(3),
            },
        ];
        let names = [Span::new(0, 1), Span::new(2, 1)];
        let node_ids = [(1_u128 << 64) | 7, (2_u128 << 64) | 7, (3_u128 << 64) | 8];
        let nodes = [
            Node {
                id: NodeId::new(node_ids[0]).unwrap(),
                revision: GraphRevision::new(3).unwrap(),
                generation: GraphGeneration::new(0),
                key: Some(Key {
                    kind: EntityKind::Node,
                    namespace: Span::new(13, 2),
                    value: Span::new(10, 3),
                }),
                labels: Span::new(0, 2),
                properties: Span::new(0, 2),
                text: None,
                vector: None,
            },
            Node {
                id: NodeId::new(node_ids[1]).unwrap(),
                revision: GraphRevision::new(4).unwrap(),
                generation: GraphGeneration::new(0),
                key: None,
                labels: Span::new(0, 0),
                properties: Span::new(0, 0),
                text: Some(Span::new(3, 0)),
                vector: Some(Span::new(0, 2)),
            },
            Node {
                id: NodeId::new(node_ids[2]).unwrap(),
                revision: GraphRevision::new(5).unwrap(),
                generation: GraphGeneration::new(0),
                key: None,
                labels: Span::new(0, 0),
                properties: Span::new(0, 0),
                text: Some(Span::new(3, 4)),
                vector: None,
            },
        ];
        let relationship_id = (4_u128 << 64) | 9;
        let relationships = [Relationship {
            id: RelId::new(relationship_id).unwrap(),
            revision: GraphRevision::new(6).unwrap(),
            generation: GraphGeneration::new(0),
            key: Some(Key {
                kind: EntityKind::Relationship,
                namespace: Span::new(13, 2),
                value: Span::new(10, 3),
            }),
            source: NodeId::new(node_ids[0]).unwrap(),
            target: NodeId::new(node_ids[1]).unwrap(),
            relationship_type: Span::new(15, 7),
            properties: Span::new(0, 2),
        }];
        let vectors = [0x8000_0000, 1];
        let columns = [
            Column {
                name: Span::new(22, 3),
                kinds: ValueKinds::ANY,
            },
            Column {
                name: Span::new(25, 3),
                kinds: ValueKinds::ANY,
            },
        ];
        let cells = [ValueIndex(0), ValueIndex(13), ValueIndex(0), ValueIndex(13)];
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    values: &values,
                    bytes,
                    columns: &columns,
                    cells: &cells,
                    children: &children,
                    names: &names,
                    properties: &properties,
                    nodes: &nodes,
                    relationships: &relationships,
                    vectors: &vectors,
                    reports: &[],
                    receipts: &[],
                },
                rows: 2,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let mut output = convert(context, &source, 2);
        assert_eq!(source.calls.get(), 1);
        assert_eq!(
            (output.row_count, output.column_count, output.cell_count),
            (2, 2, 4)
        );
        assert_eq!((output.disposition, output.has_changed_generation), (0, 0));
        assert_eq!(
            (output.has_admitted_generation, output.admitted_generation),
            (1, 0)
        );
        assert_eq!(
            (output.diagnostics, output.diagnostic_count),
            (std::ptr::null(), 0)
        );
        assert_eq!(
            (
                output.work_count,
                output.global_work.start,
                output.global_work.count
            ),
            (23, 0, 23)
        );
        let c_values = unsafe { output_slice(output.pool.values, output.pool.value_count) };
        assert_eq!(c_values.len(), 14);
        assert_eq!(c_values[0].tag, 0);
        assert_eq!((c_values[1].tag, c_values[1].boolean), (1, 1));
        assert_eq!((c_values[2].tag, c_values[2].boolean), (1, 0));
        assert_eq!((c_values[3].tag, c_values[3].integer), (2, -17));
        assert_eq!(c_values[4].floating.to_bits(), 0x8000_0000_0000_0000);
        assert_eq!((c_values[5].range.start, c_values[5].range.count), (7, 3));
        assert_eq!((c_values[6].tag, c_values[6].entity_index), (5, 1));
        assert_eq!((c_values[7].tag, c_values[7].entity_index), (6, 0));
        assert_eq!((c_values[8].list_kind, c_values[8].range.count), (5, 0));
        assert_eq!(c_values[9].list_kind, 1);
        assert_eq!(c_values[10].list_kind, 2);
        assert_eq!(c_values[11].list_kind, 3);
        assert_eq!(c_values[12].list_kind, 4);
        assert_eq!(
            (
                c_values[13].list_kind,
                c_values[13].range.start,
                c_values[13].range.count
            ),
            (0, 5, 4)
        );
        for value in c_values {
            assert_eq!(
                (value.abi_size, value.abi_reserved),
                (size_of::<ZeGraphValue>() as u32, 0)
            );
        }
        assert_eq!(
            unsafe { output_slice(output.pool.children, output.pool.child_count) },
            [1, 2, 3, 4, 5, 0, 8, 9, 9]
        );
        assert_eq!(
            unsafe { output_slice(output.pool.bytes, output.pool.byte_count) },
            bytes
        );
        let c_nodes = unsafe { output_slice(output.pool.nodes, output.pool.node_count) };
        assert_eq!((c_nodes[0].id.high, c_nodes[0].id.low), (1, 7));
        assert_eq!((c_nodes[1].id.high, c_nodes[1].id.low), (2, 7));
        assert_eq!(
            (
                c_nodes[0].has_key,
                c_nodes[0].has_text,
                c_nodes[0].has_vector
            ),
            (1, 0, 0)
        );
        assert_eq!(
            (
                c_nodes[1].has_key,
                c_nodes[1].has_text,
                c_nodes[1].text.start,
                c_nodes[1].text.count
            ),
            (0, 1, 3, 0)
        );
        assert_eq!(
            (
                c_nodes[2].has_text,
                c_nodes[2].text.start,
                c_nodes[2].text.count
            ),
            (1, 3, 4)
        );
        assert_eq!(
            (
                c_nodes[1].has_vector,
                c_nodes[1].vector.start,
                c_nodes[1].vector.count
            ),
            (1, 0, 2)
        );
        let c_relationships =
            unsafe { output_slice(output.pool.relationships, output.pool.relationship_count) };
        assert_eq!(
            (c_relationships[0].id.high, c_relationships[0].id.low),
            (4, 9)
        );
        assert_eq!(
            (
                c_relationships[0].source.high,
                c_relationships[0].source.low
            ),
            (1, 7)
        );
        assert_eq!(
            (
                c_relationships[0].target.high,
                c_relationships[0].target.low
            ),
            (2, 7)
        );
        assert_eq!(
            (
                c_relationships[0].revision,
                c_relationships[0].last_change_generation
            ),
            (6, 0)
        );
        let c_properties =
            unsafe { output_slice(output.pool.properties, output.pool.property_count) };
        assert_eq!(
            (
                c_properties[0].name.start,
                c_properties[0].name.count,
                c_properties[0].value
            ),
            (0, 1, 8)
        );
        assert_eq!(
            (
                c_properties[1].name.start,
                c_properties[1].name.count,
                c_properties[1].value
            ),
            (2, 1, 3)
        );
        let c_names = unsafe { output_slice(output.pool.names, output.pool.name_count) };
        assert_eq!(
            (
                c_names[0].start,
                c_names[0].count,
                c_names[1].start,
                c_names[1].count
            ),
            (0, 1, 2, 1)
        );
        let c_vectors = unsafe { output_slice(output.pool.vectors, output.pool.vector_count) };
        assert_eq!(
            (c_vectors[0].to_bits(), c_vectors[1].to_bits()),
            (0x8000_0000, 1)
        );
        let c_columns = unsafe { output_slice(output.columns, output.column_count) };
        assert_eq!(
            (
                c_columns[0].name.start,
                c_columns[0].name.count,
                c_columns[0].kinds
            ),
            (22, 3, 255)
        );
        assert_eq!(
            (
                c_columns[1].name.start,
                c_columns[1].name.count,
                c_columns[1].kinds
            ),
            (25, 3, 255)
        );
        assert_eq!(
            unsafe { output_slice(output.cells, output.cell_count) },
            [0, 13, 0, 13]
        );
        assert_eq!(
            (output.receipts, output.receipt_count),
            (std::ptr::null(), 0)
        );
        assert_eq!((output.reports, output.report_count), (std::ptr::null(), 0));
        REGISTRY.free(&mut output).unwrap();

        let empty = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools::default(),
                rows: 3,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let mut empty_output = convert(context, &empty, 3);
        assert_eq!(
            (
                empty_output.row_count,
                empty_output.column_count,
                empty_output.cell_count
            ),
            (3, 0, 0)
        );
        assert_eq!(
            (empty_output.work_count, empty_output.global_work.count),
            (23, 23)
        );
        assert!(empty_output.columns.is_null());
        assert!(empty_output.cells.is_null());
        assert!(empty_output.pool.values.is_null());
        REGISTRY.free(&mut empty_output).unwrap();
    });
}

#[allow(
    clippy::too_many_arguments,
    reason = "the exhaustive report fixture keeps each independent native field visible"
)]
fn report_fixture(
    call: u32,
    kind: SearchKind,
    requested_tier: Option<SearchTier>,
    actual_tier: Option<ActualTier>,
    precision: ScorePrecision,
    coverage: CandidateCoverage,
    vector_leg: LegState,
    lexical_leg: LegState,
    epochs: (Option<u64>, Option<u64>, Option<u64>),
    work: WorkCounters,
) -> SearchReport {
    SearchReport {
        call: SearchCallId(call),
        generation: GraphGeneration::new(0),
        kind,
        requested_tier,
        actual_tier,
        precision,
        coverage,
        vector_leg,
        lexical_leg,
        document_epoch: epochs.0,
        query_epoch: epochs.1,
        tokenizer_epoch: epochs.2,
        effective_alpha_bits: 0.25_f64.to_bits(),
        normalization_version: 7,
        rules_version: 9,
        candidate_count: 3,
        cross_scored_count: if kind == SearchKind::Hybrid { 3 } else { 2 },
        fallback_count: u64::from(call) + 1,
        cross_score_complete: kind == SearchKind::Hybrid,
        work,
    }
}

#[test]
fn graph_result_native_reports_receipts_outcomes_are_lossless() {
    use zeppelin_embed::lifecycle::GraphSearchOptions;
    use zeppelin_embed::property_graph::EntityId;
    use zeppelin_embed::property_graph::staging::ItemReceipt;

    super::super::tests::with_context(|context| {
        let graph = SearchTier::Graph(GraphSearchOptions::default());
        let reports = [
            report_fixture(
                0,
                SearchKind::Lexical,
                None,
                None,
                ScorePrecision::NotApplicable,
                CandidateCoverage::Exact,
                LegState::NotRequested,
                LegState::NoQueryMatches,
                (None, None, Some(0)),
                context.counters(),
            ),
            report_fixture(
                1,
                SearchKind::Vector,
                None,
                Some(ActualTier::Exact),
                ScorePrecision::Original,
                CandidateCoverage::Exact,
                LegState::Nonempty,
                LegState::NotRequested,
                (Some(0), Some(19), None),
                context.counters(),
            ),
            report_fixture(
                2,
                SearchKind::Vector,
                Some(SearchTier::Auto),
                Some(ActualTier::Scan),
                ScorePrecision::Quantized,
                CandidateCoverage::Approximate,
                LegState::NoIndexedPopulation,
                LegState::NotRequested,
                (Some(23), Some(0), None),
                context.counters(),
            ),
            report_fixture(
                3,
                SearchKind::Vector,
                Some(SearchTier::Exact),
                Some(ActualTier::Graph),
                ScorePrecision::Mixed,
                CandidateCoverage::Approximate,
                LegState::NoEligibleMembers,
                LegState::NotRequested,
                (None, Some(29), None),
                context.counters(),
            ),
            report_fixture(
                4,
                SearchKind::Hybrid,
                Some(SearchTier::Scan),
                Some(ActualTier::Scan),
                ScorePrecision::Original,
                CandidateCoverage::Exact,
                LegState::Nonempty,
                LegState::Nonempty,
                (Some(31), Some(32), Some(33)),
                context.counters(),
            ),
            report_fixture(
                5,
                SearchKind::Hybrid,
                Some(graph),
                Some(ActualTier::Graph),
                ScorePrecision::Mixed,
                CandidateCoverage::Approximate,
                LegState::NoIndexedPopulation,
                LegState::NoEligibleMembers,
                (Some(0), Some(0), Some(0)),
                context.counters(),
            ),
            report_fixture(
                6,
                SearchKind::Hybrid,
                Some(SearchTier::Auto),
                Some(ActualTier::Exact),
                ScorePrecision::Original,
                CandidateCoverage::Exact,
                LegState::NoEligibleMembers,
                LegState::NoQueryMatches,
                (None, None, None),
                context.counters(),
            ),
            report_fixture(
                7,
                SearchKind::Lexical,
                None,
                None,
                ScorePrecision::NotApplicable,
                CandidateCoverage::Approximate,
                LegState::NotRequested,
                LegState::NoIndexedPopulation,
                (None, None, Some(41)),
                context.counters(),
            ),
        ];
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    reports: &reports,
                    ..Pools::default()
                },
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let mut output = convert(context, &source, 0);
        assert_eq!(output.report_count, 8);
        assert_eq!(output.work_count, 23 + 22 * 8);
        let c_reports = unsafe { output_slice(output.reports, output.report_count) };
        for (ordinal, mapped) in c_reports.iter().enumerate() {
            assert_eq!(mapped.call_id as usize, ordinal);
            assert_eq!(
                (mapped.work.start as usize, mapped.work.count),
                (23 + 22 * ordinal, 22)
            );
            assert_eq!(mapped.effective_alpha.to_bits(), 0.25_f64.to_bits());
            assert_eq!((mapped.normalization_version, mapped.rules_version), (7, 9));
            assert_eq!(mapped.fallback_count, ordinal as u64 + 1);
        }
        assert_eq!(
            (c_reports[0].has_requested_tier, c_reports[0].requested_tier),
            (0, 0)
        );
        assert_eq!(
            (c_reports[1].has_actual_tier, c_reports[1].actual_tier),
            (1, 1)
        );
        assert_eq!(
            (c_reports[2].has_requested_tier, c_reports[2].requested_tier),
            (1, 0)
        );
        assert_eq!(
            (c_reports[3].requested_tier, c_reports[3].actual_tier),
            (1, 3)
        );
        assert_eq!((c_reports[4].requested_tier, c_reports[4].kind), (2, 2));
        assert_eq!(
            (c_reports[5].requested_tier, c_reports[5].actual_tier),
            (3, 3)
        );
        assert_eq!(
            (
                c_reports[0].has_tokenizer_epoch,
                c_reports[0].tokenizer_epoch
            ),
            (1, 0)
        );
        assert_eq!(
            (c_reports[1].has_document_epoch, c_reports[1].document_epoch),
            (1, 0)
        );
        assert_eq!(
            (c_reports[6].has_document_epoch, c_reports[6].document_epoch),
            (0, 0)
        );
        assert_eq!((c_reports[5].vector_leg, c_reports[5].lexical_leg), (2, 3));
        REGISTRY.free(&mut output).unwrap();

        let receipts = [
            Receipt {
                item_index: 0,
                deleted: false,
                receipt: ItemReceipt {
                    entity: EntityId::Node(NodeId::new((8_u128 << 64) | 3).unwrap()),
                    revision: GraphRevision::new(5).unwrap(),
                    generation: GraphGeneration::new(0),
                    replayed: true,
                },
            },
            Receipt {
                item_index: 1,
                deleted: true,
                receipt: ItemReceipt {
                    entity: EntityId::Relationship(RelId::new((9_u128 << 64) | 4).unwrap()),
                    revision: GraphRevision::new(6).unwrap(),
                    generation: GraphGeneration::new(1),
                    replayed: false,
                },
            },
        ];
        let committed = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    receipts: &receipts,
                    ..Pools::default()
                },
                rows: 0,
                outcome: Outcome::Committed {
                    changed: GraphGeneration::new(1),
                },
            },
            calls: Cell::new(0),
        };
        let mut output = convert(context, &committed, 0);
        assert_eq!(
            (
                output.disposition,
                output.has_changed_generation,
                output.changed_generation
            ),
            (2, 1, 1)
        );
        let c_receipts = unsafe { output_slice(output.receipts, output.receipt_count) };
        assert_eq!(
            (
                c_receipts[0].entity_kind,
                c_receipts[0].node.high,
                c_receipts[0].node.low
            ),
            (0, 8, 3)
        );
        assert_eq!(
            (
                c_receipts[0].disposition,
                c_receipts[0].deleted,
                c_receipts[0].generation
            ),
            (3, 0, 0)
        );
        assert_eq!(
            (
                c_receipts[1].entity_kind,
                c_receipts[1].relationship.high,
                c_receipts[1].relationship.low
            ),
            (1, 9, 4)
        );
        assert_eq!(
            (
                c_receipts[1].disposition,
                c_receipts[1].deleted,
                c_receipts[1].generation
            ),
            (2, 1, 1)
        );
        REGISTRY.free(&mut output).unwrap();

        let replayed_receipts = [Receipt {
            item_index: 0,
            deleted: false,
            receipt: ItemReceipt {
                entity: EntityId::Node(NodeId::new(77).unwrap()),
                revision: GraphRevision::new(2).unwrap(),
                generation: GraphGeneration::new(0),
                replayed: true,
            },
        }];
        for (outcome, disposition) in [(Outcome::Replayed, 3), (Outcome::NoOp, 4)] {
            let pools = if outcome == Outcome::Replayed {
                Pools {
                    receipts: &replayed_receipts,
                    ..Pools::default()
                }
            } else {
                Pools::default()
            };
            let source = NativeSource {
                input: ResultInput {
                    view: context.view(),
                    pools,
                    rows: 0,
                    outcome,
                },
                calls: Cell::new(0),
            };
            let mut output = convert(context, &source, 0);
            assert_eq!(
                (
                    output.disposition,
                    output.has_changed_generation,
                    output.changed_generation
                ),
                (disposition, 0, 0)
            );
            REGISTRY.free(&mut output).unwrap();
        }

        let baseline = context.memory().reserved_bytes();
        let invalid_values = [Value::String(Span::new(0, 1))];
        let invalid = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    values: &invalid_values,
                    bytes: &[0xff],
                    ..Pools::default()
                },
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        assert!(matches!(
            prepare_native(&REGISTRY, &invalid, context),
            Err(ConversionError::Completed(CompletedError::Utf8))
        ));
        assert_eq!(context.memory().reserved_bytes(), baseline);

        let missing = EntityId::Node(NodeId::new(91).unwrap());
        let deleted = EntityId::Relationship(RelId::new(92).unwrap());
        for expected in [
            SourceError::Missing(missing),
            SourceError::Deleted(deleted),
            SourceError::Storage,
        ] {
            let error = match prepare_native(&REGISTRY, &ErrorSource(expected), context) {
                Err(error) => error,
                Ok(_) => panic!("native source error must be retained"),
            };
            assert!(matches!(
                error,
                ConversionError::Completed(CompletedError::Source(observed))
                    if observed == expected
            ));
            assert_eq!(context.memory().reserved_bytes(), baseline);
        }

        let malformed_string = [Value::String(Span::new(u32::MAX, 2))];
        let cyclic_list = [Value::List {
            children: Span::new(0, 1),
            element: ListKind::Query,
        }];
        let cyclic_child = [ValueIndex(0)];
        let scalar = [Value::I64(1)];
        let column = [Column {
            name: Span::new(0, 0),
            kinds: ValueKinds::I64,
        }];
        let invalid_cell = [ValueIndex(1)];
        let mut contradictory_report = report_fixture(
            0,
            SearchKind::Vector,
            None,
            Some(ActualTier::Exact),
            ScorePrecision::Original,
            CandidateCoverage::Exact,
            LegState::Nonempty,
            LegState::NotRequested,
            (None, None, None),
            context.counters(),
        );
        contradictory_report.lexical_leg = LegState::Nonempty;
        let contradictory_reports = [contradictory_report];
        for (pools, outcome, rows) in [
            (
                Pools {
                    values: &malformed_string,
                    bytes: b"x",
                    ..Pools::default()
                },
                Outcome::Read,
                0,
            ),
            (
                Pools {
                    values: &cyclic_list,
                    children: &cyclic_child,
                    ..Pools::default()
                },
                Outcome::Read,
                0,
            ),
            (
                Pools {
                    values: &scalar,
                    columns: &column,
                    cells: &invalid_cell,
                    ..Pools::default()
                },
                Outcome::Read,
                1,
            ),
            (
                Pools::default(),
                Outcome::Committed {
                    changed: context.view().generation(),
                },
                0,
            ),
            (
                Pools {
                    reports: &contradictory_reports,
                    ..Pools::default()
                },
                Outcome::Read,
                0,
            ),
        ] {
            let malformed = NativeSource {
                input: ResultInput {
                    view: context.view(),
                    pools,
                    rows,
                    outcome,
                },
                calls: Cell::new(0),
            };
            assert!(matches!(
                prepare_native(&REGISTRY, &malformed, context),
                Err(ConversionError::Completed(CompletedError::Shape))
            ));
            assert_eq!(malformed.calls.get(), 1);
            assert_eq!(context.memory().reserved_bytes(), baseline);
        }
    });
}

#[test]
fn graph_result_native_context_identity_and_source_snapshot_are_single() {
    struct Changing<'a> {
        first: ResultInput<'a>,
        second: ResultInput<'a>,
        calls: Cell<usize>,
    }
    impl ResultSource for Changing<'_> {
        fn result_input(&self) -> Result<ResultInput<'_>, SourceError> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            Ok(if call == 0 { self.first } else { self.second })
        }
    }

    super::super::tests::with_context(|context| {
        let foreign = zeppelin_embed::property_graph::query::QueryView::new(
            context.view().store(),
            context.view().generation(),
        );
        let source = NativeSource {
            input: ResultInput {
                view: &foreign,
                pools: Pools::default(),
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let baseline = context.memory().reserved_bytes();
        assert!(matches!(
            prepare_native(&REGISTRY, &source, context),
            Err(ConversionError::Completed(CompletedError::Source(
                SourceError::ForeignView
            )))
        ));
        assert_eq!(source.calls.get(), 1);
        assert_eq!(context.memory().reserved_bytes(), baseline);

        let first_values = [Value::I64(17)];
        let second_values = [Value::I64(99)];
        let columns = [Column {
            name: Span::new(0, 0),
            kinds: ValueKinds::I64,
        }];
        let cells = [ValueIndex(0)];
        let changing = Changing {
            first: ResultInput {
                view: context.view(),
                pools: Pools {
                    values: &first_values,
                    columns: &columns,
                    cells: &cells,
                    ..Pools::default()
                },
                rows: 1,
                outcome: Outcome::Read,
            },
            second: ResultInput {
                view: context.view(),
                pools: Pools {
                    values: &second_values,
                    columns: &columns,
                    cells: &cells,
                    ..Pools::default()
                },
                rows: 1,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let mut output = convert(context, &changing, 1);
        assert_eq!(changing.calls.get(), 1);
        let mapped = unsafe { output_slice(output.pool.values, output.pool.value_count) };
        assert_eq!(mapped[0].integer, 17);
        REGISTRY.free(&mut output).unwrap();

        let before = context.counters().get(WorkKind::Lookups);
        context.charge(WorkKind::Lookups, 37).unwrap();
        let carry = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools::default(),
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let mut output = convert(context, &carry, 0);
        let work = unsafe { output_slice(output.work, output.work_count) };
        assert_eq!(work[WorkKind::Lookups as usize].value, before + 37);
        REGISTRY.free(&mut output).unwrap();
    });

    with_checkpoint_context(0, |context, view| {
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools::default(),
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        view.polls.set(0);
        view.cancel_at.set(1);
        view.close_at.set(1);
        assert!(matches!(
            prepare_native(&REGISTRY, &source, context),
            Err(ConversionError::Completed(CompletedError::Runtime(
                RuntimeError::Value(QueryError::ReadCancelled)
            )))
        ));
        assert_eq!(source.calls.get(), 0);
    });
}

#[test]
fn graph_result_native_driver_finalizes_all_23_exact_counters() {
    super::super::tests::with_context(|context| {
        for (ordinal, (kind, _)) in WORK_KINDS.into_iter().enumerate() {
            let amount = if kind == WorkKind::SearchInvocations {
                1
            } else {
                ordinal as u64 + 2
            };
            context.charge(kind, amount).unwrap();
        }
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools::default(),
                rows: 1,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let execution = execute_conversion(context, &source, 1);
        let work_pointer = execution.output.response.descriptor().work;
        let counters = execution.counters;
        let peak = execution.peak_query_bytes;
        let before_counters = context.counters();
        let before_peak = context.memory().peak_reserved_bytes();
        let (mut output, final_audit) = super::super::audit::run(0, true, || {
            let finalized = finalize_native(execution);
            let (owner, outcome) = finalized.into_parts();
            owner.expose(outcome)
        });
        assert_eq!(final_audit.attempts, 0);
        assert_eq!(output.work, work_pointer);
        assert_eq!(context.counters(), before_counters);
        assert_eq!(context.memory().peak_reserved_bytes(), before_peak);
        let rows = unsafe { output_slice(output.work, output.work_count) };
        assert_eq!(rows.len(), 23);
        for (ordinal, (kind, c_kind)) in WORK_KINDS.into_iter().enumerate() {
            assert_eq!(
                (rows[ordinal].kind, rows[ordinal].value),
                (c_kind, counters.get(kind))
            );
        }
        assert_eq!((rows[22].kind, rows[22].value), (22, peak as u64));
        let native_bytes = size_of::<
            zeppelin_embed::property_graph::query::completed::CompletedGraphResult,
        >() as u64;
        let c_bytes = (23 * size_of::<ZeGraphWorkCounter>()) as u64;
        assert_eq!(counters.get(WorkKind::CompletedRows), 6 + 1);
        assert_eq!(counters.get(WorkKind::CompletedBytes), 7 + native_bytes);
        assert_eq!(counters.get(WorkKind::CompletedAbiBytes), 9 + c_bytes);
        assert_eq!(counters.get(WorkKind::CopiedBytes), 23 + c_bytes);
        let (free, free_audit) = super::super::audit::run(0, true, || REGISTRY.free(&mut output));
        assert!(free.is_ok());
        assert_eq!(free_audit.attempts, 0);
        assert_eq!(free_audit.frees, 2);
    });
}

#[test]
fn graph_result_native_geometry_limits_and_real_overlap_reject() {
    use zeppelin_embed::property_graph::EntityId;
    use zeppelin_embed::property_graph::staging::ItemReceipt;

    super::super::tests::with_context(|context| {
        let values = [
            Value::Bool(true),
            Value::List {
                children: Span::new(0, 1),
                element: ListKind::Query,
            },
        ];
        let children = [ValueIndex(0)];
        let bytes = b"ankrtcol";
        let names = [Span::new(0, 1)];
        let properties = [Property {
            name: Span::new(0, 1),
            value: ValueIndex(0),
        }];
        let nodes = [Node {
            id: NodeId::new(1).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            generation: GraphGeneration::new(0),
            key: None,
            labels: Span::new(0, 1),
            properties: Span::new(0, 1),
            text: Some(Span::new(1, 1)),
            vector: Some(Span::new(0, 1)),
        }];
        let relationships = [Relationship {
            id: RelId::new(2).unwrap(),
            revision: GraphRevision::new(2).unwrap(),
            generation: GraphGeneration::new(0),
            key: None,
            source: NodeId::new(1).unwrap(),
            target: NodeId::new(3).unwrap(),
            relationship_type: Span::new(2, 1),
            properties: Span::new(0, 1),
        }];
        let vectors = [0x8000_0000];
        let columns = [Column {
            name: Span::new(4, 4),
            kinds: ValueKinds::LIST,
        }];
        let cells = [ValueIndex(1)];
        let receipts = [Receipt {
            item_index: 0,
            deleted: false,
            receipt: ItemReceipt {
                entity: EntityId::Node(NodeId::new(1).unwrap()),
                revision: GraphRevision::new(1).unwrap(),
                generation: GraphGeneration::new(1),
                replayed: false,
            },
        }];
        let reports = [report_fixture(
            0,
            SearchKind::Vector,
            None,
            Some(ActualTier::Exact),
            ScorePrecision::Original,
            CandidateCoverage::Exact,
            LegState::Nonempty,
            LegState::NotRequested,
            (None, None, None),
            context.counters(),
        )];
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    values: &values,
                    bytes,
                    columns: &columns,
                    cells: &cells,
                    children: &children,
                    names: &names,
                    properties: &properties,
                    nodes: &nodes,
                    relationships: &relationships,
                    vectors: &vectors,
                    reports: &reports,
                    receipts: &receipts,
                },
                rows: 1,
                outcome: Outcome::Committed {
                    changed: GraphGeneration::new(1),
                },
            },
            calls: Cell::new(0),
        };
        let execution = execute_conversion(context, &source, 1);
        let observed_peak = execution.peak_query_bytes;
        let finalized = finalize_native(execution);
        let (owner, outcome) = finalized.into_parts();
        let arena_bytes = owner.arena_bytes();
        assert!(owner.reserved_bytes() > owner.allocation_bytes());
        let mut output = owner.expose(outcome);
        let pointers = [
            output.pool.values.cast::<u8>(),
            output.pool.children.cast::<u8>(),
            output.pool.bytes,
            output.pool.nodes.cast::<u8>(),
            output.pool.relationships.cast::<u8>(),
            output.pool.properties.cast::<u8>(),
            output.pool.names.cast::<u8>(),
            output.pool.vectors.cast::<u8>(),
            output.columns.cast::<u8>(),
            output.cells.cast::<u8>(),
            output.receipts.cast::<u8>(),
            output.reports.cast::<u8>(),
            output.diagnostics.cast::<u8>(),
            output.work.cast::<u8>(),
        ];
        let counts = [
            output.pool.value_count,
            output.pool.child_count,
            output.pool.byte_count,
            output.pool.node_count,
            output.pool.relationship_count,
            output.pool.property_count,
            output.pool.name_count,
            output.pool.vector_count,
            output.column_count,
            output.cell_count,
            output.receipt_count,
            output.report_count,
            output.diagnostic_count,
            output.work_count,
        ];
        let layouts = [
            std::alloc::Layout::new::<ZeGraphValue>(),
            std::alloc::Layout::new::<u32>(),
            std::alloc::Layout::new::<u8>(),
            std::alloc::Layout::new::<ZeGraphNode>(),
            std::alloc::Layout::new::<ZeGraphRelationship>(),
            std::alloc::Layout::new::<ZeGraphProperty>(),
            std::alloc::Layout::new::<ZeGraphRange>(),
            std::alloc::Layout::new::<f32>(),
            std::alloc::Layout::new::<ZeGraphColumn>(),
            std::alloc::Layout::new::<u32>(),
            std::alloc::Layout::new::<ZeGraphReceipt>(),
            std::alloc::Layout::new::<ZeGraphSearchReport>(),
            std::alloc::Layout::new::<ZeGraphDiagnostic>(),
            std::alloc::Layout::new::<ZeGraphWorkCounter>(),
        ];
        let base = pointers[0] as usize;
        let mut cursor = 0_usize;
        let mut previous_end = base;
        for ordinal in 0..14 {
            let align = layouts[ordinal].align();
            cursor = (cursor + align - 1) & !(align - 1);
            if counts[ordinal] == 0 {
                assert!(pointers[ordinal].is_null());
            } else {
                assert_eq!(pointers[ordinal] as usize, base + cursor);
                assert!(pointers[ordinal] as usize >= previous_end);
                previous_end =
                    pointers[ordinal] as usize + counts[ordinal] * layouts[ordinal].size();
            }
            cursor += counts[ordinal] * layouts[ordinal].size();
        }
        let max_align = layouts.iter().map(std::alloc::Layout::align).max().unwrap();
        let independently_padded = (cursor + max_align - 1) & !(max_align - 1);
        assert_eq!(arena_bytes, independently_padded);
        assert!(observed_peak > arena_bytes);
        REGISTRY.free(&mut output).unwrap();

        let count = 4 * 1024 * 1024 / size_of::<ZeGraphValue>() + 1;
        let native_represented = size_of::<
            zeppelin_embed::property_graph::query::completed::CompletedGraphResult,
        >() + count * size_of::<Value>();
        assert!(native_represented < 4 * 1024 * 1024);
        let mut large = QueryArena::new(context.memory(), count).unwrap();
        for _ in 0..count {
            large.push(Value::Null).unwrap();
        }
        let baseline = context.memory().reserved_bytes();
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    values: large.as_slice(),
                    ..Pools::default()
                },
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        assert!(matches!(
            prepare_native(&REGISTRY, &source, context),
            Err(ConversionError::Owner(OwnerError::Limit))
        ));
        assert_eq!(context.memory().reserved_bytes(), baseline);
    });

    fn overlap_case(context: &mut RuntimeContext<'_, '_, '_>) -> Result<usize, ConversionError> {
        let payload_len = 70 * 1024;
        let mut bytes = QueryArena::new(context.memory(), payload_len + 4096)
            .map_err(|error| ConversionError::Completed(CompletedError::Runtime(error.into())))?;
        for _ in 0..payload_len {
            bytes.push(b'x').map_err(|error| {
                ConversionError::Completed(CompletedError::Runtime(error.into()))
            })?;
        }
        let mut values = QueryArena::new(context.memory(), 1)
            .map_err(|error| ConversionError::Completed(CompletedError::Runtime(error.into())))?;
        values
            .push(Value::String(Span::new(0, payload_len as u32)))
            .map_err(|error| ConversionError::Completed(CompletedError::Runtime(error.into())))?;
        let baseline = context.memory().reserved_bytes();
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    values: values.as_slice(),
                    bytes: bytes.as_slice(),
                    ..Pools::default()
                },
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let prepared = prepare_native(&REGISTRY, &source, context)?;
        let peak = context.memory().peak_reserved_bytes();
        drop(prepared);
        assert_eq!(context.memory().reserved_bytes(), baseline);
        Ok(peak)
    }

    let required = with_checkpoint_memory(24 * 1024 * 1024, 0, |context, _| {
        overlap_case(context).unwrap()
    });
    let tightened = required.checked_sub(1).unwrap();
    with_checkpoint_memory(tightened, 0, |context, _| {
        let baseline = context.memory().reserved_bytes();
        assert!(matches!(
            overlap_case(context),
            Err(ConversionError::Owner(OwnerError::Memory(
                MemoryError::Limit
            )))
        ));
        assert_eq!(context.memory().reserved_bytes(), baseline);
    });

    let dir = tempfile::tempdir().unwrap();
    let aggregate_limit = 4_usize * 1024 * 1024;
    let store = Store::open(
        dir.path(),
        OpenOptions::new().with_max_resident_bytes(aggregate_limit as u64),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&resources, 1024 * 1024).unwrap();
    let cancellation = CancelToken::new();
    let view = CheckpointView {
        token: QueryView::new(StoreInstanceId::new(142).unwrap(), GraphGeneration::new(0)),
        lease: store.snapshot().unwrap(),
        cancellation: cancellation.clone(),
        cancel_at: Cell::new(0),
        close_at: Cell::new(0),
        polls: Cell::new(0),
    };
    let control = QueryControl::Cancel(cancellation);
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
    let payload_len = 70 * 1024;
    let mut bytes = QueryArena::new(context.memory(), payload_len + 4096).unwrap();
    for _ in 0..payload_len {
        bytes.push(b'y').unwrap();
    }
    let values = [Value::String(Span::new(0, payload_len as u32))];
    let source = NativeSource {
        input: ResultInput {
            view: context.view(),
            pools: Pools {
                values: &values,
                bytes: bytes.as_slice(),
                ..Pools::default()
            },
            rows: 0,
            outcome: Outcome::Read,
        },
        calls: Cell::new(0),
    };
    let query_baseline = context.memory().reserved_bytes();
    let shared_baseline = resources.reserved_bytes().unwrap() as usize;
    drop(prepare_native(&REGISTRY, &source, &mut context).unwrap());
    let conversion_peak = context
        .memory()
        .peak_reserved_bytes()
        .checked_sub(query_baseline)
        .unwrap();
    assert_eq!(context.memory().reserved_bytes(), query_baseline);
    assert_eq!(
        resources.reserved_bytes().unwrap() as usize,
        shared_baseline
    );
    let held_bytes = aggregate_limit
        .checked_sub(shared_baseline)
        .and_then(|available| available.checked_sub(conversion_peak))
        .and_then(|bytes| bytes.checked_add(1))
        .unwrap();
    let held = resources.reserve(held_bytes).unwrap();
    assert!(matches!(
        prepare_native(&REGISTRY, &source, &mut context),
        Err(ConversionError::Owner(OwnerError::Memory(
            MemoryError::Store(_)
        )))
    ));
    assert_eq!(context.memory().reserved_bytes(), query_baseline);
    drop(held);
    drop(prepare_native(&REGISTRY, &source, &mut context).unwrap());
    assert_eq!(context.memory().reserved_bytes(), query_baseline);
    assert_eq!(
        resources.reserved_bytes().unwrap() as usize,
        shared_baseline
    );
}

#[test]
fn graph_result_native_every_allocation_and_copy_checkpoint_cleans() {
    fn case(fail_at: usize) -> (Result<(), &'static str>, super::super::audit::Snapshot) {
        super::super::tests::with_context(|context| {
            let bytes = vec![b'x'; 65537];
            let values = [Value::String(Span::new(0, bytes.len() as u32))];
            let source = NativeSource {
                input: ResultInput {
                    view: context.view(),
                    pools: Pools {
                        values: &values,
                        bytes: &bytes,
                        ..Pools::default()
                    },
                    rows: 0,
                    outcome: Outcome::Read,
                },
                calls: Cell::new(0),
            };
            let baseline = context.memory().reserved_bytes();
            let (classification, audit) = super::super::audit::run(fail_at, false, || {
                let result = prepare_native(&REGISTRY, &source, context);
                let classification = match &result {
                    Ok(_) => Ok(()),
                    Err(ConversionError::Completed(CompletedError::Runtime(
                        RuntimeError::Memory(MemoryError::Allocation),
                    ))) => Err("native"),
                    Err(ConversionError::Owner(OwnerError::Allocation)) => Err("c"),
                    Err(other) => panic!("unexpected allocation failure: {other:?}"),
                };
                drop(result);
                classification
            });
            assert_eq!(context.memory().reserved_bytes(), baseline);
            assert_eq!(audit.bytes, 0);
            (classification, audit)
        })
    }

    let (clean, inventory) = case(0);
    assert_eq!(clean, Ok(()));
    assert!(inventory.allocations >= 5);
    assert_eq!(inventory.allocations, inventory.frees);
    for ordinal in 1..=inventory.attempts {
        let (failure, audit) = case(ordinal);
        assert!(failure.is_err(), "allocation ordinal {ordinal} must fire");
        assert_eq!(audit.attempts, ordinal);
        assert_eq!(audit.allocations, audit.frees);
        assert_eq!(audit.bytes, 0);
    }

    #[cfg(feature = "graph-result-test-support")]
    for ordinal in 1..=2 {
        super::super::tests::with_context(|context| {
            let source = NativeSource {
                input: ResultInput {
                    view: context.view(),
                    pools: Pools::default(),
                    rows: 0,
                    outcome: Outcome::Read,
                },
                calls: Cell::new(0),
            };
            let baseline = context.memory().reserved_bytes();
            let scope = super::super::test_support::AllocationFaultScope::arm(ordinal);
            assert!(matches!(
                prepare_native(&REGISTRY, &source, context),
                Err(ConversionError::Owner(OwnerError::Allocation))
            ));
            assert_eq!(
                scope.receipt(),
                super::super::test_support::AllocationFaultReceipt {
                    matching_sites: ordinal,
                    fires: 1,
                }
            );
            assert_eq!(context.memory().reserved_bytes(), baseline);
        });
    }

    let final_checkpoint = with_checkpoint_context(0, |context, view| {
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools::default(),
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let baseline = context.memory().reserved_bytes();
        drop(prepare_native(&REGISTRY, &source, context).unwrap());
        assert_eq!(context.memory().reserved_bytes(), baseline);
        view.polls.get()
    });
    eprintln!("ZE-141 empty prepare final checkpoint ordinal={final_checkpoint}");
    assert_eq!(final_checkpoint, 5);
    with_checkpoint_context(final_checkpoint, |context, view| {
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools::default(),
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let baseline = context.memory().reserved_bytes();
        assert!(matches!(
            prepare_native(&REGISTRY, &source, context),
            Err(ConversionError::Owner(OwnerError::Runtime(
                RuntimeError::Value(QueryError::Cancelled)
            )))
        ));
        assert_eq!(view.polls.get(), final_checkpoint);
        assert_eq!(context.memory().reserved_bytes(), baseline);
    });

    #[derive(Debug)]
    struct FailedRun {
        driver: RuntimeError,
        conversion: Option<ConversionError>,
        counters: WorkCounters,
    }

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let shared_baseline = resources.reserved_bytes().unwrap();
    let payload_len = 70 * 1024;
    let node_count = 65536 / size_of::<ZeGraphNode>() + 1;
    assert!(node_count * size_of::<ZeGraphNode>() > 65536);
    let run = |cancel_at: usize, close_at: usize, limits: RuntimeLimits| {
        let memory = QueryMemory::new(&resources, 24 * 1024 * 1024).unwrap();
        let cancellation = CancelToken::new();
        let view = CheckpointView {
            token: QueryView::new(StoreInstanceId::new(143).unwrap(), GraphGeneration::new(0)),
            lease: store.snapshot().unwrap(),
            cancellation: cancellation.clone(),
            cancel_at: Cell::new(0),
            close_at: Cell::new(0),
            polls: Cell::new(0),
        };
        let control = QueryControl::Cancel(cancellation);
        let mut context = RuntimeContext::new(&view, &control, &memory, limits).unwrap();
        let mut bytes = QueryArena::new(context.memory(), payload_len + 4096).unwrap();
        for _ in 0..payload_len {
            bytes.push(b'z').unwrap();
        }
        let mut nodes = QueryArena::new(context.memory(), node_count).unwrap();
        for ordinal in 0..node_count {
            nodes
                .push(Node {
                    id: NodeId::new(ordinal as u128 + 1).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    generation: context.view().generation(),
                    key: None,
                    labels: Span::new(0, 0),
                    properties: Span::new(0, 0),
                    text: None,
                    vector: None,
                })
                .unwrap();
        }
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    bytes: bytes.as_slice(),
                    nodes: nodes.as_slice(),
                    ..Pools::default()
                },
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let baseline = context.memory().reserved_bytes();
        let result = with_plan(&mut context, |plan, context| {
            view.polls.set(0);
            view.cancel_at.set(cancel_at);
            view.close_at.set(close_at);
            let error = RefCell::new(None);
            let mut completion = Convert {
                source: &source,
                error: &error,
            };
            let mut pull = Rows {
                count: 0,
                done: false,
            };
            match execute_in(
                context,
                plan,
                &mut pull,
                &mut completion,
                ExecutionCapacity {
                    batch_rows: 1,
                    result_rows: 1,
                    ..ExecutionCapacity::default()
                },
            ) {
                Ok(execution) => {
                    let counters = execution.counters;
                    drop(finalize_native(execution));
                    Ok(counters)
                }
                Err(failure) => Err(FailedRun {
                    driver: failure.error,
                    conversion: error.into_inner(),
                    counters: failure.counters,
                }),
            }
        });
        assert_eq!(context.memory().reserved_bytes(), baseline);
        let polls = view.polls.get();
        drop(nodes);
        drop(bytes);
        drop(context);
        drop(memory);
        assert_eq!(resources.reserved_bytes().unwrap(), shared_baseline);
        (polls, result)
    };

    let (clean_polls, clean_result) = run(0, 0, RuntimeLimits::default());
    let clean_counters = clean_result.unwrap();
    assert!(clean_polls > node_count);
    let mut expected_prefixes = std::collections::BTreeSet::from([0_u64]);
    let mut prefix = 0_u64;
    for (element_size, count) in [
        (1, payload_len),
        (size_of::<Node>(), node_count),
        (1, payload_len),
        (size_of::<ZeGraphNode>(), node_count),
        (size_of::<ZeGraphWorkCounter>(), GLOBAL_WORK_COUNT),
    ] {
        let chunk_elements = (65536 / element_size).max(1);
        let mut remaining = count;
        while remaining != 0 {
            let chunk = remaining.min(chunk_elements);
            prefix += (chunk * element_size) as u64;
            expected_prefixes.insert(prefix);
            remaining -= chunk;
        }
    }
    assert_eq!(clean_counters.get(WorkKind::CopiedBytes), prefix);
    let mut observed_prefixes = std::collections::BTreeSet::new();
    for ordinal in 1..=clean_polls {
        let (polls, failure) = run(ordinal, 0, RuntimeLimits::default());
        assert_eq!(polls, ordinal);
        let failure = failure.expect_err("every observed checkpoint must cancel");
        assert!(
            matches!(failure.driver, RuntimeError::Value(QueryError::Cancelled))
                || matches!(
                    failure.conversion,
                    Some(ConversionError::Completed(CompletedError::Runtime(
                        RuntimeError::Value(QueryError::Cancelled)
                    ))) | Some(ConversionError::Owner(OwnerError::Runtime(
                        RuntimeError::Value(QueryError::Cancelled)
                    )))
                ),
            "checkpoint {ordinal} returned {failure:?}"
        );
        observed_prefixes.insert(failure.counters.get(WorkKind::CopiedBytes));
    }
    assert_eq!(observed_prefixes, expected_prefixes);
    eprintln!(
        "ZE-141 checkpoint sweep polls={clean_polls} node_descriptors={node_count} copied_prefixes={expected_prefixes:?}"
    );

    let native_bytes = payload_len + node_count * size_of::<Node>();
    let native_limit = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, 65535)
        .unwrap();
    let (_, native_failure) = run(0, 0, native_limit);
    let native_failure = native_failure.expect_err("native copied-work seam must refuse");
    assert!(matches!(
        native_failure.conversion,
        Some(ConversionError::Completed(CompletedError::Runtime(
            RuntimeError::Limit(WorkKind::CopiedBytes)
        )))
    ));
    assert_eq!(native_failure.counters.get(WorkKind::CopiedBytes), 0);

    let c_limit = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, native_bytes as u64)
        .unwrap();
    let (_, c_failure) = run(0, 0, c_limit);
    let c_failure = c_failure.expect_err("first C copied-work chunk must refuse");
    assert!(matches!(
        c_failure.conversion,
        Some(ConversionError::Owner(OwnerError::Runtime(
            RuntimeError::Limit(WorkKind::CopiedBytes)
        )))
    ));
    assert_eq!(
        c_failure.counters.get(WorkKind::CopiedBytes),
        native_bytes as u64
    );

    let (_, close_failure) = run(clean_polls, clean_polls, RuntimeLimits::default());
    let close_failure = close_failure.expect_err("final close/cancel must refuse");
    assert!(close_failure.conversion.is_none());
    assert!(matches!(
        close_failure.driver,
        RuntimeError::Value(QueryError::ReadCancelled)
    ));
    assert_eq!(
        close_failure.counters.get(WorkKind::CompletedBytes),
        size_of::<zeppelin_embed::property_graph::query::completed::CompletedGraphResult>() as u64
            + native_bytes as u64
    );

    with_checkpoint_limits(
        24 * 1024 * 1024,
        0,
        RuntimeLimits::default()
            .with_limit(WorkKind::CopiedBytes, 0)
            .unwrap(),
        |context, _| {
            let source = NativeSource {
                input: ResultInput {
                    view: context.view(),
                    pools: Pools::default(),
                    rows: 0,
                    outcome: Outcome::Read,
                },
                calls: Cell::new(0),
            };
            let baseline = context.memory().reserved_bytes();
            assert!(matches!(
                prepare_native(&REGISTRY, &source, context),
                Err(ConversionError::Owner(OwnerError::Runtime(
                    RuntimeError::Limit(WorkKind::CopiedBytes)
                )))
            ));
            assert_eq!(context.counters().get(WorkKind::CopiedBytes), 0);
            assert_eq!(context.memory().reserved_bytes(), baseline);
        },
    );

    let represented_native =
        size_of::<zeppelin_embed::property_graph::query::completed::CompletedGraphResult>() as u64;
    let represented_c = (GLOBAL_WORK_COUNT * size_of::<ZeGraphWorkCounter>()) as u64;
    for (kind, limit, completed_bytes) in [
        (WorkKind::CompletedBytes, represented_native - 1, 0),
        (
            WorkKind::CompletedAbiBytes,
            represented_c - 1,
            represented_native,
        ),
    ] {
        let limits = RuntimeLimits::default().with_limit(kind, limit).unwrap();
        with_checkpoint_limits(24 * 1024 * 1024, 0, limits, |context, _| {
            let source = NativeSource {
                input: ResultInput {
                    view: context.view(),
                    pools: Pools::default(),
                    rows: 0,
                    outcome: Outcome::Read,
                },
                calls: Cell::new(0),
            };
            let baseline = context.memory().reserved_bytes();
            let failure = with_plan(context, |plan, context| {
                let error = RefCell::new(None);
                let mut completion = Convert {
                    source: &source,
                    error: &error,
                };
                let mut pull = Rows {
                    count: 0,
                    done: false,
                };
                let failure = match execute_in(
                    context,
                    plan,
                    &mut pull,
                    &mut completion,
                    ExecutionCapacity {
                        batch_rows: 1,
                        result_rows: 1,
                        ..ExecutionCapacity::default()
                    },
                ) {
                    Err(failure) => failure,
                    Ok(_) => panic!("final completed-byte charge must refuse"),
                };
                assert!(error.into_inner().is_none());
                failure
            });
            assert!(matches!(failure.error, RuntimeError::Limit(observed) if observed == kind));
            assert_eq!(
                failure.counters.get(WorkKind::CompletedBytes),
                completed_bytes
            );
            assert_eq!(failure.counters.get(WorkKind::CompletedAbiBytes), 0);
            assert_eq!(context.memory().reserved_bytes(), baseline);
        });
    }

    use std::time::Duration;
    use zeppelin_embed::lifecycle::Deadline;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let resources = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&resources, 24 * 1024 * 1024).unwrap();
    let deadline = Deadline::after(Duration::ZERO).unwrap();
    let view = CheckpointView {
        token: QueryView::new(StoreInstanceId::new(144).unwrap(), GraphGeneration::new(0)),
        lease: store.snapshot().unwrap(),
        cancellation: CancelToken::new(),
        cancel_at: Cell::new(0),
        close_at: Cell::new(0),
        polls: Cell::new(0),
    };
    let control = QueryControl::Deadline(deadline);
    let baseline = memory.reserved_bytes();
    assert!(matches!(
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()),
        Err(RuntimeError::Value(QueryError::Timeout))
    ));
    assert_eq!(memory.reserved_bytes(), baseline);
}

#[test]
#[allow(
    clippy::drop_non_drop,
    reason = "the test deliberately ends every borrowed source lifetime before reading C backing"
)]
fn graph_result_native_private_drop_and_source_independence_are_heap_flat() {
    super::super::tests::with_context(|context| {
        let baseline = context.memory().reserved_bytes();
        let empty = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools::default(),
                rows: 0,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let unfinalized = execute_conversion(context, &empty, 0);
        let mut unfinalized_descriptor = unfinalized.output.response.descriptor();
        assert!(matches!(
            REGISTRY.free(&mut unfinalized_descriptor),
            Err(OwnerError::InvalidOwner)
        ));
        drop(unfinalized);
        assert!(matches!(
            REGISTRY.free(&mut unfinalized_descriptor),
            Err(OwnerError::InvalidOwner)
        ));
        assert_eq!(context.memory().reserved_bytes(), baseline);

        let finalized = finalize_native(execute_conversion(context, &empty, 0));
        let mut finalized_descriptor = finalized.response.descriptor();
        drop(finalized);
        assert!(matches!(
            REGISTRY.free(&mut finalized_descriptor),
            Err(OwnerError::InvalidOwner)
        ));
        assert_eq!(context.memory().reserved_bytes(), baseline);

        let values = vec![Value::String(Span::new(0, 5))];
        let bytes = b"alive".to_vec();
        let columns = vec![Column {
            name: Span::new(0, 0),
            kinds: ValueKinds::STRING,
        }];
        let cells = vec![ValueIndex(0)];
        let source = NativeSource {
            input: ResultInput {
                view: context.view(),
                pools: Pools {
                    values: &values,
                    bytes: &bytes,
                    columns: &columns,
                    cells: &cells,
                    ..Pools::default()
                },
                rows: 1,
                outcome: Outcome::Read,
            },
            calls: Cell::new(0),
        };
        let mut output = convert(context, &source, 1);
        drop(source);
        drop(values);
        drop(bytes);
        drop(columns);
        drop(cells);
        let copied = unsafe { output_slice(output.pool.bytes, output.pool.byte_count) };
        assert_eq!(copied, b"alive");
        let mut forged = output;
        forged.pool.byte_count += 1;
        assert!(matches!(
            REGISTRY.free(&mut forged),
            Err(OwnerError::InvalidOwner)
        ));
        let stale = output;
        REGISTRY.free(&mut output).unwrap();
        assert!(REGISTRY.free(&mut output).is_ok());
        let mut stale = stale;
        assert!(matches!(
            REGISTRY.free(&mut stale),
            Err(OwnerError::InvalidOwner)
        ));

        let ((), audit) = super::super::audit::run(0, false, || {
            for _ in 0..32 {
                let source = NativeSource {
                    input: ResultInput {
                        view: context.view(),
                        pools: Pools::default(),
                        rows: 0,
                        outcome: Outcome::Read,
                    },
                    calls: Cell::new(0),
                };
                let mut output = convert(context, &source, 0);
                REGISTRY.free(&mut output).unwrap();
            }
            for _ in 0..32 {
                let source = NativeSource {
                    input: ResultInput {
                        view: context.view(),
                        pools: Pools::default(),
                        rows: 0,
                        outcome: Outcome::Read,
                    },
                    calls: Cell::new(0),
                };
                drop(prepare_native(&REGISTRY, &source, context).unwrap());
            }
        });
        assert_eq!(audit.allocations, audit.frees);
        assert_eq!(audit.bytes, 0);
    });
}
