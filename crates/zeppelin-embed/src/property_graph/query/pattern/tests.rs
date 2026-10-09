#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions and fixed fixture indices"
)]

use super::*;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{
    CancelToken, Deadline, OpenOptions, QueryControl, Store, SystemMonotonicClock,
};
use crate::property_graph::query::Arithmetic;
use crate::property_graph::query::plan::{
    BinaryExpression, ExprId, Expression, Literal, NodeFacts, Operator, OperatorKind, Parameter,
    ParameterId, PatternId, PlanBacking, PlanDescription, PlanFootprint, Projection,
    RetainedRegion, SlotId, UnaryExpression, VALIDATION_SCRATCH_BYTES, ValueKinds,
};
use crate::property_graph::query::resources::{
    MemoryError, QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{
    ArenaCapacity, Completion, Execution, ExecutionCapacity, FrozenOutput, PreparedRows,
    RuntimeFailure, RuntimeLimits, WorkKind, execute_in,
};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::allocation::OsEntropy;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphDeleteMode, GraphName,
    GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue, RelId,
};
use crate::vfs::{StdVfs, SyncKind, Vfs, VfsFile};
use std::fs::File;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

type PrimitiveRow = (u128, u128, u128, u128, i64);

struct FreezePrimitiveRows;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezePrimitiveRows {
    type Output = ([Option<PrimitiveRow>; 8], usize);

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        let mut output = [None; 8];
        if rows.rows() > output.len() || rows.columns() != 4 {
            return Err(RuntimeError::Batch.into());
        }
        for (row, slot) in output.iter_mut().enumerate().take(rows.rows()) {
            let source = match rows.value(row, 0) {
                Some(super::super::QueryValue::NodeRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let relationship = match rows.value(row, 1) {
                Some(super::super::QueryValue::RelRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let target = match rows.value(row, 2) {
                Some(super::super::QueryValue::NodeRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let weight = match rows.value(row, 3) {
                Some(super::super::QueryValue::I64(value)) => value,
                _ => return Err(RuntimeError::Batch.into()),
            };
            *slot = Some((
                source.get(),
                relationship.get(),
                source.get(),
                target.get(),
                weight,
            ));
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<PrimitiveRow>(),
            0,
        )
        .map_err(Into::into)
    }
}

struct FirstPatternConsumer;

impl
    NativeReadConsumer<
        Result<Execution<([Option<PrimitiveRow>; 8], usize)>, RuntimeFailure<NativeExecutionError>>,
    > for FirstPatternConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<PrimitiveRow>; 8], usize)>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let memory = runtime.memory();
        let weight = String::from("weight");
        let scan_inputs = [PlanNodeId(0)];
        let expand_inputs = [PlanNodeId(1)];
        let filter_inputs = [PlanNodeId(2)];
        let project_inputs = [PlanNodeId(3)];
        let collect_inputs = [PlanNodeId(4)];
        let projections = [
            Projection {
                slot: SlotId(10),
                expression: ExprId(4),
            },
            Projection {
                slot: SlotId(11),
                expression: ExprId(5),
            },
            Projection {
                slot: SlotId(12),
                expression: ExprId(6),
            },
            Projection {
                slot: SlotId(13),
                expression: ExprId(7),
            },
        ];
        let expressions = [
            Expression::Slot(SlotId(2)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&weight).expect("property name"),
            },
            Expression::Literal(Literal::I64(0)),
            Expression::Binary {
                operation: BinaryExpression::Comparison(super::super::Comparison::Greater),
                left: ExprId(1),
                right: ExprId(2),
            },
            Expression::Slot(SlotId(0)),
            Expression::Slot(SlotId(2)),
            Expression::Slot(SlotId(1)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&weight).expect("property name"),
            },
        ];
        let operators = [
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
                inputs: &expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: crate::property_graph::query::plan::Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &filter_inputs,
                kind: OperatorKind::Filter(ExprId(3)),
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
        let mut facts = QueryArena::new(memory, 16).expect("fact arena");
        for _ in 0..operators.len() {
            facts.push(NodeFacts::default()).expect("fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
            RetainedRegion::slice(&scan_inputs).unwrap(),
            RetainedRegion::slice(&expand_inputs).unwrap(),
            RetainedRegion::slice(&filter_inputs).unwrap(),
            RetainedRegion::slice(&project_inputs).unwrap(),
            RetainedRegion::slice(&collect_inputs).unwrap(),
            RetainedRegion::slice(&projections).unwrap(),
            RetainedRegion::declared(weight.as_ptr() as usize, weight.capacity()).unwrap(),
        ];
        regions.sort();
        let mut external = memory
            .reserve_external_capacity()
            .expect("external plan backing");
        external
            .reserve_additional(
                VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("plan validation backing");
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(5),
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                runtime.values(),
            )
            .expect("validate pattern plan");
        let owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::array(&expressions).unwrap(),
            facts_owner,
            RetainedAllocation::array(&scan_inputs).unwrap(),
            RetainedAllocation::array(&expand_inputs).unwrap(),
            RetainedAllocation::array(&filter_inputs).unwrap(),
            RetainedAllocation::array(&project_inputs).unwrap(),
            RetainedAllocation::array(&collect_inputs).unwrap(),
            RetainedAllocation::array(&projections).unwrap(),
            RetainedAllocation::string(&weight).unwrap(),
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            runtime.values(),
        )
        .expect("retain pattern plan")
        .admit_plan(&plan, runtime.values())
        .expect("admit pattern plan");
        let mut source = NativePattern::new(
            view,
            &admitted,
            PlanNodeId(5),
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 16,
                    max_rows: 16,
                    payload_bytes: 4096,
                    variable: ArenaCapacity::default(),
                },
                expression: ExpressionCapacity {
                    cells: 16,
                    string_bytes: 256,
                },
            },
            runtime,
        )
        .expect("construct native pattern");
        Ok(execute_in(
            runtime,
            &admitted,
            &mut source,
            &mut FreezePrimitiveRows,
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 8,
                batch_payload_bytes: 4096,
                result_payload_bytes: 4096,
                batch: ArenaCapacity::default(),
                result: ArenaCapacity::default(),
            },
        ))
    }
}

fn exact_bag(mut rows: [Option<PrimitiveRow>; 8], count: usize) -> Vec<PrimitiveRow> {
    let mut values = rows
        .iter_mut()
        .take(count)
        .filter_map(Option::take)
        .collect::<Vec<_>>();
    values.sort_unstable();
    values
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PrimitiveCell {
    Unset,
    Null,
    Bool(bool),
    I64(i64),
    F64(u64),
    Node(u128),
    Relationship(u128),
    Relationships([u128; 16], u8),
    String([u8; 32], u8),
    Strings([[u8; 32]; 4], [u8; 4], u8),
}

fn primitive_string(value: &str) -> Result<([u8; 32], u8), RuntimeError> {
    if value.len() > 32 {
        return Err(RuntimeError::Batch);
    }
    let mut bytes = [0; 32];
    let destination = bytes.get_mut(..value.len()).ok_or(RuntimeError::Batch)?;
    destination.copy_from_slice(value.as_bytes());
    Ok((
        bytes,
        u8::try_from(value.len()).map_err(|_| RuntimeError::Batch)?,
    ))
}

fn string_cell(value: &str) -> PrimitiveCell {
    let (bytes, length) = primitive_string(value).expect("test string fits primitive cell");
    PrimitiveCell::String(bytes, length)
}

fn strings_cell(values: &[&str]) -> PrimitiveCell {
    let mut strings = [[0; 32]; 4];
    let mut lengths = [0; 4];
    for (position, value) in values.iter().enumerate() {
        let (bytes, length) =
            primitive_string(value).expect("test list string fits primitive cell");
        strings[position] = bytes;
        lengths[position] = length;
    }
    PrimitiveCell::Strings(
        strings,
        lengths,
        u8::try_from(values.len()).expect("test list length fits primitive cell"),
    )
}

/// One frozen batch of test cells. `PrimitiveCell` is 272 bytes wide because
/// of its `Relationships` variant, so this whole fixture is about 34 KiB.
///
/// ZE-183: the completion output travels by value through the runtime's
/// generic frames and out of every `with_native_read` call. An unoptimized
/// build gives each of those moves its own stack slot and merges none of
/// them, so a test with ten such calls in one body needed roughly 1.9 MiB of
/// stack and overflowed libtest's 2 MiB thread. Every owner below therefore
/// holds it behind a `Box`, which keeps those frames pointer-sized. Assert it
/// so a future widening of `PrimitiveCell` cannot quietly restore the frames.
/// Counters and the boxed output only; one `PrimitiveRows` is ~34 KiB.
const _: () = assert!(size_of::<Execution<Box<PrimitiveRows>>>() <= 1024);

#[derive(Clone, Debug, PartialEq)]
struct PrimitiveRows {
    rows: [[PrimitiveCell; 8]; 16],
    row_count: usize,
    column_count: usize,
}

struct FreezePrimitiveValues;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezePrimitiveValues {
    type Output = Box<PrimitiveRows>;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > 16 || rows.columns() > 8 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [[PrimitiveCell::Unset; 8]; 16];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            for (column, cell) in destination.iter_mut().enumerate().take(rows.columns()) {
                *cell = match rows.value(row, column).ok_or(RuntimeError::Batch)? {
                    QueryValue::Null => PrimitiveCell::Null,
                    QueryValue::Bool(value) => PrimitiveCell::Bool(value),
                    QueryValue::I64(value) => PrimitiveCell::I64(value),
                    QueryValue::F64(value) => PrimitiveCell::F64(value.to_bits()),
                    QueryValue::NodeRef(value) => PrimitiveCell::Node(value.id().get()),
                    QueryValue::RelRef(value) => PrimitiveCell::Relationship(value.id().get()),
                    QueryValue::List(list) => {
                        if list.len() > 16 {
                            return Err(RuntimeError::Batch.into());
                        }
                        let relationship_list = (0..list.len()).all(|position| {
                            matches!(list.get(position), Some(QueryValue::RelRef(_)))
                        });
                        if relationship_list {
                            let mut relationships = [0; 16];
                            for (position, relationship) in
                                relationships.iter_mut().enumerate().take(list.len())
                            {
                                *relationship = match list.get(position) {
                                    Some(QueryValue::RelRef(value)) => value.id().get(),
                                    _ => return Err(RuntimeError::Batch.into()),
                                };
                            }
                            PrimitiveCell::Relationships(
                                relationships,
                                u8::try_from(list.len()).map_err(|_| RuntimeError::Batch)?,
                            )
                        } else {
                            if list.len() > 4 {
                                return Err(RuntimeError::Batch.into());
                            }
                            let mut strings = [[0; 32]; 4];
                            let mut lengths = [0; 4];
                            for position in 0..list.len() {
                                let value = match list.get(position) {
                                    Some(QueryValue::String(value)) => value,
                                    _ => return Err(RuntimeError::Batch.into()),
                                };
                                let (bytes, length) = primitive_string(value)?;
                                *strings.get_mut(position).ok_or(RuntimeError::Batch)? = bytes;
                                *lengths.get_mut(position).ok_or(RuntimeError::Batch)? = length;
                            }
                            PrimitiveCell::Strings(
                                strings,
                                lengths,
                                u8::try_from(list.len()).map_err(|_| RuntimeError::Batch)?,
                            )
                        }
                    }
                    QueryValue::String(value) => {
                        let (bytes, length) = primitive_string(value)?;
                        PrimitiveCell::String(bytes, length)
                    }
                };
            }
        }
        FrozenOutput::new(
            Box::new(PrimitiveRows {
                rows: output,
                row_count: rows.rows(),
                column_count: rows.columns(),
            }),
            rows.rows(),
            rows.rows() * rows.columns() * size_of::<PrimitiveCell>(),
            0,
        )
        .map_err(Into::into)
    }
}

struct RecordingCompletion {
    called: Arc<AtomicBool>,
}

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for RecordingCompletion {
    type Output = Box<PrimitiveRows>;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        self.called.store(true, Ordering::SeqCst);
        FreezePrimitiveValues.complete(rows, context)
    }
}

macro_rules! execute_pattern {
    (@wrap $source:expr, $wrap:expr) => {
        $wrap($source)
    };
    (@wrap $source:expr) => {
        $source
    };
    (@execution $execution:expr) => {
        $execution
    };
    (@execution) => {
        ExecutionCapacity {
            batch_rows: 1,
            result_rows: 16,
            batch_payload_bytes: 8192,
            result_payload_bytes: 8192,
            batch: ArenaCapacity {
                string_bytes: 8192,
                list_cells: 256,
                node_ids: 64,
                relationship_ids: 256,
            },
            result: ArenaCapacity {
                string_bytes: 8192,
                list_cells: 256,
                node_ids: 64,
                relationship_ids: 256,
            },
        }
    };
    (@pattern_rows $rows:expr) => {
        $rows
    };
    (@pattern_rows) => {
        64
    };
    ($view:expr, $runtime:expr, $operators:ident, $expressions:ident,
     $regions:expr, $owners:expr, $completion:expr $(, wrap = $wrap:expr)?
     $(, execution = $execution:expr)? $(, pattern_rows = $pattern_rows:expr)?) => {{
        let memory = $runtime.memory();
        let mut facts = QueryArena::new(memory, $operators.len()).expect("fact arena");
        for _ in 0..$operators.len() {
            facts.push(NodeFacts::default()).expect("fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&$operators).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
        ];
        if !$expressions.is_empty() {
            regions.push(RetainedRegion::slice(&$expressions).unwrap());
        }
        regions.extend($regions);
        regions.sort();
        let retained_bytes = regions
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.end() - region.start())
            })
            .expect("retained plan byte total");
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
        let description = PlanDescription {
            operators: &$operators,
            expressions: &$expressions,
            parameters: &[],
            root: PlanNodeId(u32::try_from($operators.len() - 1).unwrap()),
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                $runtime.values(),
            )
            .expect("validate pattern plan");
        let mut owners = vec![RetainedAllocation::array(&$operators).unwrap(), facts_owner];
        if !$expressions.is_empty() {
            owners.push(RetainedAllocation::array(&$expressions).unwrap());
        }
        owners.extend($owners);
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            $runtime.values(),
        )
        .expect("retain pattern plan")
        .admit_plan(&plan, $runtime.values())
        .expect("admit pattern plan");
        let pattern_rows = execute_pattern!(@pattern_rows $($pattern_rows)?);
        let source = NativePattern::new(
            $view,
            &admitted,
            PlanNodeId(u32::try_from($operators.len() - 1).unwrap()),
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: pattern_rows,
 max_rows: pattern_rows,
                    payload_bytes: 8192,
                    variable: ArenaCapacity {
                        string_bytes: 8192,
                        list_cells: 256,
                        node_ids: 64,
                        relationship_ids: 256,
                    },
                },
                expression: ExpressionCapacity {
                    cells: 64,
                    string_bytes: 8192,
                },
            },
            $runtime,
        )
        .expect("construct native pattern");
        let mut source = execute_pattern!(@wrap source $(, $wrap)?);
        let execution_capacity = execute_pattern!(@execution $($execution)?);
        execute_in(
            $runtime,
            &admitted,
            &mut source,
            $completion,
            execution_capacity,
        )
    }};
}

struct PublishAfterFirst<O> {
    source: O,
    action: Option<PublicationAction>,
}

struct PublicationAction {
    store: Arc<Store>,
    source: NodeId,
    receipts: Arc<Mutex<Option<(NodeId, RelId)>>>,
}

struct FailMapAfterFirst<O> {
    source: O,
    vfs: Arc<ScheduledMapVfs>,
    armed: bool,
    arm_fault: bool,
}

struct ActAfterFirst<O, F> {
    source: O,
    action: Option<F>,
}

impl<'v, 'm, 'g, O, F> PullOperator<'v, 'm, 'g, NativeExecutionError> for ActAfterFirst<O, F>
where
    O: PullOperator<'v, 'm, 'g, NativeExecutionError>,
    F: FnOnce(),
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
        if output.rows() != 0
            && let Some(action) = self.action.take()
        {
            action();
        }
        Ok(state)
    }
}

impl<'v, 'm, 'g, O> PullOperator<'v, 'm, 'g, NativeExecutionError> for FailMapAfterFirst<O>
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
        if self.arm_fault && !self.armed && output.rows() != 0 {
            self.vfs.arm_next();
            self.armed = true;
        }
        Ok(state)
    }
}

