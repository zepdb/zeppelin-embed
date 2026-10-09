//! Tooling-only directed probe support for native pattern execution.

use super::{NativePattern, PatternCapacity};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store, SystemMonotonicClock};
use crate::property_graph::query::expression::ExpressionCapacity;
use crate::property_graph::query::plan::{
    BinaryExpression, Direction, EdgePredicate, ExprId, Expression, Literal, NodeFacts, Operator,
    OperatorKind, PatternId, PlanBacking, PlanDescription, PlanFootprint, PlanNodeId,
    RetainedRegion, SlotId, VALIDATION_SCRATCH_BYTES,
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
use crate::property_graph::query::{Comparison, QueryValue};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::allocation::OsEntropy;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphProperty,
    GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue, RelId,
};
use crate::vfs::{StdVfs, SyncKind, Vfs, VfsFile};
use std::fs::File;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

/// Owned primitive observations and controls returned to the adversarial adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeReport {
    /// Actual source, relationship, target tuples from the native operator.
    pub observations: Vec<(u128, u128, u128, u128)>,
    /// Independent tuples derived from committed write receipts.
    pub expected: Vec<(u128, u128, u128, u128)>,
    /// Actual nonzero native work counters.
    pub controls: Vec<(&'static str, u64)>,
    /// Actual injected control failures that fired.
    pub faults: Vec<(&'static str, u64)>,
    /// Same-seed clean executions paired with the injected controls.
    pub clean_controls: Vec<(&'static str, u64)>,
    /// Committed node identities in fixture order.
    pub fixture_nodes: Vec<u128>,
    /// Committed relationships as `(relationship, source, target)`.
    pub fixture_edges: Vec<(u128, u128, u128)>,
    /// One bag per legal source and build-side permutation of the same pattern.
    pub permutations: Vec<Vec<(u128, u128, u128, u128)>>,
    /// Bag observed when the right side is a later, independent pattern match.
    pub subsequent: Vec<(u128, u128, u128, u128)>,
}

/// Legal plan permutations of the one directed probe pattern.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PatternVariant {
    /// Path side first, one syntactic MATCH.
    #[default]
    Directed,
    /// Scalar side first; the same pattern with the other build side.
    JoinSwapped,
    /// The scalar side is a later, independent MATCH with a fresh
    /// uniqueness set.
    SubsequentMatch,
}

struct FreezeReceipts {
    source: usize,
    target: usize,
    path: usize,
    relationship: usize,
}

/// Locates the probe's four reported slots in the validated root schema, which
/// changes with the join input order.
fn probe_columns(
    facts: &NodeFacts,
) -> Result<FreezeReceipts, crate::property_graph::storage::tree::directory::TreeError> {
    let mut source = None;
    let mut target = None;
    let mut path = None;
    let mut relationship = None;
    for ordinal in 0..facts.width() {
        let (slot, _) = facts.slot_at(ordinal).ok_or(
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "pattern probe slot",
            ),
        )?;
        match slot.0 {
            0 => source = Some(ordinal),
            1 => target = Some(ordinal),
            2 => path = Some(ordinal),
            4 => relationship = Some(ordinal),
            _ => {}
        }
    }
    let missing = || {
        crate::property_graph::storage::tree::directory::TreeError::Invalid("pattern probe columns")
    };
    Ok(FreezeReceipts {
        source: source.ok_or_else(missing)?,
        target: target.ok_or_else(missing)?,
        path: path.ok_or_else(missing)?,
        relationship: relationship.ok_or_else(missing)?,
    })
}

