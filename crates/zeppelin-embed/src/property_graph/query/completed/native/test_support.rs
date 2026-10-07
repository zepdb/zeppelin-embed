//! Tooling-only directed proof for actual native completed results.

use super::super::{CompletedGraphResult, Value};
use super::{NativeCompletionStage, NativeResultError, execute_native_result_source_observed};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::Arithmetic;
use crate::property_graph::query::expression::{
    ExpressionCapacity, ExpressionError, ExpressionFailure,
};
use crate::property_graph::query::pattern::PatternCapacity;
use crate::property_graph::query::plan::{
    BinaryExpression, ExprId, Expression, Literal, NodeFacts, Operator, OperatorKind, PlanBacking,
    PlanDescription, PlanFootprint, PlanNodeId, Projection, RetainedRegion, SlotId,
    VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{
    ArenaCapacity, ExecutionCapacity, NativeExecutionError, RuntimeContext, RuntimeError,
    RuntimeFailure, RuntimeLimits, WorkKind,
};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphRevision,
};
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Independent primitive observations plus exact directed receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeReport {
    /// Actual `(node, scalar, generation, rows)` copied from completed ownership.
    pub observations: Vec<(u128, i64, u64, u32)>,
    /// Independent values derived from committed write receipts and the plan literal.
    pub expected: Vec<(u128, i64, u64, u32)>,
    /// Exact evidence inventory; every value is measured from its named path.
    pub receipts: Vec<(&'static str, u64)>,
}

struct MaterializeConsumer {
    arithmetic_failure: bool,
    cancel: Option<CancelToken>,
    staged_copied: Arc<AtomicU64>,
    #[allow(
        clippy::type_complexity,
        reason = "test observer owns one fallible callback"
    )]
    after_pull: Option<Box<dyn FnMut(usize) -> Result<(), NativeResultError>>>,
}

pub(super) fn materialize_with_source_observer<H>(
    after_pull: H,
) -> impl NativeReadConsumer<Result<CompletedGraphResult, RuntimeFailure<NativeResultError>>>
where
    H: FnMut(usize) -> Result<(), NativeResultError> + 'static,
{
    MaterializeConsumer {
        arithmetic_failure: false,
        cancel: None,
        staged_copied: Arc::new(AtomicU64::new(0)),
        after_pull: Some(Box::new(after_pull)),
    }
}

