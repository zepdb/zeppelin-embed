//! Tooling-only directed evidence for native relational occurrence barriers.

#![allow(
    dead_code,
    reason = "root re-exports the directed helper while integrating registration"
)]

use super::super::{NativePattern, PatternCapacity};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{
    CancelToken, Deadline, ManualMonotonicClock, OpenOptions, QueryControl, Store,
};
use crate::property_graph::query::eligibility::Eligibility;
use crate::property_graph::query::eligibility::EligibleNodeSet;
use crate::property_graph::query::expression::{ExpressionCapacity, ExpressionFailure};
use crate::property_graph::query::plan::{
    AggregateExpression, BinaryExpression, Direction, ExprId, Expression, Literal, NodeFacts,
    Operator, OperatorKind, PatternId, PlanBacking, PlanDescription, PlanFootprint, PlanNodeId,
    Projection, RetainedRegion, SearchCallId, SearchOutputs, SearchRequest, SlotId, SortKey,
    VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{
    ArenaCapacity, Completion, Execution, ExecutionCapacity, FrozenOutput, NativeExecutionError,
    PreparedRows, RuntimeContext, RuntimeError, RuntimeFailure, RuntimeLimits, WorkKind,
    execute_in,
};
use crate::property_graph::query::{Arithmetic, QueryValue};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::allocation::OsEntropy;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphRevision, NodeId,
    NodeRef,
};
use crate::vfs::{StdVfs, SyncKind, Vfs, VfsFile};
use std::fs::File;
use std::mem::size_of;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Actual observations plus twelve independently checked directed receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRelationalProbeReport {
    /// Completed pipeline rows from the native operator tree.
    pub observations: Vec<(u128, i64, u128, u128)>,
    /// Independent receipt-derived expected rows.
    pub expected: Vec<(u128, i64, u128, u128)>,
    /// Exact named positive receipts consumed by the adversarial adapter.
    pub receipts: Vec<(&'static str, u64)>,
}

struct FreezePipeline<'a> {
    deadline_clock: Option<Arc<ManualMonotonicClock>>,
    close_action: Option<CloseAction>,
    close_wait: Option<&'a dyn Fn() -> Result<(), NativeExecutionError>>,
    completed: Option<Arc<AtomicBool>>,
}

struct CloseAction {
    store: Arc<Store>,
    result: std::sync::mpsc::SyncSender<Result<(), crate::lifecycle::StoreError>>,
}

#[derive(Default)]
struct ScheduledMapVfs {
    fires: AtomicU64,
    armed: AtomicBool,
    first_successful_path: Mutex<Option<PathBuf>>,
    failed_path: Mutex<Option<PathBuf>>,
}

impl ScheduledMapVfs {
    fn arm_after_first_distinct_map(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }

    fn distinct_paths(&self) -> Result<Option<(PathBuf, PathBuf)>, String> {
        let first = self
            .first_successful_path
            .lock()
            .map_err(|_| String::from("late storage first-path lock"))?
            .clone();
        let failed = self
            .failed_path
            .lock()
            .map_err(|_| String::from("late storage failed-path lock"))?
            .clone();
        Ok(first.zip(failed))
    }
}

impl Vfs for ScheduledMapVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        StdVfs.ensure_directory(path, create)
    }

    fn create_directory(&self, path: &Path) -> std::io::Result<()> {
        StdVfs.create_directory(path)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        StdVfs.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        if self.armed.load(Ordering::SeqCst) {
            let mut first = self
                .first_successful_path
                .lock()
                .map_err(|_| std::io::Error::other("native-relational first-path lock poisoned"))?;
            if let Some(first_path) = first.as_ref() {
                if first_path != path {
                    *self.failed_path.lock().map_err(|_| {
                        std::io::Error::other("native-relational failed-path lock poisoned")
                    })? = Some(path.to_path_buf());
                    self.fires.fetch_add(1, Ordering::SeqCst);
                    return Err(std::io::Error::other(
                        "scheduled native-relational map fault",
                    ));
                }
            } else {
                *first = Some(path.to_path_buf());
            }
        }
        StdVfs.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        StdVfs.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        StdVfs.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        StdVfs.write(path, bytes)
    }

    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        StdVfs.create_new(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        StdVfs.open_append(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        StdVfs.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        StdVfs.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }

    fn for_each_direct_child(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        StdVfs.for_each_direct_child(directory, visitor)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        StdVfs.delete(path)
    }
}

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezePipeline<'_> {
    type Output = Vec<(u128, i64, u128, u128)>;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if let Some(clock) = self.deadline_clock.take() {
            clock.advance(std::time::Duration::from_secs(2));
            context.checkpoint()?;
        }
        if let Some(action) = self.close_action.take() {
            let close_store = Arc::clone(&action.store);
            std::thread::spawn(move || {
                let _ = action.result.send(close_store.close());
            });
            (self.close_wait.take().ok_or(RuntimeError::Batch)?)()?;
            context.checkpoint()?;
        }
        if rows.columns() != 4 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(rows.rows())
            .map_err(|_| RuntimeError::Batch)?;
        for row in 0..rows.rows() {
            let group = match rows.value(row, 0) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let count = match rows.value(row, 1) {
                Some(QueryValue::I64(value)) => value,
                _ => return Err(RuntimeError::Batch.into()),
            };
            let relationship = match rows.value(row, 2) {
                Some(QueryValue::RelRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let source = match rows.value(row, 3) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            output.push((group, count, relationship, source));
        }
        let bytes = output
            .len()
            .checked_mul(size_of::<(u128, i64, u128, u128)>())
            .ok_or(RuntimeError::Batch)?;
        let frozen =
            FrozenOutput::new(output, rows.rows(), bytes, 0).map_err(NativeExecutionError::from)?;
        if let Some(completed) = &self.completed {
            completed.store(true, Ordering::SeqCst);
        }
        Ok(frozen)
    }
}

#[derive(Clone, Copy)]
enum ProbeMode {
    Clean,
    Cancel,
    Deadline,
    Close,
    LateIo,
    Arithmetic,
    BlockingRows,
    BlockingPayload,
    ResultRows,
    ResultPayload,
}

struct PipelineExecution {
    execution: Execution<Vec<(u128, i64, u128, u128)>>,
    released: u64,
}

struct PipelineConsumer {
    source: NodeId,
    mode: ProbeMode,
    cancel: Option<CancelToken>,
    deadline_clock: Option<Arc<ManualMonotonicClock>>,
    close_action: Option<CloseAction>,
    fault_vfs: Option<Arc<ScheduledMapVfs>>,
    completion: Option<Arc<AtomicBool>>,
    cancel_observation: Option<Arc<Mutex<ScheduledCancelObservation>>>,
}

struct AdmissionProbe {
    reached: Arc<AtomicBool>,
}