#[derive(Default)]
struct ScheduledMapVfs {
    calls: AtomicU64,
    fail_at: AtomicU64,
    fires: AtomicU64,
}

impl ScheduledMapVfs {
    fn arm_next(&self) {
        self.fail_at
            .store(self.calls.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    }

    fn disarm(&self) {
        self.fail_at.store(0, Ordering::SeqCst);
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
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_at.load(Ordering::SeqCst) == call {
            self.fires.fetch_add(1, Ordering::SeqCst);
            return Err(std::io::Error::other("scheduled native-pattern map fault"));
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

impl<'v, 'm, 'g, O> PullOperator<'v, 'm, 'g, NativeExecutionError> for PublishAfterFirst<O>
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
        if output.rows() != 0
            && let Some(action) = self.action.take()
        {
            std::thread::spawn(move || {
                crate::property_graph::with_local_refs(|refs| {
                    let mut labels = [GraphName::new("Target").expect("publication label")];
                    let mut properties = [GraphProperty::new(
                        GraphName::new("name").expect("publication property name"),
                        PropertyValue::new(PropertyData::String("new-target"))
                            .expect("publication property value"),
                    )];
                    let node = CanonicalContents::node(
                        &mut labels,
                        &mut properties,
                        Some("new-text"),
                        None,
                    )
                    .expect("publication node");
                    let published = action
                        .store
                        .apply_native_graph(
                            &[
                                StructuredWrite {
                                    key: ApplicationKey::new(
                                        EntityKind::Node,
                                        "publication",
                                        "new",
                                    )
                                    .expect("publication key"),
                                    revision: GraphRevision::new(1).expect("publication revision"),
                                    operation: StructuredOperation::Create,
                                    image: Some(WriteImage::Node(&node)),
                                },
                                StructuredWrite {
                                    key: ApplicationKey::new(
                                        EntityKind::Relationship,
                                        "publication",
                                        "new-link",
                                    )
                                    .expect("publication relationship key"),
                                    revision: GraphRevision::new(1)
                                        .expect("publication relationship revision"),
                                    operation: StructuredOperation::Create,
                                    image: Some(WriteImage::Relationship {
                                        source: NodeRef::Existing(action.source),
                                        target: NodeRef::Local(
                                            refs.node(0).expect("publication local node"),
                                        ),
                                        relationship_type: GraphName::new("NEW_LINK")
                                            .expect("publication relationship type"),
                                        properties: &[],
                                    }),
                                },
                            ],
                            &QueryControl::Cancel(CancelToken::new()),
                        )
                        .expect("publish while old graph view is paused");
                    let node = match published[0].entity {
                        EntityId::Node(id) => id,
                        _ => panic!("publication node receipt kind"),
                    };
                    let relationship = match published[1].entity {
                        EntityId::Relationship(id) => id,
                        _ => panic!("publication relationship receipt kind"),
                    };
                    *action.receipts.lock().expect("publication receipt lock") =
                        Some((node, relationship));
                });
            })
            .join()
            .expect("publication thread");
        }
        Ok(state)
    }
}

#[test]
fn native_pattern_scan_expand_scalar_bag() {
    let directory = tempfile::tempdir().expect("native pattern store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native graph");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let mut first_properties = [GraphProperty::new(
            GraphName::new("ordinal").unwrap(),
            PropertyValue::new(PropertyData::I64(1)).unwrap(),
        )];
        let mut second_properties = [GraphProperty::new(
            GraphName::new("ordinal").unwrap(),
            PropertyValue::new(PropertyData::I64(2)).unwrap(),
        )];
        let first = CanonicalContents::node(&mut [], &mut first_properties, None, None).unwrap();
        let second = CanonicalContents::node(&mut [], &mut second_properties, None, None).unwrap();
        let relationship_properties = [1_i64, 2, 3].map(|weight| {
            [GraphProperty::new(
                GraphName::new("weight").unwrap(),
                PropertyValue::new(PropertyData::I64(weight)).unwrap(),
            )]
        });
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "pattern", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "pattern", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "pattern", "ab-1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &relationship_properties[0],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "pattern", "ab-2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &relationship_properties[1],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "pattern", "aa").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(0).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &relationship_properties[2],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish first pattern fixture")
    });
    let node_a = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("first receipt kind"),
    };
    let node_b = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("second receipt kind"),
    };
    let relationships = receipts[2..]
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Relationship(id) => id,
            _ => panic!("relationship receipt kind"),
        })
        .collect::<Vec<RelId>>();
    let expected = vec![
        (
            node_a.get(),
            relationships[0].get(),
            node_a.get(),
            node_b.get(),
            1,
        ),
        (
            node_a.get(),
            relationships[1].get(),
            node_a.get(),
            node_b.get(),
            2,
        ),
        (
            node_a.get(),
            relationships[2].get(),
            node_a.get(),
            node_a.get(),
            3,
        ),
    ];
    let execution = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            FirstPatternConsumer,
        )
        .expect("admit real native view")
        .expect("execute native pattern");
    let observed = exact_bag(execution.output.0, execution.output.1);
    let mut expected = expected;
    expected.sort_unstable();
    assert_eq!(observed, expected);
    assert!(execution.counters.get(WorkKind::Scans) > 0);
    assert!(execution.counters.get(WorkKind::AdjacencyEntries) > 0);
    let mut omitted = observed.clone();
    omitted.remove(1);
    assert_ne!(
        omitted, expected,
        "omission control must reject a lost parallel edge"
    );
    store.close().expect("close native pattern store");
}

struct KeyPatternConsumer {
    node: NodeId,
    relationship: RelId,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for KeyPatternConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let namespace = String::from("keys");
        let key = String::from("same");
        let unit_input = [PlanNodeId(0)];
        let node_input = [PlanNodeId(1)];
        let relationship_input = [PlanNodeId(2)];
        let node_key_input = [PlanNodeId(3)];
        let relationship_key_input = [PlanNodeId(4)];
        let expressions = [Expression::Literal(Literal::String(&key))];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &unit_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.node,
                },
            },
            Operator {
                inputs: &node_input,
                kind: OperatorKind::LookupRelationship {
                    output: SlotId(1),
                    id: self.relationship,
                },
            },
            Operator {
                inputs: &relationship_input,
                kind: OperatorKind::LookupKey {
                    output: SlotId(2),
                    namespace: GraphName::new(&namespace).expect("key namespace"),
                    key: ExprId(0),
                    kind: EntityKind::Node,
                },
            },
            Operator {
                inputs: &node_key_input,
                kind: OperatorKind::LookupKey {
                    output: SlotId(3),
                    namespace: GraphName::new(&namespace).expect("key namespace"),
                    key: ExprId(0),
                    kind: EntityKind::Relationship,
                },
            },
            Operator {
                inputs: &relationship_key_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&unit_input).unwrap(),
                RetainedRegion::slice(&node_input).unwrap(),
                RetainedRegion::slice(&relationship_input).unwrap(),
                RetainedRegion::slice(&node_key_input).unwrap(),
                RetainedRegion::slice(&relationship_key_input).unwrap(),
                RetainedRegion::declared(namespace.as_ptr() as usize, namespace.capacity())
                    .unwrap(),
                RetainedRegion::declared(key.as_ptr() as usize, key.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&unit_input).unwrap(),
                RetainedAllocation::array(&node_input).unwrap(),
                RetainedAllocation::array(&relationship_input).unwrap(),
                RetainedAllocation::array(&node_key_input).unwrap(),
                RetainedAllocation::array(&relationship_key_input).unwrap(),
                RetainedAllocation::string(&namespace).unwrap(),
                RetainedAllocation::string(&key).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

struct IdentityPatternConsumer(EntityId);

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for IdentityPatternConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let collect_input = [PlanNodeId(1)];
        let kind = match self.0 {
            EntityId::Node(id) => OperatorKind::LookupNode {
                output: SlotId(0),
                id,
            },
            EntityId::Relationship(id) => OperatorKind::LookupRelationship {
                output: SlotId(0),
                id,
            },
        };
        let expressions = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind,
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

enum KeySourceCase {
    Literal {
        namespace: &'static str,
        key: &'static str,
        kind: EntityKind,
    },
    Null,
}

struct KeySourceConsumer(KeySourceCase);

struct KeyLimitCompletion {
    called: Arc<AtomicBool>,
}

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for KeyLimitCompletion {
    type Output = ();

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        self.called.store(true, Ordering::SeqCst);
        FrozenOutput::new((), rows.rows(), 0, 0).map_err(Into::into)
    }
}

struct DynamicKeyLimitConsumer {
    namespace: String,
    key: String,
    completion_called: Arc<AtomicBool>,
}

impl NativeReadConsumer<Result<Execution<()>, RuntimeFailure<NativeExecutionError>>>
    for DynamicKeyLimitConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<()>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let parameter_name = String::from("key");
        let lookup_input = [PlanNodeId(0)];
        let collect_input = [PlanNodeId(1)];
        let parameters = [Parameter {
            name: &parameter_name,
            kinds: ValueKinds::STRING,
        }];
        let bindings = [ParameterBinding {
            name: &parameter_name,
            value: QueryValue::String(&self.key),
        }];
        let expressions = [Expression::Parameter(ParameterId(0))];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupKey {
                    output: SlotId(0),
                    namespace: GraphName::new(&self.namespace).expect("dynamic key namespace"),
                    key: ExprId(0),
                    kind: EntityKind::Node,
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len()).expect("dynamic key facts");
        for _ in 0..operators.len() {
            facts
                .push(NodeFacts::default())
                .expect("dynamic key fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::slice(&parameters).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
            RetainedRegion::slice(&lookup_input).unwrap(),
            RetainedRegion::slice(&collect_input).unwrap(),
            RetainedRegion::declared(parameter_name.as_ptr() as usize, parameter_name.capacity())
                .unwrap(),
            RetainedRegion::declared(self.namespace.as_ptr() as usize, self.namespace.capacity())
                .unwrap(),
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.end() - region.start())
            })
            .expect("dynamic key retained bytes");
        let mut external = memory
            .reserve_external_capacity()
            .expect("dynamic key external backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("dynamic key validation backing");
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &parameters,
            root: PlanNodeId(2),
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                runtime.values(),
            )
            .expect("validate dynamic key plan");
        let owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::array(&expressions).unwrap(),
            RetainedAllocation::array(&parameters).unwrap(),
            facts_owner,
            RetainedAllocation::array(&lookup_input).unwrap(),
            RetainedAllocation::array(&collect_input).unwrap(),
            RetainedAllocation::string(&parameter_name).unwrap(),
            RetainedAllocation::string(&self.namespace).unwrap(),
            RetainedAllocation::array(&bindings).unwrap(),
            RetainedAllocation::string(&self.key).unwrap(),
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            runtime.values(),
        )
        .expect("retain dynamic key inputs")
        .admit_plan(&plan, runtime.values())
        .expect("admit dynamic key plan");
        let mut source = NativePattern::new(
            view,
            &admitted,
            PlanNodeId(2),
            &bindings,
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 1,
                    max_rows: 1,
                    payload_bytes: 64,
                    variable: ArenaCapacity {
                        string_bytes: 64,
                        list_cells: 1,
                        node_ids: 1,
                        relationship_ids: 1,
                    },
                },
                expression: ExpressionCapacity {
                    cells: 1,
                    string_bytes: self.key.len(),
                },
            },
            runtime,
        )
        .expect("construct dynamic key pattern");
        Ok(execute_in(
            runtime,
            &admitted,
            &mut source,
            &mut KeyLimitCompletion {
                called: Arc::clone(&self.completion_called),
            },
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 1,
                batch_payload_bytes: 64,
                result_payload_bytes: 64,
                batch: ArenaCapacity {
                    string_bytes: 64,
                    list_cells: 1,
                    node_ids: 1,
                    relationship_ids: 1,
                },
                result: ArenaCapacity {
                    string_bytes: 64,
                    list_cells: 1,
                    node_ids: 1,
                    relationship_ids: 1,
                },
            },
        ))
    }
}

struct StaticKeyLimitConsumer {
    key: String,
}