impl NativeReadConsumer<Result<CompletedGraphResult, RuntimeFailure<NativeResultError>>>
    for MaterializeConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<CompletedGraphResult, RuntimeFailure<NativeResultError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let memory = runtime.memory();
        let scan_inputs = [PlanNodeId(0)];
        let project_inputs = [PlanNodeId(1)];
        let projections = [
            Projection {
                slot: SlotId(10),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(11),
                expression: if self.arithmetic_failure {
                    ExprId(3)
                } else {
                    ExprId(1)
                },
            },
        ];
        let success_expressions = [
            Expression::Slot(SlotId(0)),
            Expression::Literal(Literal::I64(42)),
        ];
        let arithmetic_expressions = [
            Expression::Slot(SlotId(0)),
            Expression::Literal(Literal::I64(42)),
            Expression::Literal(Literal::I64(0)),
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Divide),
                left: ExprId(1),
                right: ExprId(2),
            },
        ];
        let expressions: &[Expression<'_>] = if self.arithmetic_failure {
            &arithmetic_expressions
        } else {
            &success_expressions
        };
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
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
        ];
        let mut facts = QueryArena::new(memory, operators.len())
            .map_err(RuntimeError::Memory)
            .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?;
        for _ in 0..operators.len() {
            facts
                .push(NodeFacts::default())
                .map_err(RuntimeError::Memory)
                .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?;
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe operator region",
                )
            })?,
            RetainedRegion::slice(expressions).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe expression region",
                )
            })?,
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "native result probe facts region",
                    )
                })?,
            RetainedRegion::slice(&scan_inputs).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe scan region",
                )
            })?,
            RetainedRegion::slice(&project_inputs).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe project region",
                )
            })?,
            RetainedRegion::slice(&projections).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe projections region",
                )
            })?,
        ];
        regions.sort();
        let mut external = memory
            .reserve_external_capacity()
            .map_err(RuntimeError::Memory)
            .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?;
        external
            .reserve_additional(
                VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .map_err(RuntimeError::Memory)
            .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?;
        let description = PlanDescription {
            operators: &operators,
            expressions,
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
                        "native result probe plan backing",
                    )
                })?,
                runtime.values(),
            )
            .map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe plan validation",
                )
            })?;
        let expression_owner = if self.arithmetic_failure {
            RetainedAllocation::array(&arithmetic_expressions)
        } else {
            RetainedAllocation::array(&success_expressions)
        }
        .map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "native result probe expression owner",
            )
        })?;
        let owners = vec![
            RetainedAllocation::array(&operators).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe operator owner",
                )
            })?,
            expression_owner,
            facts_owner,
            RetainedAllocation::array(&scan_inputs).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe scan owner",
                )
            })?,
            RetainedAllocation::array(&project_inputs).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe project owner",
                )
            })?,
            RetainedAllocation::array(&projections).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe projections owner",
                )
            })?,
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe inventory",
                )
            })?,
            runtime.values(),
        )
        .map_err(RuntimeError::Memory)
        .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?
        .admit_plan(&plan, runtime.values())
        .map_err(RuntimeError::Memory)
        .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?;
        let columns = [
            GraphName::new("entity").map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe column",
                )
            })?,
            GraphName::new("answer").map_err(|_| {
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native result probe column",
                )
            })?,
        ];
        let cancel = self.cancel.take();
        let copied = Arc::clone(&self.staged_copied);
        let after_pull = &mut self.after_pull;
        Ok(execute_native_result_source_observed(
            view,
            runtime,
            &admitted,
            &[],
            &columns,
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 4,
                    max_rows: 4,
                    payload_bytes: 1024,
                    variable: ArenaCapacity::default(),
                },
                expression: ExpressionCapacity {
                    cells: 8,
                    string_bytes: 64,
                },
            },
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 4,
                batch_payload_bytes: 1024,
                result_payload_bytes: 1024,
                batch: ArenaCapacity::default(),
                result: ArenaCapacity::default(),
            },
            move |rows| match after_pull {
                Some(observer) => observer(rows),
                None => Ok(()),
            },
            move |stage, counters| {
                if stage == NativeCompletionStage::BeforeDestination {
                    copied.store(counters.get(WorkKind::CopiedBytes), Ordering::SeqCst);
                    if let Some(token) = &cancel {
                        token.cancel();
                    }
                }
                Ok(())
            },
        ))
    }
}

struct ReleaseMeasured<C> {
    inner: C,
    released: Arc<AtomicBool>,
}

impl<T, C: NativeReadConsumer<T>> NativeReadConsumer<T> for ReleaseMeasured<C> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<T, crate::property_graph::storage::tree::directory::TreeError> {
        let baseline = runtime.memory().reserved_bytes();
        let result = self.inner.consume(view, runtime);
        self.released.store(
            runtime.memory().reserved_bytes() == baseline,
            Ordering::SeqCst,
        );
        result
    }
}

fn probe_directory(seed: u64) -> PathBuf {
    std::env::temp_dir().join(format!(
        "zeppelin-native-result-{}-{seed}",
        std::process::id()
    ))
}

fn completed_row(result: &CompletedGraphResult) -> Result<(u128, i64, u64, u32), String> {
    let Value::Node(node) = *result.cell(0, 0).ok_or("missing probe node cell")? else {
        return Err(String::from("wrong probe node cell"));
    };
    let Value::I64(answer) = *result.cell(0, 1).ok_or("missing probe answer cell")? else {
        return Err(String::from("wrong probe answer cell"));
    };
    let record = result
        .pools()
        .nodes
        .get(node as usize)
        .ok_or("missing probe node record")?;
    Ok((
        record.id.get(),
        answer,
        result.metadata().generation.get(),
        result.metadata().rows,
    ))
}

