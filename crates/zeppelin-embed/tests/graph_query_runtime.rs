#![allow(clippy::expect_used, clippy::panic)]

use zeppelin_embed::lifecycle::{OpenOptions, Store, StoreError};
use zeppelin_embed::property_graph::resources::{GraphResources, MAX_GRAPH_RESIDENT_BYTES};

#[test]
fn graph_resources_share_store_reservations_and_record_transient_peak() {
    let root = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(MAX_GRAPH_RESIDENT_BYTES),
    )
    .expect("bounded store");
    let resources = GraphResources::from_store(&store).expect("same accounting");
    let other = GraphResources::from_store(&store).expect("same owner");
    let baseline = resources.reserved_bytes().expect("initial bytes");
    let mut first = resources.reserve(96).expect("writer participant");
    let second = other.reserve(64).expect("query participant");
    assert_eq!(
        store.stats().expect("stats").resident_owned_bytes,
        baseline + 160
    );
    first.resize(128).expect("capacity reconciliation");
    assert_eq!(resources.reserved_bytes().expect("live"), baseline + 192);
    let peak = resources.peak_reserved_bytes().expect("peak");
    assert!(peak >= baseline + 192);
    assert!(matches!(
        first.resize(MAX_GRAPH_RESIDENT_BYTES as usize),
        Err(StoreError::BudgetExceeded { .. })
    ));
    assert_eq!(first.bytes(), 128);
    assert_eq!(
        resources.reserved_bytes().expect("unchanged"),
        baseline + 192
    );
    drop(second);
    drop(first);
    assert_eq!(resources.reserved_bytes().expect("released"), baseline);
    assert_eq!(resources.peak_reserved_bytes().expect("monotone"), peak);
    store.close().expect("close");
}

#[test]
fn query_arena_charges_actual_capacity_overlap_and_releases_on_failure() {
    use zeppelin_embed::property_graph::query::resources::{QueryArena, QueryMemory};
    let root = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(MAX_GRAPH_RESIDENT_BYTES),
    )
    .expect("bounded store");
    let shared = GraphResources::from_store(&store).expect("shared resources");
    let baseline = shared.reserved_bytes().expect("baseline");
    {
        let memory = QueryMemory::new(&shared, 1024).expect("query allowance");
        let base = memory.reserved_bytes();
        let mut arena = QueryArena::<u64>::new(&memory, 16).expect("fixed arena");
        assert_eq!(arena.capacity(), 16);
        assert_eq!(arena.heap_bytes(), 128);
        assert_eq!(memory.reserved_bytes(), base + arena.reserved_bytes());
        for value in 0..16 {
            arena.push(value).expect("fixed slots");
        }
        assert!(arena.push(17).is_err());
        assert_eq!(arena.len(), 16);
        let before = memory.reserved_bytes();
        assert!(QueryArena::<u64>::new(&memory, 128).is_err());
        assert_eq!(memory.reserved_bytes(), before);
        let mut replacement =
            QueryArena::new(&memory, 32).expect("simultaneously reserved replacement");
        assert!(memory.peak_reserved_bytes() >= before + replacement.reserved_bytes());
        for value in arena.as_slice() {
            replacement.push(*value).expect("bounded fixture copy");
        }
        assert_eq!(replacement.as_slice(), &(0..16).collect::<Vec<_>>());
        assert_eq!(replacement.capacity(), 32);
        drop(arena);
        drop(replacement);
        assert_eq!(memory.reserved_bytes(), base);
    }
    assert_eq!(shared.reserved_bytes().expect("all released"), baseline);
    store.close().expect("close");
}