impl NativeReadConsumer<Result<(), MemoryError>> for StaticKeyLimitConsumer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        _: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Result<(), MemoryError>, crate::property_graph::storage::tree::directory::TreeError>
    {
        let namespace = String::from("0123456789");
        let lookup_input = [PlanNodeId(0)];
        let collect_input = [PlanNodeId(1)];
        let expressions = [Expression::Literal(Literal::String(&self.key))];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupKey {
                    output: SlotId(0),
                    namespace: GraphName::new(&namespace).expect("static key namespace"),
                    key: ExprId(0),
                    kind: EntityKind::Node,
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len()).expect("static key facts");
        for _ in 0..operators.len() {
            facts
                .push(NodeFacts::default())
                .expect("static key fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
            RetainedRegion::slice(&lookup_input).unwrap(),
            RetainedRegion::slice(&collect_input).unwrap(),
            RetainedRegion::declared(namespace.as_ptr() as usize, namespace.capacity()).unwrap(),
            RetainedRegion::declared(self.key.as_ptr() as usize, self.key.capacity()).unwrap(),
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.end() - region.start())
            })
            .expect("static key retained bytes");
        let mut external = memory
            .reserve_external_capacity()
            .expect("static key external backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("static key validation backing");
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(2),
            eager_searches: &[],
        };
        Ok(facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                runtime.values(),
            )
            .map(|_| ()))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for KeySourceConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let (namespace_text, mut key_text, kind, literal) = match self.0 {
            KeySourceCase::Literal {
                namespace,
                key,
                kind,
            } => (String::from(namespace), String::from(key), kind, true),
            KeySourceCase::Null => (
                String::from("keys"),
                String::with_capacity(1),
                EntityKind::Node,
                false,
            ),
        };
        if key_text.capacity() == 0 {
            key_text.reserve_exact(1);
        }
        let lookup_input = [PlanNodeId(0)];
        let collect_input = [PlanNodeId(1)];
        let expressions = [if literal {
            Expression::Literal(Literal::String(&key_text))
        } else {
            Expression::Literal(Literal::Null)
        }];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupKey {
                    output: SlotId(0),
                    namespace: GraphName::new(&namespace_text).expect("key namespace"),
                    key: ExprId(0),
                    kind,
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::declared(
                    namespace_text.as_ptr() as usize,
                    namespace_text.capacity(),
                )
                .unwrap(),
                RetainedRegion::declared(key_text.as_ptr() as usize, key_text.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::string(&namespace_text).unwrap(),
                RetainedAllocation::string(&key_text).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

struct LabelPatternConsumer {
    missing_label: bool,
    all_missing_types: bool,
    direction: Direction,
}

struct InIdentityConsumer {
    node: NodeId,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for InIdentityConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let filter_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let list_items = [ExprId(0)];
        let expressions = [
            Expression::Slot(SlotId(0)),
            Expression::List(&list_items),
            Expression::Binary {
                operation: BinaryExpression::In,
                left: ExprId(0),
                right: ExprId(1),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.node,
                },
            },
            Operator {
                inputs: &filter_input,
                kind: OperatorKind::Filter(ExprId(2)),
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&filter_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&list_items).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&filter_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&list_items).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

struct DynamicKeyConsumer {
    node: NodeId,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for DynamicKeyConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let namespace = String::from("keys");
        let property = String::from("key_value");
        let lookup_input = [PlanNodeId(0)];
        let key_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let expressions = [
            Expression::Slot(SlotId(0)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&property).expect("key property"),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.node,
                },
            },
            Operator {
                inputs: &key_input,
                kind: OperatorKind::LookupKey {
                    output: SlotId(1),
                    namespace: GraphName::new(&namespace).expect("dynamic key namespace"),
                    key: ExprId(1),
                    kind: EntityKind::Node,
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&key_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::declared(namespace.as_ptr() as usize, namespace.capacity())
                    .unwrap(),
                RetainedRegion::declared(property.as_ptr() as usize, property.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&key_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::string(&namespace).unwrap(),
                RetainedAllocation::string(&property).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for LabelPatternConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let label = String::from(if self.missing_label {
            "Missing"
        } else {
            "Person"
        });
        let known_type = String::from("LINKS");
        let missing_type = String::from("MISSING");
        let alternatives = if self.all_missing_types {
            [
                GraphName::new(&missing_type).expect("missing type"),
                GraphName::new(&missing_type).expect("missing type"),
                GraphName::new(&missing_type).expect("missing type"),
            ]
        } else {
            [
                GraphName::new(&known_type).expect("known type"),
                GraphName::new(&known_type).expect("duplicate type"),
                GraphName::new(&missing_type).expect("missing type"),
            ]
        };
        let scan_input = [PlanNodeId(0)];
        let expand_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let expressions = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &scan_input,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: Some(GraphName::new(&label).expect("scan label")),
                },
            },
            Operator {
                inputs: &expand_input,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: self.direction,
                    relationship_types: &alternatives,
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_input).unwrap(),
                RetainedRegion::slice(&expand_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&alternatives).unwrap(),
                RetainedRegion::declared(label.as_ptr() as usize, label.capacity()).unwrap(),
                RetainedRegion::declared(known_type.as_ptr() as usize, known_type.capacity())
                    .unwrap(),
                RetainedRegion::declared(missing_type.as_ptr() as usize, missing_type.capacity(),)
                    .unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_input).unwrap(),
                RetainedAllocation::array(&expand_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&alternatives).unwrap(),
                RetainedAllocation::string(&label).unwrap(),
                RetainedAllocation::string(&known_type).unwrap(),
                RetainedAllocation::string(&missing_type).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

#[test]
fn native_pattern_keys_labels_liveness_full_ids() {
    const LONG_KEY: &str = "long-key-00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";
    let directory = tempfile::tempdir().expect("native key pattern store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native graph");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let mut labels_a = [GraphName::new("Person").unwrap()];
        let mut labels_b = [GraphName::new("Person").unwrap()];
        let mut properties_a = [GraphProperty::new(
            GraphName::new("key_value").unwrap(),
            PropertyValue::new(PropertyData::String("same")).unwrap(),
        )];
        let mut properties_b = [GraphProperty::new(
            GraphName::new("key_value").unwrap(),
            PropertyValue::new(PropertyData::I64(7)).unwrap(),
        )];
        let first = CanonicalContents::node(&mut labels_a, &mut properties_a, None, None).unwrap();
        let second = CanonicalContents::node(&mut labels_b, &mut properties_b, None, None).unwrap();
        let mut no_labels: [GraphName<'_>; 0] = [];
        let mut no_properties: [GraphProperty<'_>; 0] = [];
        let unlabelled =
            CanonicalContents::node(&mut no_labels, &mut no_properties, None, None).unwrap();
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "keys", "same").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "keys", "other").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "keys", "same").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "keys", "").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&unlabelled)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "keys", "\0").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&unlabelled)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "keys", LONG_KEY).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&unlabelled)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "keys", "self").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(0).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish key fixture")
    });
    let node = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("node receipt kind"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("relationship receipt kind"),
    };
    let other = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("other node receipt kind"),
    };
    let execution = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            KeyPatternConsumer { node, relationship },
        )
        .expect("admit key view")
        .expect("execute key lookups");
    assert_eq!(execution.output.row_count, 1);
    assert_eq!(execution.output.column_count, 4);
    assert_eq!(
        &execution.output.rows[0][..4],
        &[
            PrimitiveCell::Node(node.get()),
            PrimitiveCell::Relationship(relationship.get()),
            PrimitiveCell::Node(node.get()),
            PrimitiveCell::Relationship(relationship.get()),
        ]
    );
    let high_node = NodeId::new(node.get() | (1_u128 << 96)).unwrap();
    let high_relationship = RelId::new(relationship.get() | (1_u128 << 96)).unwrap();
    assert_ne!(node, high_node);
    assert_ne!(relationship, high_relationship);
    for (receipt, key) in receipts[3..6].iter().zip(["", "\0", LONG_KEY]) {
        let expected = match receipt.entity {
            EntityId::Node(id) => id,
            _ => panic!("exact key receipt kind"),
        };
        let exact = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                KeySourceConsumer(KeySourceCase::Literal {
                    namespace: "keys",
                    key,
                    kind: EntityKind::Node,
                }),
            )
            .expect("admit exact key view")
            .expect("execute exact key lookup");
        assert_eq!(exact.output.row_count, 1);
        assert_eq!(exact.output.rows[0][0], PrimitiveCell::Node(expected.get()));
    }
    for identity in [
        EntityId::Node(high_node),
        EntityId::Relationship(high_relationship),
    ] {
        let missing = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                IdentityPatternConsumer(identity),
            )
            .expect("admit full-width identity view")
            .expect("execute full-width identity lookup");
        assert_eq!(
            missing.output.row_count, 0,
            "upper identity bits must not alias"
        );
    }
    for case in [
        KeySourceCase::Literal {
            namespace: "missing",
            key: "same",
            kind: EntityKind::Node,
        },
        KeySourceCase::Literal {
            namespace: "keys",
            key: "missing",
            kind: EntityKind::Node,
        },
    ] {
        let missing = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                KeySourceConsumer(case),
            )
            .expect("admit missing key view")
            .expect("execute missing key lookup");
        assert_eq!(missing.output.row_count, 0);
    }
    let null_key = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            KeySourceConsumer(KeySourceCase::Null),
        )
        .expect("admit null key view")
        .expect("execute null key lookup");
    assert_eq!(null_key.output.row_count, 0);
    assert_eq!(null_key.counters.get(WorkKind::Lookups), 0);
    let static_limit = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            24 * 1024 * 1024,
            64,
            StaticKeyLimitConsumer {
                key: "k".repeat(MAX_GRAPH_INPUT_BYTES - 9),
            },
        )
        .expect("admit static key limit view");
    assert!(matches!(
        static_limit,
        Err(MemoryError::Plan(PlanError::Limit))
    ));
    for (namespace, key_bytes) in [
        ("0123456789", MAX_GRAPH_INPUT_BYTES - 9),
        ("n", MAX_GRAPH_INPUT_BYTES - 8),
    ] {
        let completion_called = Arc::new(AtomicBool::new(false));
        let failure = match store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                24 * 1024 * 1024,
                64,
                DynamicKeyLimitConsumer {
                    namespace: String::from(namespace),
                    key: "k".repeat(key_bytes),
                    completion_called: Arc::clone(&completion_called),
                },
            )
            .expect("admit dynamic key limit view")
        {
            Ok(_) => panic!("dynamic oversized key must fail"),
            Err(failure) => failure,
        };
        assert!(matches!(
            failure.error,
            NativeExecutionError::Expression(ExpressionError {
                expression: ExprId(0),
                failure: ExpressionFailure::Plan(PlanError::Limit),
            })
        ));
        assert!(failure.counters.get(WorkKind::Expressions) > 0);
        assert!(failure.counters.get(WorkKind::CopiedBytes) > 0);
        assert_eq!(failure.counters.get(WorkKind::Lookups), 0);
        assert!(!completion_called.load(Ordering::SeqCst));
    }
    let non_string = match store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            DynamicKeyConsumer { node: other },
        )
        .expect("admit dynamic non-string key")
    {
        Ok(_) => panic!("dynamic non-string key must fail"),
        Err(failure) => failure,
    };
    assert_eq!(non_string.operator, PlanNodeId(3));
    match non_string.error {
        NativeExecutionError::Expression(ExpressionError {
            expression: ExprId(1),
            failure: ExpressionFailure::Runtime(RuntimeError::Value(QueryError::Type)),
        }) => {}
        error => panic!("unexpected dynamic key type failure: {error:?}"),
    }
    let typed = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            LabelPatternConsumer {
                missing_label: false,
                all_missing_types: false,
                direction: Direction::Outgoing,
            },
        )
        .expect("admit typed expansion")
        .expect("execute typed expansion");
    assert_eq!(
        typed.output.row_count, 2,
        "duplicate type alternatives do not multiply rows"
    );
    let self_relationship = match receipts[6].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("self relationship receipt kind"),
    };
    let mut typed_bag = typed.output.rows[..typed.output.row_count]
        .iter()
        .map(|row| match (row[0], row[1], row[2]) {
            (
                PrimitiveCell::Node(source),
                PrimitiveCell::Node(target),
                PrimitiveCell::Relationship(relationship),
            ) => (source, target, relationship),
            values => panic!("typed expansion row: {values:?}"),
        })
        .collect::<Vec<_>>();
    typed_bag.sort_unstable();
    let mut expected_typed = vec![
        (node.get(), other.get(), relationship.get()),
        (node.get(), node.get(), self_relationship.get()),
    ];
    expected_typed.sort_unstable();
    assert_eq!(typed_bag, expected_typed);
    let undirected = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            LabelPatternConsumer {
                missing_label: false,
                all_missing_types: false,
                direction: Direction::Either,
            },
        )
        .expect("admit undirected typed expansion")
        .expect("execute undirected typed expansion");
    let mut undirected_bag = undirected.output.rows[..undirected.output.row_count]
        .iter()
        .map(|row| match (row[0], row[1], row[2]) {
            (
                PrimitiveCell::Node(source),
                PrimitiveCell::Node(target),
                PrimitiveCell::Relationship(relationship),
            ) => (source, target, relationship),
            values => panic!("undirected expansion row: {values:?}"),
        })
        .collect::<Vec<_>>();
    undirected_bag.sort_unstable();
    let mut expected_undirected = vec![
        (node.get(), other.get(), relationship.get()),
        (other.get(), node.get(), relationship.get()),
        (node.get(), node.get(), self_relationship.get()),
    ];
    expected_undirected.sort_unstable();
    assert_eq!(undirected_bag, expected_undirected);
    let in_identity = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            InIdentityConsumer { node },
        )
        .expect("admit same-view identity IN")
        .expect("execute same-view identity IN");
    assert_eq!(in_identity.output.row_count, 1);
    assert_eq!(
        in_identity.output.rows[0][0],
        PrimitiveCell::Node(node.get())
    );
    for consumer in [
        LabelPatternConsumer {
            missing_label: true,
            all_missing_types: false,
            direction: Direction::Outgoing,
        },
        LabelPatternConsumer {
            missing_label: false,
            all_missing_types: true,
            direction: Direction::Outgoing,
        },
    ] {
        let missing = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                consumer,
            )
            .expect("admit missing symbol view")
            .expect("execute missing symbol source");
        assert_eq!(missing.output.row_count, 0);
    }

    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "keys", "other").unwrap(),
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Delete(receipts[1].entity, GraphDeleteMode::Detach),
                image: None,
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("detach relationship endpoint");
    let hidden = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            KeyPatternConsumer { node, relationship },
        )
        .expect("admit detached view")
        .expect("execute detached key lookups");
    assert_eq!(
        hidden.output.row_count, 0,
        "detached endpoint hides relationship keys"
    );
    let recreated = crate::property_graph::with_local_refs(|_| {
        let mut labels = [GraphName::new("Person").unwrap()];
        let mut properties = [GraphProperty::new(
            GraphName::new("key_value").unwrap(),
            PropertyValue::new(PropertyData::I64(8)).unwrap(),
        )];
        let image = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "keys", "other").unwrap(),
                    revision: GraphRevision::new(3).unwrap(),
                    operation: StructuredOperation::Recreate(GraphRevision::new(2).unwrap()),
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("recreate detached key")
    });
    let recreated_node = match recreated[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("recreated node kind"),
    };
    assert_ne!(recreated_node, other);
    let recreated_lookup = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            KeySourceConsumer(KeySourceCase::Literal {
                namespace: "keys",
                key: "other",
                kind: EntityKind::Node,
            }),
        )
        .expect("admit recreated key view")
        .expect("execute recreated key lookup");
    assert_eq!(recreated_lookup.output.row_count, 1);
    assert_eq!(
        recreated_lookup.output.rows[0][0],
        PrimitiveCell::Node(recreated_node.get())
    );
    store.close().expect("close key pattern store");
}

struct PathPatternConsumer {
    start: NodeId,
    min: u8,
    max: u8,
}

struct PredicatePathConsumer {
    start: NodeId,
    completed: bool,
}

struct ZeroHopPredicateConsumer {
    start: NodeId,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for ZeroHopPredicateConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let path_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let expressions = [
            Expression::Literal(Literal::I64(1)),
            Expression::Literal(Literal::I64(0)),
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Divide),
                left: ExprId(0),
                right: ExprId(1),
            },
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Greater),
                left: ExprId(2),
                right: ExprId(1),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationships: SlotId(2),
                    edge_predicate: Some(EdgePredicate {
                        current_edge: SlotId(9),
                        expression: ExprId(3),
                    }),
                    completed_edge_predicate: Some(CompletedEdgePredicate {
                        current_edge: SlotId(9),
                        expression: ExprId(3),
                    }),
                    min: 0,
                    max: 0,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&path_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&path_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut FreezePrimitiveValues,
            execution = ExecutionCapacity {
                batch_rows: 1,
                result_rows: 24,
                batch_payload_bytes: 8192,
                result_payload_bytes: 8192,
                batch: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
                result: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
            }
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for PredicatePathConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let path_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        if self.completed {
            let property = String::from("expected_length");
            let expressions = [
                Expression::Slot(SlotId(9)),
                Expression::Property {
                    entity: ExprId(0),
                    name: GraphName::new(&property).expect("completed property"),
                },
                Expression::Slot(SlotId(2)),
                Expression::Unary {
                    operation: UnaryExpression::Size,
                    operand: ExprId(2),
                },
                Expression::Binary {
                    operation: BinaryExpression::Comparison(Comparison::Equal),
                    left: ExprId(1),
                    right: ExprId(3),
                },
            ];
            let operators = [
                Operator {
                    inputs: &[],
                    kind: OperatorKind::Unit,
                },
                Operator {
                    inputs: &lookup_input,
                    kind: OperatorKind::LookupNode {
                        output: SlotId(0),
                        id: self.start,
                    },
                },
                Operator {
                    inputs: &path_input,
                    kind: OperatorKind::BoundedExpand {
                        source: SlotId(0),
                        node: SlotId(1),
                        relationships: SlotId(2),
                        edge_predicate: None,
                        completed_edge_predicate: Some(CompletedEdgePredicate {
                            current_edge: SlotId(9),
                            expression: ExprId(4),
                        }),
                        min: 0,
                        max: 2,
                        direction: Direction::Outgoing,
                        relationship_types: &[],
                        pattern: PatternId(0),
                    },
                },
                Operator {
                    inputs: &collect_input,
                    kind: OperatorKind::Collect,
                },
            ];
            return Ok(execute_pattern!(
                view,
                runtime,
                operators,
                expressions,
                vec![
                    RetainedRegion::slice(&lookup_input).unwrap(),
                    RetainedRegion::slice(&path_input).unwrap(),
                    RetainedRegion::slice(&collect_input).unwrap(),
                    RetainedRegion::declared(property.as_ptr() as usize, property.capacity())
                        .unwrap(),
                ],
                vec![
                    RetainedAllocation::array(&lookup_input).unwrap(),
                    RetainedAllocation::array(&path_input).unwrap(),
                    RetainedAllocation::array(&collect_input).unwrap(),
                    RetainedAllocation::string(&property).unwrap(),
                ],
                &mut FreezePrimitiveValues
            ));
        }
        let property = String::from("pre");
        let expressions = [
            Expression::Slot(SlotId(9)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&property).expect("edge property"),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationships: SlotId(2),
                    edge_predicate: Some(EdgePredicate {
                        current_edge: SlotId(9),
                        expression: ExprId(1),
                    }),
                    completed_edge_predicate: None,
                    min: 0,
                    max: 2,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&path_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::declared(property.as_ptr() as usize, property.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&path_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::string(&property).unwrap(),
            ],
            &mut FreezePrimitiveValues,
            execution = ExecutionCapacity {
                batch_rows: 1,
                result_rows: 24,
                batch_payload_bytes: 8192,
                result_payload_bytes: 8192,
                batch: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
                result: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
            }
        ))
    }
}

struct LateErrorPathConsumer {
    start: NodeId,
    completion_called: Arc<AtomicBool>,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for LateErrorPathConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let property = String::from("divisor");
        let lookup_input = [PlanNodeId(0)];
        let path_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let expressions = [
            Expression::Slot(SlotId(9)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&property).expect("divisor property"),
            },
            Expression::Literal(Literal::I64(1)),
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Divide),
                left: ExprId(2),
                right: ExprId(1),
            },
            Expression::Literal(Literal::I64(0)),
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Greater),
                left: ExprId(3),
                right: ExprId(4),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationships: SlotId(2),
                    edge_predicate: None,
                    completed_edge_predicate: Some(CompletedEdgePredicate {
                        current_edge: SlotId(9),
                        expression: ExprId(5),
                    }),
                    min: 1,
                    max: 1,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&path_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::declared(property.as_ptr() as usize, property.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&path_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::string(&property).unwrap(),
            ],
            &mut RecordingCompletion {
                called: Arc::clone(&self.completion_called),
            }
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for PathPatternConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let path_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let expressions = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationships: SlotId(2),
                    edge_predicate: None,
                    completed_edge_predicate: None,
                    min: self.min,
                    max: self.max,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&path_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&path_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut FreezePrimitiveValues,
            execution = ExecutionCapacity {
                batch_rows: 1,
                result_rows: 24,
                batch_payload_bytes: 8192,
                result_payload_bytes: 8192,
                batch: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
                result: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
            }
        ))
    }
}