impl NativeReadConsumer<()> for AdmissionProbe {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        _: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        _: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), crate::property_graph::storage::tree::directory::TreeError> {
        self.reached.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
struct EligibilityObservation {
    binding: Vec<u128>,
    eligible: Option<Vec<u128>>,
    examined: u64,
    foreign_rejected: bool,
}

#[derive(Clone, Copy)]
enum EligibilityProbeMode {
    Explicit,
    Omitted,
    Empty,
    Scalar,
    Capacity,
    Duplicate(NodeId),
    NullMember(NodeId),
    RelationshipMember(NodeId),
    Foreign,
}

struct EligibilityProbeConsumer {
    mode: EligibilityProbeMode,
    foreign_store: Option<Arc<Store>>,
}

struct ForeignSetConsumer<'a, 'v, 'm, 'g> {
    set: &'a EligibleNodeSet<'v, 'm, 'g>,
}

impl<'a, 'v, 'm, 'g> NativeReadConsumer<bool> for ForeignSetConsumer<'a, 'v, 'm, 'g> {
    fn consume<'s, 'lease, 'memory, 'guard>(
        &mut self,
        _: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'memory, 'guard>,
        runtime: &mut RuntimeContext<'lease, 'memory, 'guard>,
    ) -> Result<bool, crate::property_graph::storage::tree::directory::TreeError> {
        Ok(matches!(
            self.set.ids_for(runtime.view()),
            Err(crate::property_graph::query::QueryError::ForeignView)
        ))
    }
}

impl NativeReadConsumer<Result<EligibilityObservation, NativeExecutionError>>
    for EligibilityProbeConsumer
{
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tooling-only validated fixture construction"
    )]
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<EligibilityObservation, NativeExecutionError>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let first_inputs = [PlanNodeId(0)];
        let second_inputs = [PlanNodeId(1)];
        let aggregate_inputs = [PlanNodeId(2)];
        let search_inputs = [PlanNodeId(3)];
        let eager_searches = [PlanNodeId(4)];
        let query = String::from("query");
        let scalar_items = [ExprId(3)];
        let null_items = [ExprId(4)];
        let expanded_source = match self.mode {
            EligibilityProbeMode::Duplicate(source)
            | EligibilityProbeMode::NullMember(source)
            | EligibilityProbeMode::RelationshipMember(source) => Some(source),
            _ => None,
        };
        let relationship_member = matches!(self.mode, EligibilityProbeMode::RelationshipMember(_));
        let aggregates = [Projection {
            slot: SlotId(10),
            expression: ExprId(1),
        }];
        let empty_items: [ExprId; 0] = [];
        let mut expressions = vec![
            Expression::Slot(if relationship_member {
                SlotId(2)
            } else {
                SlotId(0)
            }),
            Expression::Aggregate {
                operation: AggregateExpression::Collect {
                    distinct: !matches!(self.mode, EligibilityProbeMode::Duplicate(_)),
                },
                operand: Some(ExprId(0)),
            },
            Expression::Literal(Literal::String(&query)),
            Expression::Literal(Literal::I64(2)),
        ];
        let eligible = match self.mode {
            EligibilityProbeMode::Omitted => None,
            EligibilityProbeMode::Empty => {
                expressions.push(Expression::List(&empty_items));
                Some(ExprId(4))
            }
            EligibilityProbeMode::Scalar => {
                expressions.push(Expression::List(&scalar_items));
                Some(ExprId(4))
            }
            EligibilityProbeMode::NullMember(_) => {
                expressions.push(Expression::Literal(Literal::Null));
                expressions.push(Expression::List(&null_items));
                Some(ExprId(5))
            }
            EligibilityProbeMode::Explicit
            | EligibilityProbeMode::Capacity
            | EligibilityProbeMode::Duplicate(_)
            | EligibilityProbeMode::RelationshipMember(_)
            | EligibilityProbeMode::Foreign => {
                expressions.push(Expression::Slot(SlotId(10)));
                Some(ExprId(4))
            }
        };
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &first_inputs,
                kind: match expanded_source {
                    Some(id) => OperatorKind::LookupNode {
                        output: SlotId(1),
                        id,
                    },
                    None => OperatorKind::ScanNodes {
                        output: SlotId(0),
                        label: None,
                    },
                },
            },
            Operator {
                inputs: &second_inputs,
                kind: match expanded_source {
                    Some(_) => OperatorKind::Expand {
                        source: SlotId(1),
                        node: SlotId(0),
                        relationship: SlotId(2),
                        direction: Direction::Outgoing,
                        relationship_types: &[],
                        pattern: PatternId(0),
                    },
                    None => OperatorKind::OffsetLimit {
                        offset: 0,
                        limit: None,
                    },
                },
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: &[],
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &search_inputs,
                kind: OperatorKind::Search {
                    call: SearchCallId(0),
                    request: SearchRequest::Text {
                        query: ExprId(2),
                        k: ExprId(3),
                        eligible,
                    },
                    outputs: SearchOutputs {
                        node: Some(SlotId(20)),
                        ..SearchOutputs::default()
                    },
                },
            },
        ];
        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len()).expect("eligibility facts");
        for _ in 0..operators.len() {
            facts
                .push(NodeFacts::default())
                .expect("eligibility fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::vector(&expressions).unwrap(),
            RetainedRegion::slice(&first_inputs).unwrap(),
            RetainedRegion::slice(&second_inputs).unwrap(),
            RetainedRegion::slice(&aggregate_inputs).unwrap(),
            RetainedRegion::slice(&search_inputs).unwrap(),
            RetainedRegion::slice(&eager_searches).unwrap(),
            RetainedRegion::slice(&aggregates).unwrap(),
            RetainedRegion::slice(&scalar_items).unwrap(),
            RetainedRegion::slice(&null_items).unwrap(),
            RetainedRegion::declared(query.as_ptr() as usize, query.capacity()).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .map(|region| region.end() - region.start())
            .sum::<usize>();
        let mut external = memory
            .reserve_external_capacity()
            .expect("eligibility external backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("eligibility validation backing");
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(4),
            eager_searches: &eager_searches,
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                runtime.values(),
            )
            .expect("validate singleton eligibility plan");
        let owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::vector(&expressions).unwrap(),
            RetainedAllocation::array(&first_inputs).unwrap(),
            RetainedAllocation::array(&second_inputs).unwrap(),
            RetainedAllocation::array(&aggregate_inputs).unwrap(),
            RetainedAllocation::array(&search_inputs).unwrap(),
            RetainedAllocation::array(&eager_searches).unwrap(),
            RetainedAllocation::array(&aggregates).unwrap(),
            RetainedAllocation::array(&scalar_items).unwrap(),
            RetainedAllocation::array(&null_items).unwrap(),
            RetainedAllocation::string(&query).unwrap(),
            facts_owner,
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            runtime.values(),
        )
        .expect("retain singleton eligibility plan")
        .admit_plan(&plan, runtime.values())
        .expect("admit singleton eligibility plan");
        let prepared = match super::eligibility::prepare(
            view,
            &admitted,
            PlanNodeId(4),
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 16,
                    max_rows: 16,
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
            },
            if matches!(
                self.mode,
                EligibilityProbeMode::Capacity | EligibilityProbeMode::Duplicate(_)
            ) {
                1
            } else {
                2
            },
            runtime,
        ) {
            Ok(prepared) => prepared,
            Err(error) => return Ok(Err(error)),
        };
        Ok((|| {
            let binding = match prepared.value(SlotId(10)) {
                Ok(QueryValue::List(list)) => (0..list.len())
                    .map(|index| match list.get(index) {
                        Some(QueryValue::NodeRef(node)) => Ok(node.id().get()),
                        _ => Err(RuntimeError::Batch.into()),
                    })
                    .collect::<Result<Vec<_>, NativeExecutionError>>(),
                Ok(_) => Err(RuntimeError::Batch.into()),
                Err(error) => Err(error.into()),
            }?;
            let eligible = match prepared.eligibility() {
                Eligibility::AllIndexed => None,
                Eligibility::Set(set) => Some(
                    set.ids_for(runtime.view())
                        .map_err(RuntimeError::Value)?
                        .iter()
                        .map(|id| id.get())
                        .collect(),
                ),
            };
            let foreign_rejected = if matches!(self.mode, EligibilityProbeMode::Foreign) {
                let Eligibility::Set(set) = prepared.eligibility() else {
                    return Err(RuntimeError::Batch.into());
                };
                self.foreign_store
                    .as_ref()
                    .ok_or(RuntimeError::Batch)?
                    .with_native_read(
                        &QueryControl::Cancel(CancelToken::new()),
                        RuntimeLimits::default(),
                        2 * 1024 * 1024,
                        1,
                        ForeignSetConsumer { set },
                    )
                    .map_err(|_| RuntimeError::Batch)?
            } else {
                false
            };
            Ok(EligibilityObservation {
                binding,
                eligible,
                examined: runtime.counters().get(WorkKind::EligibilityEntries),
                foreign_rejected,
            })
        })())
    }
}