#[test]
fn retained_input_capabilities_charge_spare_capacity_once_and_reject_missing_spans() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
    use zeppelin_embed::property_graph::query::resources::{
        QueryInputs, QueryMemory, RetainedAllocation, RetentionInventory,
    };
    use zeppelin_embed::property_graph::query::{QueryView, ValueContext};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    let root = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared");
    let memory = QueryMemory::new(&shared, 8192).expect("query memory");
    let mut text = String::with_capacity(1024);
    text.push_str("shared");
    let numbers = Vec::<u64>::with_capacity(32);
    let unrelated = [1u8; 8];
    let view = QueryView::new(
        StoreInstanceId::new(1).expect("store id"),
        GraphGeneration::new(0),
    );
    let control = QueryControl::Cancel(CancelToken::new());
    let mut values = ValueContext::new(&view, &control, 10000).expect("control");
    let proofs = [
        RetainedAllocation::string(&text).expect("string owner"),
        RetainedAllocation::string(&text).expect("alias"),
        RetainedAllocation::vector(&numbers).expect("vector owner"),
    ];
    let base = memory.reserved_bytes();
    {
        let inputs = QueryInputs::reserve(&memory, RetentionInventory::array(&proofs), &mut values)
            .expect("retained owners");
        assert_eq!(
            inputs.backing_bytes(),
            text.capacity() + numbers.capacity() * 8
        );
        inputs
            .verify_span(text.as_bytes(), &mut values)
            .expect("included substring");
        assert!(inputs.verify_span(&unrelated, &mut values).is_err());
        assert_eq!(
            memory.reserved_bytes(),
            base + 1280
                + std::mem::size_of_val(&inputs)
                + proofs.len()
                    * std::mem::size_of::<
                        zeppelin_embed::property_graph::query::plan::RetainedRegion,
                    >(),
            "embedded region-arena controls must not be charged twice"
        );
    }
    assert_eq!(memory.reserved_bytes(), base);
    store.close().expect("close");
}

#[test]
fn runtime_counters_enforce_limits_before_work_and_keep_one_view_control() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl, SnapshotLease};
    use zeppelin_embed::property_graph::query::resources::QueryMemory;
    use zeppelin_embed::property_graph::query::runtime::{
        RetainedView, RuntimeContext, RuntimeLimits, WorkKind,
    };
    use zeppelin_embed::property_graph::query::{QueryError, QueryView};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    struct View {
        token: QueryView,
        lease: SnapshotLease,
    }
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.token
        }
        fn check_active(&self) -> Result<(), QueryError> {
            self.lease
                .check_active()
                .map_err(|_| QueryError::ReadCancelled)
        }
    }
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("accounting");
    let memory = QueryMemory::new(&shared, 8192).expect("query");
    let view = View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("real lifecycle lease"),
    };
    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::OperatorRows, 2)
        .expect("tighten");
    assert!(
        RuntimeLimits::default()
            .with_limit(WorkKind::OperatorRows, 4_000_001)
            .is_err()
    );
    let base = memory.reserved_bytes();
    {
        let mut runtime = RuntimeContext::new(&view, &control, &memory, limits).expect("runtime");
        runtime.charge(WorkKind::OperatorRows, 2).expect("at cap");
        assert!(runtime.charge(WorkKind::OperatorRows, 1).is_err());
        assert_eq!(runtime.counters().get(WorkKind::OperatorRows), 2);
        runtime
            .charge(WorkKind::VectorBytes, 64)
            .expect("independent counter");
        token.cancel();
        assert!(runtime.charge(WorkKind::VectorBytes, 1).is_err());
        assert_eq!(runtime.counters().get(WorkKind::VectorBytes), 64);
    }
    assert_eq!(memory.reserved_bytes(), base);
    drop(view);
    store.close().expect("drained");
}