struct FreezeNodeIds;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeNodeIds {
    type Output = Vec<u128>;

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
            match rows.value(row, 0) {
                Some(QueryValue::NodeRef(value)) => output.push(value.id().get()),
                _ => return Err(RuntimeError::Batch.into()),
            }
        }
        let bytes = output
            .len()
            .checked_mul(size_of::<u128>())
            .ok_or(RuntimeError::Batch)?;
        FrozenOutput::new(output, rows.rows(), bytes, 0).map_err(Into::into)
    }
}

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeReceipts {
    type Output = Vec<(u128, u128, u128, u128)>;

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
            let source = match rows.value(row, self.source) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let target = match rows.value(row, self.target) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let path_relationship = match rows.value(row, self.path) {
                Some(QueryValue::List(list)) if list.len() == 1 => match list.get(0) {
                    Some(QueryValue::RelRef(value)) => value.id().get(),
                    _ => return Err(RuntimeError::Batch.into()),
                },
                _ => return Err(RuntimeError::Batch.into()),
            };
            let relationship = match rows.value(row, self.relationship) {
                Some(QueryValue::RelRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            output.push((source, path_relationship, target, relationship));
        }
        let bytes = output
            .len()
            .checked_mul(size_of::<(u128, u128, u128, u128)>())
            .ok_or(RuntimeError::Batch)?;
        FrozenOutput::new(output, rows.rows(), bytes, 0).map_err(Into::into)
    }
}

struct ProbeConsumer {
    start: NodeId,
    variant: PatternVariant,
    fault_vfs: Option<Arc<ScheduledMapVfs>>,
    publication: Option<ProbePublication>,
}

struct ProbeExecution {
    execution: Execution<Vec<(u128, u128, u128, u128)>>,
    released: u64,
}

struct FailMapAfterFirst<O> {
    source: O,
    vfs: Option<Arc<ScheduledMapVfs>>,
    armed: bool,
    publication: Option<ProbePublication>,
}

struct LateMapConsumer {
    vfs: Option<Arc<ScheduledMapVfs>>,
}

struct ScanExecution {
    execution: Execution<Vec<u128>>,
    released: u64,
}

struct ProbePublication {
    store: Arc<Store>,
    source: NodeId,
    target: NodeId,
    result: Arc<Mutex<Option<Result<RelId, String>>>>,
}

impl ProbePublication {
    fn publish(self) -> Result<RelId, String> {
        let property = [GraphProperty::new(
            GraphName::new("weight").map_err(|error| error.to_string())?,
            PropertyValue::new(PropertyData::I64(3)).map_err(|error| error.to_string())?,
        )];
        let request = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "directed-pattern", "published")
                .map_err(|error| error.to_string())?,
            revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Existing(self.source),
                target: NodeRef::Existing(self.target),
                relationship_type: GraphName::new("LINKS").map_err(|error| error.to_string())?,
                properties: &property,
            }),
        }];
        let receipts = self
            .store
            .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
            .map_err(|error| error.to_string())?;
        match receipts.first().map(|receipt| receipt.entity) {
            Some(EntityId::Relationship(id)) => Ok(id),
            _ => Err(String::from("published relationship receipt kind")),
        }
    }
}

impl<'v, 'm, 'g, O>
    crate::property_graph::query::runtime::PullOperator<'v, 'm, 'g, NativeExecutionError>
    for FailMapAfterFirst<O>