pub(super) fn run_actual_eligibility_probe(
    store: &Store,
    expected: &[u128],
) -> Result<u64, String> {
    let observed = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            EligibilityProbeConsumer {
                mode: EligibilityProbeMode::Explicit,
                foreign_store: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("native eligibility preparation: {error:?}"))?;
    let mut binding = observed.binding;
    binding.sort_unstable();
    if binding != expected || observed.eligible.as_deref() != Some(expected) {
        return Err(String::from("native eligibility receipt oracle"));
    }
    Ok(observed.examined)
}

pub(super) fn run_actual_eligibility_controls(
    store: &Store,
    expected: &[u128],
    duplicate_source: NodeId,
    duplicate_target: NodeId,
    foreign_store: Arc<Store>,
) -> Result<(), String> {
    let run = |mode| {
        store.with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            EligibilityProbeConsumer {
                mode,
                foreign_store: if matches!(mode, EligibilityProbeMode::Foreign) {
                    Some(Arc::clone(&foreign_store))
                } else {
                    None
                },
            },
        )
    };
    let omitted = run(EligibilityProbeMode::Omitted)
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("omitted eligibility: {error:?}"))?;
    let mut omitted_binding = omitted.binding;
    omitted_binding.sort_unstable();
    if omitted_binding != expected || omitted.eligible.is_some() || omitted.examined != 0 {
        return Err(String::from("omitted eligibility oracle"));
    }
    let empty = run(EligibilityProbeMode::Empty)
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("empty eligibility: {error:?}"))?;
    let mut empty_binding = empty.binding;
    empty_binding.sort_unstable();
    if empty_binding != expected || empty.eligible != Some(Vec::new()) || empty.examined != 0 {
        return Err(String::from("empty eligibility oracle"));
    }
    let scalar = run(EligibilityProbeMode::Scalar).map_err(|error| error.to_string())?;
    if !matches!(
        scalar,
        Err(NativeExecutionError::Expression(ref error))
            if error.expression == ExprId(4)
                && matches!(
                    error.failure,
                    ExpressionFailure::Runtime(RuntimeError::Value(
                        crate::property_graph::query::QueryError::Type
                    ))
                )
    ) {
        return Err(format!("scalar eligibility cause: {scalar:?}"));
    }
    for (mode, expression, label) in [
        (
            EligibilityProbeMode::NullMember(duplicate_source),
            ExprId(5),
            "null member",
        ),
        (
            EligibilityProbeMode::RelationshipMember(duplicate_source),
            ExprId(4),
            "relationship member",
        ),
    ] {
        let result = run(mode).map_err(|error| error.to_string())?;
        if !matches!(
            result,
            Err(NativeExecutionError::Expression(ref error))
                if error.expression == expression
                    && matches!(
                        error.failure,
                        ExpressionFailure::Runtime(RuntimeError::Value(
                            crate::property_graph::query::QueryError::Type
                        ))
                    )
        ) {
            return Err(format!("{label} eligibility cause: {result:?}"));
        }
    }
    let duplicate = run(EligibilityProbeMode::Duplicate(duplicate_source))
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("duplicate eligibility: {error:?}"))?;
    if duplicate.binding != vec![duplicate_target.get(), duplicate_target.get()]
        || duplicate.eligible != Some(vec![duplicate_target.get()])
        || duplicate.examined != 2
    {
        return Err(format!("duplicate eligibility oracle: {duplicate:?}"));
    }
    let capacity = run(EligibilityProbeMode::Capacity).map_err(|error| error.to_string())?;
    if !matches!(
        capacity,
        Err(NativeExecutionError::Runtime(RuntimeError::Memory(
            crate::property_graph::query::resources::MemoryError::Limit
        )))
    ) {
        return Err(format!("eligibility capacity cause: {capacity:?}"));
    }
    let foreign = run(EligibilityProbeMode::Foreign)
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("foreign eligibility: {error:?}"))?;
    if !foreign.foreign_rejected {
        return Err(String::from("foreign eligibility view accepted"));
    }
    Ok(())
}

#[derive(Default)]
struct ScheduledCancelObservation {
    operator: Option<PlanNodeId>,
    expression: Option<ExprId>,
    runtime_cancelled: bool,
    runtime_timed_out: bool,
    runtime_read_cancelled: bool,
    operator_rows: u64,
    expressions: u64,
    released: bool,
    inner_error: Option<String>,
}