fn collect_path_oracle(
    current: u128,
    edges: &[(u128, u128, u128)],
    used: &mut Vec<u128>,
    output: &mut Vec<(u128, Vec<u128>)>,
    max: usize,
) {
    if used.len() >= max {
        return;
    }
    for (source, target, relationship) in edges {
        if *source != current || used.contains(relationship) {
            continue;
        }
        used.push(*relationship);
        output.push((*target, used.clone()));
        collect_path_oracle(*target, edges, used, output, max);
        let _ = used.pop();
    }
}

struct FreezePathValues;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezePathValues {
    type Output = Vec<(u128, Vec<u128>)>;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(rows.rows())
            .map_err(|_| RuntimeError::Batch)?;
        for row in 0..rows.rows() {
            let endpoint = match rows.value(row, 1) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let list = match rows.value(row, 2) {
                Some(QueryValue::List(value)) => value,
                _ => return Err(RuntimeError::Batch.into()),
            };
            let mut path = Vec::new();
            path.try_reserve_exact(list.len())
                .map_err(|_| RuntimeError::Batch)?;
            for position in 0..list.len() {
                match list.get(position) {
                    Some(QueryValue::RelRef(value)) => path.push(value.id().get()),
                    _ => return Err(RuntimeError::Batch.into()),
                }
            }
            output.push((endpoint, path));
        }
        let core_bytes = output.iter().try_fold(0usize, |total, (_, path)| {
            total.checked_add(
                size_of::<(u128, Vec<u128>)>()
                    .checked_add(path.len().checked_mul(size_of::<u128>())?)?,
            )
        });
        FrozenOutput::new(
            output,
            rows.rows(),
            core_bytes.ok_or(RuntimeError::Batch)?,
            0,
        )
        .map_err(Into::into)
    }
}

struct CapPathConsumer {
    start: NodeId,
}

impl
    NativeReadConsumer<
        Result<Execution<Vec<(u128, Vec<u128>)>>, RuntimeFailure<NativeExecutionError>>,
    > for CapPathConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Vec<(u128, Vec<u128>)>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let path_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let expressions = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationships: SlotId(2),
                    edge_predicate: None,
                    completed_edge_predicate: None,
                    min: 1,
                    max: 16,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&path_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&path_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut FreezePathValues,
            execution = ExecutionCapacity {
                batch_rows: 1,
                result_rows: 24,
                batch_payload_bytes: 8192,
                result_payload_bytes: 8192,
                batch: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
                result: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
            }
        ))
    }
}

#[test]
fn native_pattern_bounded_paths_predicates() {
    let directory = tempfile::tempdir().expect("native path pattern store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native graph");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let mut no_labels_a: [GraphName<'_>; 0] = [];
        let mut no_labels_b: [GraphName<'_>; 0] = [];
        let mut no_properties_a: [GraphProperty<'_>; 0] = [];
        let mut no_properties_b: [GraphProperty<'_>; 0] = [];
        let first =
            CanonicalContents::node(&mut no_labels_a, &mut no_properties_a, None, None).unwrap();
        let second =
            CanonicalContents::node(&mut no_labels_b, &mut no_properties_b, None, None).unwrap();
        let ab1_properties = [
            GraphProperty::new(
                GraphName::new("pre").unwrap(),
                PropertyValue::new(PropertyData::Bool(true)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("expected_length").unwrap(),
                PropertyValue::new(PropertyData::I64(2)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("divisor").unwrap(),
                PropertyValue::new(PropertyData::I64(1)).unwrap(),
            ),
        ];
        let ab2_properties = [
            GraphProperty::new(
                GraphName::new("pre").unwrap(),
                PropertyValue::new(PropertyData::Bool(false)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("divisor").unwrap(),
                PropertyValue::new(PropertyData::I64(0)).unwrap(),
            ),
        ];
        let ba_properties = [
            GraphProperty::new(
                GraphName::new("expected_length").unwrap(),
                PropertyValue::new(PropertyData::I64(3)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("divisor").unwrap(),
                PropertyValue::new(PropertyData::I64(1)).unwrap(),
            ),
        ];
        let aa_properties = [
            GraphProperty::new(
                GraphName::new("pre").unwrap(),
                PropertyValue::new(PropertyData::Bool(true)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("expected_length").unwrap(),
                PropertyValue::new(PropertyData::I64(2)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("divisor").unwrap(),
                PropertyValue::new(PropertyData::I64(1)).unwrap(),
            ),
        ];
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "paths", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "paths", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "paths", "ab-1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("STEP").unwrap(),
                    properties: &ab1_properties,
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "paths", "ab-2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("STEP").unwrap(),
                    properties: &ab2_properties,
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "paths", "ba").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(1).unwrap()),
                    target: NodeRef::Local(refs.node(0).unwrap()),
                    relationship_type: GraphName::new("STEP").unwrap(),
                    properties: &ba_properties,
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "paths", "aa").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(0).unwrap()),
                    relationship_type: GraphName::new("STEP").unwrap(),
                    properties: &aa_properties,
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish path fixture")
    });
    let start = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("path start kind"),
    };
    let zero = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PathPatternConsumer {
                start,
                min: 0,
                max: 0,
            },
        )
        .expect("admit zero-hop path")
        .expect("execute zero-hop path");
    assert_eq!(zero.output.row_count, 1, "0..0 emits only the start row");
    assert_eq!(
        &zero.output.rows[0][..3],
        &[
            PrimitiveCell::Node(start.get()),
            PrimitiveCell::Node(start.get()),
            PrimitiveCell::Relationships([0; 16], 0),
        ]
    );
    let zero_with_errors = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ZeroHopPredicateConsumer { start },
        )
        .expect("admit zero-hop predicate guard")
        .expect("zero-hop must not evaluate edge predicates");
    assert_eq!(zero_with_errors.output, zero.output);
    assert_eq!(zero_with_errors.counters.get(WorkKind::Expressions), 0);
    let endpoint = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("path endpoint kind"),
    };
    let relationships = receipts[2..]
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Relationship(id) => id,
            _ => panic!("path relationship kind"),
        })
        .collect::<Vec<_>>();
    let path_bag = |rows: &PrimitiveRows| {
        let mut bag = rows
            .rows
            .iter()
            .take(rows.row_count)
            .map(|row| {
                let node = match row[1] {
                    PrimitiveCell::Node(id) => id,
                    value => panic!("path node output: {value:?}"),
                };
                let (ids, count) = match row[2] {
                    PrimitiveCell::Relationships(ids, count) => (ids, usize::from(count)),
                    value => panic!("path list output: {value:?}"),
                };
                (node, ids[..count].to_vec())
            })
            .collect::<Vec<_>>();
        bag.sort_unstable();
        bag
    };
    let paths = |min, max| {
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                PathPatternConsumer { start, min, max },
            )
            .expect("admit bounded path")
            .expect("execute bounded path")
            .output
    };
    let mut one_hop = vec![
        (endpoint.get(), vec![relationships[0].get()]),
        (endpoint.get(), vec![relationships[1].get()]),
        (start.get(), vec![relationships[3].get()]),
    ];
    one_hop.sort_unstable();
    assert_eq!(path_bag(&paths(1, 1)), one_hop);
    let mut through_two = vec![
        (start.get(), vec![]),
        (endpoint.get(), vec![relationships[0].get()]),
        (endpoint.get(), vec![relationships[1].get()]),
        (start.get(), vec![relationships[3].get()]),
        (
            start.get(),
            vec![relationships[0].get(), relationships[2].get()],
        ),
        (
            start.get(),
            vec![relationships[1].get(), relationships[2].get()],
        ),
        (
            endpoint.get(),
            vec![relationships[3].get(), relationships[0].get()],
        ),
        (
            endpoint.get(),
            vec![relationships[3].get(), relationships[1].get()],
        ),
    ];
    through_two.sort_unstable();
    assert_eq!(path_bag(&paths(0, 2)), through_two);
    let mut through_three = through_two[1..].to_vec();
    through_three.extend([
        (
            endpoint.get(),
            vec![
                relationships[0].get(),
                relationships[2].get(),
                relationships[1].get(),
            ],
        ),
        (
            start.get(),
            vec![
                relationships[0].get(),
                relationships[2].get(),
                relationships[3].get(),
            ],
        ),
        (
            endpoint.get(),
            vec![
                relationships[1].get(),
                relationships[2].get(),
                relationships[0].get(),
            ],
        ),
        (
            start.get(),
            vec![
                relationships[1].get(),
                relationships[2].get(),
                relationships[3].get(),
            ],
        ),
        (
            start.get(),
            vec![
                relationships[3].get(),
                relationships[0].get(),
                relationships[2].get(),
            ],
        ),
        (
            start.get(),
            vec![
                relationships[3].get(),
                relationships[1].get(),
                relationships[2].get(),
            ],
        ),
    ]);
    through_three.sort_unstable();
    assert_eq!(path_bag(&paths(1, 3)), through_three);
    assert_eq!(through_three.len(), 13);
    assert!(through_three.iter().all(|(_, path)| {
        path.iter()
            .enumerate()
            .all(|(position, id)| !path[..position].contains(id))
    }));
    let directed_edges = [
        (start.get(), endpoint.get(), relationships[0].get()),
        (start.get(), endpoint.get(), relationships[1].get()),
        (endpoint.get(), start.get(), relationships[2].get()),
        (start.get(), start.get(), relationships[3].get()),
    ];
    let mut cap_sixteen_oracle = Vec::new();
    collect_path_oracle(
        start.get(),
        &directed_edges,
        &mut Vec::new(),
        &mut cap_sixteen_oracle,
        16,
    );
    cap_sixteen_oracle.sort_unstable();
    let mut cap_sixteen = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            CapPathConsumer { start },
        )
        .expect("admit cap-sixteen path")
        .expect("execute cap-sixteen path")
        .output;
    cap_sixteen.sort_unstable();
    assert_eq!(cap_sixteen, cap_sixteen_oracle);
    let edge_filtered = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PredicatePathConsumer {
                start,
                completed: false,
            },
        )
        .expect("admit edge predicate path")
        .expect("execute edge predicate path");
    let mut expected_edge = vec![
        (start.get(), vec![]),
        (endpoint.get(), vec![relationships[0].get()]),
        (start.get(), vec![relationships[3].get()]),
        (
            endpoint.get(),
            vec![relationships[3].get(), relationships[0].get()],
        ),
    ];
    expected_edge.sort_unstable();
    assert_eq!(path_bag(&edge_filtered.output), expected_edge);
    let completed_filtered = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PredicatePathConsumer {
                start,
                completed: true,
            },
        )
        .expect("admit completed predicate path")
        .expect("execute completed predicate path");
    let mut expected_completed = vec![
        (start.get(), vec![]),
        (
            endpoint.get(),
            vec![relationships[3].get(), relationships[0].get()],
        ),
    ];
    expected_completed.sort_unstable();
    assert_eq!(path_bag(&completed_filtered.output), expected_completed);
    let completion_called = Arc::new(AtomicBool::new(false));
    let late_error = match store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            LateErrorPathConsumer {
                start,
                completion_called: Arc::clone(&completion_called),
            },
        )
        .expect("admit late completed predicate error")
    {
        Ok(_) => panic!("later completed predicate must fail"),
        Err(failure) => failure,
    };
    assert_eq!(late_error.operator, PlanNodeId(3));
    match late_error.error {
        NativeExecutionError::Expression(ExpressionError {
            expression: ExprId(5),
            failure: ExpressionFailure::Runtime(RuntimeError::Value(QueryError::DivisionByZero)),
        }) => {}
        error => panic!("unexpected completed predicate failure: {error:?}"),
    }
    assert!(!completion_called.load(Ordering::SeqCst));
    store.close().expect("close path pattern store");
}

struct JoinPatternConsumer;

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for JoinPatternConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let left_input = [PlanNodeId(0)];
        let right_input = [PlanNodeId(0)];
        let join_inputs = [PlanNodeId(1), PlanNodeId(2)];
        let collect_input = [PlanNodeId(3)];
        let expressions = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &left_input,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: None,
                },
            },
            Operator {
                inputs: &right_input,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: None,
                },
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join { predicate: None },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&left_input).unwrap(),
                RetainedRegion::slice(&right_input).unwrap(),
                RetainedRegion::slice(&join_inputs).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&left_input).unwrap(),
                RetainedAllocation::array(&right_input).unwrap(),
                RetainedAllocation::array(&join_inputs).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

struct SelectiveDuplicateJoinConsumer {
    start: NodeId,
}

struct SemanticListJoinConsumer {
    left: NodeId,
    right: NodeId,
    second_shared_key_mismatch: bool,
}

#[derive(Clone, Copy)]
enum ScalarJoinCase {
    NullCollision,
    Nan,
    ResidualFalse,
    ResidualNull,
    Disjoint,
}

