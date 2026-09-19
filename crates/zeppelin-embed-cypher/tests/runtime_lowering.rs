//! Controlled literal producer proves the compiled owner/driver seam only.
//! Native graph operators, public graph execution and TCK remain ZE-56.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod support;

use std::{cell::Cell, mem::size_of};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{MAX_VALUE_WORK, QueryError, QueryValue, QueryView, plan::*, resources::*, runtime::*},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{CompileLimits, ErrorKind, ResourceError, compile_read_in};

fn fixture(run: impl FnOnce(&Store, &QueryMemory<'_>)) {
    let path = support::unique_temp_dir("ze126-runtime");
    std::fs::create_dir(&path).unwrap();
    let store = Store::open(
        &path,
        OpenOptions::new()
            .with_max_resident_bytes(32 * 1024 * 1024)
            .with_reader_drain_timeout(std::time::Duration::ZERO),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    {
        let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
        let before = memory.reserved_bytes();
        run(&store, &memory);
        assert_eq!(memory.reserved_bytes(), before);
    }
    store.close().unwrap();
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}
struct View<'a> {
    token: QueryView,
    lease: SnapshotLease,
    polls: Cell<usize>,
    fire: Cell<usize>,
    start: Option<&'a std::sync::mpsc::SyncSender<()>>,
    cancel: CancelToken,
}
impl RetainedView for View<'_> {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        let count = self.polls.get() + 1;
        self.polls.set(count);
        if count == self.fire.get() {
            self.cancel.cancel();
            self.start.unwrap().send(()).unwrap();
            self.lease
                .wait_for_close_cancellation()
                .map_err(|_| QueryError::Control)?;
        }
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)
    }
}
fn view(store: &Store) -> View<'static> {
    View {
        token: QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0)),
        lease: store.snapshot().unwrap(),
        polls: Cell::new(0),
        fire: Cell::new(usize::MAX),
        start: None,
        cancel: CancelToken::new(),
    }
}
struct LiteralSource {
    node: PlanNodeId,
    value: i64,
    calls: usize,
}
impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for LiteralSource {
    fn node(&self) -> PlanNodeId {
        self.node
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
        out: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, RuntimeError> {
        self.calls += 1;
        context.charge(WorkKind::Expressions, 2)?;
        for _ in 0..3 {
            QueryValue::I64(7).equivalent(QueryValue::I64(7), context.values())?;
        }
        out.push_row(&[QueryValue::I64(self.value)], context)?;
        Ok(PullState::Done)
    }
}
struct Complete {
    calls: usize,
}
impl<'m, 'g> Completion<'m, 'g> for Complete {
    type Output = (i64, u64);
    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
        self.calls += 1;
        QueryValue::I64(9).equivalent(QueryValue::I64(9), context.values())?;
        let QueryValue::I64(value) = rows.value(0, 0).ok_or(RuntimeError::Batch)? else {
            return Err(RuntimeError::Batch);
        };
        FrozenOutput::new((value, context.values().work()), rows.rows(), 8, 0)
    }
}

