//! Plans, fixtures and the directed adversarial probe for the structured
//! execution seam (ZE-53 S3, ZE-192).
//!
//! Every statement here is built the way a real statement driver builds one:
//! inside the admission, in the admitted query memory, and handed to the
//! seam's executor. The probe drives the seam's write path through its
//! failure sites and checks each outcome against an oracle that never reads
//! the result collector: the node values the fixture wrote, the statement's
//! own arithmetic, and the published generation.

use super::super::{CompletedGraphResult, Outcome, Value};
use super::entry::{Executed, GraphQuery, GraphQueryExecutor, GraphQueryOptions, NoSearch};
use super::error::{GraphQueryError, GraphQueryErrorKind};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::tests::publication::{FaultPoint, RecordingVfs};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::Arithmetic;
use crate::property_graph::query::expression::ExpressionCapacity;
use crate::property_graph::query::pattern::PatternCapacity;
use crate::property_graph::query::plan::{
    BinaryExpression, ExprId, Expression, Literal, Mutation, NodeFacts, Operator, OperatorKind,
    PlanBacking, PlanDescription, PlanFootprint, PlanNodeId, Projection, RetainedRegion, SlotId,
    VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{QueryArena, RetainedAllocation, RetentionInventory};
use crate::property_graph::query::runtime::{
    ArenaCapacity, ExecutionCapacity, RuntimeContext, RuntimeLimits,
};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphProperty,
    GraphRevision, NodeId, PropertyData, PropertyValue,
};
use crate::vfs::Vfs;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Plan construction inside the admission
// ---------------------------------------------------------------------------

/// The actual owners of every vector or string a plan borrows besides its
/// operator, expression and eager arenas.
#[derive(Default)]
pub(crate) struct Backing<'a> {
    regions: Vec<RetainedRegion>,
    owners: Vec<RetainedAllocation<'a>>,
}

impl<'a> Backing<'a> {
    pub(crate) fn vec<T>(&mut self, value: &'a Vec<T>) -> Result<(), GraphQueryError> {
        if value.capacity() != 0 {
            self.regions.push(RetainedRegion::vector(value)?);
            self.owners.push(RetainedAllocation::vector(value)?);
        }
        Ok(())
    }

    pub(crate) fn string(&mut self, value: &'a String) -> Result<(), GraphQueryError> {
        if value.capacity() != 0 {
            self.regions.push(RetainedRegion::declared(
                value.as_ptr() as usize,
                value.capacity(),
            )?);
            self.owners.push(RetainedAllocation::string(value)?);
        }
        Ok(())
    }
}

/// Validates the plan rooted at its last operator in the admitted query
/// memory, retains every owner, and hands it to the seam.
pub(crate) fn run_plan<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
    operators: &Vec<Operator<'_>>,
    expressions: &Vec<Expression<'_>>,
    eager: &Vec<PlanNodeId>,
    backing: &Backing<'_>,
    columns: &[&str],
) -> Result<Executed, GraphQueryError> {
    let memory = runtime.memory();
    let mut facts = QueryArena::new(memory, operators.len())?;
    for _ in 0..operators.len() {
        facts.push(NodeFacts::default())?;
    }
    let mut regions = Vec::new();
    regions.push(RetainedRegion::vector(operators)?);
    regions.push(RetainedRegion::vector(expressions)?);
    if eager.capacity() != 0 {
        regions.push(RetainedRegion::vector(eager)?);
    }
    regions.push(RetainedRegion::declared(
        facts.as_slice().as_ptr() as usize,
        facts.heap_bytes(),
    )?);
    regions.extend(backing.regions.iter().copied());
    regions.sort();
    let retained = regions
        .iter()
        .try_fold(0usize, |total, region| {
            total.checked_add(region.end() - region.start())
        })
        .ok_or(GraphQueryError::contract("retained plan bytes overflow"))?;
    let mut external = memory.reserve_external_capacity()?;
    external.reserve_additional(
        retained
            + VALIDATION_SCRATCH_BYTES
            + regions.capacity() * size_of::<RetainedRegion>()
            + size_of::<PlanDescription<'_>>(),
    )?;
    let root = operators
        .len()
        .checked_sub(1)
        .and_then(|root| u32::try_from(root).ok())
        .ok_or(GraphQueryError::contract("empty plan"))?;
    let description = PlanDescription {
        operators,
        expressions,
        parameters: &[],
        root: PlanNodeId(root),
        eager_searches: eager,
    };
    let (plan, facts_owner) = facts.validate_plan(
        description,
        PlanFootprint::declared(memory.reserved_bytes()),
        PlanBacking::vector(&regions)?,
        runtime.values(),
    )?;
    let mut owners = vec![
        RetainedAllocation::vector(operators)?,
        RetainedAllocation::vector(expressions)?,
        facts_owner,
    ];
    if eager.capacity() != 0 {
        owners.push(RetainedAllocation::vector(eager)?);
    }
    owners.extend(backing.owners.iter().copied());
    let names = columns
        .iter()
        .map(|column| GraphName::new(column))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| GraphQueryError::contract("invalid column name"))?;
    executor.run(
        runtime,
        GraphQuery {
            plan: &plan,
            inventory: RetentionInventory::vector(&owners)?,
            bindings: &[],
            columns: &names,
        },
    )
}