struct ScalarJoinConsumer(ScalarJoinCase);

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for SemanticListJoinConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let property = String::from("number");
        let left_lookup_input = [PlanNodeId(0)];
        let left_project_input = [PlanNodeId(1)];
        let right_lookup_input = [PlanNodeId(0)];
        let right_project_input = [PlanNodeId(3)];
        let join_inputs = [PlanNodeId(2), PlanNodeId(4)];
        let output_input = [PlanNodeId(5)];
        let collect_input = [PlanNodeId(6)];
        let left_inner_items = [ExprId(2)];
        let left_outer_items = [ExprId(2), ExprId(3)];
        let right_inner_items = [ExprId(7)];
        let right_outer_items = [ExprId(7), ExprId(8)];
        let left_projections = [
            Projection {
                slot: SlotId(0),
                expression: ExprId(1),
            },
            Projection {
                slot: SlotId(1),
                expression: ExprId(4),
            },
        ];
        let right_projections = [
            Projection {
                slot: SlotId(0),
                expression: ExprId(6),
            },
            Projection {
                slot: SlotId(1),
                expression: ExprId(9),
            },
        ];
        let output_projection = [Projection {
            slot: SlotId(10),
            expression: ExprId(10),
        }];
        let expressions = [
            Expression::Slot(SlotId(20)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&property).expect("mixed numeric property"),
            },
            Expression::Literal(Literal::I64(7)),
            Expression::List(&left_inner_items),
            Expression::List(&left_outer_items),
            Expression::Slot(SlotId(21)),
            Expression::Property {
                entity: ExprId(5),
                name: GraphName::new(&property).expect("mixed numeric property"),
            },
            Expression::Literal(Literal::I64(if self.second_shared_key_mismatch {
                8
            } else {
                7
            })),
            Expression::List(&right_inner_items),
            Expression::List(&right_outer_items),
            Expression::Literal(Literal::Bool(true)),
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &left_lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(20),
                    id: self.left,
                },
            },
            Operator {
                inputs: &left_project_input,
                kind: OperatorKind::Project(&left_projections),
            },
            Operator {
                inputs: &right_lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(21),
                    id: self.right,
                },
            },
            Operator {
                inputs: &right_project_input,
                kind: OperatorKind::Project(&right_projections),
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join {
                    predicate: Some(ExprId(10)),
                },
            },
            Operator {
                inputs: &output_input,
                kind: OperatorKind::Project(&output_projection),
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&left_lookup_input).unwrap(),
                RetainedRegion::slice(&left_project_input).unwrap(),
                RetainedRegion::slice(&right_lookup_input).unwrap(),
                RetainedRegion::slice(&right_project_input).unwrap(),
                RetainedRegion::slice(&join_inputs).unwrap(),
                RetainedRegion::slice(&output_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&left_inner_items).unwrap(),
                RetainedRegion::slice(&left_outer_items).unwrap(),
                RetainedRegion::slice(&right_inner_items).unwrap(),
                RetainedRegion::slice(&right_outer_items).unwrap(),
                RetainedRegion::slice(&left_projections).unwrap(),
                RetainedRegion::slice(&right_projections).unwrap(),
                RetainedRegion::slice(&output_projection).unwrap(),
                RetainedRegion::declared(property.as_ptr() as usize, property.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&left_lookup_input).unwrap(),
                RetainedAllocation::array(&left_project_input).unwrap(),
                RetainedAllocation::array(&right_lookup_input).unwrap(),
                RetainedAllocation::array(&right_project_input).unwrap(),
                RetainedAllocation::array(&join_inputs).unwrap(),
                RetainedAllocation::array(&output_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&left_inner_items).unwrap(),
                RetainedAllocation::array(&left_outer_items).unwrap(),
                RetainedAllocation::array(&right_inner_items).unwrap(),
                RetainedAllocation::array(&right_outer_items).unwrap(),
                RetainedAllocation::array(&left_projections).unwrap(),
                RetainedAllocation::array(&right_projections).unwrap(),
                RetainedAllocation::array(&output_projection).unwrap(),
                RetainedAllocation::string(&property).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for ScalarJoinConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let left_input = [PlanNodeId(0)];
        let right_input = [PlanNodeId(0)];
        let join_inputs = [PlanNodeId(1), PlanNodeId(2)];
        let output_input = [PlanNodeId(3)];
        let collect_input = [PlanNodeId(4)];
        let (left, right, right_slot, residual) = match self.0 {
            ScalarJoinCase::NullCollision => {
                (Literal::Null, Literal::Null, SlotId(0), Literal::Bool(true))
            }
            ScalarJoinCase::Nan => (
                Literal::F64(f64::NAN),
                Literal::F64(f64::NAN),
                SlotId(0),
                Literal::Bool(true),
            ),
            ScalarJoinCase::ResidualFalse => (
                Literal::I64(1),
                Literal::I64(1),
                SlotId(0),
                Literal::Bool(false),
            ),
            ScalarJoinCase::ResidualNull => {
                (Literal::I64(1), Literal::I64(1), SlotId(0), Literal::Null)
            }
            ScalarJoinCase::Disjoint => (
                Literal::I64(1),
                Literal::I64(2),
                SlotId(1),
                Literal::Bool(true),
            ),
        };
        let left_projections = [Projection {
            slot: SlotId(0),
            expression: ExprId(0),
        }];
        let right_projections = [Projection {
            slot: right_slot,
            expression: ExprId(1),
        }];
        let output_projection = [Projection {
            slot: SlotId(10),
            expression: ExprId(2),
        }];
        let expressions = [
            Expression::Literal(left),
            Expression::Literal(right),
            Expression::Literal(residual),
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &left_input,
                kind: OperatorKind::Project(&left_projections),
            },
            Operator {
                inputs: &right_input,
                kind: OperatorKind::Project(&right_projections),
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join {
                    predicate: Some(ExprId(2)),
                },
            },
            Operator {
                inputs: &output_input,
                kind: OperatorKind::Project(&output_projection),
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&left_input).unwrap(),
                RetainedRegion::slice(&right_input).unwrap(),
                RetainedRegion::slice(&join_inputs).unwrap(),
                RetainedRegion::slice(&output_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&left_projections).unwrap(),
                RetainedRegion::slice(&right_projections).unwrap(),
                RetainedRegion::slice(&output_projection).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&left_input).unwrap(),
                RetainedAllocation::array(&right_input).unwrap(),
                RetainedAllocation::array(&join_inputs).unwrap(),
                RetainedAllocation::array(&output_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&left_projections).unwrap(),
                RetainedAllocation::array(&right_projections).unwrap(),
                RetainedAllocation::array(&output_projection).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for SelectiveDuplicateJoinConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let expand_input = [PlanNodeId(1)];
        let scan_input = [PlanNodeId(0)];
        let join_inputs = [PlanNodeId(3), PlanNodeId(2)];
        let collect_input = [PlanNodeId(4)];
        let expressions = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(10),
                    id: self.start,
                },
            },
            Operator {
                inputs: &expand_input,
                kind: OperatorKind::Expand {
                    source: SlotId(10),
                    node: SlotId(1),
                    relationship: SlotId(11),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &scan_input,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(1),
                    label: None,
                },
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join { predicate: None },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&expand_input).unwrap(),
                RetainedRegion::slice(&scan_input).unwrap(),
                RetainedRegion::slice(&join_inputs).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&expand_input).unwrap(),
                RetainedAllocation::array(&scan_input).unwrap(),
                RetainedAllocation::array(&join_inputs).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut FreezePrimitiveValues,
            pattern_rows = 2
        ))
    }
}

#[test]
fn native_pattern_hash_nested_and_selective_equivalence() {
    let planner_lookup_input = [PlanNodeId(0)];
    let planner_filter_input = [PlanNodeId(1)];
    let planner_scan_input = [PlanNodeId(0)];
    let planner_wrapped_source_input = [PlanNodeId(2)];
    let planner_operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &planner_lookup_input,
            kind: OperatorKind::LookupNode {
                output: SlotId(0),
                id: NodeId::new(1).expect("planner lookup id"),
            },
        },
        Operator {
            inputs: &planner_filter_input,
            kind: OperatorKind::Filter(ExprId(0)),
        },
        Operator {
            inputs: &planner_scan_input,
            kind: OperatorKind::ScanNodes {
                output: SlotId(0),
                label: None,
            },
        },
        Operator {
            inputs: &planner_wrapped_source_input,
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(2).expect("wrapped planner lookup id"),
            },
        },
    ];
    assert_eq!(
        planner::join_plan(1, &planner_operators, PlanNodeId(1), PlanNodeId(3),).build,
        planner::BuildSide::Left,
    );
    assert_eq!(
        planner::join_plan(1, &planner_operators, PlanNodeId(2), PlanNodeId(3),).build,
        planner::BuildSide::Right,
    );
    assert_eq!(
        planner::join_plan(1, &planner_operators, PlanNodeId(4), PlanNodeId(3),).build,
        planner::BuildSide::Right,
    );
    let directory = tempfile::tempdir().expect("native join pattern store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native graph");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let mut labels_a: [GraphName<'_>; 0] = [];
        let mut labels_b: [GraphName<'_>; 0] = [];
        let mut labels_c: [GraphName<'_>; 0] = [];
        let mut properties_a = [GraphProperty::new(
            GraphName::new("number").unwrap(),
            PropertyValue::new(PropertyData::I64(7)).unwrap(),
        )];
        let mut properties_b = [GraphProperty::new(
            GraphName::new("number").unwrap(),
            PropertyValue::new(PropertyData::F64(7.0)).unwrap(),
        )];
        let mut properties_c: [GraphProperty<'_>; 0] = [];
        let images = [
            CanonicalContents::node(&mut labels_a, &mut properties_a, None, None).unwrap(),
            CanonicalContents::node(&mut labels_b, &mut properties_b, None, None).unwrap(),
            CanonicalContents::node(&mut labels_c, &mut properties_c, None, None).unwrap(),
        ];
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "joins", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&images[0])),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "joins", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&images[1])),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "joins", "c").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&images[2])),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "joins", "ab-1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "joins", "ab-2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish join fixture")
    });
    let extra_receipts = crate::property_graph::with_local_refs(|_| {
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let requests = ["d", "e", "f"].map(|key| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "joins", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        });
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish streaming join probes")
    });
    let mut expected = receipts
        .iter()
        .chain(extra_receipts.iter())
        .filter_map(|receipt| match receipt.entity {
            EntityId::Node(id) => Some(id.get()),
            EntityId::Relationship(_) => None,
        })
        .collect::<Vec<_>>();
    expected.sort_unstable();
    for strategy in [planner::JoinStrategy::Hash, planner::JoinStrategy::Nested] {
        planner::force_join_strategy(Some(strategy));
        let execution = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                JoinPatternConsumer,
            )
            .expect("admit forced join")
            .expect("execute forced join");
        planner::force_join_strategy(None);
        let mut observed = execution.output.rows[..execution.output.row_count]
            .iter()
            .map(|row| match row[0] {
                PrimitiveCell::Node(id) => id,
                value => panic!("join node output: {value:?}"),
            })
            .collect::<Vec<_>>();
        observed.sort_unstable();
        assert_eq!(observed, expected);
        match strategy {
            planner::JoinStrategy::Hash => {
                assert_eq!(execution.counters.get(WorkKind::HashProbes), 6);
                assert_eq!(execution.counters.get(WorkKind::JoinProbes), 6);
            }
            planner::JoinStrategy::Nested => {
                assert_eq!(execution.counters.get(WorkKind::HashProbes), 0);
                assert_eq!(execution.counters.get(WorkKind::JoinProbes), 36);
            }
        }
    }
    let start = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("join start kind"),
    };
    let target_id = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("join target kind"),
    };
    let target = target_id.get();
    let relationship_ids = receipts
        .iter()
        .filter_map(|receipt| match receipt.entity {
            EntityId::Relationship(id) => Some(id.get()),
            EntityId::Node(_) => None,
        })
        .collect::<Vec<_>>();
    let mut expected_selective = relationship_ids
        .iter()
        .map(|relationship| (target, start.get(), *relationship))
        .collect::<Vec<_>>();
    expected_selective.sort_unstable();
    for strategy in [planner::JoinStrategy::Hash, planner::JoinStrategy::Nested] {
        planner::force_join_strategy(Some(strategy));
        let selective = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                SelectiveDuplicateJoinConsumer { start },
            )
            .expect("admit selective duplicate join")
            .expect("execute selective duplicate join");
        planner::force_join_strategy(None);
        let mut observed = selective.output.rows[..selective.output.row_count]
            .iter()
            .map(|row| match (row[0], row[1], row[2]) {
                (
                    PrimitiveCell::Node(target),
                    PrimitiveCell::Node(source),
                    PrimitiveCell::Relationship(relationship),
                ) => (target, source, relationship),
                values => panic!("selective join row: {values:?}"),
            })
            .collect::<Vec<_>>();
        observed.sort_unstable();
        assert_eq!(observed, expected_selective);
        assert!(selective.counters.get(WorkKind::Scans) >= 6);
        match strategy {
            planner::JoinStrategy::Hash => {
                assert!(selective.counters.get(WorkKind::HashProbes) >= 2);
                assert_eq!(
                    selective.counters.get(WorkKind::JoinProbes),
                    selective.counters.get(WorkKind::HashProbes)
                );
            }
            planner::JoinStrategy::Nested => {
                assert_eq!(selective.counters.get(WorkKind::HashProbes), 0);
                assert_eq!(selective.counters.get(WorkKind::JoinProbes), 12);
            }
        }
    }
    for strategy in [planner::JoinStrategy::Hash, planner::JoinStrategy::Nested] {
        for second_shared_key_mismatch in [false, true] {
            planner::force_join_strategy(Some(strategy));
            let semantic = store
                .with_native_read(
                    &QueryControl::Cancel(CancelToken::new()),
                    RuntimeLimits::default(),
                    16 * 1024 * 1024,
                    64,
                    SemanticListJoinConsumer {
                        left: start,
                        right: target_id,
                        second_shared_key_mismatch,
                    },
                )
                .expect("admit semantic list join")
                .expect("execute semantic list join");
            planner::force_join_strategy(None);
            assert_eq!(
                semantic.output.row_count,
                usize::from(!second_shared_key_mismatch)
            );
            if !second_shared_key_mismatch {
                assert_eq!(semantic.output.rows[0][0], PrimitiveCell::Bool(true));
            }
            let hash_probes = semantic.counters.get(WorkKind::HashProbes);
            let join_probes = semantic.counters.get(WorkKind::JoinProbes);
            assert_eq!(
                hash_probes,
                u64::from(strategy == planner::JoinStrategy::Hash && !second_shared_key_mismatch),
                "strategy={strategy:?} mismatch={second_shared_key_mismatch} rows={} hash={hash_probes} join={join_probes}",
                semantic.output.row_count,
            );
            assert_eq!(
                join_probes,
                u64::from(strategy == planner::JoinStrategy::Nested || !second_shared_key_mismatch),
                "strategy={strategy:?} mismatch={second_shared_key_mismatch} rows={} hash={hash_probes} join={join_probes}",
                semantic.output.row_count,
            );
        }
        for (case, expected_rows) in [
            (ScalarJoinCase::NullCollision, 0),
            (ScalarJoinCase::Nan, 0),
            (ScalarJoinCase::ResidualFalse, 0),
            (ScalarJoinCase::ResidualNull, 0),
            (ScalarJoinCase::Disjoint, 1),
        ] {
            planner::force_join_strategy(Some(strategy));
            let execution = store
                .with_native_read(
                    &QueryControl::Cancel(CancelToken::new()),
                    RuntimeLimits::default(),
                    16 * 1024 * 1024,
                    64,
                    ScalarJoinConsumer(case),
                )
                .expect("admit scalar join")
                .expect("execute scalar join");
            planner::force_join_strategy(None);
            assert_eq!(execution.output.row_count, expected_rows);
            if expected_rows == 1 {
                assert_eq!(execution.output.rows[0][0], PrimitiveCell::Bool(true));
            }
            assert_eq!(
                execution.counters.get(WorkKind::HashProbes),
                u64::from(strategy == planner::JoinStrategy::Hash)
            );
            assert_eq!(execution.counters.get(WorkKind::JoinProbes), 1);
        }
    }
    store.close().expect("close join pattern store");
}

struct OptionalAnchorConsumer {
    start: NodeId,
}

struct OptionalRebindingConsumer {
    start: NodeId,
}

#[derive(Clone, Copy)]
enum OptionalContract {
    CorrelatedNull,
    IndependentNull,
    RejectedNested,
}