#[test]
fn compiled_read_drains_through_the_same_runtime_context_and_real_owners() {
    fixture(|store, memory| {
        let mut view_charge = memory.reserve_external_capacity().unwrap();
        view_charge
            .reserve_additional(size_of::<View<'_>>())
            .unwrap();
        let retained = view(store);
        let control = QueryControl::Cancel(retained.cancel.clone());
        let mut context =
            RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default()).unwrap();
        for _ in 0..11 {
            QueryValue::I64(1)
                .equivalent(QueryValue::I64(1), context.values())
                .unwrap();
        }
        context.charge(WorkKind::Expressions, 5).unwrap();
        let expected_context = &context as *const _ as usize;
        for exhaust in [false, true] {
            let baseline = memory.reserved_bytes();
            compile_read_in("RETURN 7 AS answer",&[],CompileLimits::default(),memory,&mut context,|read,context| {
                assert_eq!(context as *const _ as usize,expected_context);
                let description=read.plan().description();
                let OperatorKind::Project(items)=description.operators[description.root.0 as usize].kind else{panic!("controlled one-column projection")};
                let Expression::Literal(Literal::I64(value))=description.expressions[items[0].expression.0 as usize] else{panic!("controlled literal source")};
                let mut inventory_charge=memory.reserve_external_capacity().unwrap();inventory_charge.reserve_additional(std::mem::size_of_val(read.owners())+size_of::<Vec<RetainedAllocation<'_>>>()).unwrap();
                let mut owners=Vec::new();owners.try_reserve_exact(read.owners().len()).unwrap();owners.extend_from_slice(read.owners());
                let before=memory.reserved_bytes();
                let inputs=QueryInputs::reserve(memory,RetentionInventory::vector(&owners).unwrap(),context.values()).unwrap();
                let credited=memory.reserved_bytes()-before;drop(inputs);
                // A conservative plain fact certificate must add precisely its
                // full real capacity, unlike the authentic QueryArena credit.
                let owner=owners[0];owners[0]=RetainedAllocation::plan_facts(read.plan()).unwrap();
                let duplicate=QueryInputs::reserve(memory,RetentionInventory::vector(&owners).unwrap(),context.values()).unwrap();
                assert_eq!(memory.reserved_bytes()-before,credited+description.operators.len()*size_of::<NodeFacts>());
                drop(duplicate);owners[0]=owner;
                let inputs=QueryInputs::reserve(memory,RetentionInventory::vector(&owners).unwrap(),context.values()).unwrap();
                let admitted=inputs.admit_plan(read.plan(),context.values()).unwrap();
                let prepared_work=context.values().work();assert!(prepared_work>11);
                if exhaust {while context.values().work()<MAX_VALUE_WORK-2 {QueryValue::I64(1).equivalent(QueryValue::I64(1),context.values()).unwrap();}}
                let mut source=LiteralSource {node:description.root,value,calls:0};let mut complete=Complete {calls:0};
                let result=execute_in(context,&admitted,&mut source,&mut complete,ExecutionCapacity {batch_rows:1,result_rows:1,..ExecutionCapacity::default()});
                if exhaust {
                    assert!(matches!(result.err().unwrap().error,RuntimeError::Value(QueryError::WorkLimit)));
                    assert_eq!((source.calls,complete.calls),(1,0));assert_eq!(context.values().work(),MAX_VALUE_WORK);
                } else {
                    let result=result.unwrap();assert_eq!((source.calls,complete.calls),(1,1));
                    // Three producer comparisons, two actual row copies, one completion comparison.
                    assert_eq!(result.output,(7,prepared_work+6));assert_eq!(context.values().work(),result.output.1);
                    assert_eq!(result.counters.get(WorkKind::Expressions),7);
                    assert_eq!(result.counters.get(WorkKind::CompletedRows),1);
                    eprintln!("prior=11 prepared={prepared_work} completed={} fact_bytes={} admission_delta={credited}",result.output.1,description.operators.len()*size_of::<NodeFacts>());
                }
                Ok(())
            }).unwrap();
            assert_eq!(memory.reserved_bytes(), baseline);
        }
    });
}

#[test]
fn compiled_read_parser_checkpoint_preserves_close_before_simultaneous_cancel() {
    fixture(|store, memory| {
        std::thread::scope(|scope| {
            let (start, wait) = std::sync::mpsc::sync_channel(0);
            let closing = scope.spawn(move || {
                wait.recv().unwrap();
                store.close().unwrap();
            });
            {
                let mut view_charge = memory.reserve_external_capacity().unwrap();
                view_charge
                    .reserve_additional(size_of::<View<'_>>())
                    .unwrap();
                let mut retained = view(store);
                retained.start = Some(&start);
                let control = QueryControl::Cancel(retained.cancel.clone());
                let mut context =
                    RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default())
                        .unwrap();
                retained.polls.set(0);
                retained.fire.set(8);
                let baseline = memory.reserved_bytes();
                let mut entered = false;
                let error = compile_read_in(
                    "RETURN 'parser bytes' AS x",
                    &[],
                    CompileLimits::default(),
                    memory,
                    &mut context,
                    |_, _| {
                        entered = true;
                        Ok(())
                    },
                )
                .unwrap_err();
                assert_eq!(
                    error.kind,
                    ErrorKind::Resource(ResourceError::ReadCancelled)
                );
                assert!(!entered);
                assert_eq!(retained.polls.get(), 8);
                assert_eq!(
                    context.values().work(),
                    0,
                    "failure occurs in compiler, before core validator"
                );
                assert_eq!(memory.reserved_bytes(), baseline);
            }
            closing.join().unwrap();
        })
    });
}

struct ChargedOutput<'m, 'g, 'd> {
    bytes: QueryArena<'m, 'g, u8>,
    dropped: &'d Cell<usize>,
}
impl Drop for ChargedOutput<'_, '_, '_> {
    fn drop(&mut self) {
        assert_eq!(self.bytes.as_slice(), &[7]);
        self.dropped.set(self.dropped.get() + 1);
    }
}
struct ChargedComplete<'m, 'g, 'd> {
    memory: &'m QueryMemory<'g>,
    dropped: &'d Cell<usize>,
}
impl<'m, 'g, 'd> Completion<'m, 'g> for ChargedComplete<'m, 'g, 'd> {
    type Output = ChargedOutput<'m, 'g, 'd>;
    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
        let mut bytes = QueryArena::new(self.memory, 8)?;
        bytes.push(7)?;
        FrozenOutput::new(
            ChargedOutput {
                bytes,
                dropped: self.dropped,
            },
            rows.rows(),
            8,
            0,
        )
    }
}