/// `MATCH (n) RETURN n, n.p`.
pub(crate) fn read_p<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
) -> Result<Executed, GraphQueryError> {
    let p = String::from("p");
    let name = GraphName::new(&p).map_err(|_| GraphQueryError::contract("name"))?;
    let unit = vec![PlanNodeId(0)];
    let scan = vec![PlanNodeId(1)];
    let projections = vec![
        Projection {
            slot: SlotId(10),
            expression: ExprId(0),
        },
        Projection {
            slot: SlotId(11),
            expression: ExprId(1),
        },
    ];
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
            kind: OperatorKind::Project(&projections),
        },
    ];
    let expressions = vec![
        Expression::Slot(SlotId(0)),
        Expression::Property {
            entity: ExprId(0),
            name,
        },
    ];
    let mut backing = Backing::default();
    backing.string(&p)?;
    backing.vec(&unit)?;
    backing.vec(&scan)?;
    backing.vec(&projections)?;
    run_plan(
        runtime,
        executor,
        &operators,
        &expressions,
        &Vec::new(),
        &backing,
        &["n", "p"],
    )
}

/// What a write statement assigns to every scanned node's `p`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Assign {
    /// `SET n.p = n.p + 1`.
    Increment,
    /// `SET n.p = 6 / (n.p - pole)`: every row before the one whose `p`
    /// equals `pole` stages its value, and that row divides by zero.
    DivideAround(i64),
}

/// `MATCH (n) SET n.p = <assign> RETURN n, n.p`.
pub(crate) fn write_p<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
    assign: Assign,
) -> Result<Executed, GraphQueryError> {
    let p = String::from("p");
    let name = GraphName::new(&p).map_err(|_| GraphQueryError::contract("name"))?;
    let unit = vec![PlanNodeId(0)];
    let scan = vec![PlanNodeId(1)];
    let eager = vec![PlanNodeId(2)];
    let mutate = vec![PlanNodeId(3)];
    let (value, mut expressions) = match assign {
        Assign::Increment => (
            ExprId(3),
            vec![
                Expression::Literal(Literal::I64(1)),
                Expression::Binary {
                    operation: BinaryExpression::Arithmetic(Arithmetic::Add),
                    left: ExprId(1),
                    right: ExprId(2),
                },
            ],
        ),
        Assign::DivideAround(pole) => (
            ExprId(5),
            vec![
                Expression::Literal(Literal::I64(pole)),
                Expression::Binary {
                    operation: BinaryExpression::Arithmetic(Arithmetic::Subtract),
                    left: ExprId(1),
                    right: ExprId(2),
                },
                Expression::Literal(Literal::I64(6)),
                Expression::Binary {
                    operation: BinaryExpression::Arithmetic(Arithmetic::Divide),
                    left: ExprId(4),
                    right: ExprId(3),
                },
            ],
        ),
    };
    expressions.splice(
        0..0,
        [
            Expression::Slot(SlotId(0)),
            Expression::Property {
                entity: ExprId(0),
                name,
            },
        ],
    );
    let mutations = vec![Mutation::SetProperty {
        entity: ExprId(0),
        name,
        value,
    }];
    let projections = vec![
        Projection {
            slot: SlotId(10),
            expression: ExprId(0),
        },
        Projection {
            slot: SlotId(11),
            expression: ExprId(1),
        },
    ];
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
            kind: OperatorKind::Eager,
        },
        Operator {
            inputs: &eager,
            kind: OperatorKind::Mutate(&mutations),
        },
        Operator {
            inputs: &mutate,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let mut backing = Backing::default();
    backing.string(&p)?;
    backing.vec(&unit)?;
    backing.vec(&scan)?;
    backing.vec(&eager)?;
    backing.vec(&mutate)?;
    backing.vec(&mutations)?;
    backing.vec(&projections)?;
    run_plan(
        runtime,
        executor,
        &operators,
        &expressions,
        &Vec::new(),
        &backing,
        &["n", "p"],
    )
}