#[test]
fn execution_plan_requires_retained_owners_for_every_visible_span() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
    use zeppelin_embed::property_graph::query::plan::*;
    use zeppelin_embed::property_graph::query::resources::{
        QueryInputs, QueryMemory, RetainedAllocation, RetentionInventory,
    };
    use zeppelin_embed::property_graph::query::{QueryView, ValueContext};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared");
    let memory = QueryMemory::new(&shared, 131072).expect("memory");
    let operators = [Operator {
        inputs: &[],
        kind: OperatorKind::Unit,
    }];
    let mut facts = Vec::with_capacity(16);
    facts.push(NodeFacts::default());
    let actual_fact_bytes = facts.capacity() * std::mem::size_of::<NodeFacts>();
    let mut regions = vec![
        RetainedRegion::slice(&operators).expect("operators"),
        RetainedRegion::vector(&facts).expect("facts"),
    ];
    regions.sort();
    let view = QueryView::new(
        StoreInstanceId::new(1).expect("identity"),
        GraphGeneration::new(0),
    );
    let control = QueryControl::Cancel(CancelToken::new());
    let mut values = ValueContext::new(&view, &control, 10000).expect("control");
    let plan = GraphPlan::validate_with_fact_vec(
        PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(0),
            eager_searches: &[],
        },
        &mut facts,
        PlanFootprint::declared(100000),
        PlanBacking::vector(&regions).expect("validation proof"),
        &mut values,
    )
    .expect("typed plan");
    let missing = [RetainedAllocation::array(&operators).expect("operators")];
    let incomplete =
        QueryInputs::reserve(&memory, RetentionInventory::array(&missing), &mut values)
            .expect("partial owners");
    assert!(incomplete.admit_plan(&plan, &mut values).is_err());
    // The private certificate captures full Vec capacity before the retained
    // validation loan, so no second mutable/immutable borrow is manufactured.
    let complete = [
        RetainedAllocation::array(&operators).expect("operators"),
        RetainedAllocation::plan_facts(&plan).expect("actual facts owner"),
    ];
    let inputs = QueryInputs::reserve(&memory, RetentionInventory::array(&complete), &mut values)
        .expect("complete owners");
    assert_eq!(
        inputs.backing_bytes(),
        actual_fact_bytes + std::mem::size_of_val(&operators)
    );
    let before_admit = memory.reserved_bytes();
    let input_controls = std::mem::size_of_val(&inputs);
    let admitted = inputs
        .admit_plan(&plan, &mut values)
        .expect("runtime-owned plan");
    assert_eq!(admitted.plan().description().root, PlanNodeId(0));
    assert_eq!(
        memory.reserved_bytes() - before_admit,
        std::mem::size_of_val(&plan) + std::mem::size_of_val(&admitted) - input_controls,
        "plan and runtime descriptor storage must be charged too"
    );
    drop(admitted);
    let smaller = QueryMemory::new(&shared, 80000).expect("visible-only size would fit");
    let smaller_inputs =
        QueryInputs::reserve(&smaller, RetentionInventory::array(&complete), &mut values)
            .expect("backing fits before scratch");
    assert!(
        smaller_inputs.admit_plan(&plan, &mut values).is_err(),
        "actual spare capacity plus validation scratch must not undercharge"
    );
    assert_eq!(smaller.reserved_bytes(), std::mem::size_of_val(&smaller));
    let raw = GraphPlan::validate(
        PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(0),
            eager_searches: &[],
        },
        &mut facts,
        PlanFootprint::declared(100000),
        PlanBacking::vector(&regions).expect("raw backing"),
        &mut values,
    )
    .expect("structural raw-slice plan");
    assert!(
        RetainedAllocation::plan_facts(&raw).is_err(),
        "a declaration cannot upgrade raw facts into an actual-capacity certificate"
    );
    let raw_inputs =
        QueryInputs::reserve(&memory, RetentionInventory::array(&missing), &mut values)
            .expect("raw inputs");
    assert!(raw_inputs.admit_plan(&raw, &mut values).is_err());
    store.close().expect("close");
}