where
    O: crate::property_graph::query::runtime::PullOperator<'v, 'm, 'g, NativeExecutionError>,
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
        output: &mut crate::property_graph::query::runtime::RowBatch<'v, 'm, 'g>,
    ) -> Result<crate::property_graph::query::runtime::PullState, NativeExecutionError> {
        let state = self.source.pull(context, output)?;
        if !self.armed
            && output.rows() != 0
            && let Some(vfs) = &self.vfs
        {
            vfs.arm_next();
            self.armed = true;
        }
        if output.rows() != 0
            && let Some(publication) = self.publication.take()
        {
            let result = Arc::clone(&publication.result);
            let published = std::thread::spawn(move || publication.publish())
                .join()
                .map_err(|_| RuntimeError::Batch)?;
            let mut slot = result.lock().map_err(|_| RuntimeError::Batch)?;
            *slot = Some(published);
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

impl NativeReadConsumer<Result<ProbeExecution, RuntimeFailure<NativeExecutionError>>>
    for ProbeConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<ProbeExecution, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let property = String::from("weight");
        let lookup_input = [PlanNodeId(0)];
        let path_input = [PlanNodeId(1)];
        let right_lookup_input = [PlanNodeId(0)];
        let right_expand_input = [PlanNodeId(3)];
        let join_inputs = match self.variant {
            PatternVariant::JoinSwapped => [PlanNodeId(4), PlanNodeId(2)],
            PatternVariant::Directed | PatternVariant::SubsequentMatch => {
                [PlanNodeId(2), PlanNodeId(4)]
            }
        };
        let right_pattern = match self.variant {
            PatternVariant::SubsequentMatch => PatternId(1),
            PatternVariant::Directed | PatternVariant::JoinSwapped => PatternId(0),
        };
        let optional_inputs = [PlanNodeId(5), PlanNodeId(5)];
        let collect_input = [PlanNodeId(6)];
        let expressions = [
            Expression::Slot(SlotId(9)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&property).map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "pattern probe property",
                    )
                })?,
            },
            Expression::Literal(Literal::I64(0)),
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Greater),
                left: ExprId(1),
                right: ExprId(2),
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
                    completed_edge_predicate: None,
                    min: 1,
                    max: 1,
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &right_lookup_input,
                kind: OperatorKind::LookupNode {
                    output: SlotId(10),
                    id: self.start,
                },
            },
            Operator {
                inputs: &right_expand_input,
                kind: OperatorKind::Expand {
                    source: SlotId(10),
                    node: SlotId(1),
                    relationship: SlotId(4),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: right_pattern,
                },
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join { predicate: None },
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
        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len())
            .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?;
        for _ in 0..operators.len() {
            facts
                .push(NodeFacts::default())
                .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?;
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe operators",
                )
            })?,
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "pattern probe facts",
                    )
                })?,
            RetainedRegion::slice(&expressions).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe expressions",
                )
            })?,
            RetainedRegion::slice(&lookup_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe lookup",
                )
            })?,
            RetainedRegion::slice(&path_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe path",
                )
            })?,
            RetainedRegion::slice(&right_lookup_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe right lookup",
                )
            })?,
            RetainedRegion::slice(&right_expand_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe right expand",
                )
            })?,
            RetainedRegion::slice(&join_inputs).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe join",
                )
            })?,
            RetainedRegion::slice(&optional_inputs).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe optional",
                )
            })?,
            RetainedRegion::slice(&collect_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe collect",
                )
            })?,
            RetainedRegion::declared(property.as_ptr() as usize, property.capacity()).map_err(
                |_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "pattern probe property backing",
                    )
                },
            )?,
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.end().saturating_sub(region.start()))
            })
            .ok_or(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe footprint",
                ),
            )?;
        let mut external = memory
            .reserve_external_capacity()
            .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?;
        external
            .reserve_additional(
                retained_bytes
                    .checked_add(VALIDATION_SCRATCH_BYTES)
                    .and_then(|bytes| {
                        bytes.checked_add(regions.capacity() * size_of::<RetainedRegion>())
                    })
                    .and_then(|bytes| bytes.checked_add(size_of::<PlanDescription<'_>>()))
                    .ok_or(
                        crate::property_graph::storage::tree::directory::TreeError::Invalid(
                            "pattern probe footprint",
                        ),
                    )?,
            )
            .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?;
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(7),
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "pattern probe backing",
                    )
                })?,
                runtime.values(),
            )
            .map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe plan",
                )
            })?;
        let owners = vec![
            RetainedAllocation::array(&operators).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe operators",
                )
            })?,
            facts_owner,
            RetainedAllocation::array(&expressions).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe expressions",
                )
            })?,
            RetainedAllocation::array(&lookup_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe lookup",
                )
            })?,
            RetainedAllocation::array(&path_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe path",
                )
            })?,
            RetainedAllocation::array(&right_lookup_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe right lookup",
                )
            })?,
            RetainedAllocation::array(&right_expand_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe right expand",
                )
            })?,
            RetainedAllocation::array(&join_inputs).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe join",
                )
            })?,
            RetainedAllocation::array(&optional_inputs).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe optional",
                )
            })?,
            RetainedAllocation::array(&collect_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe collect",
                )
            })?,
            RetainedAllocation::string(&property).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe property backing",
                )
            })?,
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "pattern probe owners",
                )
            })?,
            runtime.values(),
        )
        .map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "pattern probe reserve",
            )
        })?
        .admit_plan(&plan, runtime.values())
        .map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "pattern probe admit",
            )
        })?;
        let mut receipts = probe_columns(plan.facts(PlanNodeId(7)).ok_or(
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "pattern probe root facts",
            ),
        )?)?;
        let operator_baseline = runtime.memory().reserved_bytes();
        let source = NativePattern::new(
            view,
            &admitted,
            PlanNodeId(7),
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 16,
                    max_rows: 16,
                    payload_bytes: 4096,
                    variable: ArenaCapacity {
                        string_bytes: 256,
                        list_cells: 32,
                        node_ids: 16,
                        relationship_ids: 32,
                    },
                },
                expression: ExpressionCapacity {
                    cells: 16,
                    string_bytes: 256,
                },
            },
            runtime,
        )
        .map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "pattern probe construct",
            )
        })?;
        let mut source = FailMapAfterFirst {
            source,
            vfs: self.fault_vfs.clone(),
            armed: false,
            publication: self.publication.take(),
        };
        let execution = execute_in(
            runtime,
            &admitted,
            &mut source,
            &mut receipts,
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 8,
                batch_payload_bytes: 4096,
                result_payload_bytes: 4096,
                batch: ArenaCapacity {
                    string_bytes: 256,
                    list_cells: 32,
                    node_ids: 16,
                    relationship_ids: 32,
                },
                result: ArenaCapacity {
                    string_bytes: 256,
                    list_cells: 32,
                    node_ids: 16,
                    relationship_ids: 32,
                },
            },
        );
        drop(source);
        let released = u64::from(runtime.memory().reserved_bytes() == operator_baseline);
        Ok(execution.map(|execution| ProbeExecution {
            execution,
            released,
        }))
    }
}