/// `CREATE (n) DELETE n RETURN 0`: consumes an identity and publishes no
/// entity, which the commit tail cannot publish yet.
pub(crate) fn create_then_delete<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
) -> Result<Executed, GraphQueryError> {
    let unit = vec![PlanNodeId(0)];
    let eager = vec![PlanNodeId(1)];
    let mutate = vec![PlanNodeId(2)];
    let relationship_type = String::from("R");
    let mutations = vec![
        Mutation::CreateNode {
            output: SlotId(0),
            labels: &[],
        },
        Mutation::CreateNode {
            output: SlotId(1),
            labels: &[],
        },
        Mutation::CreateRelationship {
            output: SlotId(2),
            source: ExprId(0),
            target: ExprId(1),
            relationship_type: GraphName::new(&relationship_type)
                .map_err(|_| GraphQueryError::contract("probe relationship type"))?,
        },
        Mutation::Delete {
            entity: ExprId(0),
            detach: false,
        },
        Mutation::Delete {
            entity: ExprId(1),
            detach: false,
        },
        Mutation::Delete {
            entity: ExprId(2),
            detach: false,
        },
    ];
    let projections = vec![Projection {
        slot: SlotId(10),
        expression: ExprId(3),
    }];
    let operators = vec![
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &unit,
            kind: OperatorKind::Eager,
        },
        Operator {
            inputs: &eager,
            kind: OperatorKind::Mutate(&mutations),
        },
        Operator {
            inputs: &mutate,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let expressions = vec![
        Expression::Slot(SlotId(0)),
        Expression::Slot(SlotId(1)),
        Expression::Slot(SlotId(2)),
        Expression::Literal(Literal::I64(0)),
    ];
    let mut backing = Backing::default();
    backing.string(&relationship_type)?;
    backing.vec(&unit)?;
    backing.vec(&eager)?;
    backing.vec(&mutate)?;
    backing.vec(&mutations)?;
    backing.vec(&projections)?;
    run_plan(
        runtime,
        executor,
        &operators,
        &expressions,
        &Vec::new(),
        &backing,
        &["zero"],
    )
}

pub(crate) fn restrict_delete<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
) -> Result<Executed, GraphQueryError> {
    let unit = vec![PlanNodeId(0)];
    let scan = vec![PlanNodeId(1)];
    let eager = vec![PlanNodeId(2)];
    let mutate = vec![PlanNodeId(3)];
    let mutations = vec![Mutation::Delete {
        entity: ExprId(0),
        detach: false,
    }];
    let projections = vec![Projection {
        slot: SlotId(10),
        expression: ExprId(1),
    }];
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
            kind: OperatorKind::Eager,
        },
        Operator {
            inputs: &eager,
            kind: OperatorKind::Mutate(&mutations),
        },
        Operator {
            inputs: &mutate,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let expressions = vec![
        Expression::Slot(SlotId(0)),
        Expression::Literal(Literal::I64(0)),
    ];
    let mut backing = Backing::default();
    backing.vec(&unit)?;
    backing.vec(&scan)?;
    backing.vec(&eager)?;
    backing.vec(&mutate)?;
    backing.vec(&mutations)?;
    backing.vec(&projections)?;
    run_plan(
        runtime,
        executor,
        &operators,
        &expressions,
        &Vec::new(),
        &backing,
        &["zero"],
    )
}

// ---------------------------------------------------------------------------
// Options, fixture and result observations
// ---------------------------------------------------------------------------

/// Capacities for the fixture's three-node statements.
pub(crate) fn options(image_capacity: usize) -> GraphQueryOptions {
    let variable = ArenaCapacity {
        string_bytes: 4096,
        list_cells: 128,
        node_ids: 64,
        relationship_ids: 64,
    };
    GraphQueryOptions {
        slot_column_names: false,
        limits: RuntimeLimits::default(),
        memory_limit: 16 * 1024 * 1024,
        source_slots: 64,
        pattern: PatternCapacity {
            rows: StorageCapacity {
                rows: 16,
                max_rows: 16,
                payload_bytes: 8192,
                variable,
            },
            expression: ExpressionCapacity {
                cells: 32,
                string_bytes: 4096,
            },
        },
        execution: ExecutionCapacity {
            batch_rows: 1,
            result_rows: 16,
            batch_payload_bytes: 8192,
            result_payload_bytes: 8192,
            batch: variable,
            result: variable,
        },
        lazy_targets: 16,
        overlay_capacity: 16,
        image_capacity,
    }
}

pub(crate) fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn store_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

/// A native store on a recording VFS holding three nodes whose `p` values
/// are `base + 1`, `base + 2` and `base + 3`, in node-ID order.
pub(crate) struct Fixture {
    pub(crate) directory: PathBuf,
    pub(crate) vfs: Arc<RecordingVfs>,
    pub(crate) store: Store,
    pub(crate) nodes: [NodeId; 3],
    pub(crate) values: [i64; 3],
}