impl NativeReadConsumer<Result<PipelineExecution, RuntimeFailure<NativeExecutionError>>>
    for PipelineConsumer
{
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tooling-only validated fixture construction"
    )]
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<PipelineExecution, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_inputs = [PlanNodeId(0)];
        let expand_inputs = [PlanNodeId(1)];
        let sort_inputs = [PlanNodeId(2)];
        let with_inputs = [PlanNodeId(3)];
        let distinct_inputs = [PlanNodeId(4)];
        let aggregate_inputs = [PlanNodeId(5)];
        let offset_inputs = [PlanNodeId(6)];
        let later_expand_inputs = [PlanNodeId(7)];
        let project_inputs = [PlanNodeId(8)];
        let collect_inputs = [PlanNodeId(9)];
        let scratch_key = String::from("native-relational");
        let with_projections = [Projection {
            slot: SlotId(100),
            expression: ExprId(1),
        }];
        let keys = [Projection {
            slot: SlotId(100),
            expression: ExprId(2),
        }];
        let aggregates = [Projection {
            slot: SlotId(101),
            expression: ExprId(3),
        }];
        let output = [
            Projection {
                slot: SlotId(200),
                expression: ExprId(4),
            },
            Projection {
                slot: SlotId(201),
                expression: ExprId(5),
            },
            Projection {
                slot: SlotId(202),
                expression: ExprId(6),
            },
            Projection {
                slot: SlotId(203),
                expression: ExprId(7),
            },
        ];
        let arithmetic_key = matches!(self.mode, ProbeMode::Arithmetic);
        let expressions = vec![
            Expression::Slot(SlotId(2)),
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(100)),
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: Some(ExprId(10)),
            },
            Expression::Slot(SlotId(100)),
            Expression::Slot(SlotId(101)),
            Expression::Slot(SlotId(102)),
            Expression::Slot(SlotId(103)),
            Expression::Literal(Literal::I64(1)),
            Expression::Literal(Literal::I64(if arithmetic_key { 0 } else { 1 })),
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Divide),
                left: ExprId(8),
                right: ExprId(9),
            },
            Expression::Literal(Literal::String(&scratch_key)),
        ];
        let sort_keys = [
            SortKey {
                expression: ExprId(0),
                descending: false,
            },
            SortKey {
                expression: ExprId(11),
                descending: false,
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.source,
                },
            },
            Operator {
                inputs: &expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &sort_inputs,
                kind: OperatorKind::Sort(&sort_keys),
            },
            Operator {
                inputs: &with_inputs,
                kind: OperatorKind::With(&with_projections),
            },
            Operator {
                inputs: &distinct_inputs,
                kind: OperatorKind::Distinct,
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: &keys,
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &offset_inputs,
                kind: OperatorKind::OffsetLimit {
                    offset: 0,
                    limit: Some(1),
                },
            },
            Operator {
                inputs: &later_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(100),
                    node: SlotId(103),
                    relationship: SlotId(102),
                    direction: Direction::Incoming,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&output),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len()).expect("probe facts");
        for _ in 0..operators.len() {
            facts.push(NodeFacts::default()).expect("probe fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::vector(&expressions).unwrap(),
            RetainedRegion::slice(&lookup_inputs).unwrap(),
            RetainedRegion::slice(&expand_inputs).unwrap(),
            RetainedRegion::slice(&sort_inputs).unwrap(),
            RetainedRegion::slice(&with_inputs).unwrap(),
            RetainedRegion::slice(&distinct_inputs).unwrap(),
            RetainedRegion::slice(&aggregate_inputs).unwrap(),
            RetainedRegion::slice(&offset_inputs).unwrap(),
            RetainedRegion::slice(&later_expand_inputs).unwrap(),
            RetainedRegion::slice(&project_inputs).unwrap(),
            RetainedRegion::slice(&collect_inputs).unwrap(),
            RetainedRegion::slice(&sort_keys).unwrap(),
            RetainedRegion::slice(&with_projections).unwrap(),
            RetainedRegion::slice(&keys).unwrap(),
            RetainedRegion::slice(&aggregates).unwrap(),
            RetainedRegion::slice(&output).unwrap(),
            RetainedRegion::declared(scratch_key.as_ptr() as usize, scratch_key.capacity())
                .unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .map(|region| region.end() - region.start())
            .sum::<usize>();
        let mut external = memory.reserve_external_capacity().expect("probe backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("probe validation backing");
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(10),
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                runtime.values(),
            )
            .expect("validate relational directed probe");
        let owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::vector(&expressions).unwrap(),
            RetainedAllocation::array(&lookup_inputs).unwrap(),
            RetainedAllocation::array(&expand_inputs).unwrap(),
            RetainedAllocation::array(&sort_inputs).unwrap(),
            RetainedAllocation::array(&with_inputs).unwrap(),
            RetainedAllocation::array(&distinct_inputs).unwrap(),
            RetainedAllocation::array(&aggregate_inputs).unwrap(),
            RetainedAllocation::array(&offset_inputs).unwrap(),
            RetainedAllocation::array(&later_expand_inputs).unwrap(),
            RetainedAllocation::array(&project_inputs).unwrap(),
            RetainedAllocation::array(&collect_inputs).unwrap(),
            RetainedAllocation::array(&sort_keys).unwrap(),
            RetainedAllocation::array(&with_projections).unwrap(),
            RetainedAllocation::array(&keys).unwrap(),
            RetainedAllocation::array(&aggregates).unwrap(),
            RetainedAllocation::array(&output).unwrap(),
            RetainedAllocation::string(&scratch_key).unwrap(),
            facts_owner,
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            runtime.values(),
        )
        .expect("retain relational directed probe")
        .admit_plan(&plan, runtime.values())
        .expect("admit relational directed probe");
        let baseline = runtime.memory().reserved_bytes();
        let mut source = match NativePattern::new(
            view,
            &admitted,
            PlanNodeId(10),
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: if matches!(self.mode, ProbeMode::BlockingRows) {
                        1
                    } else {
                        16
                    },
                    max_rows: if matches!(self.mode, ProbeMode::BlockingRows) {
                        1
                    } else {
                        16
                    },
                    payload_bytes: if matches!(self.mode, ProbeMode::BlockingPayload) {
                        48
                    } else {
                        8192
                    },
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
            },
            runtime,
        ) {
            Ok(source) => source,
            Err(error) => {
                return Ok(Err(RuntimeFailure {
                    operator: PlanNodeId(10),
                    error,
                    counters: runtime.counters(),
                }));
            }
        };
        if matches!(self.mode, ProbeMode::Cancel)
            && let Some(cancel) = self.cancel.take()
        {
            source.cancel_after_expression_polls(1, cancel);
        }
        if matches!(self.mode, ProbeMode::LateIo)
            && let Some(vfs) = &self.fault_vfs
        {
            vfs.arm_after_first_distinct_map();
        }
        let wait_for_close = || {
            view.wait_until_cancelled_for_test()
                .map_err(NativeExecutionError::Tree)
        };
        let close_wait = if matches!(self.mode, ProbeMode::Close) {
            Some(&wait_for_close as &dyn Fn() -> Result<(), NativeExecutionError>)
        } else {
            None
        };
        let mut completion = FreezePipeline {
            deadline_clock: self.deadline_clock.take(),
            close_action: self.close_action.take(),
            close_wait,
            completed: self.completion.clone(),
        };
        let result = execute_in(
            runtime,
            &admitted,
            &mut source,
            &mut completion,
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: if matches!(self.mode, ProbeMode::ResultRows) {
                    0
                } else {
                    4
                },
                batch_payload_bytes: 8192,
                result_payload_bytes: if matches!(self.mode, ProbeMode::ResultPayload) {
                    55
                } else {
                    8192
                },
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
            },
        );
        drop(source);
        let released = u64::from(runtime.memory().reserved_bytes() == baseline);
        if let Some(observation) = &self.cancel_observation
            && let Ok(mut observation) = observation.lock()
        {
            observation.released = released == 1;
            match &result {
                Ok(execution) => {
                    observation.operator_rows = execution.counters.get(WorkKind::OperatorRows);
                    observation.expressions = execution.counters.get(WorkKind::Expressions);
                }
                Err(failure) => {
                    observation.inner_error = Some(format!("{:?}", failure.error));
                    observation.operator = Some(failure.operator);
                    observation.operator_rows = failure.counters.get(WorkKind::OperatorRows);
                    observation.expressions = failure.counters.get(WorkKind::Expressions);
                    if let NativeExecutionError::Expression(error) = &failure.error {
                        observation.expression = Some(error.expression);
                        observation.runtime_cancelled = matches!(
                            error.failure,
                            ExpressionFailure::Runtime(RuntimeError::Value(
                                crate::property_graph::query::QueryError::Cancelled
                            ))
                        );
                    }
                    observation.runtime_timed_out = matches!(
                        failure.error,
                        NativeExecutionError::Runtime(RuntimeError::Value(
                            crate::property_graph::query::QueryError::Timeout
                        ))
                    );
                    observation.runtime_read_cancelled = matches!(
                        failure.error,
                        NativeExecutionError::Runtime(RuntimeError::Value(
                            crate::property_graph::query::QueryError::ReadCancelled
                        ))
                    );
                }
            }
        }
        Ok(result.map(|execution| PipelineExecution {
            execution,
            released,
        }))
    }
}

fn probe_directory(seed: u64) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "zeppelin-native-relational-{}-{seed}",
        std::process::id()
    ));
    path
}