#[test]
fn flat_batches_preserve_scalar_bags_and_fail_before_partial_row_exposure() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl, SnapshotLease};
    use zeppelin_embed::property_graph::query::resources::QueryMemory;
    use zeppelin_embed::property_graph::query::runtime::{
        RetainedView, RowBatch, RuntimeContext, RuntimeLimits, WorkKind,
    };
    use zeppelin_embed::property_graph::query::{QueryError, QueryValue, QueryView};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    struct View {
        token: QueryView,
        lease: SnapshotLease,
    }
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.token
        }
        fn check_active(&self) -> Result<(), QueryError> {
            self.lease
                .check_active()
                .map_err(|_| QueryError::ReadCancelled)
        }
    }
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(MAX_GRAPH_RESIDENT_BYTES),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("accounting");
    let memory = QueryMemory::new(&shared, 8192).expect("query");
    let view = View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("real lifecycle lease"),
    };
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).expect("runtime");
    let mut batch = RowBatch::new(&context, 2, 2, 64).expect("flat batch");
    let row = [QueryValue::I64(9007199254740993), QueryValue::Null];
    batch.push_row(&row, &mut context).expect("first row");
    batch
        .push_row(&row, &mut context)
        .expect("duplicate bag row");
    assert_eq!(batch.rows(), 2);
    assert!(matches!(
        batch.value(0, 0),
        Some(QueryValue::I64(9007199254740993))
    ));
    assert!(matches!(batch.value(1, 1), Some(QueryValue::Null)));
    let error = batch
        .push_row(&row, &mut context)
        .expect_err("row capacity");
    assert!(matches!(
        error,
        zeppelin_embed::property_graph::query::runtime::RuntimeError::BatchCapacity
    ));
    assert_eq!(batch.rows(), 2);
    assert_eq!(context.counters().get(WorkKind::RowsOut), 2);
    assert_eq!(context.counters().get(WorkKind::CopiedBytes), 16);
    batch.clear();
    assert_eq!(batch.rows(), 0);
    let error = batch
        .push_row(&[QueryValue::I64(1)], &mut context)
        .expect_err("invalid shape");
    assert!(matches!(
        error,
        zeppelin_embed::property_graph::query::runtime::RuntimeError::Batch
    ));
    assert_eq!(batch.rows(), 0);
    drop(batch);
    drop(context);
    drop(view);
    store.close().expect("close");
}

#[test]
fn flat_variable_arenas_own_nested_strings_and_preserve_packed_full_ids() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl, SnapshotLease};
    use zeppelin_embed::property_graph::query::resources::QueryMemory;
    use zeppelin_embed::property_graph::query::runtime::{
        ArenaCapacity, RetainedView, RowBatch, RuntimeContext, RuntimeLimits,
    };
    use zeppelin_embed::property_graph::query::{QueryError, QueryList, QueryValue, QueryView};
    use zeppelin_embed::property_graph::{GraphGeneration, NodeId, StoreInstanceId};
    struct View {
        token: QueryView,
        lease: SnapshotLease,
    }
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.token
        }
        fn check_active(&self) -> Result<(), QueryError> {
            self.lease
                .check_active()
                .map_err(|_| QueryError::ReadCancelled)
        }
    }
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("accounting");
    let memory = QueryMemory::new(&shared, 16384).expect("query");
    let view = View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("lease"),
    };
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).expect("runtime");
    let mut batch = RowBatch::with_arenas(
        &context,
        2,
        1,
        4096,
        ArenaCapacity {
            string_bytes: 64,
            list_cells: 8,
            node_ids: 2,
            relationship_ids: 0,
        },
    )
    .expect("owned flat arenas");
    {
        let text = String::from("a\0界");
        let child = [QueryValue::String(&text), QueryValue::Null];
        let inner = QueryList::new(&child, context.values()).expect("nested list");
        let outer_values = [
            QueryValue::List(inner),
            QueryValue::F64(f64::from_bits(0x7ff8_0000_0000_0055)),
        ];
        let outer = QueryList::new(&outer_values, context.values()).expect("outer list");
        let ids = [
            NodeId::new(7).expect("id"),
            NodeId::new((1u128 << 100) | 7).expect("full id"),
        ];
        let packed = QueryList::nodes(&view.token, &ids, context.values()).expect("packed list");
        batch
            .push_row(
                &[QueryValue::List(outer), QueryValue::List(packed)],
                &mut context,
            )
            .expect("copy owns all backing");
    }
    let Some(QueryValue::List(outer)) = batch.value(0, 0) else {
        panic!("outer list");
    };
    let Some(QueryValue::List(inner)) = outer.get(0) else {
        panic!("inner list");
    };
    assert!(matches!(inner.get(0), Some(QueryValue::String("a\0界"))));
    assert!(matches!(inner.get(1), Some(QueryValue::Null)));
    assert!(
        matches!(outer.get(1), Some(QueryValue::F64(v)) if v.to_bits() == 0x7ff8_0000_0000_0055)
    );
    let Some(QueryValue::List(ids)) = batch.value(0, 1) else {
        panic!("packed ids");
    };
    assert_eq!(ids.borrowed_bytes(), 32);
    assert!(
        matches!(ids.get(1), Some(QueryValue::NodeRef(v)) if v.id().get() == (1u128 << 100) | 7)
    );
    assert_eq!(batch.arena_usage().node_ids, 2);
    assert_eq!(batch.arena_usage().list_cells, 4);
    drop(batch);
    drop(context);
    drop(view);
    store.close().expect("close");
}