impl Fixture {
    pub(crate) fn create(directory: PathBuf, base: i64) -> Result<Self, String> {
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let vfs = Arc::new(RecordingVfs::default());
        let infrastructure: Arc<dyn Vfs> = vfs.clone();
        let store = Store::create_native_graph_with_infrastructure(
            directory.join("native"),
            store_options(),
            None,
            infrastructure,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .map_err(|error| error.to_string())?;
        let values = [base + 1, base + 2, base + 3];
        let p = GraphName::new("p").map_err(|error| error.to_string())?;
        let mut properties = values.map(|value| {
            PropertyValue::new(PropertyData::I64(value))
                .map(|value| [GraphProperty::new(p, value)])
                .map_err(|error| error.to_string())
        });
        let mut images = Vec::new();
        for property in &mut properties {
            let property = property.as_mut().map_err(|error| error.clone())?;
            images.push(
                CanonicalContents::node(&mut [], property, None, None)
                    .map_err(|error| error.to_string())?,
            );
        }
        let keys = ["one", "two", "three"];
        let mut requests = Vec::new();
        for (key, image) in keys.iter().zip(&images) {
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze53-s3", key)
                    .map_err(|error| error.to_string())?,
                revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(image)),
            });
        }
        let receipts = store
            .apply_native_graph(&requests, &control())
            .map_err(|error| error.to_string())?;
        let mut nodes = Vec::new();
        for receipt in receipts.iter() {
            match receipt.entity {
                EntityId::Node(node) => nodes.push(node),
                EntityId::Relationship(_) => return Err(String::from("fixture receipt kind")),
            }
        }
        let nodes: [NodeId; 3] = nodes
            .try_into()
            .map_err(|_| String::from("fixture receipt count"))?;
        Ok(Self {
            directory,
            vfs,
            store,
            nodes,
            values,
        })
    }

    pub(crate) fn generation(&self) -> Result<u64, String> {
        Ok(self
            .store
            .admit_native_read()
            .map_err(|error| error.to_string())?
            .bundle()
            .base()
            .generation
            .get())
    }

    pub(crate) fn read(&self) -> Result<CompletedGraphResult, GraphQueryError> {
        self.store
            .execute_graph_query(&control(), &options(16), None::<&mut NoSearch>, read_p)
    }

    pub(crate) fn write(
        &self,
        control: &QueryControl,
        image_capacity: usize,
        assign: Assign,
    ) -> Result<CompletedGraphResult, GraphQueryError> {
        self.store.execute_graph_query(
            control,
            &options(image_capacity),
            None::<&mut NoSearch>,
            |runtime, executor| write_p(runtime, executor, assign),
        )
    }

    pub(crate) fn remove(self) -> Result<(), String> {
        let Self {
            directory, store, ..
        } = self;
        store.close().map_err(|error| error.to_string())?;
        std::fs::remove_dir_all(&directory).map_err(|error| error.to_string())
    }
}

/// `(node, p)` per row of a two-column `n, n.p` result, sorted by node.
pub(crate) fn node_values(result: &CompletedGraphResult) -> Result<Vec<(u128, i64)>, String> {
    let mut rows = Vec::new();
    for row in 0..result.metadata().rows as usize {
        let Some(Value::Node(index)) = result.cell(row, 0) else {
            return Err(format!("row {row} has no node"));
        };
        let Some(Value::I64(value)) = result.cell(row, 1) else {
            return Err(format!("row {row} has no p"));
        };
        let node = result
            .pools()
            .nodes
            .get(*index as usize)
            .ok_or("missing node record")?;
        rows.push((node.id.get(), *value));
    }
    rows.sort_unstable();
    Ok(rows)
}

// ---------------------------------------------------------------------------
// The directed probe
// ---------------------------------------------------------------------------

/// Independent observations plus exact directed receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeReport {
    /// `(node, p)` rows the committed clean statement returned, then the rows
    /// a fresh read returns, then the committed generation.
    pub observations: Vec<(u128, i64)>,
    /// The same, derived from the fixture's values and the statement's
    /// arithmetic without reading any result.
    pub expected: Vec<(u128, i64)>,
    /// Exact evidence inventory; every count is measured from its named path.
    pub receipts: Vec<(&'static str, u64)>,
}

/// One directory per call: concurrent probes of the same seed in one process
/// must not share a store (the ZE-196 race).
fn probe_directory(seed: u64, arm: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "zeppelin-ze53-s3-{arm}-{}-{seed}-{call}",
        std::process::id()
    ))
}

struct ObserveRelationship(crate::property_graph::RelId);

impl crate::lifecycle::native_graph::NativeReadConsumer<Vec<u8>> for ObserveRelationship {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Vec<u8>, crate::property_graph::storage::tree::directory::TreeError> {
        use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
        let mut resources = TreeResources::for_query(runtime)?;
        let relationship = view
            .lookup_relationship(self.0, &mut resources)?
            .ok_or(TreeError::Missing)?;
        let canonical = relationship.record().canonical_bytes();
        let mut bytes = vec![0; canonical.len() as usize];
        canonical.read_at(0, &mut bytes, &mut resources)?;
        Ok(bytes)
    }
}