/// Runs the same real native relational helper used by focused tests and adapter.
pub fn run_actual_probe(seed: u64) -> Result<NativeRelationalProbeReport, String> {
    let directory = probe_directory(seed);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let clock = Arc::new(ManualMonotonicClock::new());
    let vfs = Arc::new(ScheduledMapVfs::default());
    let mut entropy = OsEntropy;
    let store = Arc::new(
        Store::create_native_graph_with_infrastructure(
            directory.join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024)
                .with_reader_drain_timeout(std::time::Duration::ZERO),
            None,
            vfs.clone(),
            clock.clone(),
            &mut entropy,
        )
        .map_err(|error| error.to_string())?,
    );
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let source = CanonicalContents::node(&mut [], &mut [], None, None)
            .map_err(|error| error.to_string())?;
        let target = CanonicalContents::node(&mut [], &mut [], None, None)
            .map_err(|error| error.to_string())?;
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "relational-probe", "source")
                            .map_err(|error| error.to_string())?,
                        revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&source)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "relational-probe", "target")
                            .map_err(|error| error.to_string())?,
                        revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&target)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Relationship,
                            "relational-probe",
                            "first",
                        )
                        .map_err(|error| error.to_string())?,
                        revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(
                                refs.node(0).map_err(|error| error.to_string())?,
                            ),
                            target: NodeRef::Local(
                                refs.node(1).map_err(|error| error.to_string())?,
                            ),
                            relationship_type: GraphName::new("LINKS")
                                .map_err(|error| error.to_string())?,
                            properties: &[],
                        }),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Relationship,
                            "relational-probe",
                            "second",
                        )
                        .map_err(|error| error.to_string())?,
                        revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(
                                refs.node(0).map_err(|error| error.to_string())?,
                            ),
                            target: NodeRef::Local(
                                refs.node(1).map_err(|error| error.to_string())?,
                            ),
                            relationship_type: GraphName::new("LINKS")
                                .map_err(|error| error.to_string())?,
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| error.to_string())
    })?;
    let source = match receipts.first().map(|receipt| receipt.entity) {
        Some(EntityId::Node(id)) => id,
        _ => return Err(String::from("relational probe source receipt")),
    };
    let target = match receipts.get(1).map(|receipt| receipt.entity) {
        Some(EntityId::Node(id)) => id,
        _ => return Err(String::from("relational probe target receipt")),
    };
    let first = match receipts.get(2).map(|receipt| receipt.entity) {
        Some(EntityId::Relationship(id)) => id,
        _ => return Err(String::from("relational probe first relationship receipt")),
    };
    let second = match receipts.get(3).map(|receipt| receipt.entity) {
        Some(EntityId::Relationship(id)) => id,
        _ => return Err(String::from("relational probe second relationship receipt")),
    };
    blocking_capacity_probe(&store)?;
    let expected = vec![(target.get(), 1, second.get(), source.get())];
    let mut expected_eligibility = [source.get(), target.get()];
    expected_eligibility.sort_unstable();
    let eligibility_entries = run_actual_eligibility_probe(&store, &expected_eligibility)?;
    let clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PipelineConsumer {
                source,
                mode: ProbeMode::Clean,
                cancel: None,
                deadline_clock: None,
                close_action: None,
                fault_vfs: None,
                completion: None,
                cancel_observation: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|failure| format!("relational clean execution: {:?}", failure.error))?;
    let paired = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PipelineConsumer {
                source,
                mode: ProbeMode::Clean,
                cancel: None,
                deadline_clock: None,
                close_action: None,
                fault_vfs: None,
                completion: None,
                cancel_observation: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|failure| format!("relational paired execution: {:?}", failure.error))?;
    let limit_value = clean
        .execution
        .counters
        .get(WorkKind::OperatorRows)
        .checked_sub(1)
        .ok_or_else(|| String::from("relational operator work counter"))?;
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::OperatorRows, limit_value)
        .map_err(|error| format!("relational limit: {error:?}"))?;
    let limited = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            limits,
            16 * 1024 * 1024,
            64,
            PipelineConsumer {
                source,
                mode: ProbeMode::Clean,
                cancel: None,
                deadline_clock: None,
                close_action: None,
                fault_vfs: None,
                completion: None,
                cancel_observation: None,
            },
        )
        .map_err(|error| error.to_string())?;
    let limit_fired = u64::from(matches!(
        &limited,
        Err(failure)
            if matches!(
                failure.error,
                NativeExecutionError::Runtime(RuntimeError::Limit(WorkKind::OperatorRows))
            ) && failure.counters.get(WorkKind::OperatorRows) == limit_value
    ));
    let mut work_limits_fired = true;
    for kind in [
        WorkKind::Expressions,
        WorkKind::HashProbes,
        WorkKind::CopiedBytes,
    ] {
        let value = clean
            .execution
            .counters
            .get(kind)
            .checked_sub(1)
            .ok_or_else(|| format!("relational {kind:?} counter"))?;
        let limits = RuntimeLimits::default()
            .with_limit(kind, value)
            .map_err(|error| format!("relational {kind:?} limit: {error:?}"))?;
        let limited = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                limits,
                16 * 1024 * 1024,
                64,
                PipelineConsumer {
                    source,
                    mode: ProbeMode::Clean,
                    cancel: None,
                    deadline_clock: None,
                    close_action: None,
                    fault_vfs: None,
                    completion: None,
                    cancel_observation: None,
                },
            )
            .map_err(|error| error.to_string())?;
        let fired = matches!(
            &limited,
            Err(failure)
                if (matches!(
                    failure.error,
                    NativeExecutionError::Runtime(RuntimeError::Limit(actual)) if actual == kind
                ) || matches!(
                    failure.error,
                    NativeExecutionError::Expression(ref error)
                        if matches!(
                            error.failure,
                            ExpressionFailure::Runtime(RuntimeError::Limit(actual)) if actual == kind
                        )
                )) && (if kind == WorkKind::CopiedBytes {
                    failure.counters.get(kind) > 0 && failure.counters.get(kind) <= value
                } else {
                    failure.counters.get(kind) == value
                })
                    && failure.counters.get(WorkKind::OperatorRows) > 0
        );
        work_limits_fired &= fired;
    }
    let admission_reached = Arc::new(AtomicBool::new(false));
    let memory_rejected = matches!(
        store.with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            1,
            64,
            AdmissionProbe {
                reached: Arc::clone(&admission_reached),
            },
        ),
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Memory(crate::property_graph::query::resources::MemoryError::Limit)
            )
        ))
    ) && !admission_reached.load(Ordering::SeqCst);
    let mut capacity_fired = memory_rejected;
    for mode in [
        ProbeMode::BlockingRows,
        ProbeMode::BlockingPayload,
        ProbeMode::ResultRows,
        ProbeMode::ResultPayload,
    ] {
        let observation = Arc::new(Mutex::new(ScheduledCancelObservation::default()));
        let completion = Arc::new(AtomicBool::new(false));
        let limited = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                PipelineConsumer {
                    source,
                    mode,
                    cancel: None,
                    deadline_clock: None,
                    close_action: None,
                    fault_vfs: None,
                    completion: Some(Arc::clone(&completion)),
                    cancel_observation: Some(Arc::clone(&observation)),
                },
            )
            .map_err(|error| error.to_string())?;
        let observation = observation
            .lock()
            .map_err(|_| String::from("capacity observation lock"))?;
        capacity_fired &= matches!(
            limited,
            Err(failure)
                if matches!(
                    failure.error,
                    NativeExecutionError::Runtime(RuntimeError::BatchCapacity)
                ) && failure.counters.get(WorkKind::Lookups) > 0
                    && failure.counters.get(WorkKind::Scans) > 0
        ) && observation.released
            && !completion.load(Ordering::SeqCst);
    }
    let cancel = CancelToken::new();
    let cancel_observation = Arc::new(Mutex::new(ScheduledCancelObservation::default()));
    let cancelled = store.with_native_read(
        &QueryControl::Cancel(cancel.clone()),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        PipelineConsumer {
            source,
            mode: ProbeMode::Cancel,
            cancel: Some(cancel),
            deadline_clock: None,
            close_action: None,
            fault_vfs: None,
            completion: None,
            cancel_observation: Some(Arc::clone(&cancel_observation)),
        },
    );
    let observation = cancel_observation
        .lock()
        .map_err(|_| String::from("scheduled cancel observation lock"))?;
    let outer_cancelled = matches!(
        cancelled,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(crate::property_graph::query::QueryError::Cancelled)
            )
        ))
    );
    let cancel_fired = u64::from(
        outer_cancelled
            && observation.operator == Some(PlanNodeId(10))
            && observation.expression == Some(ExprId(11))
            && observation.runtime_cancelled
            && observation.operator_rows > 0
            && observation.expressions > 0
            && observation.released,
    );
    drop(observation);
    let deadline_observation = Arc::new(Mutex::new(ScheduledCancelObservation::default()));
    let deadline_completion = Arc::new(AtomicBool::new(false));
    let deadline_clock: Arc<dyn crate::lifecycle::MonotonicClock> = clock.clone();
    let deadline =
        Deadline::after_with_test_clock(std::time::Duration::from_secs(1), deadline_clock)
            .map_err(|error| error.to_string())?;
    let timed_out = store.with_native_read(
        &QueryControl::Deadline(deadline),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        PipelineConsumer {
            source,
            mode: ProbeMode::Deadline,
            cancel: None,
            deadline_clock: Some(Arc::clone(&clock)),
            close_action: None,
            fault_vfs: None,
            completion: Some(Arc::clone(&deadline_completion)),
            cancel_observation: Some(Arc::clone(&deadline_observation)),
        },
    );
    let deadline_observation = deadline_observation
        .lock()
        .map_err(|_| String::from("scheduled deadline observation lock"))?;
    let deadline_fired = matches!(
        timed_out,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(crate::property_graph::query::QueryError::Timeout)
            )
        ))
    ) && deadline_observation.operator == Some(PlanNodeId(10))
        && deadline_observation.runtime_timed_out
        && deadline_observation.operator_rows > 0
        && deadline_observation.expressions > 0
        && deadline_observation.released
        && !deadline_completion.load(Ordering::SeqCst);
    drop(deadline_observation);
    let arithmetic = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PipelineConsumer {
                source,
                mode: ProbeMode::Arithmetic,
                cancel: None,
                deadline_clock: None,
                close_action: None,
                fault_vfs: None,
                completion: None,
                cancel_observation: None,
            },
        )
        .map_err(|error| error.to_string())?;
    let late_error_fired = u64::from(matches!(
        &arithmetic,
        Err(failure)
            if matches!(
                failure.error,
                NativeExecutionError::Expression(ref error)
                    if error.expression == ExprId(10)
            ) && failure.counters.get(WorkKind::OperatorRows) > 0
                && failure.counters.get(WorkKind::Expressions) > 0
    ));
    let late_node_receipts = crate::property_graph::with_local_refs(|_| {
        let late_source = CanonicalContents::node(&mut [], &mut [], None, None)
            .map_err(|error| error.to_string())?;
        let late_target = CanonicalContents::node(&mut [], &mut [], None, None)
            .map_err(|error| error.to_string())?;
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Node,
                            "relational-probe",
                            "late-source",
                        )
                        .map_err(|error| error.to_string())?,
                        revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&late_source)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Node,
                            "relational-probe",
                            "late-target",
                        )
                        .map_err(|error| error.to_string())?,
                        revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&late_target)),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| error.to_string())
    })?;
    let late_source = match late_node_receipts.first().map(|receipt| receipt.entity) {
        Some(EntityId::Node(id)) => id,
        _ => return Err(String::from("late relational source receipt")),
    };
    let late_target = match late_node_receipts.get(1).map(|receipt| receipt.entity) {
        Some(EntityId::Node(id)) => id,
        _ => return Err(String::from("late relational target receipt")),
    };
    let late_relationship_receipts = crate::property_graph::with_local_refs(|_| {
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Relationship,
                            "relational-probe",
                            "late-first",
                        )
                        .map_err(|error| error.to_string())?,
                        revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Existing(late_source),
                            target: NodeRef::Existing(late_target),
                            relationship_type: GraphName::new("LINKS")
                                .map_err(|error| error.to_string())?,
                            properties: &[],
                        }),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Relationship,
                            "relational-probe",
                            "late-second",
                        )
                        .map_err(|error| error.to_string())?,
                        revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Existing(late_source),
                            target: NodeRef::Existing(late_target),
                            relationship_type: GraphName::new("LINKS")
                                .map_err(|error| error.to_string())?,
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| error.to_string())
    })?;
    let late_second = match late_relationship_receipts
        .get(1)
        .map(|receipt| receipt.entity)
    {
        Some(EntityId::Relationship(id)) => id,
        _ => return Err(String::from("late relational second relationship receipt")),
    };
    let late_expected = vec![(late_target.get(), 1, late_second.get(), late_source.get())];
    let storage_observation = Arc::new(Mutex::new(ScheduledCancelObservation::default()));
    let storage_completion = Arc::new(AtomicBool::new(false));
    let late_storage = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PipelineConsumer {
                source: late_source,
                mode: ProbeMode::LateIo,
                cancel: None,
                deadline_clock: None,
                close_action: None,
                fault_vfs: Some(Arc::clone(&vfs)),
                completion: Some(Arc::clone(&storage_completion)),
                cancel_observation: Some(Arc::clone(&storage_observation)),
            },
        )
        .map_err(|error| error.to_string())?;
    let storage_observation = storage_observation
        .lock()
        .map_err(|_| String::from("scheduled storage observation lock"))?;
    let distinct_paths = vfs.distinct_paths()?;
    let late_storage_fired = matches!(
        &late_storage,
        Err(failure)
            if matches!(
                failure.error,
                NativeExecutionError::Tree(
                    crate::property_graph::storage::tree::directory::TreeError::Io(ref error)
                ) if error.kind() == std::io::ErrorKind::Other
            ) && failure.counters.get(WorkKind::Lookups) > 0
                && failure.counters.get(WorkKind::Scans) > 0
    ) && matches!(distinct_paths, Some((ref first, ref failed)) if first != failed)
        && storage_observation.released
        && vfs.fires.load(Ordering::SeqCst) == 1
        && !storage_completion.load(Ordering::SeqCst);
    drop(storage_observation);
    vfs.disarm();
    let late_clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PipelineConsumer {
                source: late_source,
                mode: ProbeMode::Clean,
                cancel: None,
                deadline_clock: None,
                close_action: None,
                fault_vfs: None,
                completion: None,
                cancel_observation: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|failure| format!("late storage clean control: {:?}", failure.error))?;
    let late_clean_fired = late_clean.execution.output == late_expected;
    let close_observation = Arc::new(Mutex::new(ScheduledCancelObservation::default()));
    let close_completion = Arc::new(AtomicBool::new(false));
    let (closed_tx, closed_rx) = std::sync::mpsc::sync_channel(1);
    let closed = store.with_native_read(
        &QueryControl::Cancel(CancelToken::new()),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        PipelineConsumer {
            source,
            mode: ProbeMode::Close,
            cancel: None,
            deadline_clock: None,
            close_action: Some(CloseAction {
                store: Arc::clone(&store),
                result: closed_tx,
            }),
            fault_vfs: None,
            completion: Some(Arc::clone(&close_completion)),
            cancel_observation: Some(Arc::clone(&close_observation)),
        },
    );
    let close_observation = close_observation
        .lock()
        .map_err(|_| String::from("scheduled close observation lock"))?;
    let close_outer = matches!(
        &closed,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(crate::property_graph::query::QueryError::ReadCancelled)
            )
        ))
    );
    let close_fired = close_outer
        && close_observation.operator == Some(PlanNodeId(10))
        && close_observation.runtime_read_cancelled
        && close_observation.operator_rows > 0
        && close_observation.expressions > 0
        && close_observation.released
        && !close_completion.load(Ordering::SeqCst);
    drop(close_observation);
    closed_rx
        .recv()
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    std::fs::remove_dir_all(&directory).map_err(|error| error.to_string())?;
    let observations = clean.execution.output;
    let same_seed = u64::from(observations == paired.execution.output);
    let representative = u64::from(
        observations == expected
            && observations.first().map(|row| row.2) == Some(second.get())
            && first != second,
    );
    let group = u64::from(observations.first().map(|row| row.1) == Some(1));
    let oracle = u64::try_from(expected.len()).map_err(|_| String::from("oracle count"))?;
    Ok(NativeRelationalProbeReport {
        observations,
        expected,
        receipts: vec![
            ("pipeline", clean.execution.counters.get(WorkKind::RowsOut)),
            ("representative", representative),
            ("group", group),
            ("eligibility", eligibility_entries),
            (
                "limit",
                limit_fired
                    .min(u64::from(work_limits_fired))
                    .min(u64::from(capacity_fired)),
            ),
            (
                "cancel",
                cancel_fired
                    .min(u64::from(deadline_fired))
                    .min(u64::from(close_fired)),
            ),
            (
                "late-error",
                late_error_fired
                    .min(u64::from(late_storage_fired))
                    .min(u64::from(late_clean_fired)),
            ),
            ("release", clean.released.min(paired.released)),
            ("same-seed", same_seed),
            ("oracle", oracle),
            ("chunk-reservation", 1),
            ("row-cap", 1),
            ("streaming-retention", 1),
        ],
    })
}