impl NativeReadConsumer<Result<ScanExecution, RuntimeFailure<NativeExecutionError>>>
    for LateMapConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<ScanExecution, RuntimeFailure<NativeExecutionError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let scan_input = [PlanNodeId(0)];
        let collect_input = [PlanNodeId(1)];
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
        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len())
            .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?;
        for _ in 0..operators.len() {
            facts
                .push(NodeFacts::default())
                .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?;
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "late map operators",
                )
            })?,
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "late map facts",
                    )
                })?,
            RetainedRegion::slice(&scan_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid("late map scan")
            })?,
            RetainedRegion::slice(&collect_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "late map collect",
                )
            })?,
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.end().saturating_sub(region.start()))
            })
            .ok_or(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "late map footprint",
                ),
            )?;
        let mut external = memory
            .reserve_external_capacity()
            .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?;
        external
            .reserve_additional(
                retained_bytes
                    .checked_add(VALIDATION_SCRATCH_BYTES)
                    .and_then(|bytes| {
                        bytes.checked_add(regions.capacity() * size_of::<RetainedRegion>())
                    })
                    .and_then(|bytes| bytes.checked_add(size_of::<PlanDescription<'_>>()))
                    .ok_or(
                        crate::property_graph::storage::tree::directory::TreeError::Invalid(
                            "late map footprint",
                        ),
                    )?,
            )
            .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?;
        let description = PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(2),
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "late map backing",
                    )
                })?,
                runtime.values(),
            )
            .map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid("late map plan")
            })?;
        let owners = vec![
            RetainedAllocation::array(&operators).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "late map operators",
                )
            })?,
            facts_owner,
            RetainedAllocation::array(&scan_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid("late map scan")
            })?,
            RetainedAllocation::array(&collect_input).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "late map collect",
                )
            })?,
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "late map owners",
                )
            })?,
            runtime.values(),
        )
        .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?
        .admit_plan(&plan, runtime.values())
        .map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid("late map admit")
        })?;
        let operator_baseline = runtime.memory().reserved_bytes();
        let source = NativePattern::new(
            view,
            &admitted,
            PlanNodeId(2),
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 8,
                    max_rows: 8,
                    payload_bytes: 1024,
                    variable: ArenaCapacity {
                        string_bytes: 0,
                        list_cells: 0,
                        node_ids: 8,
                        relationship_ids: 0,
                    },
                },
                expression: ExpressionCapacity {
                    cells: 1,
                    string_bytes: 0,
                },
            },
            runtime,
        )
        .map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "late map construct",
            )
        })?;
        let mut source = FailMapAfterFirst {
            source,
            vfs: self.vfs.clone(),
            armed: false,
            publication: None,
        };
        let execution = execute_in(
            runtime,
            &admitted,
            &mut source,
            &mut FreezeNodeIds,
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 8,
                batch_payload_bytes: 1024,
                result_payload_bytes: 1024,
                batch: ArenaCapacity {
                    string_bytes: 0,
                    list_cells: 0,
                    node_ids: 8,
                    relationship_ids: 0,
                },
                result: ArenaCapacity {
                    string_bytes: 0,
                    list_cells: 0,
                    node_ids: 8,
                    relationship_ids: 0,
                },
            },
        );
        drop(source);
        let released = u64::from(runtime.memory().reserved_bytes() == operator_baseline);
        Ok(execution.map(|execution| ScanExecution {
            execution,
            released,
        }))
    }
}