fn incident_probe(seed: u64, base: i64) -> Result<u64, String> {
    use super::error::GraphQueryCause;
    use crate::lifecycle::native_graph::NativeGraphError;
    use crate::property_graph::NodeRef;
    use crate::property_graph::staging::StageError;
    let fixture = Fixture::create(probe_directory(seed, "incident"), base)?;
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Relationship, "ze192", "edge")
            .map_err(|e| e.to_string())?,
        revision: GraphRevision::new(1).map_err(|e| e.to_string())?,
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Relationship {
            source: NodeRef::Existing(fixture.nodes[0]),
            target: NodeRef::Existing(fixture.nodes[1]),
            relationship_type: GraphName::new("LINK").map_err(|e| e.to_string())?,
            properties: &[],
        }),
    }];
    let receipts = fixture
        .store
        .apply_native_graph(&request, &control())
        .map_err(|e| e.to_string())?;
    let Some(EntityId::Relationship(id)) = receipts.iter().next().map(|r| r.entity) else {
        return Err(String::from("missing relationship receipt"));
    };
    let read = || {
        fixture
            .store
            .with_native_read(
                &control(),
                RuntimeLimits::default(),
                8 * 1024 * 1024,
                16,
                ObserveRelationship(id),
            )
            .map_err(|e| e.to_string())
    };
    let relationship = read()?;
    let before = fixture.generation()?;
    let outcome = fixture.store.execute_graph_query(
        &control(),
        &options(16),
        None::<&mut NoSearch>,
        restrict_delete,
    );
    if !matches!(&outcome, Err(error) if matches!(error.cause(), GraphQueryCause::Graph(NativeGraphError::Stage(StageError::IncidentRelationship))))
    {
        return Err(format!(
            "restrict delete missed finalize incident check: {:?}",
            outcome.err()
        ));
    }
    let receipt = refused(&fixture, outcome, GraphQueryErrorKind::Constraint, before)?;
    if read()? != relationship {
        return Err(String::from("refused delete changed relationship"));
    }
    fixture.remove()?;
    Ok(receipt)
}

/// One refused statement: its group, and whether the store kept its
/// generation and every `p` value.
fn refused(
    fixture: &Fixture,
    outcome: Result<CompletedGraphResult, GraphQueryError>,
    expected: GraphQueryErrorKind,
    before: u64,
) -> Result<u64, String> {
    let error = match outcome {
        Ok(result) => {
            return Err(format!(
                "a {expected:?} fault site published {:?}",
                result.metadata().outcome
            ));
        }
        Err(error) => error,
    };
    if error.kind() != expected || !error.nothing_committed() {
        return Err(format!("expected a definite {expected:?}, got {error}"));
    }
    if fixture.generation()? != before {
        return Err(format!("a refused {expected:?} advanced the generation"));
    }
    let unchanged = node_values(&fixture.read().map_err(|error| error.to_string())?)?;
    let original: Vec<_> = fixture
        .nodes
        .iter()
        .zip(fixture.values)
        .map(|(node, value)| (node.get(), value))
        .collect();
    if unchanged != original {
        return Err(format!("a refused {expected:?} changed stored values"));
    }
    Ok(1)
}

type IdentityObservations = Vec<(u128, i64)>;