#[test]
fn failed_variable_reservation_does_not_count_uncopied_bytes() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
    use zeppelin_embed::property_graph::query::resources::QueryMemory;
    use zeppelin_embed::property_graph::query::runtime::{
        ArenaCapacity, RetainedView, RowBatch, RuntimeContext, RuntimeLimits, WorkKind,
    };
    use zeppelin_embed::property_graph::query::{QueryError, QueryValue, QueryView};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    struct View(QueryView);
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.0
        }
        fn check_active(&self) -> Result<(), QueryError> {
            Ok(())
        }
    }
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(MAX_GRAPH_RESIDENT_BYTES),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("accounting");
    let memory = QueryMemory::new(&shared, 8192).expect("query");
    let view = View(QueryView::new(
        StoreInstanceId::new(1).expect("identity"),
        GraphGeneration::new(0),
    ));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).expect("runtime");
    let mut batch = RowBatch::with_arenas(
        &context,
        1,
        1,
        64,
        ArenaCapacity {
            string_bytes: 1,
            ..ArenaCapacity::default()
        },
    )
    .expect("tiny capacity");
    let error = batch
        .push_row(&[QueryValue::String("three")], &mut context)
        .expect_err("string arena capacity");
    assert!(matches!(
        error,
        zeppelin_embed::property_graph::query::runtime::RuntimeError::BatchCapacity
    ));
    assert_eq!(batch.rows(), 0);
    assert_eq!(batch.arena_usage(), ArenaCapacity::default());
    assert_eq!(
        context.counters().get(WorkKind::CopiedBytes),
        0,
        "failed fixed-capacity check performed no copy"
    );
    drop(batch);
    drop(context);
    store.close().expect("close");
}