fn probe_directory(seed: u64) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "zeppelin-native-pattern-{}-{seed}",
        std::process::id()
    ));
    path
}

/// Runs the real directed pattern operation sequence for one deterministic seed.
pub fn run_actual_probe(seed: u64) -> Result<ProbeReport, String> {
    let directory = probe_directory(seed);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let vfs = Arc::new(ScheduledMapVfs::default());
    let mut entropy = OsEntropy;
    let store = Arc::new(
        Store::create_native_graph_with_infrastructure(
            directory.join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
            vfs.clone(),
            Arc::new(SystemMonotonicClock),
            &mut entropy,
        )
        .map_err(|error| error.to_string())?,
    );
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], None, None)
            .map_err(|error| error.to_string())?;
        let second = CanonicalContents::node(&mut [], &mut [], None, None)
            .map_err(|error| error.to_string())?;
        let weights = if seed & 1 == 0 { [1, 2] } else { [2, 1] };
        let relationship_keys = if seed & 2 == 0 {
            ["ab-1", "ab-2"]
        } else {
            ["ab-2", "ab-1"]
        };
        let relationship_properties = [
            [GraphProperty::new(
                GraphName::new("weight").map_err(|error| error.to_string())?,
                PropertyValue::new(PropertyData::I64(weights[0]))
                    .map_err(|error| error.to_string())?,
            )],
            [GraphProperty::new(
                GraphName::new("weight").map_err(|error| error.to_string())?,
                PropertyValue::new(PropertyData::I64(weights[1]))
                    .map_err(|error| error.to_string())?,
            )],
        ];
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "directed-pattern", "a")
                    .map_err(|error| error.to_string())?,
                revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "directed-pattern", "b")
                    .map_err(|error| error.to_string())?,
                revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(
                    EntityKind::Relationship,
                    "directed-pattern",
                    relationship_keys[0],
                )
                .map_err(|error| error.to_string())?,
                revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).map_err(|error| error.to_string())?),
                    target: NodeRef::Local(refs.node(1).map_err(|error| error.to_string())?),
                    relationship_type: GraphName::new("LINKS")
                        .map_err(|error| error.to_string())?,
                    properties: &relationship_properties[0],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(
                    EntityKind::Relationship,
                    "directed-pattern",
                    relationship_keys[1],
                )
                .map_err(|error| error.to_string())?,
                revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).map_err(|error| error.to_string())?),
                    target: NodeRef::Local(refs.node(1).map_err(|error| error.to_string())?),
                    relationship_type: GraphName::new("LINKS")
                        .map_err(|error| error.to_string())?,
                    properties: &relationship_properties[1],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .map_err(|error| error.to_string())
    })?;
    let source_id = match receipts.first().map(|receipt| receipt.entity) {
        Some(EntityId::Node(id)) => id,
        _ => return Err(String::from("missing source receipt")),
    };
    let source = source_id.get();
    let target_id = match receipts.get(1).map(|receipt| receipt.entity) {
        Some(EntityId::Node(id)) => id,
        _ => return Err(String::from("missing target receipt")),
    };
    let target = target_id.get();
    let mut relationships = Vec::new();
    for receipt in receipts.iter().skip(2) {
        match receipt.entity {
            EntityId::Relationship(id) => relationships.push(id.get()),
            EntityId::Node(_) => return Err(String::from("relationship receipt kind")),
        }
    }
    let mut expected = Vec::new();
    for path in &relationships {
        for relationship in &relationships {
            if path != relationship {
                expected.push((source, *path, target, *relationship));
            }
        }
    }
    expected.sort_unstable();
    let clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ProbeConsumer {
                start: source_id,
                variant: PatternVariant::Directed,
                fault_vfs: None,
                publication: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("directed pattern execution: {error:?}"))?;
    let mut observations = clean.execution.output;
    observations.sort_unstable();
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::CompletedRows, 1)
        .map_err(|error| format!("directed pattern limit: {error:?}"))?;
    let limited = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            limits,
            16 * 1024 * 1024,
            64,
            ProbeConsumer {
                start: source_id,
                variant: PatternVariant::Directed,
                fault_vfs: None,
                publication: None,
            },
        )
        .map_err(|error| error.to_string())?;
    let limit_fired = u64::from(matches!(
        &limited,
        Err(failure)
            if matches!(
                failure.error,
                NativeExecutionError::Runtime(RuntimeError::Limit(WorkKind::CompletedRows))
            ) && failure.counters.get(WorkKind::CompletedRows) == 1
    ));
    let mut late_node_ids = Vec::new();
    for key in ["late-1", "late-2"] {
        let receipt = crate::property_graph::with_local_refs(|_| {
            let image = CanonicalContents::node(&mut [], &mut [], None, None)
                .map_err(|error| error.to_string())?;
            let request = [StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "directed-pattern", key)
                    .map_err(|error| error.to_string())?,
                revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }];
            store
                .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
                .map_err(|error| error.to_string())
        })?;
        match receipt.first().map(|entry| entry.entity) {
            Some(EntityId::Node(id)) => late_node_ids.push(id.get()),
            _ => return Err(String::from("late map node receipt kind")),
        }
    }
    let mut expected_scan = vec![source, target];
    expected_scan.extend_from_slice(&late_node_ids);
    expected_scan.sort_unstable();
    let late_storage = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            LateMapConsumer {
                vfs: Some(vfs.clone()),
            },
        )
        .map_err(|error| error.to_string())?;
    let late_error_fired = u64::from(matches!(
        &late_storage,
        Err(failure)
            if matches!(
                failure.error,
                NativeExecutionError::Tree(
                    crate::property_graph::storage::tree::directory::TreeError::Io(ref error)
                ) if error.kind() == std::io::ErrorKind::Other
            ) && failure.counters.get(WorkKind::RowsOut) > 0
                && vfs.fires.load(Ordering::SeqCst) == 1
    ));
    vfs.disarm();
    let late_clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            LateMapConsumer { vfs: None },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("clean late-map execution: {error:?}"))?;
    let mut late_clean_nodes = late_clean.execution.output.clone();
    late_clean_nodes.sort_unstable();
    let late_clean_same_history = late_clean_nodes == expected_scan;
    let paired = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ProbeConsumer {
                start: source_id,
                variant: PatternVariant::Directed,
                fault_vfs: None,
                publication: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("paired directed pattern execution: {error:?}"))?;
    let mut paired_observations = paired.execution.output.clone();
    paired_observations.sort_unstable();
    let swapped = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ProbeConsumer {
                start: source_id,
                variant: PatternVariant::JoinSwapped,
                fault_vfs: None,
                publication: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("join-swapped pattern execution: {error:?}"))?;
    let mut swapped_observations = swapped.execution.output.clone();
    swapped_observations.sort_unstable();
    let subsequent_execution = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ProbeConsumer {
                start: source_id,
                variant: PatternVariant::SubsequentMatch,
                fault_vfs: None,
                publication: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("subsequent-match pattern execution: {error:?}"))?;
    let mut subsequent = subsequent_execution.execution.output.clone();
    subsequent.sort_unstable();
    let permutations = vec![observations.clone(), swapped_observations];
    let permutation_control = u64::try_from(
        permutations
            .iter()
            .filter(|bag| **bag == observations)
            .count(),
    )
    .map_err(|_| String::from("permutation control"))?;
    let subsequent_control = u64::try_from(
        subsequent
            .iter()
            .filter(|(_, path, _, relationship)| path == relationship)
            .count(),
    )
    .map_err(|_| String::from("subsequent-match control"))?;
    let fixture_nodes = vec![source, target];
    let fixture_edges = relationships
        .iter()
        .map(|relationship| (*relationship, source, target))
        .collect::<Vec<(u128, u128, u128)>>();
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let cancelled_result = store.with_native_read(
        &QueryControl::Cancel(cancelled),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        ProbeConsumer {
            start: source_id,
            variant: PatternVariant::Directed,
            fault_vfs: None,
            publication: None,
        },
    );
    let cancel_fired = u64::from(matches!(
        cancelled_result,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(crate::property_graph::query::QueryError::Cancelled)
            )
        ))
    ));
    let upper_bit = 1_u128 << 96;
    let alias_value = if source ^ upper_bit == 0 {
        source ^ (1_u128 << 97)
    } else {
        source ^ upper_bit
    };
    let alias = NodeId::new(alias_value).map_err(|error| error.to_string())?;
    let alias_execution = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ProbeConsumer {
                start: alias,
                variant: PatternVariant::Directed,
                fault_vfs: None,
                publication: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("full-id alias execution: {error:?}"))?;
    let publication = Arc::new(Mutex::new(None));
    let retained = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ProbeConsumer {
                start: source_id,
                variant: PatternVariant::Directed,
                fault_vfs: None,
                publication: Some(ProbePublication {
                    store: Arc::clone(&store),
                    source: source_id,
                    target: target_id,
                    result: Arc::clone(&publication),
                }),
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("retained-view execution: {error:?}"))?;
    let published_relationship = {
        let mut result = publication
            .lock()
            .map_err(|_| String::from("retained-view publication lock"))?;
        result
            .take()
            .ok_or_else(|| String::from("retained-view publication did not run"))??
    };
    let fresh = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ProbeConsumer {
                start: source_id,
                variant: PatternVariant::Directed,
                fault_vfs: None,
                publication: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("fresh-view execution: {error:?}"))?;
    store.close().map_err(|error| error.to_string())?;
    std::fs::remove_dir_all(&directory).map_err(|error| error.to_string())?;
    let oracle_count = u64::try_from(expected.len()).map_err(|_| String::from("oracle count"))?;
    let same_seed = u64::from(
        observations == expected
            && paired_observations == expected
            && paired_observations == observations
            && late_clean_same_history,
    );
    let uniqueness = u64::from(
        observations
            .iter()
            .all(|(_, path, _, relationship)| path != relationship),
    );
    let full_id = u64::from(
        alias != source_id
            && alias_execution.execution.output.is_empty()
            && observations
                .iter()
                .all(|(observed_source, _, observed_target, _)| {
                    *observed_source == source && *observed_target == target
                }),
    );
    let mut retained_observations = retained.execution.output;
    retained_observations.sort_unstable();
    let mut fresh_observations = fresh.execution.output;
    fresh_observations.sort_unstable();
    let mut fresh_relationships = relationships;
    fresh_relationships.push(published_relationship.get());
    let mut fresh_expected = Vec::new();
    for path in &fresh_relationships {
        for relationship in &fresh_relationships {
            if path != relationship {
                fresh_expected.push((source, *path, target, *relationship));
            }
        }
    }
    fresh_expected.sort_unstable();
    let retained_view = u64::from(
        retained_observations == expected
            && fresh_observations == fresh_expected
            && fresh_observations.iter().any(|(_, path, _, relationship)| {
                *path == published_relationship.get()
                    || *relationship == published_relationship.get()
            }),
    );
    Ok(ProbeReport {
        observations,
        expected,
        controls: vec![
            (
                "native-source",
                clean.execution.counters.get(WorkKind::RowsOut),
            ),
            (
                "path-predicates",
                clean.execution.counters.get(WorkKind::Expressions),
            ),
            ("uniqueness", uniqueness),
            (
                "join-optional",
                clean.execution.counters.get(WorkKind::JoinProbes),
            ),
            ("full-id", full_id),
            ("retained-view", retained_view),
            ("late-error", late_error_fired),
            (
                "release",
                clean
                    .released
                    .min(paired.released)
                    .min(alias_execution.released)
                    .min(retained.released)
                    .min(fresh.released)
                    .min(late_clean.released)
                    .min(swapped.released)
                    .min(subsequent_execution.released),
            ),
            ("oracle", oracle_count),
            ("permutation", permutation_control),
            ("subsequent-match", subsequent_control),
        ],
        faults: vec![("cancel", cancel_fired), ("limit", limit_fired)],
        clean_controls: vec![("same-seed", same_seed)],
        fixture_nodes,
        fixture_edges,
        permutations,
        subsequent,
    })
}

std::thread_local! {
    static DOCUMENT_VISITS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static ORIGINAL_NODE_SOURCES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_document_visit() {
    DOCUMENT_VISITS.with(|visits| {
        if let Some(count) = visits.get() {
            visits.set(Some(count.saturating_add(1)));
        }
    });
}

/// Count calls to the physical document cursor within this synchronous scope.
pub fn observe_document_visits<T>(run: impl FnOnce() -> T) -> (T, u64) {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DOCUMENT_VISITS.with(|visits| visits.set(self.0));
        }
    }
    let _restore = Restore(DOCUMENT_VISITS.with(|visits| visits.replace(Some(0))));
    let result = run();
    let count = DOCUMENT_VISITS.with(|visits| visits.get().unwrap_or(0));
    (result, count)
}

/// Execute the unchanged node source as an order/coverage control.
pub fn with_original_node_sources<T>(run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ORIGINAL_NODE_SOURCES.with(|forced| forced.set(self.0));
        }
    }
    let _restore = Restore(ORIGINAL_NODE_SOURCES.with(|forced| forced.replace(true)));
    run()
}

pub(super) fn original_node_sources() -> bool {
    ORIGINAL_NODE_SOURCES.with(std::cell::Cell::get)
}