/// Isolated allocator-only publication, with a clean control and the existing
/// durable-commit/publication fault. Expected fences count the plan's creates.
pub(crate) fn fence_only_probe(
    seed: u64,
    base: i64,
    inject: bool,
) -> Result<(IdentityObservations, IdentityObservations), String> {
    use crate::lifecycle::native_graph::NativeGraphError;
    use crate::lifecycle::native_graph::tests::publication::{
        arm_query_publication_fault, query_publication_fault_fired,
    };
    let fixture = Fixture::create(
        probe_directory(
            seed,
            if inject {
                "fence-recovery"
            } else {
                "fence-commit"
            },
        ),
        base,
    )?;
    let lease = fixture
        .store
        .admit_native_read()
        .map_err(|e| e.to_string())?;
    let before = lease.bundle().base().generation.get();
    let roots = lease.bundle().roots().references();
    let high = lease.bundle().high_waters();
    drop(lease);
    let unchanged = node_values(&fixture.read().map_err(|e| e.to_string())?)?;
    if inject {
        arm_query_publication_fault(&fixture.store);
    }
    let result = fixture.store.execute_graph_query(
        &control(),
        &options(16),
        None::<&mut NoSearch>,
        create_then_delete,
    );
    if inject {
        if !matches!(result, Err(ref e) if e.kind() == GraphQueryErrorKind::WriteIndeterminate && !e.nothing_committed())
            || !query_publication_fault_fired(&fixture.store)
            || !matches!(
                fixture.store.admit_native_read(),
                Err(NativeGraphError::ReadAdmissionsStopped)
            )
            || !matches!(fixture.write(&control(), 16, Assign::Increment), Err(e) if e.kind() == GraphQueryErrorKind::Unavailable)
        {
            return Err(String::from(
                "fence-only durable commit did not stop admissions",
            ));
        }
    } else {
        let result = result.map_err(|e| e.to_string())?;
        if result.metadata().outcome
            != (Outcome::Committed {
                changed: crate::property_graph::GraphGeneration::new(before + 1),
            })
        {
            return Err(String::from("fence-only statement did not advance once"));
        }
    }
    let Fixture {
        directory, store, ..
    } = fixture;
    // Recover from WAL, without writing a checkpoint via close.
    drop(store);
    let store = Store::open_native_graph(directory.join("native"), store_options(), None)
        .map_err(|e| e.to_string())?;
    let lease = store.admit_native_read().map_err(|e| e.to_string())?;
    let recovered = lease.bundle().high_waters();
    if lease.bundle().base().generation.get() != before + 1
        || lease.bundle().roots().references() != roots
        || recovered.node != high.node + 2
        || recovered.relationship != high.relationship + 1
        || recovered.symbols != high.symbols
    {
        return Err(String::from(
            "fence-only recovery changed graph or lost counted fences",
        ));
    }
    drop(lease);
    let rows = store
        .execute_graph_query(&control(), &options(16), None::<&mut NoSearch>, read_p)
        .map_err(|e| e.to_string())?;
    if node_values(&rows)? != unchanged {
        return Err(String::from("fence-only recovery changed records"));
    }
    let mut properties = [];
    let image =
        CanonicalContents::node(&mut [], &mut properties, None, None).map_err(|e| e.to_string())?;
    let receipt = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze190", "next")
                    .map_err(|e| e.to_string())?,
                revision: GraphRevision::new(1).map_err(|e| e.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .map_err(|e| e.to_string())?;
    let node = match receipt.iter().next().map(|r| r.entity) {
        Some(EntityId::Node(node)) => node,
        _ => return Err(String::from("fence-only next node receipt missing")),
    };
    drop(receipt);
    let receipt = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze190", "next")
                    .map_err(|e| e.to_string())?,
                revision: GraphRevision::new(1).map_err(|e| e.to_string())?,
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: crate::property_graph::NodeRef::Existing(node),
                    target: crate::property_graph::NodeRef::Existing(node),
                    relationship_type: GraphName::new("R").map_err(|e| e.to_string())?,
                    properties: &[],
                }),
            }],
            &control(),
        )
        .map_err(|e| e.to_string())?;
    let rel = match receipt.iter().next().map(|r| r.entity) {
        Some(EntityId::Relationship(rel)) => rel,
        _ => return Err(String::from("fence-only next relationship receipt missing")),
    };
    drop(receipt);
    let actual = vec![(node.get(), 0), (rel.get(), 0)];
    let expected = vec![(high.node + 3, 0), (high.relationship + 2, 0)];
    if actual != expected {
        return Err(String::from("fence-only next identities reused burned ids"));
    }
    store.close().map_err(|e| e.to_string())?;
    std::fs::remove_dir_all(directory).map_err(|e| e.to_string())?;
    Ok((actual, expected))
}

/// Directed close schedules shared by the lifetime tests and runner.
#[derive(Clone, Copy)]
pub(crate) enum CloseSchedule {
    Runtime,
    BeforeAppend,
    AfterAppend,
}