#[test]
fn query_and_writer_share_one_backing_charge_with_nested_local_limits() {
    use zeppelin_embed::property_graph::query::resources::QueryMemory;
    let root = tempfile::tempdir().expect("fixture");
    let other_root = tempfile::tempdir().expect("other fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let other_store = Store::open(
        other_root.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("other store");
    let shared = GraphResources::from_store(&store).expect("shared");
    let other = GraphResources::from_store(&other_store).expect("foreign shared");
    let memory = QueryMemory::new(&shared, 4096).expect("query");
    let backing = shared.reserve(1024).expect("writer-owned backing");
    let foreign = other.reserve(1024).expect("foreign backing");
    let local_before = memory.reserved_bytes();
    let shared_before = shared.reserved_bytes().expect("shared baseline");
    let Err((_, foreign)) = memory.adopt_shared(foreign) else {
        panic!("foreign owner must reject")
    };
    assert_eq!(
        foreign.bytes(),
        1024,
        "failure returns the original sole owner"
    );
    let too_small = QueryMemory::new(&shared, 1024).expect("small local allowance");
    let Err((_, backing)) = too_small.adopt_shared(backing) else {
        panic!("local cap must reject")
    };
    assert_eq!(backing.bytes(), 1024);
    assert_eq!(
        too_small.reserved_bytes(),
        std::mem::size_of_val(&too_small)
    );
    drop(too_small);
    {
        let joint = memory
            .adopt_shared(backing)
            .unwrap_or_else(|(error, _)| panic!("{error}"));
        let moved = joint;
        assert_eq!(moved.bytes(), 1024);
        assert_eq!(
            memory.reserved_bytes(),
            local_before + 1024 + std::mem::size_of_val(&moved)
        );
        assert_eq!(
            shared.reserved_bytes().expect("one backing charge"),
            shared_before + std::mem::size_of_val(&moved) as u64
        );
    }
    assert_eq!(memory.reserved_bytes(), local_before);
    assert_eq!(
        shared.reserved_bytes().expect("joint released"),
        shared_before - 1024
    );
    store.close().expect("close");
    other_store.close().expect("other close");
}

#[test]
fn pull_driver_drains_private_batches_before_completion() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl, SnapshotLease};
    use zeppelin_embed::property_graph::query::plan::*;
    use zeppelin_embed::property_graph::query::resources::{
        QueryInputs, QueryMemory, RetainedAllocation, RetentionInventory,
    };
    use zeppelin_embed::property_graph::query::runtime::*;
    use zeppelin_embed::property_graph::query::{QueryError, QueryView, ValueContext};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    struct View {
        token: QueryView,
        lease: SnapshotLease,
    }
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.token
        }
        fn check_active(&self) -> Result<(), QueryError> {
            self.lease
                .check_active()
                .map_err(|_| QueryError::ReadCancelled)
        }
    }
    struct Source(u8);
    impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for Source {
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
            context.charge(WorkKind::OperatorRows, 2)?;
            output.push_row(&[], context)?;
            self.0 += 1;
            Ok(if self.0 == 2 {
                PullState::Done
            } else {
                PullState::More
            })
        }
    }
    struct Freeze;
    impl<'m, 'g> Completion<'m, 'g> for Freeze {
        type Output = usize;
        fn complete<'v>(
            &mut self,
            rows: &PreparedRows<'v, 'm, 'g>,
            context: &mut RuntimeContext<'v, 'm, 'g>,
        ) -> Result<FrozenOutput<usize>, RuntimeError> {
            context.checkpoint()?;
            FrozenOutput::new(rows.rows(), rows.rows(), 0, 0)
        }
    }
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("accounting");
    let memory = QueryMemory::new(&shared, 131072).expect("query");
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
    let mut values = ValueContext::new(&token, &control, 10000).expect("values");
    let plan = GraphPlan::validate_with_fact_vec(
        PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(0),
            eager_searches: &[],
        },
        &mut facts,
        PlanFootprint::declared(100000),
        PlanBacking::vector(&regions).expect("proof"),
        &mut values,
    )
    .expect("plan");
    let owners = [
        RetainedAllocation::array(&operators).expect("operators"),
        RetainedAllocation::plan_facts(&plan).expect("facts"),
    ];
    let admitted = QueryInputs::reserve(&memory, RetentionInventory::array(&owners), &mut values)
        .expect("owners")
        .admit_plan(&plan, &mut values)
        .expect("runtime plan");
    let view = View {
        token: QueryView::new(token.store(), token.generation()),
        lease: store.snapshot().expect("lease"),
    };
    let base = memory.reserved_bytes();
    #[cfg(feature = "allocation-audit")]
    let legacy = {
        let directory = tempfile::tempdir().expect("independent cold legacy fixture");
        let legacy_store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
        )
        .expect("legacy store");
        let lease = legacy_store.snapshot().expect("cold legacy lease");
        let (_, cold) = zeppelin_embed::adversarial_test_support::audit_engine_path(|| drop(lease));
        let lease = legacy_store.snapshot().expect("second legacy lease");
        let (_, second) =
            zeppelin_embed::adversarial_test_support::audit_engine_path(|| drop(lease));
        assert_eq!(
            second.allocations, 0,
            "baseline must be first-use lease release only"
        );
        assert_eq!(
            cold.attributed_bytes, 0,
            "baseline executes no graph allocation path"
        );
        legacy_store.close().expect("legacy close");
        cold
    };
    let run = || {
        execute(
            view,
            &control,
            &memory,
            &admitted,
            &mut Source(0),
            &mut Freeze,
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 2,
                ..ExecutionCapacity::default()
            },
            RuntimeLimits::default(),
        )
    };
    #[cfg(feature = "allocation-audit")]
    let (result, audit) = zeppelin_embed::adversarial_test_support::audit_engine_path(run);
    #[cfg(not(feature = "allocation-audit"))]
    let result = run();
    let result = result.expect("complete execution");
    #[cfg(feature = "allocation-audit")]
    {
        assert_eq!(
            audit.unattributed_bytes, legacy.unattributed_bytes,
            "runtime must add no unattributed allocation beyond independently measured cold legacy lease release"
        );
        assert_eq!(
            audit.allocations,
            legacy.allocations + 2,
            "two reserved offset arrays plus independently measured cold lease release"
        );
        assert_eq!(audit.attributed_bytes, 48);
        println!(
            "runtime audit: graph_attributed={} graph_allocations=2 legacy_unattributed={} legacy_allocations={} second_lease_allocations=0",
            audit.attributed_bytes, legacy.unattributed_bytes, legacy.allocations
        );
    }
    assert_eq!(result.output, 2);
    assert_eq!(result.counters.get(WorkKind::OperatorRows), 4);
    assert_eq!(result.counters.get(WorkKind::RowsOut), 2);
    assert_eq!(result.counters.get(WorkKind::CompletedRows), 2);
    assert_eq!(memory.reserved_bytes(), base);
    assert!(result.peak_query_bytes > base);
    store.close().expect("driver released its lease");
}