#[test]
fn compiled_read_final_check_discards_actual_completed_output_and_all_owners() {
    fixture(|store, memory| {
        std::thread::scope(|scope| {
            let (start, wait) = std::sync::mpsc::sync_channel(0);
            let closing = scope.spawn(move || {
                wait.recv().unwrap();
                store.close().unwrap();
            });
            {
                let mut view_charge = memory.reserve_external_capacity().unwrap();
                view_charge
                    .reserve_additional(size_of::<View<'_>>())
                    .unwrap();
                let mut retained = view(store);
                retained.start = Some(&start);
                let control = QueryControl::Cancel(retained.cancel.clone());
                let mut context =
                    RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default())
                        .unwrap();
                let dropped = Cell::new(0);
                let baseline = memory.reserved_bytes();
                let error = compile_read_in(
                    "RETURN 7 AS answer",
                    &[],
                    CompileLimits::default(),
                    memory,
                    &mut context,
                    |read, context| {
                        let mut inventory_charge = memory.reserve_external_capacity().unwrap();
                        inventory_charge
                            .reserve_additional(
                                std::mem::size_of_val(read.owners())
                                    + size_of::<Vec<RetainedAllocation<'_>>>(),
                            )
                            .unwrap();
                        let mut owners = Vec::new();
                        owners.try_reserve_exact(read.owners().len()).unwrap();
                        owners.extend_from_slice(read.owners());
                        let inputs = QueryInputs::reserve(
                            memory,
                            RetentionInventory::vector(&owners).unwrap(),
                            context.values(),
                        )
                        .unwrap();
                        let admitted = inputs.admit_plan(read.plan(), context.values()).unwrap();
                        let mut source = LiteralSource {
                            node: read.plan().description().root,
                            value: 7,
                            calls: 0,
                        };
                        let mut complete = ChargedComplete {
                            memory,
                            dropped: &dropped,
                        };
                        let output = execute_in(
                            context,
                            &admitted,
                            &mut source,
                            &mut complete,
                            ExecutionCapacity {
                                batch_rows: 1,
                                result_rows: 1,
                                ..ExecutionCapacity::default()
                            },
                        )
                        .unwrap()
                        .output;
                        assert_eq!(source.calls, 1);
                        assert_eq!(dropped.get(), 0);
                        assert!(memory.reserved_bytes() > baseline);
                        // The real driver's final check already passed. Fire only at
                        // the outer compiler callback-return boundary.
                        retained.fire.set(retained.polls.get() + 1);
                        Ok(output)
                    },
                );
                assert!(matches!(
                    error,
                    Err(zeppelin_embed_cypher::ParseError {
                        kind: ErrorKind::Resource(ResourceError::ReadCancelled),
                        ..
                    })
                ));
                assert_eq!(dropped.get(), 1);
                assert_eq!(memory.reserved_bytes(), baseline);
            }
            closing.join().unwrap();
        })
    });
}

#[test]
fn compiled_read_validation_reports_original_cumulative_work_exhaustion() {
    fixture(|store, memory| {
        let mut view_charge = memory.reserve_external_capacity().unwrap();
        view_charge
            .reserve_additional(size_of::<View<'_>>())
            .unwrap();
        let retained = view(store);
        let control = QueryControl::Cancel(retained.cancel.clone());
        let mut context =
            RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default()).unwrap();
        while context.values().work() < MAX_VALUE_WORK {
            QueryValue::I64(1)
                .equivalent(QueryValue::I64(1), context.values())
                .unwrap();
        }
        let baseline = memory.reserved_bytes();
        let mut entered = false;
        let error = compile_read_in(
            "RETURN 7 AS answer",
            &[],
            CompileLimits::default(),
            memory,
            &mut context,
            |_, _| {
                entered = true;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::WorkLimit));
        assert!(!entered);
        assert_eq!(context.values().work(), MAX_VALUE_WORK);
        assert_eq!(memory.reserved_bytes(), baseline);
    });
}