/// Runs actual native source, completion, control, limit and release paths.
#[allow(
    clippy::indexing_slicing,
    reason = "test fixture has one asserted write receipt"
)]
pub fn run_actual_probe(seed: u64) -> Result<ProbeReport, String> {
    let directory = probe_directory(seed);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let store = Store::create_native_graph(
        directory.join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        None,
    )
    .map_err(|error| error.to_string())?;
    let image =
        CanonicalContents::node(&mut [], &mut [], None, None).map_err(|error| error.to_string())?;
    let receipt = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "native-result-probe", "node")
                    .map_err(|error| error.to_string())?,
                revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?[0];
    let node = match receipt.entity {
        EntityId::Node(node) => node,
        _ => return Err(String::from("native result probe receipt kind")),
    };

    let released = Arc::new(AtomicBool::new(false));
    let clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseMeasured {
                inner: MaterializeConsumer {
                    arithmetic_failure: false,
                    cancel: None,
                    staged_copied: Arc::new(AtomicU64::new(0)),
                    after_pull: None,
                },
                released: Arc::clone(&released),
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let observation = completed_row(&clean)?;
    let expected = (node.get(), 42, receipt.generation.get(), 1);
    let copied = clean.metadata().counters.get(WorkKind::CopiedBytes);

    let limited = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default()
                .with_limit(WorkKind::CopiedBytes, copied.saturating_sub(1))
                .map_err(|error| error.to_string())?,
            16 * 1024 * 1024,
            64,
            MaterializeConsumer {
                arithmetic_failure: false,
                cancel: None,
                staged_copied: Arc::new(AtomicU64::new(0)),
                after_pull: None,
            },
        )
        .map_err(|error| error.to_string())?;
    let limit_fire = match limited {
        Err(failure)
            if matches!(
                failure.error,
                NativeResultError::Native(NativeExecutionError::Runtime(RuntimeError::Limit(
                    WorkKind::CopiedBytes
                ))) | NativeResultError::Completed(super::super::CompletedError::Runtime(
                    RuntimeError::Limit(WorkKind::CopiedBytes)
                ))
            ) =>
        {
            failure.counters.get(WorkKind::CopiedBytes).max(1)
        }
        _ => return Err(String::from("native result copied-byte limit did not fire")),
    };

    let cancel = CancelToken::new();
    let staged = Arc::new(AtomicU64::new(0));
    let cancel_released = Arc::new(AtomicBool::new(false));
    let cancelled = store.with_native_read(
        &QueryControl::Cancel(cancel.clone()),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        ReleaseMeasured {
            inner: MaterializeConsumer {
                arithmetic_failure: false,
                cancel: Some(cancel),
                staged_copied: Arc::clone(&staged),
                after_pull: None,
            },
            released: Arc::clone(&cancel_released),
        },
    );
    let control_fire = u64::from(matches!(
        cancelled,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(crate::property_graph::query::QueryError::Cancelled)
            )
        ))
    ));
    if control_fire == 0 || staged.load(Ordering::SeqCst) == 0 {
        return Err(String::from(
            "native result staged cancellation did not fire",
        ));
    }

    let arithmetic_released = Arc::new(AtomicBool::new(false));
    let arithmetic = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseMeasured {
                inner: MaterializeConsumer {
                    arithmetic_failure: true,
                    cancel: None,
                    staged_copied: Arc::new(AtomicU64::new(0)),
                    after_pull: None,
                },
                released: Arc::clone(&arithmetic_released),
            },
        )
        .map_err(|error| error.to_string())?;
    let late_error_fire = match arithmetic {
        Err(failure)
            if matches!(
                failure.error,
                NativeResultError::Native(NativeExecutionError::Expression(ExpressionError {
                    expression: ExprId(3),
                    failure: ExpressionFailure::Runtime(RuntimeError::Value(
                        crate::property_graph::query::QueryError::DivisionByZero
                    )),
                }))
            ) =>
        {
            failure.counters.get(WorkKind::Expressions)
        }
        _ => return Err(String::from("native result late arithmetic did not fire")),
    };

    let paired = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            MaterializeConsumer {
                arithmetic_failure: false,
                cancel: None,
                staged_copied: Arc::new(AtomicU64::new(0)),
                after_pull: None,
            },
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    if completed_row(&paired)? != observation {
        return Err(String::from("native result paired clean mismatch"));
    }
    let mut perturbed = observation;
    perturbed.1 ^= 1;
    let oracle_fire = u64::from(perturbed != expected && observation == expected);
    let release = u64::from(
        released.load(Ordering::SeqCst)
            && cancel_released.load(Ordering::SeqCst)
            && arithmetic_released.load(Ordering::SeqCst),
    );
    let receipts = vec![
        ("copy", copied),
        ("identity", clean.pools().nodes.len() as u64),
        (
            "same-view",
            u64::from(observation.2 == receipt.generation.get()),
        ),
        ("limit.fire", limit_fire),
        ("control.fire", control_fire),
        ("late-error.fire", late_error_fire),
        ("release", release),
        ("oracle.can-fire", oracle_fire),
    ];
    store.close().map_err(|error| error.to_string())?;
    std::fs::remove_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(ProbeReport {
        observations: vec![observation],
        expected: vec![expected],
        receipts,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
pub(super) mod ze202 {
    use super::*;
    use crate::property_graph::{
        CanonicalEmbedding, GraphProperty, NodeId, NodeRef, PropertyData, PropertyValue, RelId,
    };

    pub const A: u128 = 7;
    pub const B: u128 = 7 + (1_u128 << 64);
    pub const R: u128 = 11;
    pub const S: u128 = 11 + (1_u128 << 64);

    pub fn options() -> OpenOptions {
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024)
    }

    pub fn fixture(document: Option<crate::epoch::EmbeddingTower>) -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create_native_graph_with_allocator_seed_for_test(
            dir.path().join("native"),
            options(),
            document.clone(),
            NodeId::new(A).unwrap(),
            RelId::new(R).unwrap(),
        )
        .unwrap();
        for (index, (id, rel, name, value)) in
            [(A, R, "a", 101), (B, S, "b", 202)].into_iter().enumerate()
        {
            if index == 1 {
                store
                    .jump_native_graph_allocators_for_test(
                        NodeId::new(B).unwrap(),
                        RelId::new(S).unwrap(),
                        &QueryControl::Cancel(CancelToken::new()),
                    )
                    .unwrap();
            }
            let mut props = [GraphProperty::new(
                GraphName::new("p").unwrap(),
                PropertyValue::new(PropertyData::I64(value)).unwrap(),
            )];
            let coords = [index as f32, 0.0];
            let embedding = document
                .as_ref()
                .map(|tower| CanonicalEmbedding::new(tower, &coords).unwrap());
            let contents =
                CanonicalContents::node(&mut [], &mut props, Some("amber"), embedding).unwrap();
            let receipts = store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "ze202", name).unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&contents)),
                    }],
                    &QueryControl::Cancel(CancelToken::new()),
                )
                .unwrap();
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].entity, EntityId::Node(NodeId::new(id).unwrap()));
            assert_eq!(receipts[0].revision.get(), 1);
            assert_eq!(receipts[0].generation.get(), if index == 0 { 2 } else { 5 });
            drop(receipts);
            let rel_props = [GraphProperty::new(
                GraphName::new("p").unwrap(),
                PropertyValue::new(PropertyData::I64(value + 10)).unwrap(),
            )];
            let receipts = store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "ze202", name).unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Existing(NodeId::new(id).unwrap()),
                            target: NodeRef::Existing(NodeId::new(A).unwrap()),
                            relationship_type: GraphName::new("LINKS").unwrap(),
                            properties: &rel_props,
                        }),
                    }],
                    &QueryControl::Cancel(CancelToken::new()),
                )
                .unwrap();
            assert_eq!(receipts.len(), 1);
            assert_eq!(
                receipts[0].entity,
                EntityId::Relationship(RelId::new(rel).unwrap())
            );
            assert_eq!(receipts[0].revision.get(), 1);
            assert_eq!(receipts[0].generation.get(), if index == 0 { 3 } else { 6 });
        }
        (store, dir)
    }
}