#[test]
fn packed_eight_mib_query_list_fits_actual_memory_but_not_four_mib_result_payload() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl, SnapshotLease};
    use zeppelin_embed::property_graph::query::resources::*;
    use zeppelin_embed::property_graph::query::runtime::*;
    use zeppelin_embed::property_graph::query::{
        QueryError, QueryList, QueryValue, QueryView, ValueContext,
    };
    use zeppelin_embed::property_graph::{GraphGeneration, NodeId, StoreInstanceId};
    struct View {
        token: QueryView,
        lease: SnapshotLease,
    }
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.token
        }
        fn check_active(&self) -> Result<(), QueryError> {
            self.lease
                .check_active()
                .map_err(|_| QueryError::ReadCancelled)
        }
    }
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(32 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared");
    let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).expect("query");
    let retained = View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("lease"),
    };
    let control = QueryControl::Cancel(CancelToken::new());
    let ids = vec![NodeId::new((1u128 << 100) | 7).expect("full ID"); 524288];
    let owners = [RetainedAllocation::vector(&ids).expect("real capacity")];
    let mut values = ValueContext::new(&retained.token, &control, 8_000_000).expect("control");
    let inputs = QueryInputs::reserve(&memory, RetentionInventory::array(&owners), &mut values)
        .expect("retained input");
    assert_eq!(inputs.backing_bytes(), 8_388_608);
    let mut context = RuntimeContext::new(&retained, &control, &memory, RuntimeLimits::default())
        .expect("context");
    let list = QueryList::nodes(&retained.token, &ids, context.values()).expect("exact list cap");
    {
        let mut batch = RowBatch::with_arenas(
            &context,
            1,
            1,
            8_388_608,
            ArenaCapacity {
                node_ids: 524288,
                ..ArenaCapacity::default()
            },
        )
        .expect("packed flat storage");
        batch
            .push_row(&[QueryValue::List(list)], &mut context)
            .expect("8 MiB fits query intermediate");
        assert_eq!(batch.arena_usage().node_ids, 524288);
        assert_eq!(batch.payload_bytes(), 8_388_608);
        assert!(
            memory.peak_reserved_bytes() < 17 * 1024 * 1024,
            "packed input and owned arena must not expand into per-ID fat values"
        );
        assert!(
            matches!(batch.value(0, 0), Some(QueryValue::List(v)) if matches!(v.get(524287), Some(QueryValue::NodeRef(id)) if id.id().get() == (1u128 << 100) | 7))
        );
    }
    {
        let mut result = RowBatch::with_arenas(
            &context,
            1,
            1,
            4_194_304,
            ArenaCapacity {
                node_ids: 524288,
                ..ArenaCapacity::default()
            },
        )
        .expect("reserved result copy storage");
        assert!(
            result
                .push_row(&[QueryValue::List(list)], &mut context)
                .is_err()
        );
        assert_eq!(result.rows(), 0);
        assert_eq!(result.arena_usage(), ArenaCapacity::default());
    }
    drop(context);
    drop(retained);
    drop(inputs);
    store.close().expect("close");
}