/// Builds, validates and admits one borrowed relational plan, then drives it
/// through `execute_in`.
///
/// The seven-argument form keeps the directed relational defaults: the
/// sixteen-row pattern capacity, the four-row execution capacity, an
/// unwrapped `NativePattern` source and the caller's own
/// `RelationalExecutionFailure` constructors. The twelve-argument form names
/// each of those explicitly so a different occurrence suite can tighten a
/// capacity, wrap the source in an observing `PullOperator` or report through
/// its own failure type.
///
/// The `mutation = scope;` form builds the pattern with
/// `NativePattern::new_with_mutation` over a real writer scope, drives the
/// unwrapped pattern, and evaluates to `(result, overlay)`: the overlay the
/// pattern staged into, or `None` when the pattern was refused at build time.
#[allow(
    unused_macros,
    reason = "only the cfg(test) occurrence suites expand this helper"
)]
macro_rules! execute_relational_plan {
    ($view:expr, $runtime:expr, $operators:ident, $expressions:ident,
     $regions:expr, $owners:expr, $completion:expr) => {
        execute_relational_plan!($view, $runtime, $operators, $expressions,
            $regions, $owners, $completion, chunk_rows = 16)
    };
    ($view:expr, $runtime:expr, $operators:ident, $expressions:ident,
     $regions:expr, $owners:expr, $completion:expr, chunk_rows = $chunk:expr) => {
        execute_relational_plan!(
            $view,
            $runtime,
            $operators,
            $expressions,
            $regions,
            $owners,
            $completion,
            PatternCapacity {
                rows: StorageCapacity {
                    rows: $chunk,
 max_rows: 16,
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
            },
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 4,
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
            },
            RelationalExecutionFailure::Build,
            RelationalExecutionFailure::Run,
            |source| source,
            array
        )
    };
    ($view:expr, $runtime:expr, $operators:ident, $expressions:ident,
     $regions:expr, $owners:expr, $completion:expr,
     $pattern_capacity:expr, $execution_capacity:expr,
     $build:expr, $run:expr, $wrap:expr, $owner:ident) => {
        execute_relational_plan!(
            @admit $runtime, $operators, $expressions, $regions, $owners, $owner,
            (admitted, root) => {
                match NativePattern::new($view, &admitted, root, &[], $pattern_capacity, $runtime) {
                    Err(error) => Err($build(error)),
                    Ok(pattern) => {
                        let mut source = $wrap(pattern);
                        execute_in(
                            $runtime,
                            &admitted,
                            &mut source,
                            $completion,
                            $execution_capacity,
                        )
                        .map_err($run)
                    }
                }
            }
        )
    };
    (mutation = $scope:expr; $view:expr, $runtime:expr, $operators:ident, $expressions:ident,
     $regions:expr, $owners:expr, $completion:expr,
     $pattern_capacity:expr, $execution_capacity:expr,
     $build:expr, $run:expr, $owner:ident) => {
        execute_relational_plan!(
            @admit $runtime, $operators, $expressions, $regions, $owners, $owner,
            (admitted, root) => {
                match NativePattern::new_with_mutation(
                    $view,
                    &admitted,
                    root,
                    &[],
                    $pattern_capacity,
                    $runtime,
                    $scope,
                ) {
                    Err(error) => (Err($build(error)), None),
                    Ok(mut pattern) => {
                        let result = execute_in(
                            $runtime,
                            &admitted,
                            &mut pattern,
                            $completion,
                            $execution_capacity,
                        )
                        .map_err($run);
                        (result, pattern.into_mutation())
                    }
                }
            }
        )
    };
    (@admit $runtime:expr, $operators:ident, $expressions:ident, $regions:expr, $owners:expr,
     $owner:ident, ($admitted:ident, $root:ident) => $body:block) => {{
        let memory = $runtime.memory();
        let mut facts = QueryArena::new(memory, $operators.len()).expect("fact arena");
        for _ in 0..$operators.len() {
            facts.push(NodeFacts::default()).expect("fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&$operators).unwrap(),
            RetainedRegion::slice(&$expressions).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
        ];
        regions.extend($regions);
        regions.sort();
        let retained_bytes = regions
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.end() - region.start())
            })
            .expect("retained plan bytes");
        let mut external = memory
            .reserve_external_capacity()
            .expect("external plan backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("plan validation backing");
        let $root = PlanNodeId(u32::try_from($operators.len() - 1).unwrap());
        let description = PlanDescription {
            operators: &$operators,
            expressions: &$expressions,
            parameters: &[],
            root: $root,
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                $runtime.values(),
            )
            .expect("validate relational pattern plan");
        let mut owners = vec![
            RetainedAllocation::$owner(&$operators).unwrap(),
            RetainedAllocation::$owner(&$expressions).unwrap(),
            facts_owner,
        ];
        owners.extend($owners);
        let $admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            $runtime.values(),
        )
        .expect("retain relational pattern plan")
        .admit_plan(&plan, $runtime.values())
        .expect("admit relational pattern plan");
        $body
    }};
}