pub(crate) fn close_probe(seed: u64, schedule: CloseSchedule) -> Result<u64, String> {
    use crate::lifecycle::StoreState;
    use std::cell::Cell;
    use std::sync::Barrier;
    use std::time::{Duration, Instant};

    let label = match schedule {
        CloseSchedule::Runtime => "runtime-close",
        CloseSchedule::BeforeAppend => "pre-append-close",
        CloseSchedule::AfterAppend => "post-append-close",
    };
    let fixture = Fixture::create(probe_directory(seed, label), (seed % 1000) as i64 * 10)?;
    let before = fixture.generation()?;
    let wait_closing = || -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while fixture.store.state().map_err(|e| e.to_string())? != StoreState::Closing {
            if Instant::now() >= deadline {
                return Err(String::from("close did not begin"));
            }
            std::thread::yield_now();
        }
        Ok(())
    };
    let builds = Cell::new(0);
    let checkpoint = Cell::new(None);
    let outcome = std::thread::scope(|scope| -> Result<_, String> {
        let mut closer = None;
        let outcome = match schedule {
            CloseSchedule::Runtime => fixture.store.execute_graph_query(
                &control(),
                &options(16),
                None::<&mut NoSearch>,
                |runtime, executor| {
                    builds.set(builds.get() + 1);
                    let executed = write_p(runtime, executor, Assign::Increment)?;
                    if builds.get() == 2 {
                        closer = Some(scope.spawn(|| fixture.store.close()));
                        wait_closing().map_err(|_| GraphQueryError::contract("close wait"))?;
                        let observed = runtime.checkpoint();
                        checkpoint.set(Some(matches!(
                            observed,
                            Err(crate::property_graph::query::runtime::RuntimeError::Value(
                                crate::property_graph::query::QueryError::ReadCancelled
                            ))
                        )));
                    }
                    Ok(executed)
                },
            ),
            CloseSchedule::BeforeAppend | CloseSchedule::AfterAppend => {
                let (entered, release) = if matches!(schedule, CloseSchedule::AfterAppend) {
                    fixture.vfs.arm_wal_full_sync()
                } else {
                    let entered = Arc::new(Barrier::new(2));
                    let release = Arc::new(Barrier::new(2));
                    let e = Arc::clone(&entered);
                    let r = Arc::clone(&release);
                    fixture.vfs.after_next_create(move || {
                        e.wait();
                        r.wait();
                    });
                    (entered, release)
                };
                let writer = scope.spawn(|| fixture.write(&control(), 16, Assign::Increment));
                entered.wait();
                closer = Some(scope.spawn(|| fixture.store.close()));
                let closing = wait_closing();
                release.wait();
                let outcome = writer.join().map_err(|_| String::from("writer panicked"))?;
                closing?;
                outcome
            }
        };
        closer
            .ok_or("close was not started")?
            .join()
            .map_err(|_| String::from("closer panicked"))?
            .map_err(|e| e.to_string())?;
        Ok(outcome)
    })?;
    // Assertions happen after both threads have drained.
    let committed = matches!(schedule, CloseSchedule::AfterAppend);
    let outcome_check = if committed {
        match outcome {
            Ok(result)
                if result.metadata().outcome
                    == (Outcome::Committed {
                        changed: crate::property_graph::GraphGeneration::new(before + 1),
                    }) =>
            {
                Ok(())
            }
            other => Err(format!(
                "post-append close lost the commit: {:?}",
                other.err()
            )),
        }
    } else {
        match outcome {
            Err(e) if e.kind() == GraphQueryErrorKind::Closed && e.nothing_committed() => Ok(()),
            other => Err(format!(
                "pre-append close did not refuse: {:?}",
                other.err()
            )),
        }
    };
    let Fixture {
        directory,
        store,
        vfs,
        nodes,
        values,
    } = fixture;
    drop(store);
    let reopened = Fixture {
        store: Store::open_native_graph(directory.join("native"), store_options(), None)
            .map_err(|e| e.to_string())?,
        directory,
        vfs,
        nodes,
        values,
    };
    let observed = (
        reopened.generation()?,
        node_values(&reopened.read().map_err(|e| e.to_string())?)?,
    );
    let delta = i64::from(committed);
    let expected = (
        before + u64::from(committed),
        nodes
            .iter()
            .zip(values)
            .map(|(n, p)| (n.get(), p + delta))
            .collect(),
    );
    reopened.remove()?;
    if matches!(schedule, CloseSchedule::Runtime) && checkpoint.get() != Some(true) {
        return Err(format!(
            "writer runtime checkpoint missed close: {:?}",
            checkpoint.get()
        ));
    }
    outcome_check?;
    if observed != expected {
        return Err(format!(
            "close reopen mismatch: {observed:?} != {expected:?}"
        ));
    }
    Ok(1)
}