struct OptionalContractConsumer {
    start: NodeId,
    contract: OptionalContract,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for OptionalContractConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        match self.contract {
            OptionalContract::CorrelatedNull => {
                let scan_input = [PlanNodeId(0)];
                let left_project_input = [PlanNodeId(1)];
                let identity_project_input = [PlanNodeId(2)];
                let optional_inputs = [PlanNodeId(2), PlanNodeId(3)];
                let collect_input = [PlanNodeId(4)];
                let left_projections = [
                    Projection {
                        slot: SlotId(10),
                        expression: ExprId(0),
                    },
                    Projection {
                        slot: SlotId(11),
                        expression: ExprId(1),
                    },
                ];
                let identity_projections = [
                    Projection {
                        slot: SlotId(10),
                        expression: ExprId(2),
                    },
                    Projection {
                        slot: SlotId(11),
                        expression: ExprId(3),
                    },
                ];
                let expressions = [
                    Expression::Literal(Literal::Null),
                    Expression::Slot(SlotId(0)),
                    Expression::Slot(SlotId(10)),
                    Expression::Slot(SlotId(11)),
                ];
                let operators = [
                    Operator {
                        inputs: &[],
                        kind: OperatorKind::Unit,
                    },
                    Operator {
                        inputs: &scan_input,
                        kind: OperatorKind::ScanNodes {
                            output: SlotId(0),
                            label: None,
                        },
                    },
                    Operator {
                        inputs: &left_project_input,
                        kind: OperatorKind::Project(&left_projections),
                    },
                    Operator {
                        inputs: &identity_project_input,
                        kind: OperatorKind::Project(&identity_projections),
                    },
                    Operator {
                        inputs: &optional_inputs,
                        kind: OperatorKind::OptionalApply { predicate: None },
                    },
                    Operator {
                        inputs: &collect_input,
                        kind: OperatorKind::Collect,
                    },
                ];
                Ok(execute_pattern!(
                    view,
                    runtime,
                    operators,
                    expressions,
                    vec![
                        RetainedRegion::slice(&scan_input).unwrap(),
                        RetainedRegion::slice(&left_project_input).unwrap(),
                        RetainedRegion::slice(&identity_project_input).unwrap(),
                        RetainedRegion::slice(&optional_inputs).unwrap(),
                        RetainedRegion::slice(&collect_input).unwrap(),
                        RetainedRegion::slice(&left_projections).unwrap(),
                        RetainedRegion::slice(&identity_projections).unwrap(),
                    ],
                    vec![
                        RetainedAllocation::array(&scan_input).unwrap(),
                        RetainedAllocation::array(&left_project_input).unwrap(),
                        RetainedAllocation::array(&identity_project_input).unwrap(),
                        RetainedAllocation::array(&optional_inputs).unwrap(),
                        RetainedAllocation::array(&collect_input).unwrap(),
                        RetainedAllocation::array(&left_projections).unwrap(),
                        RetainedAllocation::array(&identity_projections).unwrap(),
                    ],
                    &mut FreezePrimitiveValues
                ))
            }
            OptionalContract::IndependentNull => {
                let left_scan_input = [PlanNodeId(0)];
                let left_project_input = [PlanNodeId(1)];
                let right_scan_input = [PlanNodeId(0)];
                let right_project_input = [PlanNodeId(3)];
                let optional_inputs = [PlanNodeId(2), PlanNodeId(4)];
                let collect_input = [PlanNodeId(5)];
                let left_projections = [
                    Projection {
                        slot: SlotId(10),
                        expression: ExprId(0),
                    },
                    Projection {
                        slot: SlotId(11),
                        expression: ExprId(1),
                    },
                ];
                let right_projections = [
                    Projection {
                        slot: SlotId(10),
                        expression: ExprId(0),
                    },
                    Projection {
                        slot: SlotId(12),
                        expression: ExprId(2),
                    },
                ];
                let expressions = [
                    Expression::Literal(Literal::Null),
                    Expression::Slot(SlotId(0)),
                    Expression::Slot(SlotId(1)),
                ];
                let operators = [
                    Operator {
                        inputs: &[],
                        kind: OperatorKind::Unit,
                    },
                    Operator {
                        inputs: &left_scan_input,
                        kind: OperatorKind::ScanNodes {
                            output: SlotId(0),
                            label: None,
                        },
                    },
                    Operator {
                        inputs: &left_project_input,
                        kind: OperatorKind::Project(&left_projections),
                    },
                    Operator {
                        inputs: &right_scan_input,
                        kind: OperatorKind::ScanNodes {
                            output: SlotId(1),
                            label: None,
                        },
                    },
                    Operator {
                        inputs: &right_project_input,
                        kind: OperatorKind::Project(&right_projections),
                    },
                    Operator {
                        inputs: &optional_inputs,
                        kind: OperatorKind::OptionalApply { predicate: None },
                    },
                    Operator {
                        inputs: &collect_input,
                        kind: OperatorKind::Collect,
                    },
                ];
                Ok(execute_pattern!(
                    view,
                    runtime,
                    operators,
                    expressions,
                    vec![
                        RetainedRegion::slice(&left_scan_input).unwrap(),
                        RetainedRegion::slice(&left_project_input).unwrap(),
                        RetainedRegion::slice(&right_scan_input).unwrap(),
                        RetainedRegion::slice(&right_project_input).unwrap(),
                        RetainedRegion::slice(&optional_inputs).unwrap(),
                        RetainedRegion::slice(&collect_input).unwrap(),
                        RetainedRegion::slice(&left_projections).unwrap(),
                        RetainedRegion::slice(&right_projections).unwrap(),
                    ],
                    vec![
                        RetainedAllocation::array(&left_scan_input).unwrap(),
                        RetainedAllocation::array(&left_project_input).unwrap(),
                        RetainedAllocation::array(&right_scan_input).unwrap(),
                        RetainedAllocation::array(&right_project_input).unwrap(),
                        RetainedAllocation::array(&optional_inputs).unwrap(),
                        RetainedAllocation::array(&collect_input).unwrap(),
                        RetainedAllocation::array(&left_projections).unwrap(),
                        RetainedAllocation::array(&right_projections).unwrap(),
                    ],
                    &mut FreezePrimitiveValues
                ))
            }
            OptionalContract::RejectedNested => {
                let lookup_input = [PlanNodeId(0)];
                let expand_input = [PlanNodeId(1)];
                let inner_optional_inputs = [PlanNodeId(1), PlanNodeId(2)];
                let outer_optional_inputs = [PlanNodeId(3), PlanNodeId(3)];
                let collect_input = [PlanNodeId(4)];
                let expressions = [Expression::Literal(Literal::Bool(false))];
                let operators = [
                    Operator {
                        inputs: &[],
                        kind: OperatorKind::Unit,
                    },
                    Operator {
                        inputs: &lookup_input,
                        kind: OperatorKind::LookupNode {
                            output: SlotId(0),
                            id: self.start,
                        },
                    },
                    Operator {
                        inputs: &expand_input,
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
                        inputs: &inner_optional_inputs,
                        kind: OperatorKind::OptionalApply {
                            predicate: Some(ExprId(0)),
                        },
                    },
                    Operator {
                        inputs: &outer_optional_inputs,
                        kind: OperatorKind::OptionalApply { predicate: None },
                    },
                    Operator {
                        inputs: &collect_input,
                        kind: OperatorKind::Collect,
                    },
                ];
                Ok(execute_pattern!(
                    view,
                    runtime,
                    operators,
                    expressions,
                    vec![
                        RetainedRegion::slice(&lookup_input).unwrap(),
                        RetainedRegion::slice(&expand_input).unwrap(),
                        RetainedRegion::slice(&inner_optional_inputs).unwrap(),
                        RetainedRegion::slice(&outer_optional_inputs).unwrap(),
                        RetainedRegion::slice(&collect_input).unwrap(),
                    ],
                    vec![
                        RetainedAllocation::array(&lookup_input).unwrap(),
                        RetainedAllocation::array(&expand_input).unwrap(),
                        RetainedAllocation::array(&inner_optional_inputs).unwrap(),
                        RetainedAllocation::array(&outer_optional_inputs).unwrap(),
                        RetainedAllocation::array(&collect_input).unwrap(),
                    ],
                    &mut FreezePrimitiveValues
                ))
            }
        }
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for OptionalAnchorConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let expand_input = [PlanNodeId(1)];
        let project_input = [PlanNodeId(2)];
        let right_expand_input = [PlanNodeId(3)];
        let optional_inputs = [PlanNodeId(3), PlanNodeId(4)];
        let collect_input = [PlanNodeId(5)];
        let projections = [Projection {
            slot: SlotId(10),
            expression: ExprId(0),
        }];
        let expressions = [Expression::Slot(SlotId(0))];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &expand_input,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: crate::property_graph::query::plan::Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &project_input,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &right_expand_input,
                kind: OperatorKind::Expand {
                    source: SlotId(10),
                    node: SlotId(11),
                    relationship: SlotId(12),
                    direction: crate::property_graph::query::plan::Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(1),
                },
            },
            Operator {
                inputs: &optional_inputs,
                kind: OperatorKind::OptionalApply { predicate: None },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&expand_input).unwrap(),
                RetainedRegion::slice(&project_input).unwrap(),
                RetainedRegion::slice(&right_expand_input).unwrap(),
                RetainedRegion::slice(&optional_inputs).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&expand_input).unwrap(),
                RetainedAllocation::array(&project_input).unwrap(),
                RetainedAllocation::array(&right_expand_input).unwrap(),
                RetainedAllocation::array(&optional_inputs).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for OptionalRebindingConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let left_path_input = [PlanNodeId(1)];
        let right_path_input = [PlanNodeId(2)];
        let filter_input = [PlanNodeId(3)];
        let project_input = [PlanNodeId(4)];
        let optional_inputs = [PlanNodeId(2), PlanNodeId(5)];
        let collect_input = [PlanNodeId(6)];
        let expressions = [
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(2)),
            Expression::Slot(SlotId(20)),
            Expression::Slot(SlotId(21)),
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Equal),
                left: ExprId(0),
                right: ExprId(2),
            },
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Equal),
                left: ExprId(1),
                right: ExprId(3),
            },
            Expression::Binary {
                operation: BinaryExpression::And,
                left: ExprId(4),
                right: ExprId(5),
            },
        ];
        let projections = [
            Projection {
                slot: SlotId(1),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(2),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(30),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(31),
                expression: ExprId(3),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &left_path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationships: SlotId(2),
                    edge_predicate: None,
                    completed_edge_predicate: None,
                    min: 1,
                    max: 1,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &right_path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(20),
                    relationships: SlotId(21),
                    edge_predicate: None,
                    completed_edge_predicate: None,
                    min: 1,
                    max: 1,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(1),
                },
            },
            Operator {
                inputs: &filter_input,
                kind: OperatorKind::Filter(ExprId(6)),
            },
            Operator {
                inputs: &project_input,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &optional_inputs,
                kind: OperatorKind::OptionalApply { predicate: None },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&left_path_input).unwrap(),
                RetainedRegion::slice(&right_path_input).unwrap(),
                RetainedRegion::slice(&filter_input).unwrap(),
                RetainedRegion::slice(&project_input).unwrap(),
                RetainedRegion::slice(&optional_inputs).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&left_path_input).unwrap(),
                RetainedAllocation::array(&right_path_input).unwrap(),
                RetainedAllocation::array(&filter_input).unwrap(),
                RetainedAllocation::array(&project_input).unwrap(),
                RetainedAllocation::array(&optional_inputs).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

#[test]
fn native_pattern_optional_anchor_and_rebinding() {
    let directory = tempfile::tempdir().expect("native optional pattern store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native graph");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let second = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "optional", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "optional", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "optional", "ab-1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "optional", "ab-2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "optional", "ab-3").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish optional fixture")
    });
    let start = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("optional start kind"),
    };
    let target = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("optional target kind"),
    };
    let relationships = receipts[2..]
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Relationship(id) => id,
            _ => panic!("optional relationship kind"),
        })
        .collect::<Vec<_>>();
    let execution = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            OptionalAnchorConsumer { start },
        )
        .expect("admit optional pattern")
        .expect("execute optional pattern");
    assert_eq!(execution.output.row_count, 9);
    let mut observed = execution.output.rows[..execution.output.row_count]
        .iter()
        .map(|row| (row[0], row[1], row[2]))
        .collect::<Vec<_>>();
    observed.sort_by_key(|row| format!("{row:?}"));
    let mut expected = relationships
        .iter()
        .flat_map(|relationship| {
            std::iter::repeat_n(
                (
                    PrimitiveCell::Node(start.get()),
                    PrimitiveCell::Node(target.get()),
                    PrimitiveCell::Relationship(relationship.get()),
                ),
                3,
            )
        })
        .collect::<Vec<_>>();
    expected.sort_by_key(|row| format!("{row:?}"));
    assert_eq!(observed, expected);
    assert_eq!(
        execution.counters.get(WorkKind::JoinProbes),
        9,
        "each duplicate left row resets and drains every retained right match"
    );
    let rebound = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            OptionalRebindingConsumer { start },
        )
        .expect("admit optional fresh-candidate rebinding")
        .expect("execute optional fresh-candidate rebinding");
    assert_eq!(rebound.output.row_count, 3);
    let mut rebound_bag = rebound.output.rows[..rebound.output.row_count]
        .iter()
        .map(|row| (row[0], row[1], row[2], row[3], row[4]))
        .collect::<Vec<_>>();
    rebound_bag.sort_by_key(|row| format!("{row:?}"));
    let mut rebound_expected = relationships
        .iter()
        .map(|relationship| {
            let mut path = [0; 16];
            path[0] = relationship.get();
            (
                PrimitiveCell::Node(start.get()),
                PrimitiveCell::Node(target.get()),
                PrimitiveCell::Relationships(path, 1),
                PrimitiveCell::Node(target.get()),
                PrimitiveCell::Relationships(path, 1),
            )
        })
        .collect::<Vec<_>>();
    rebound_expected.sort_by_key(|row| format!("{row:?}"));
    assert_eq!(rebound_bag, rebound_expected);
    let correlated_null = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            OptionalContractConsumer {
                start,
                contract: OptionalContract::CorrelatedNull,
            },
        )
        .expect("admit correlated-null optional")
        .expect("execute correlated-null optional");
    assert_eq!(correlated_null.output.row_count, 2);
    assert!(
        correlated_null.output.rows[..2]
            .iter()
            .all(|row| row[0] == PrimitiveCell::Null && matches!(row[1], PrimitiveCell::Node(_)))
    );
    let independent_null = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            OptionalContractConsumer {
                start,
                contract: OptionalContract::IndependentNull,
            },
        )
        .expect("admit independent-null optional")
        .expect("execute independent-null optional");
    assert_eq!(independent_null.output.row_count, 2);
    assert!(independent_null.output.rows[..2].iter().all(|row| {
        row[0] == PrimitiveCell::Null
            && matches!(row[1], PrimitiveCell::Node(_))
            && row[2] == PrimitiveCell::Null
    }));
    let rejected_nested = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            OptionalContractConsumer {
                start,
                contract: OptionalContract::RejectedNested,
            },
        )
        .expect("admit rejected nested optional")
        .expect("execute rejected nested optional");
    assert_eq!(rejected_nested.output.row_count, 1);
    assert_eq!(
        &rejected_nested.output.rows[0][..3],
        &[
            PrimitiveCell::Node(start.get()),
            PrimitiveCell::Null,
            PrimitiveCell::Null,
        ]
    );
    store.close().expect("close optional pattern store");
}

struct UniquenessConsumer {
    start: NodeId,
    right_pattern: PatternId,
    shared_slots: bool,
}

struct CommonOriginAliasConsumer {
    start: NodeId,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for CommonOriginAliasConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let expand_input = [PlanNodeId(1)];
        let left_project_input = [PlanNodeId(2)];
        let right_project_input = [PlanNodeId(2)];
        let join_inputs = [PlanNodeId(3), PlanNodeId(4)];
        let collect_input = [PlanNodeId(5)];
        let expressions = [
            Expression::Slot(SlotId(0)),
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(2)),
        ];
        let left_projections = [
            Projection {
                slot: SlotId(10),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(11),
                expression: ExprId(1),
            },
            Projection {
                slot: SlotId(12),
                expression: ExprId(2),
            },
        ];
        let right_projections = [
            Projection {
                slot: SlotId(20),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(21),
                expression: ExprId(1),
            },
            Projection {
                slot: SlotId(22),
                expression: ExprId(2),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &expand_input,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: crate::property_graph::query::plan::Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &left_project_input,
                kind: OperatorKind::Project(&left_projections),
            },
            Operator {
                inputs: &right_project_input,
                kind: OperatorKind::Project(&right_projections),
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join { predicate: None },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&expand_input).unwrap(),
                RetainedRegion::slice(&left_project_input).unwrap(),
                RetainedRegion::slice(&right_project_input).unwrap(),
                RetainedRegion::slice(&join_inputs).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&left_projections).unwrap(),
                RetainedRegion::slice(&right_projections).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&expand_input).unwrap(),
                RetainedAllocation::array(&left_project_input).unwrap(),
                RetainedAllocation::array(&right_project_input).unwrap(),
                RetainedAllocation::array(&join_inputs).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&left_projections).unwrap(),
                RetainedAllocation::array(&right_projections).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for UniquenessConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let left_lookup_input = [PlanNodeId(0)];
        let left_expand_input = [PlanNodeId(1)];
        let project_input = [PlanNodeId(2)];
        let inner_collect_input = [PlanNodeId(3)];
        let right_lookup_input = [PlanNodeId(0)];
        let right_expand_input = [PlanNodeId(5)];
        let join_inputs = [PlanNodeId(4), PlanNodeId(6)];
        let collect_input = [PlanNodeId(7)];
        let projections = [
            Projection {
                slot: SlotId(10),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(11),
                expression: ExprId(1),
            },
        ];
        let expressions = [Expression::Slot(SlotId(0)), Expression::Slot(SlotId(1))];
        let (right_source, right_node, right_relationship) = if self.shared_slots {
            (SlotId(10), SlotId(11), SlotId(12))
        } else {
            (SlotId(20), SlotId(21), SlotId(22))
        };
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &left_lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &left_expand_input,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: crate::property_graph::query::plan::Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &project_input,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &inner_collect_input,
                kind: OperatorKind::Collect,
            },
            Operator {
                inputs: &right_lookup_input,
                kind: OperatorKind::LookupNode {
                    output: right_source,
                    id: self.start,
                },
            },
            Operator {
                inputs: &right_expand_input,
                kind: OperatorKind::Expand {
                    source: right_source,
                    node: right_node,
                    relationship: right_relationship,
                    direction: crate::property_graph::query::plan::Direction::Outgoing,
                    relationship_types: &[],
                    pattern: self.right_pattern,
                },
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join { predicate: None },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&left_lookup_input).unwrap(),
                RetainedRegion::slice(&left_expand_input).unwrap(),
                RetainedRegion::slice(&project_input).unwrap(),
                RetainedRegion::slice(&inner_collect_input).unwrap(),
                RetainedRegion::slice(&right_lookup_input).unwrap(),
                RetainedRegion::slice(&right_expand_input).unwrap(),
                RetainedRegion::slice(&join_inputs).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&left_lookup_input).unwrap(),
                RetainedAllocation::array(&left_expand_input).unwrap(),
                RetainedAllocation::array(&project_input).unwrap(),
                RetainedAllocation::array(&inner_collect_input).unwrap(),
                RetainedAllocation::array(&right_lookup_input).unwrap(),
                RetainedAllocation::array(&right_expand_input).unwrap(),
                RetainedAllocation::array(&join_inputs).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
            ],
            &mut FreezePrimitiveValues
        ))
    }
}