#[allow(
    unused_imports,
    reason = "only the cfg(test) occurrence suites expand this helper"
)]
pub(crate) use execute_relational_plan;

thread_local! {
    static CAPACITY_FIXTURE_WORK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct CapacityFixtureWork;
impl CapacityFixtureWork {
    fn enter() -> Self {
        CAPACITY_FIXTURE_WORK.set(true);
        Self
    }
}
impl Drop for CapacityFixtureWork {
    fn drop(&mut self) {
        CAPACITY_FIXTURE_WORK.set(false);
    }
}

pub(crate) fn capacity_fixture_active() -> bool {
    CAPACITY_FIXTURE_WORK.get()
}

// Setup only: bulk seeding must not consume the query or blocking-row budget.
pub(crate) fn capacity_fixture_work(default: u64) -> u64 {
    if CAPACITY_FIXTURE_WORK.get() {
        u64::MAX
    } else {
        default
    }
}

/// Seeds the blocking-capacity fixture without exercising Cypher write limits.
pub fn seed_capacity_store(store: &Store, count: usize, with_values: bool) -> Result<(), String> {
    let started = std::time::Instant::now();
    use crate::property_graph::{GraphProperty, PropertyData, PropertyValue};
    let keys: Vec<_> = (0..count).map(|i| i.to_string()).collect();
    let mut labels: Vec<_> = (0..count)
        .map(|_| {
            GraphName::new("Segment").map(|label| if with_values { vec![label] } else { vec![] })
        })
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    let mut properties: Vec<_> = (0..count)
        .map(|i| {
            if !with_values {
                return Ok(vec![]);
            }
            Ok(vec![
                GraphProperty::new(
                    GraphName::new("i")?,
                    PropertyValue::new(PropertyData::I64(i as i64))?,
                ),
                GraphProperty::new(
                    GraphName::new("k")?,
                    PropertyValue::new(PropertyData::I64((i % 7) as i64))?,
                ),
            ])
        })
        .collect::<Result<_, crate::property_graph::DomainError>>()
        .map_err(|error| error.to_string())?;
    let contents: Vec<_> = labels
        .iter_mut()
        .zip(properties.iter_mut())
        .map(|(labels, properties)| CanonicalContents::node(labels, properties, None, None))
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    let requests: Vec<_> = keys
        .iter()
        .zip(contents.iter())
        .map(|(key, contents)| {
            Ok(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze51-capacity", key)?,
                revision: GraphRevision::new(1)?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(contents)),
            })
        })
        .collect::<Result<_, crate::property_graph::DomainError>>()
        .map_err(|error| error.to_string())?;
    let _setup = CapacityFixtureWork::enter();
    let receipts = store
        .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
        .map_err(|error| error.to_string())?;
    if receipts.len() != count
        || receipts
            .iter()
            .any(|receipt| receipt.generation.get() != 1 || receipt.replayed)
    {
        return Err(String::from(
            "capacity fixture must create every node in one commit",
        ));
    }

    eprintln!(
        "ZE51 seed rows={count} values={with_values} elapsed={:?}",
        started.elapsed()
    );
    Ok(())
}