/// Drives the seam's write path through its fault sites, each refused with
/// its typed group and a proved unchanged store, then through a cancellation
/// that arrives after the commit attempt, which must commit.
pub fn run_actual_probe(seed: u64) -> Result<ProbeReport, String> {
    let base = i64::try_from(seed % 1000).map_err(|error| error.to_string())? * 10;
    let fixture = Fixture::create(probe_directory(seed, "refusals"), base)?;
    let before = fixture.generation()?;

    // Mid-drain: the first row stages its value, the second divides by zero.
    let mid_drain = refused(
        &fixture,
        fixture.write(&control(), 16, Assign::DivideAround(fixture.values[1])),
        GraphQueryErrorKind::Expression,
        before,
    )?;
    // The statement image arena admits fewer images than rows.
    let images = refused(
        &fixture,
        fixture.write(&control(), 2, Assign::Increment),
        GraphQueryErrorKind::Limit,
        before,
    )?;
    // Cancellation inside the commit tail, before its point of no return:
    // the first private artifact is on disk, the WAL is untouched.
    let token = CancelToken::new();
    let cancel = token.clone();
    fixture.vfs.after_next_create(move || cancel.cancel());
    let precommit = refused(
        &fixture,
        fixture.write(&QueryControl::Cancel(token), 16, Assign::Increment),
        GraphQueryErrorKind::Cancelled,
        before,
    )?;

    // The clean statement, and a cancellation that arrives while the WAL
    // Full sync of its commit is held: it must still report the commit.
    let token = CancelToken::new();
    let (entered, release) = fixture.vfs.arm_wal_full_sync();
    let committed = std::thread::scope(|scope| {
        let writer = scope
            .spawn(|| fixture.write(&QueryControl::Cancel(token.clone()), 16, Assign::Increment));
        entered.wait();
        token.cancel();
        release.wait();
        writer.join()
    })
    .map_err(|_| String::from("probe writer panicked"))?
    .map_err(|error| format!("a post-commit-attempt cancel claimed rollback: {error}"))?;
    let changed = fixture.generation()?;
    if committed.metadata().outcome
        != (Outcome::Committed {
            changed: crate::property_graph::GraphGeneration::new(changed),
        })
        || changed != before + 1
    {
        return Err(String::from("post-commit cancel did not report its commit"));
    }
    let post_commit = u64::from(token.is_cancelled());
    let expected_rows: Vec<_> = fixture
        .nodes
        .iter()
        .zip(fixture.values)
        .map(|(node, value)| (node.get(), value + 1))
        .collect();
    let mut observations = node_values(&committed)?;
    observations.extend(node_values(
        &fixture.read().map_err(|error| error.to_string())?,
    )?);
    observations.push((
        0,
        i64::try_from(changed - before).map_err(|e| e.to_string())?,
    ));
    let mut expected = expected_rows.clone();
    expected.extend(expected_rows);
    expected.push((0, 1));
    fixture.remove()?;

    for inject in [false, true] {
        let (actual, oracle) = fence_only_probe(seed, base, inject)?;
        observations.extend(actual);
        expected.extend(oracle);
    }
    let runtime_close = close_probe(seed, CloseSchedule::Runtime)?;
    let pre_append_close = close_probe(seed, CloseSchedule::BeforeAppend)?;
    let post_append_close = close_probe(seed, CloseSchedule::AfterAppend)?;

    let incident = incident_probe(seed, base)?;
    for (point, key) in [
        (FaultPoint::Append, "indeterminate.fire"),
        (FaultPoint::PartialAppend, "partial-append.fire"),
        (FaultPoint::WalSync, "wal-sync.fire"),
        (FaultPoint::Publish, "publish.fire"),
    ] {
        use crate::lifecycle::native_graph::NativeGraphError;
        use crate::lifecycle::native_graph::tests::publication::{
            arm_query_publication_fault, query_publication_fault_fired,
        };
        let fixture = Fixture::create(probe_directory(seed, key), base)?;
        if point == FaultPoint::Publish {
            arm_query_publication_fault(&fixture.store);
        } else {
            fixture.vfs.arm_fault(point);
        }
        match fixture.write(&control(), 16, Assign::Increment) {
            Err(error)
                if error.kind() == GraphQueryErrorKind::WriteIndeterminate
                    && !error.nothing_committed() => {}
            outcome => {
                return Err(format!(
                    "{point:?} did not report indeterminate: {:?}",
                    outcome.err()
                ));
            }
        }
        if point == FaultPoint::Publish {
            if !query_publication_fault_fired(&fixture.store) {
                return Err(String::from("publication fault did not fire"));
            }
        } else {
            fixture.vfs.assert_fired_once();
        }
        if !matches!(
            fixture.store.admit_native_read(),
            Err(NativeGraphError::ReadAdmissionsStopped)
        ) || !matches!(fixture.write(&control(), 16, Assign::Increment), Err(e) if e.kind() == GraphQueryErrorKind::Unavailable)
        {
            return Err(String::from("indeterminate store still admits work"));
        }
        fixture.remove()?;
        let clean = Fixture::create(probe_directory(seed, "disabled"), base)?;
        let before = clean.generation()?;
        let result = clean
            .write(&control(), 16, Assign::Increment)
            .map_err(|e| e.to_string())?;
        let expected: Vec<_> = clean
            .nodes
            .iter()
            .zip(clean.values)
            .map(|(n, p)| (n.get(), p + 1))
            .collect();
        if node_values(&result)? != expected || clean.generation()? != before + 1 {
            return Err(String::from("injection-disabled control mismatch"));
        }
        clean.remove()?;
    }

    super::search_probe::preparation_refusal();
    super::search_probe::same_view(base);
    super::search_probe::approximation(base);

    let mut perturbed = observations.clone();
    if let Some(first) = perturbed.first_mut() {
        first.1 ^= 1;
    }
    let oracle = u64::from(perturbed != expected && observations == expected);
    let mut receipts = vec![("incident.fire", incident)];
    Ok(ProbeReport {
        observations,
        expected,
        receipts: {
            receipts.extend([
                ("mid-drain.fire", mid_drain),
                ("image-limit.fire", images),
                ("fence-only.commit", 1),
                ("fence-only.recovery", 1),
                ("precommit-cancel.fire", precommit),
                ("post-commit-cancel.commit", post_commit),
                ("runtime-close.fire", runtime_close),
                ("pre-append-close.fire", pre_append_close),
                ("post-append-close.commit", post_append_close),
                ("oracle.can-fire", oracle),
                ("search-preparation.fire", 1),
                ("search-report.retain", 1),
                ("search-publication.same-view", 1),
            ]);
            receipts
        },
    })
}