#[test]
fn cancellation_inside_byte_copy_rolls_back_cells_and_preserves_actual_work() {
    use std::cell::Cell;
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl, SnapshotLease};
    use zeppelin_embed::property_graph::query::resources::QueryMemory;
    use zeppelin_embed::property_graph::query::runtime::*;
    use zeppelin_embed::property_graph::query::{QueryError, QueryValue, QueryView};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    struct View {
        token: QueryView,
        lease: SnapshotLease,
        cancel: CancelToken,
        checks: Cell<u8>,
        armed: Cell<bool>,
        fires: Cell<u8>,
    }
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.token
        }
        fn check_active(&self) -> Result<(), QueryError> {
            self.lease
                .check_active()
                .map_err(|_| QueryError::ReadCancelled)?;
            if self.armed.get() {
                self.checks.set(self.checks.get() + 1);
                if self.checks.get() == 8 {
                    self.cancel.cancel();
                    self.fires.set(self.fires.get() + 1);
                }
            }
            Ok(())
        }
    }
    for fault in [true, false] {
        let root = tempfile::tempdir().expect("fixture");
        let store = Store::open(
            root.path(),
            OpenOptions::new().with_max_resident_bytes(1024 * 1024),
        )
        .expect("store");
        let shared = GraphResources::from_store(&store).expect("shared");
        let memory = QueryMemory::new(&shared, 512 * 1024).expect("query");
        let cancel = CancelToken::new();
        let control = QueryControl::Cancel(cancel.clone());
        let retained = View {
            token: QueryView::new(
                StoreInstanceId::new(1).expect("identity"),
                GraphGeneration::new(0),
            ),
            lease: store.snapshot().expect("lease"),
            cancel,
            checks: Cell::new(0),
            armed: Cell::new(false),
            fires: Cell::new(0),
        };
        let mut context =
            RuntimeContext::new(&retained, &control, &memory, RuntimeLimits::default())
                .expect("runtime");
        let mut batch = RowBatch::with_arenas(
            &context,
            1,
            1,
            131073,
            ArenaCapacity {
                string_bytes: 131073,
                ..ArenaCapacity::default()
            },
        )
        .expect("copy arena");
        // The first 64 KiB copy boundary falls inside this three-byte character.
        let mut text = "a".repeat(65535);
        text.push('界');
        text.push_str(&"b".repeat(65535));
        retained.armed.set(fault);
        let outcome = batch.push_row(&[QueryValue::String(&text)], &mut context);
        if fault {
            assert!(matches!(
                outcome,
                Err(RuntimeError::Value(QueryError::Cancelled))
            ));
            assert_eq!(batch.rows(), 0);
            assert_eq!(batch.arena_usage(), ArenaCapacity::default());
            assert_eq!(context.counters().get(WorkKind::CopiedBytes), 131072);
            assert_eq!(retained.fires.get(), 1);
        } else {
            outcome.expect("same-input clean control");
            assert_eq!(batch.rows(), 1);
            assert!(matches!(batch.value(0, 0), Some(QueryValue::String(value)) if value == text));
            assert_eq!(context.counters().get(WorkKind::CopiedBytes), 131073);
            assert_eq!(retained.fires.get(), 0);
        }
        drop(batch);
        drop(context);
        drop(retained);
        store.close().expect("close");
    }
}