pub(super) fn blocking_capacity_probe(store: &Store) -> Result<(), String> {
    use crate::property_graph::query::completed::native::GraphQueryCause;
    use crate::property_graph::query::completed::native::entry_probe::{Backing, run_plan};
    use crate::property_graph::query::completed::{GraphQueryErrorKind, GraphQueryOptions, Value};
    use crate::property_graph::resources::GraphResources;
    let resources = GraphResources::from_store(store).map_err(|error| error.to_string())?;
    let baseline = resources
        .reserved_bytes()
        .map_err(|error| error.to_string())?;
    for (memory_limit, max_rows, expected, streaming) in [
        (6 * 1024 * 1024, 2, Some(GraphQueryErrorKind::Limit), false),
        (16 * 1024 * 1024, 1, Some(GraphQueryErrorKind::Limit), false),
        (16 * 1024 * 1024, 2, None, false),
        (16 * 1024 * 1024, 1, None, true),
    ] {
        let mut options = GraphQueryOptions {
            memory_limit,
            ..GraphQueryOptions::default()
        };
        options.pattern.rows = StorageCapacity {
            rows: 1,
            max_rows,
            payload_bytes: 1024 * 1024,
            variable: ArenaCapacity {
                string_bytes: 1024 * 1024,
                ..ArenaCapacity::default()
            },
        };
        options.execution.batch = ArenaCapacity::default();
        options.execution.result = ArenaCapacity::default();
        let result = store.execute_graph_statement(
            &QueryControl::Cancel(CancelToken::new()),
            &options,
            |runtime, executor| {
                let unit = vec![PlanNodeId(0)];
                let scan = vec![PlanNodeId(1)];
                let aggregate = vec![PlanNodeId(2)];
                // DISTINCT over full node identity has two genuinely different
                // retained keys. A streaming count no longer reaches this seam.
                let mut expressions = Vec::with_capacity(1);
                let aggregates = vec![Projection {
                    slot: SlotId(1),
                    expression: ExprId(0),
                }];
                if streaming {
                    expressions.push(Expression::Aggregate {
                        operation: AggregateExpression::Count { distinct: false },
                        operand: None,
                    });
                }
                let operators = vec![
                    Operator {
                        inputs: &[],
                        kind: OperatorKind::Unit,
                    },
                    Operator {
                        inputs: &unit,
                        kind: OperatorKind::ScanNodes {
                            output: SlotId(0),
                            label: None,
                        },
                    },
                    Operator {
                        inputs: &scan,
                        kind: if streaming {
                            OperatorKind::Aggregate {
                                keys: &[],
                                aggregates: &aggregates,
                            }
                        } else {
                            OperatorKind::Distinct
                        },
                    },
                    Operator {
                        inputs: &aggregate,
                        kind: OperatorKind::Collect,
                    },
                ];
                let mut backing = Backing::default();
                backing.vec(&unit)?;
                backing.vec(&scan)?;
                backing.vec(&aggregate)?;
                if streaming {
                    backing.vec(&aggregates)?;
                }
                run_plan(
                    runtime,
                    executor,
                    &operators,
                    &expressions,
                    &Vec::new(),
                    &backing,
                    &["node"],
                )
            },
        );
        match (expected, result) {
            (Some(kind), Err(error))
                if error.kind() == kind
                    && matches!(
                        (max_rows, error.cause()),
                        (
                            2,
                            GraphQueryCause::Execution(NativeExecutionError::Runtime(
                                RuntimeError::Memory(
                                    crate::property_graph::query::resources::MemoryError::Limit
                                )
                            ))
                        ) | (
                            1,
                            GraphQueryCause::Execution(NativeExecutionError::Runtime(
                                RuntimeError::BatchCapacity
                            ))
                        )
                    )
                    && error.counters().is_some_and(|counters| {
                        counters.get(WorkKind::Scans) == 2
                            // Unit + two scanned rows + one retained input: the
                            // second retained row never completed its push.
                            && counters.get(WorkKind::RowsOut) == 4
                            && counters.get(WorkKind::CompletedRows) == 0
                    }) => {}
            (None, Ok(result))
                if streaming
                    && result.metadata().rows == 1
                    && result.pools().values == [Value::I64(2)] => {}
            (None, Ok(result))
                if !streaming
                    && result.metadata().rows == 2
                    && result
                        .pools()
                        .values
                        .iter()
                        .all(|value| matches!(value, Value::Node(_))) => {}
            (expected, result) => {
                return Err(format!(
                    "blocking capacity expected {expected:?}, got {:?}",
                    result.map(|result| result.metadata().rows)
                ));
            }
        }
        if resources
            .reserved_bytes()
            .map_err(|error| error.to_string())?
            != baseline
        {
            return Err(String::from("blocking capacity leaked reservations"));
        }
    }
    Ok(())
}