#[test]
fn native_pattern_pattern_uniqueness_across_joins() {
    let directory = tempfile::tempdir().expect("native uniqueness pattern store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native graph");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let second = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "unique", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "unique", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "unique", "ab").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish uniqueness fixture")
    });
    let start = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("uniqueness start kind"),
    };
    let execute = |right_pattern, shared_slots| {
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                UniquenessConsumer {
                    start,
                    right_pattern,
                    shared_slots,
                },
            )
            .expect("admit uniqueness pattern")
            .expect("execute uniqueness pattern")
            .output
    };
    assert_eq!(execute(PatternId(0), true).row_count, 0);
    assert_eq!(execute(PatternId(0), false).row_count, 0);
    let distinct = execute(PatternId(1), true);
    assert_eq!(distinct.row_count, 1);
    let target = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("uniqueness target kind"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("uniqueness relationship kind"),
    };
    assert_eq!(
        &distinct.rows[0][..3],
        &[
            PrimitiveCell::Node(start.get()),
            PrimitiveCell::Node(target.get()),
            PrimitiveCell::Relationship(relationship.get()),
        ]
    );
    let disconnected = execute(PatternId(1), false);
    assert_eq!(disconnected.row_count, 1);
    assert_eq!(
        &disconnected.rows[0][..5],
        &[
            PrimitiveCell::Node(start.get()),
            PrimitiveCell::Node(target.get()),
            PrimitiveCell::Node(start.get()),
            PrimitiveCell::Node(target.get()),
            PrimitiveCell::Relationship(relationship.get()),
        ]
    );
    let common_origin = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            CommonOriginAliasConsumer { start },
        )
        .expect("admit common-origin aliases")
        .expect("execute common-origin aliases")
        .output;
    assert_eq!(common_origin.row_count, 1);
    assert_eq!(
        &common_origin.rows[0][..6],
        &[
            PrimitiveCell::Node(start.get()),
            PrimitiveCell::Node(target.get()),
            PrimitiveCell::Relationship(relationship.get()),
            PrimitiveCell::Node(start.get()),
            PrimitiveCell::Node(target.get()),
            PrimitiveCell::Relationship(relationship.get()),
        ]
    );
    store.close().expect("close uniqueness pattern store");
}

struct PublicationScanConsumer {
    publish: Option<PublicationAction>,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for PublicationScanConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let label = String::from("Source");
        let property = String::from("name");
        let scan_input = [PlanNodeId(0)];
        let expand_input = [PlanNodeId(1)];
        let project_input = [PlanNodeId(2)];
        let collect_input = [PlanNodeId(3)];
        let list_items = [ExprId(1), ExprId(5)];
        let expressions = [
            Expression::Slot(SlotId(1)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&property).expect("publication property"),
            },
            Expression::Unary {
                operation: UnaryExpression::StoredText,
                operand: ExprId(0),
            },
            Expression::Unary {
                operation: UnaryExpression::Labels,
                operand: ExprId(0),
            },
            Expression::Slot(SlotId(2)),
            Expression::Unary {
                operation: UnaryExpression::RelType,
                operand: ExprId(4),
            },
            Expression::Slot(SlotId(0)),
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(2)),
            Expression::List(&list_items),
        ];
        let projections = [
            Projection {
                slot: SlotId(0),
                expression: ExprId(6),
            },
            Projection {
                slot: SlotId(1),
                expression: ExprId(7),
            },
            Projection {
                slot: SlotId(2),
                expression: ExprId(8),
            },
            Projection {
                slot: SlotId(3),
                expression: ExprId(1),
            },
            Projection {
                slot: SlotId(4),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(5),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(6),
                expression: ExprId(5),
            },
            Projection {
                slot: SlotId(7),
                expression: ExprId(9),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &scan_input,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: Some(GraphName::new(&label).expect("publication label")),
                },
            },
            Operator {
                inputs: &expand_input,
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
                inputs: &project_input,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_input).unwrap(),
                RetainedRegion::slice(&expand_input).unwrap(),
                RetainedRegion::slice(&project_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
                RetainedRegion::slice(&list_items).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
                RetainedRegion::declared(label.as_ptr() as usize, label.capacity()).unwrap(),
                RetainedRegion::declared(property.as_ptr() as usize, property.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_input).unwrap(),
                RetainedAllocation::array(&expand_input).unwrap(),
                RetainedAllocation::array(&project_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
                RetainedAllocation::array(&list_items).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
                RetainedAllocation::string(&label).unwrap(),
                RetainedAllocation::string(&property).unwrap(),
            ],
            &mut FreezePrimitiveValues,
            wrap = |source| PublishAfterFirst {
                source,
                action: self.publish.take(),
            }
        ))
    }
}

fn assert_publication_row(
    row: &[PrimitiveCell; 8],
    source: NodeId,
    target: NodeId,
    relationship: RelId,
    name: &str,
    text: &str,
    relationship_type: &str,
) {
    assert_eq!(row[0], PrimitiveCell::Node(source.get()));
    assert_eq!(row[1], PrimitiveCell::Node(target.get()));
    assert_eq!(row[2], PrimitiveCell::Relationship(relationship.get()));
    assert_eq!(row[3], string_cell(name));
    assert_eq!(row[4], string_cell(text));
    assert_eq!(row[5], strings_cell(&["Target"]));
    assert_eq!(row[6], string_cell(relationship_type));
    assert_eq!(row[7], strings_cell(&[name, relationship_type]));
}

#[test]
fn native_pattern_same_view_after_publication() {
    let directory = tempfile::tempdir().expect("native publication pattern store");
    let store = Arc::new(
        Store::create_native_graph(
            directory.path().join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
        )
        .expect("create native graph"),
    );
    let first = crate::property_graph::with_local_refs(|refs| {
        let mut source_labels = [GraphName::new("Source").unwrap()];
        let mut target_labels = [GraphName::new("Target").unwrap()];
        let mut source_properties: [GraphProperty<'_>; 0] = [];
        let mut target_properties = [GraphProperty::new(
            GraphName::new("name").unwrap(),
            PropertyValue::new(PropertyData::String("old-target")).unwrap(),
        )];
        let source = CanonicalContents::node(
            &mut source_labels,
            &mut source_properties,
            Some("source-text"),
            None,
        )
        .unwrap();
        let target = CanonicalContents::node(
            &mut target_labels,
            &mut target_properties,
            Some("old-text"),
            None,
        )
        .unwrap();
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "publication", "source")
                            .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&source)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "publication", "old").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&target)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Relationship,
                            "publication",
                            "old-link",
                        )
                        .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).unwrap()),
                            target: NodeRef::Local(refs.node(1).unwrap()),
                            relationship_type: GraphName::new("OLD_LINK").unwrap(),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("publish old fixture")
    });
    let source = match first[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("publication source node kind"),
    };
    let old_target = match first[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("publication old target kind"),
    };
    let old_relationship = match first[2].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("publication old relationship kind"),
    };
    let published = Arc::new(Mutex::new(None));
    let old = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PublicationScanConsumer {
                publish: Some(PublicationAction {
                    store: Arc::clone(&store),
                    source,
                    receipts: Arc::clone(&published),
                }),
            },
        )
        .expect("admit old publication view")
        .expect("execute old publication view");
    assert_eq!(old.output.row_count, 1);
    assert_publication_row(
        &old.output.rows[0],
        source,
        old_target,
        old_relationship,
        "old-target",
        "old-text",
        "OLD_LINK",
    );
    let old_output = old.output.clone();
    let (new_target, new_relationship) = published
        .lock()
        .expect("published receipt lock")
        .expect("published receipt values");
    let new = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PublicationScanConsumer { publish: None },
        )
        .expect("admit new publication view")
        .expect("execute new publication view");
    assert_eq!(new.output.row_count, 2);
    let old_row = new.output.rows[..2]
        .iter()
        .find(|row| row[2] == PrimitiveCell::Relationship(old_relationship.get()))
        .expect("new view old relationship row");
    assert_publication_row(
        old_row,
        source,
        old_target,
        old_relationship,
        "old-target",
        "old-text",
        "OLD_LINK",
    );
    let new_row = new.output.rows[..2]
        .iter()
        .find(|row| row[2] == PrimitiveCell::Relationship(new_relationship.get()))
        .expect("new view new relationship row");
    assert_publication_row(
        new_row,
        source,
        new_target,
        new_relationship,
        "new-target",
        "new-text",
        "NEW_LINK",
    );
    store.close().expect("close publication pattern store");
    assert_eq!(old.output, old_output);
}

fn path_failure_store(namespace: &'static str) -> (tempfile::TempDir, Arc<Store>, NodeId) {
    let directory = tempfile::tempdir().expect("native failure pattern store");
    let store = Arc::new(
        Store::create_native_graph(
            directory.path().join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024)
                .with_reader_drain_timeout(std::time::Duration::ZERO),
            None,
        )
        .expect("create native graph"),
    );
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let second = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let relationship_properties = [1_i64, 0].map(|divisor| {
            [GraphProperty::new(
                GraphName::new("divisor").unwrap(),
                PropertyValue::new(PropertyData::I64(divisor)).unwrap(),
            )]
        });
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, namespace, "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, namespace, "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, namespace, "ab-1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &relationship_properties[0],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, namespace, "ab-2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &relationship_properties[1],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish failure fixture")
    });
    let start = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("failure start kind"),
    };
    (directory, store, start)
}

struct LimitedPathConsumer {
    start: NodeId,
    completion_called: Arc<AtomicBool>,
}

struct ControlledPathConsumer<F> {
    start: NodeId,
    completion_called: Arc<AtomicBool>,
    action: Option<F>,
    result_rows: usize,
    result_payload_bytes: usize,
}

struct ReleaseChecked<C> {
    consumer: C,
    released: Arc<AtomicBool>,
}

struct AdmissionProbe;

impl NativeReadConsumer<()> for AdmissionProbe {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        _: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        _: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), crate::property_graph::storage::tree::directory::TreeError> {
        Ok(())
    }
}

impl<T, C: NativeReadConsumer<T>> NativeReadConsumer<T> for ReleaseChecked<C> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<T, crate::property_graph::storage::tree::directory::TreeError> {
        let baseline = runtime.memory().reserved_bytes() - view.retained_validation_bytes();
        let result = self.consumer.consume(view, runtime);
        // Only the exact charged immutable-page memo belongs to the statement
        // source after the operator drops. Every operator reservation releases.
        self.released.store(
            runtime.memory().reserved_bytes() - view.retained_validation_bytes() == baseline,
            Ordering::SeqCst,
        );
        result
    }
}

struct FilteredScanConsumer {
    completion_called: Arc<AtomicBool>,
}

struct PatternAllocationFailureConsumer {
    reached_constructor: Arc<AtomicBool>,
    completion_called: Arc<AtomicBool>,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for PatternAllocationFailureConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let collect_input = [PlanNodeId(0)];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len()).expect("allocation facts");
        for _ in 0..operators.len() {
            facts
                .push(NodeFacts::default())
                .expect("allocation fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
            RetainedRegion::slice(&collect_input).unwrap(),
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .map(|region| region.end() - region.start())
            .sum::<usize>();
        let mut external = memory
            .reserve_external_capacity()
            .expect("allocation external backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("allocation validation backing");
        let description = PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(1),
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                runtime.values(),
            )
            .expect("validate allocation plan");
        let owners = [
            RetainedAllocation::array(&operators).unwrap(),
            facts_owner,
            RetainedAllocation::array(&collect_input).unwrap(),
        ];
        let admitted =
            QueryInputs::reserve(memory, RetentionInventory::array(&owners), runtime.values())
                .expect("retain allocation plan")
                .admit_plan(&plan, runtime.values())
                .expect("admit allocation plan");
        self.reached_constructor.store(true, Ordering::SeqCst);
        let mut source = match NativePattern::new(
            view,
            &admitted,
            PlanNodeId(1),
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 1 << 20,
                    max_rows: 1 << 20,
                    payload_bytes: 1 << 20,
                    variable: ArenaCapacity {
                        string_bytes: 1 << 20,
                        list_cells: 1 << 20,
                        node_ids: 1 << 20,
                        relationship_ids: 1 << 20,
                    },
                },
                expression: ExpressionCapacity {
                    cells: 1 << 20,
                    string_bytes: 1 << 20,
                },
            },
            runtime,
        ) {
            Ok(source) => source,
            Err(error) => {
                return Ok(Err(RuntimeFailure {
                    operator: PlanNodeId(1),
                    error,
                    counters: Default::default(),
                }));
            }
        };
        Ok(execute_in(
            runtime,
            &admitted,
            &mut source,
            &mut RecordingCompletion {
                called: Arc::clone(&self.completion_called),
            },
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 1,
                batch_payload_bytes: 1,
                result_payload_bytes: 1,
                batch: ArenaCapacity::default(),
                result: ArenaCapacity::default(),
            },
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for FilteredScanConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let scan_input = [PlanNodeId(0)];
        let filter_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let expressions = [Expression::Literal(Literal::Bool(false))];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &scan_input,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: None,
                },
            },
            Operator {
                inputs: &filter_input,
                kind: OperatorKind::Filter(ExprId(0)),
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_input).unwrap(),
                RetainedRegion::slice(&filter_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_input).unwrap(),
                RetainedAllocation::array(&filter_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut RecordingCompletion {
                called: Arc::clone(&self.completion_called),
            }
        ))
    }
}

impl<F: FnOnce()>
    NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for ControlledPathConsumer<F>
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let path_input = [PlanNodeId(1)];
        let collect_input = [PlanNodeId(2)];
        let expressions = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationships: SlotId(2),
                    edge_predicate: None,
                    completed_edge_predicate: None,
                    min: 1,
                    max: 1,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        let action = self.action.take().expect("one controlled path action");
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&path_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&path_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut RecordingCompletion {
                called: Arc::clone(&self.completion_called),
            },
            wrap = move |source| ActAfterFirst {
                source,
                action: Some(action),
            },
            execution = ExecutionCapacity {
                batch_rows: 1,
                result_rows: self.result_rows,
                batch_payload_bytes: 8192,
                result_payload_bytes: self.result_payload_bytes,
                batch: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
                result: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 256,
                    node_ids: 64,
                    relationship_ids: 256,
                },
            }
        ))
    }
}

struct LateStorageConsumer {
    vfs: Arc<ScheduledMapVfs>,
    completion_called: Arc<AtomicBool>,
    arm_fault: bool,
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for LateStorageConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let scan_input = [PlanNodeId(0)];
        let collect_input = [PlanNodeId(1)];
        let expressions = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &scan_input,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: None,
                },
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut RecordingCompletion {
                called: Arc::clone(&self.completion_called),
            },
            wrap = |source| FailMapAfterFirst {
                source,
                vfs: Arc::clone(&self.vfs),
                armed: false,
                arm_fault: self.arm_fault,
            }
        ))
    }
}

impl NativeReadConsumer<Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>>
    for LimitedPathConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<Box<PrimitiveRows>>, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_input = [PlanNodeId(0)];
        let path_input = [PlanNodeId(1)];
        let filter_input = [PlanNodeId(2)];
        let collect_input = [PlanNodeId(3)];
        let expressions = [Expression::Literal(Literal::Bool(true))];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.start,
                },
            },
            Operator {
                inputs: &path_input,
                kind: OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationships: SlotId(2),
                    edge_predicate: None,
                    completed_edge_predicate: None,
                    min: 1,
                    max: 1,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &filter_input,
                kind: OperatorKind::Filter(ExprId(0)),
            },
            Operator {
                inputs: &collect_input,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_pattern!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_input).unwrap(),
                RetainedRegion::slice(&path_input).unwrap(),
                RetainedRegion::slice(&filter_input).unwrap(),
                RetainedRegion::slice(&collect_input).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_input).unwrap(),
                RetainedAllocation::array(&path_input).unwrap(),
                RetainedAllocation::array(&filter_input).unwrap(),
                RetainedAllocation::array(&collect_input).unwrap(),
            ],
            &mut RecordingCompletion {
                called: Arc::clone(&self.completion_called)
            }
        ))
    }
}

#[test]
fn native_pattern_limits_cancel_close_no_output() {
    let (_directory, store, start) = path_failure_store("limits");
    let clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PathPatternConsumer {
                start,
                min: 1,
                max: 1,
            },
        )
        .expect("admit clean limited path")
        .expect("execute clean limited path");
    assert_eq!(clean.output.row_count, 2);
    let extra_keys = (0..64)
        .map(|index| format!("filtered-{index:03}"))
        .collect::<Vec<_>>();
    crate::property_graph::with_local_refs(|_| {
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let requests = extra_keys
            .iter()
            .map(|key| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "limits", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            })
            .collect::<Vec<_>>();
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish long filtered fixture");
    });
    let memory_completion = Arc::new(AtomicBool::new(false));
    let memory_failure = store.with_native_read(
        &QueryControl::Cancel(CancelToken::new()),
        RuntimeLimits::default(),
        1,
        64,
        FilteredScanConsumer {
            completion_called: Arc::clone(&memory_completion),
        },
    );
    assert!(matches!(
        memory_failure,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Memory(MemoryError::Limit)
            )
        ))
    ));
    assert!(!memory_completion.load(Ordering::SeqCst));
    let allocation_reached = Arc::new(AtomicBool::new(false));
    let allocation_completion = Arc::new(AtomicBool::new(false));
    let allocation_released = Arc::new(AtomicBool::new(false));
    let allocation_failure = match store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            2 * 1024 * 1024,
            64,
            ReleaseChecked {
                consumer: PatternAllocationFailureConsumer {
                    reached_constructor: Arc::clone(&allocation_reached),
                    completion_called: Arc::clone(&allocation_completion),
                },
                released: Arc::clone(&allocation_released),
            },
        )
        .expect("admit recoverable pattern allocation failure")
    {
        Ok(_) => panic!("oversized native pattern capacity must be refused"),
        Err(failure) => failure,
    };
    assert_eq!(allocation_failure.operator, PlanNodeId(1));
    assert!(matches!(
        allocation_failure.error,
        NativeExecutionError::Runtime(RuntimeError::Memory(MemoryError::Limit))
    ));
    assert!(allocation_reached.load(Ordering::SeqCst));
    assert!(!allocation_completion.load(Ordering::SeqCst));
    assert!(allocation_released.load(Ordering::SeqCst));
    let filtered_completion = Arc::new(AtomicBool::new(false));
    let filtered_release = Arc::new(AtomicBool::new(false));
    let filtered = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            2 * 1024 * 1024,
            64,
            ReleaseChecked {
                consumer: FilteredScanConsumer {
                    completion_called: Arc::clone(&filtered_completion),
                },
                released: Arc::clone(&filtered_release),
            },
        )
        .expect("admit tightly bounded filtered scan")
        .expect("execute long filtered scan");
    assert_eq!(filtered.output.row_count, 0);
    assert!(filtered.counters.get(WorkKind::RowsIn) >= 64);
    assert!(filtered.counters.get(WorkKind::OperatorRows) >= 64);
    assert!(filtered_completion.load(Ordering::SeqCst));
    assert!(filtered_release.load(Ordering::SeqCst));
    for kind in [
        WorkKind::OperatorRows,
        WorkKind::AdjacencyEntries,
        WorkKind::Expressions,
        WorkKind::Paths,
        WorkKind::CopiedBytes,
    ] {
        let completion_called = Arc::new(AtomicBool::new(false));
        let limit = if kind == WorkKind::CopiedBytes {
            clean.counters.get(kind) / 2
        } else {
            1
        };
        let limits = RuntimeLimits::default()
            .with_limit(kind, limit)
            .expect("tighten actual path work");
        let released = Arc::new(AtomicBool::new(false));
        let failure = match store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                limits,
                16 * 1024 * 1024,
                64,
                ReleaseChecked {
                    consumer: LimitedPathConsumer {
                        start,
                        completion_called: Arc::clone(&completion_called),
                    },
                    released: Arc::clone(&released),
                },
            )
            .expect("admit limited path")
        {
            Ok(_) => panic!("actual {kind:?} work must exceed its tight limit"),
            Err(failure) => failure,
        };
        assert_eq!(failure.operator, PlanNodeId(4));
        assert!(
            matches!(&failure.error, NativeExecutionError::Runtime(RuntimeError::Limit(actual)) if *actual == kind)
                || matches!(
                    &failure.error,
                    NativeExecutionError::Expression(ExpressionError {
                        failure: ExpressionFailure::Runtime(RuntimeError::Limit(actual)),
                        ..
                    }) if *actual == kind
                )
                || matches!(
                    &failure.error,
                    NativeExecutionError::Tree(
                        crate::property_graph::storage::tree::directory::TreeError::Runtime(
                            RuntimeError::Limit(actual)
                        )
                    ) if *actual == kind
                ),
            "unexpected {kind:?} failure: {:?}",
            failure.error
        );
        if kind == WorkKind::CopiedBytes {
            assert!(failure.counters.get(kind) > 0);
            assert!(failure.counters.get(kind) <= limit);
        } else {
            assert_eq!(failure.counters.get(kind), 1, "counter for {kind:?}");
        }
        assert!(!completion_called.load(Ordering::SeqCst));
        assert!(released.load(Ordering::SeqCst));
    }

    let result_completion_called = Arc::new(AtomicBool::new(false));
    let result_released = Arc::new(AtomicBool::new(false));
    let result_failure = match store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseChecked {
                consumer: ControlledPathConsumer {
                    start,
                    completion_called: Arc::clone(&result_completion_called),
                    action: Some(|| {}),
                    result_rows: 1,
                    result_payload_bytes: 8192,
                },
                released: Arc::clone(&result_released),
            },
        )
        .expect("admit result-row bound")
    {
        Ok(_) => panic!("second result row must exceed retained capacity"),
        Err(failure) => failure,
    };
    assert!(matches!(
        result_failure.error,
        NativeExecutionError::Runtime(RuntimeError::BatchCapacity)
    ));
    assert!(!result_completion_called.load(Ordering::SeqCst));
    assert!(result_released.load(Ordering::SeqCst));

    let result_bytes_completion_called = Arc::new(AtomicBool::new(false));
    let result_bytes_released = Arc::new(AtomicBool::new(false));
    let result_bytes_failure = match store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseChecked {
                consumer: ControlledPathConsumer {
                    start,
                    completion_called: Arc::clone(&result_bytes_completion_called),
                    action: Some(|| {}),
                    result_rows: 16,
                    result_payload_bytes: 0,
                },
                released: Arc::clone(&result_bytes_released),
            },
        )
        .expect("admit result-byte bound")
    {
        Ok(_) => panic!("first relationship list must exceed zero result payload"),
        Err(failure) => failure,
    };
    assert!(matches!(
        result_bytes_failure.error,
        NativeExecutionError::Runtime(RuntimeError::BatchCapacity)
    ));
    assert!(!result_bytes_completion_called.load(Ordering::SeqCst));
    assert!(result_bytes_released.load(Ordering::SeqCst));

    let cancel_completion_called = Arc::new(AtomicBool::new(false));
    let cancel_released = Arc::new(AtomicBool::new(false));
    let cancel_token = CancelToken::new();
    let cancel_action = cancel_token.clone();
    let cancel_failure = store.with_native_read(
        &QueryControl::Cancel(cancel_token),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        ReleaseChecked {
            consumer: ControlledPathConsumer {
                start,
                completion_called: Arc::clone(&cancel_completion_called),
                action: Some(move || cancel_action.cancel()),
                result_rows: 16,
                result_payload_bytes: 8192,
            },
            released: Arc::clone(&cancel_released),
        },
    );
    assert!(matches!(
        cancel_failure,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(QueryError::Cancelled)
            )
        ))
    ));
    assert!(!cancel_completion_called.load(Ordering::SeqCst));
    assert!(cancel_released.load(Ordering::SeqCst));

    let timeout_completion_called = Arc::new(AtomicBool::new(false));
    let timeout_released = Arc::new(AtomicBool::new(false));
    let timeout_action_fired = Arc::new(AtomicBool::new(false));
    let timeout_action = Arc::clone(&timeout_action_fired);
    let timeout_control = QueryControl::Deadline(
        Deadline::after(std::time::Duration::from_millis(100)).expect("continued-work deadline"),
    );
    let timeout_failure = store.with_native_read(
        &timeout_control,
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        ReleaseChecked {
            consumer: ControlledPathConsumer {
                start,
                completion_called: Arc::clone(&timeout_completion_called),
                action: Some(move || {
                    timeout_action.store(true, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }),
                result_rows: 16,
                result_payload_bytes: 8192,
            },
            released: Arc::clone(&timeout_released),
        },
    );
    assert!(matches!(
        timeout_failure,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(QueryError::Timeout)
            )
        ))
    ));
    assert!(!timeout_completion_called.load(Ordering::SeqCst));
    assert!(timeout_action_fired.load(Ordering::SeqCst));
    assert!(timeout_released.load(Ordering::SeqCst));

    let join_limits = RuntimeLimits::default()
        .with_limit(WorkKind::JoinProbes, 1)
        .expect("tighten join probes");
    let join_released = Arc::new(AtomicBool::new(false));
    let join_failure = match store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            join_limits,
            16 * 1024 * 1024,
            64,
            ReleaseChecked {
                consumer: SelectiveDuplicateJoinConsumer { start },
                released: Arc::clone(&join_released),
            },
        )
        .expect("admit join-probe limit")
    {
        Ok(_) => panic!("second actual join probe must exceed limit one"),
        Err(failure) => failure,
    };
    assert!(matches!(
        join_failure.error,
        NativeExecutionError::Runtime(RuntimeError::Limit(WorkKind::JoinProbes))
    ));
    assert_eq!(join_failure.counters.get(WorkKind::JoinProbes), 1);
    assert!(join_released.load(Ordering::SeqCst));

    let close_completion_called = Arc::new(AtomicBool::new(false));
    let close_released = Arc::new(AtomicBool::new(false));
    let closing_observed = Arc::new(AtomicBool::new(false));
    let close_store = Arc::clone(&store);
    let close_probe_store = Arc::clone(&store);
    let close_observation = Arc::clone(&closing_observed);
    let (closed_tx, closed_rx) = std::sync::mpsc::sync_channel(1);
    let close_failure = store.with_native_read(
        &QueryControl::Cancel(CancelToken::new()),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        ReleaseChecked {
            consumer: ControlledPathConsumer {
                start,
                completion_called: Arc::clone(&close_completion_called),
                action: Some(move || {
                    std::thread::spawn(move || {
                        let _ = closed_tx.send(close_store.close());
                    });
                    for _ in 0..10_000 {
                        match close_probe_store.with_native_read(
                            &QueryControl::Cancel(CancelToken::new()),
                            RuntimeLimits::default(),
                            2 * 1024 * 1024,
                            1,
                            AdmissionProbe,
                        ) {
                            Err(crate::lifecycle::native_graph::NativeGraphError::Store(
                                crate::lifecycle::StoreError::Closing,
                            )) => {
                                close_observation.store(true, Ordering::SeqCst);
                                return;
                            }
                            Err(crate::lifecycle::native_graph::NativeGraphError::Read(
                                crate::property_graph::storage::tree::directory::TreeError::Runtime(
                                    RuntimeError::Value(QueryError::ReadCancelled),
                                ),
                            )) => std::thread::yield_now(),
                            Ok(()) => std::thread::yield_now(),
                            result => panic!("unexpected close admission result: {result:?}"),
                        }
                    }
                    panic!("store did not enter closing state");
                }),
                result_rows: 16,
                result_payload_bytes: 8192,
            },
            released: Arc::clone(&close_released),
        },
    );
    assert!(matches!(
        close_failure,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(QueryError::ReadCancelled)
            )
        ))
    ));
    assert!(!close_completion_called.load(Ordering::SeqCst));
    assert!(closing_observed.load(Ordering::SeqCst));
    assert!(close_released.load(Ordering::SeqCst));
    closed_rx
        .recv()
        .expect("close result")
        .expect("close limited pattern store");
}

#[test]
fn native_pattern_late_expression_and_storage_errors() {
    let (_directory, store, start) = path_failure_store("late-errors");
    let completion_called = Arc::new(AtomicBool::new(false));
    let expression_released = Arc::new(AtomicBool::new(false));
    let failure = match store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseChecked {
                consumer: LateErrorPathConsumer {
                    start,
                    completion_called: Arc::clone(&completion_called),
                },
                released: Arc::clone(&expression_released),
            },
        )
        .expect("admit late expression path")
    {
        Ok(_) => panic!("later path expression must fail"),
        Err(failure) => failure,
    };
    assert_eq!(failure.operator, PlanNodeId(3));
    assert!(matches!(
        failure.error,
        NativeExecutionError::Expression(ExpressionError {
            expression: ExprId(5),
            failure: ExpressionFailure::Runtime(RuntimeError::Value(QueryError::DivisionByZero)),
        })
    ));
    assert!(failure.counters.get(WorkKind::AdjacencyEntries) >= 2);
    assert!(!completion_called.load(Ordering::SeqCst));
    assert!(expression_released.load(Ordering::SeqCst));
    store.close().expect("close late-error pattern store");

    let storage_directory = tempfile::tempdir().expect("native storage-error pattern store");
    let vfs = Arc::new(ScheduledMapVfs::default());
    let mut entropy = OsEntropy;
    let storage_store = Store::create_native_graph_with_infrastructure(
        storage_directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
        vfs.clone(),
        Arc::new(SystemMonotonicClock),
        &mut entropy,
    )
    .expect("create storage-error graph");
    for key in ["a", "b", "c"] {
        crate::property_graph::with_local_refs(|_| {
            let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            storage_store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "storage-errors", key).unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node)),
                    }],
                    &QueryControl::Cancel(CancelToken::new()),
                )
                .expect("publish storage-error node");
        });
    }
    let storage_completion_called = Arc::new(AtomicBool::new(false));
    let storage_released = Arc::new(AtomicBool::new(false));
    let storage_failure = match storage_store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseChecked {
                consumer: LateStorageConsumer {
                    vfs: Arc::clone(&vfs),
                    completion_called: Arc::clone(&storage_completion_called),
                    arm_fault: true,
                },
                released: Arc::clone(&storage_released),
            },
        )
        .expect("admit late storage-error pattern")
    {
        Ok(_) => panic!("later required map read must fail"),
        Err(failure) => failure,
    };
    assert!(matches!(
        storage_failure.error,
        NativeExecutionError::Tree(
            crate::property_graph::storage::tree::directory::TreeError::Io(ref error)
        ) if error.kind() == std::io::ErrorKind::Other
    ));
    assert!(storage_failure.counters.get(WorkKind::RowsOut) >= 1);
    assert_eq!(vfs.fires.load(Ordering::SeqCst), 1);
    assert!(!storage_completion_called.load(Ordering::SeqCst));
    assert!(storage_released.load(Ordering::SeqCst));
    vfs.disarm();
    let clean_completion_called = Arc::new(AtomicBool::new(false));
    let clean_released = Arc::new(AtomicBool::new(false));
    let clean = storage_store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseChecked {
                consumer: LateStorageConsumer {
                    vfs: Arc::clone(&vfs),
                    completion_called: Arc::clone(&clean_completion_called),
                    arm_fault: false,
                },
                released: Arc::clone(&clean_released),
            },
        )
        .expect("admit clean storage-error control")
        .expect("execute clean storage-error control");
    assert_eq!(clean.output.row_count, 3);
    assert!(clean_completion_called.load(Ordering::SeqCst));
    assert!(clean_released.load(Ordering::SeqCst));
    storage_store
        .close()
        .expect("close storage-error pattern store");
}

#[test]
fn native_pattern_seeded_directed_probe_can_fire() {
    let report = super::test_support::run_actual_probe(0x5eed_cafe).expect("directed native probe");
    assert_eq!(report.observations, report.expected);
    let mut missing = report.observations.clone();
    let _ = missing.pop();
    assert_ne!(missing, report.expected);
    let mut duplicate = report.observations.clone();
    duplicate.push(report.observations[0]);
    duplicate.sort_unstable();
    assert_ne!(duplicate, report.expected);
    assert!(
        report.controls.iter().all(|(_, count)| *count > 0),
        "controls: {:?}",
        report.controls
    );
    assert!(report.faults.iter().all(|(_, count)| *count > 0));
    assert!(report.clean_controls.iter().all(|(_, count)| *count > 0));
}
