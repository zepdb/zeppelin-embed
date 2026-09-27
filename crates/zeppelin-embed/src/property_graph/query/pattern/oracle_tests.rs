//! ZE-50 differential tests: real `NativePattern` against the independent
//! std-only tiny-graph oracle.
//!
//! The oracle source is the one that ships in the adversarial oracle package;
//! it is included here by path so the unit tests and the adversarial campaign
//! prove production against the exact same independent implementation without
//! adding a dependency to this crate.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use super::*;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::plan::{
    BinaryExpression, ExprId, Expression, NodeFacts, Operator, OperatorKind, PlanBacking,
    PlanDescription, PlanFootprint, RetainedRegion, SlotId, UnaryExpression,
    VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{
    ArenaCapacity, Completion, Execution, ExecutionCapacity, FrozenOutput, PreparedRows,
    RuntimeFailure, RuntimeLimits, execute_in,
};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::tree::directory::TreeError;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphRevision, NodeRef,
};
use std::mem::size_of;
use std::path::PathBuf;

#[path = "../../../../../../tests/adversarial-oracle/src/graph_pattern.rs"]
pub(super) mod oracle;

use oracle::{Cell, Direction as TinyDirection, Predicate, TinyPattern};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// One committed tiny graph plus its primitive oracle description.
pub(super) struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    pub(super) store: Store,
    nodes: Vec<u128>,
    relationships: Vec<u128>,
    pub(super) graph: oracle::Graph,
}

/// The exact options every oracle fixture opens its native graph with.
fn fixture_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

impl Fixture {
    pub(super) fn node(&self, index: usize) -> u128 {
        self.nodes[index]
    }

    pub(super) fn relationship(&self, index: usize) -> u128 {
        self.relationships[index]
    }

    /// Closes the store and opens the same directory again, so every later
    /// observation reads durable bytes rather than the writer's live state.
    pub(super) fn reopen(&mut self) {
        self.store.close().expect("close native oracle store");
        self.store = Store::open_native_graph(&self.path, fixture_options(), None)
            .expect("reopen native oracle store");
    }

    pub(super) fn close(self) {
        self.store.close().expect("close native oracle store");
    }
}

/// Commits `nodes` (label sets) and `edges` (source, target, type) into a real
/// native graph store and mirrors the committed identities into the oracle.
pub(super) fn fixture(
    namespace: &'static str,
    nodes: &[&[&'static str]],
    edges: &[(usize, usize, &'static str)],
) -> Fixture {
    let directory = tempfile::tempdir().expect("native oracle store");
    let path = directory.path().join("native");
    let store =
        Store::create_native_graph(&path, fixture_options(), None).expect("create native graph");
    let keys = (0..nodes.len())
        .map(|index| format!("n{index}"))
        .chain((0..edges.len()).map(|index| format!("e{index}")))
        .collect::<Vec<String>>();
    let mut labels = nodes
        .iter()
        .map(|labels| {
            labels
                .iter()
                .map(|label| GraphName::new(label).expect("fixture label"))
                .collect::<Vec<GraphName<'_>>>()
        })
        .collect::<Vec<Vec<GraphName<'_>>>>();
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let mut contents = Vec::new();
        for labels in &mut labels {
            contents.push(
                CanonicalContents::node(labels.as_mut_slice(), &mut [], None, None)
                    .expect("fixture node contents"),
            );
        }
        let mut requests = Vec::new();
        for (index, contents) in contents.iter().enumerate() {
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, namespace, &keys[index])
                    .expect("fixture node key"),
                revision: GraphRevision::new(1).expect("fixture revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(contents)),
            });
        }
        for (index, (source, target, relationship_type)) in edges.iter().enumerate() {
            requests.push(StructuredWrite {
                key: ApplicationKey::new(
                    EntityKind::Relationship,
                    namespace,
                    &keys[nodes.len() + index],
                )
                .expect("fixture relationship key"),
                revision: GraphRevision::new(1).expect("fixture revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(*source).expect("fixture source")),
                    target: NodeRef::Local(refs.node(*target).expect("fixture target")),
                    relationship_type: GraphName::new(relationship_type)
                        .expect("fixture relationship type"),
                    properties: &[],
                }),
            });
        }
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish oracle fixture")
    });
    let mut committed_nodes = Vec::new();
    let mut committed_relationships = Vec::new();
    for receipt in receipts.iter() {
        match receipt.entity {
            EntityId::Node(id) => committed_nodes.push(id.get()),
            EntityId::Relationship(id) => committed_relationships.push(id.get()),
        }
    }
    assert_eq!(committed_nodes.len(), nodes.len());
    assert_eq!(committed_relationships.len(), edges.len());
    let graph = oracle::Graph {
        nodes: nodes
            .iter()
            .enumerate()
            .map(|(index, labels)| oracle::Node::new(committed_nodes[index], labels))
            .collect(),
        edges: edges
            .iter()
            .enumerate()
            .map(|(index, (source, target, relationship_type))| {
                oracle::Edge::new(
                    committed_relationships[index],
                    committed_nodes[*source],
                    committed_nodes[*target],
                    relationship_type,
                )
            })
            .collect(),
    };
    Fixture {
        _directory: directory,
        path,
        store,
        nodes: committed_nodes,
        relationships: committed_relationships,
        graph,
    }
}

// ---------------------------------------------------------------------------
// Tiny pattern compiler
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum AbstractExpression {
    Slot(u32),
    HasLabel { entity: u32, name: usize },
    Equal { left: u32, right: u32 },
    Not(u32),
}

#[derive(Clone, Debug)]
enum AbstractKind {
    Unit,
    LookupNode {
        output: u32,
        id: u128,
    },
    ScanNodes {
        output: u32,
        label: Option<usize>,
    },
    Expand {
        source: u32,
        node: u32,
        relationship: u32,
        direction: TinyDirection,
        pattern: u32,
    },
    BoundedExpand {
        source: u32,
        node: u32,
        relationships: u32,
        min: u8,
        max: u8,
        direction: TinyDirection,
        pattern: u32,
    },
    Join,
    Optional {
        predicate: Option<u32>,
    },
    Filter {
        predicate: u32,
    },
    Collect,
}

/// Owned plan storage compiled from one tiny pattern.
struct Compiled {
    kinds: Vec<AbstractKind>,
    inputs: Vec<Vec<PlanNodeId>>,
    types: Vec<Vec<usize>>,
    names: Vec<String>,
    expressions: Vec<AbstractExpression>,
    root: PlanNodeId,
}

impl Compiled {
    fn name(&mut self, value: &str) -> usize {
        if let Some(position) = self.names.iter().position(|name| name == value) {
            return position;
        }
        self.names.push(value.to_owned());
        self.names.len() - 1
    }

    fn push(
        &mut self,
        kind: AbstractKind,
        inputs: Vec<PlanNodeId>,
        types: Vec<usize>,
    ) -> PlanNodeId {
        let id = u32::try_from(self.kinds.len()).expect("plan node count");
        self.kinds.push(kind);
        self.inputs.push(inputs);
        self.types.push(types);
        PlanNodeId(id)
    }

    fn expression(&mut self, value: AbstractExpression) -> u32 {
        self.expressions.push(value);
        u32::try_from(self.expressions.len() - 1).expect("expression count")
    }

    fn predicate(&mut self, predicate: &Predicate) -> u32 {
        match predicate {
            Predicate::HasLabel { slot, label } => {
                let entity = self.expression(AbstractExpression::Slot(*slot));
                let name = self.name(label);
                self.expression(AbstractExpression::HasLabel { entity, name })
            }
            Predicate::Same { left, right } => {
                let left = self.expression(AbstractExpression::Slot(*left));
                let right = self.expression(AbstractExpression::Slot(*right));
                self.expression(AbstractExpression::Equal { left, right })
            }
            Predicate::Not(inner) => {
                let operand = self.predicate(inner);
                self.expression(AbstractExpression::Not(operand))
            }
        }
    }

    fn types(&mut self, names: &[String]) -> Vec<usize> {
        names.iter().map(|name| self.name(name)).collect()
    }

    fn pattern(&mut self, pattern: &TinyPattern, anchor: Option<PlanNodeId>) -> PlanNodeId {
        match pattern {
            TinyPattern::Unit => self.push(AbstractKind::Unit, Vec::new(), Vec::new()),
            TinyPattern::Anchor => anchor.expect("anchor outside an optional right side"),
            TinyPattern::LookupNode { input, output, id } => {
                let child = self.pattern(input, anchor);
                self.push(
                    AbstractKind::LookupNode {
                        output: *output,
                        id: *id,
                    },
                    vec![child],
                    Vec::new(),
                )
            }
            TinyPattern::ScanNodes {
                input,
                output,
                label,
            } => {
                let child = self.pattern(input, anchor);
                let label = label.as_ref().map(|label| self.name(label));
                self.push(
                    AbstractKind::ScanNodes {
                        output: *output,
                        label,
                    },
                    vec![child],
                    Vec::new(),
                )
            }
            TinyPattern::Expand {
                input,
                source,
                node,
                relationship,
                direction,
                relationship_types,
                pattern,
            } => {
                let child = self.pattern(input, anchor);
                let types = self.types(relationship_types);
                self.push(
                    AbstractKind::Expand {
                        source: *source,
                        node: *node,
                        relationship: *relationship,
                        direction: *direction,
                        pattern: *pattern,
                    },
                    vec![child],
                    types,
                )
            }
            TinyPattern::BoundedExpand {
                input,
                source,
                node,
                relationships,
                min,
                max,
                direction,
                relationship_types,
                pattern,
            } => {
                let child = self.pattern(input, anchor);
                let types = self.types(relationship_types);
                self.push(
                    AbstractKind::BoundedExpand {
                        source: *source,
                        node: *node,
                        relationships: *relationships,
                        min: *min,
                        max: *max,
                        direction: *direction,
                        pattern: *pattern,
                    },
                    vec![child],
                    types,
                )
            }
            TinyPattern::Join { left, right } => {
                let left = self.pattern(left, anchor);
                let right = self.pattern(right, anchor);
                self.push(AbstractKind::Join, vec![left, right], Vec::new())
            }
            TinyPattern::Optional {
                left,
                right,
                predicate,
            } => {
                let left = self.pattern(left, anchor);
                let right = self.pattern(right, Some(left));
                let predicate = predicate
                    .as_ref()
                    .map(|predicate| self.predicate(predicate));
                self.push(
                    AbstractKind::Optional { predicate },
                    vec![left, right],
                    Vec::new(),
                )
            }
            TinyPattern::Filter { input, predicate } => {
                let child = self.pattern(input, anchor);
                let predicate = self.predicate(predicate);
                self.push(AbstractKind::Filter { predicate }, vec![child], Vec::new())
            }
        }
    }
}

fn compile(pattern: &TinyPattern) -> Compiled {
    let mut compiled = Compiled {
        kinds: Vec::new(),
        inputs: Vec::new(),
        types: Vec::new(),
        names: Vec::new(),
        expressions: Vec::new(),
        root: PlanNodeId(0),
    };
    let body = compiled.pattern(pattern, None);
    compiled.root = compiled.push(AbstractKind::Collect, vec![body], Vec::new());
    compiled
}

const fn direction(value: TinyDirection) -> Direction {
    match value {
        TinyDirection::Out => Direction::Outgoing,
        TinyDirection::In => Direction::Incoming,
        TinyDirection::Undirected => Direction::Either,
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Freezes the observed rows as slot-keyed oracle rows.
struct FreezeOracleRows {
    slots: Vec<u32>,
}

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeOracleRows {
    type Output = Vec<oracle::Row>;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.columns() != self.slots.len() {
            return Err(RuntimeError::Batch.into());
        }
        let mut output: Vec<oracle::Row> = Vec::new();
        output
            .try_reserve_exact(rows.rows())
            .map_err(|_| RuntimeError::Batch)?;
        for row in 0..rows.rows() {
            let mut cells: oracle::Row = Vec::new();
            cells
                .try_reserve_exact(rows.columns())
                .map_err(|_| RuntimeError::Batch)?;
            for column in 0..rows.columns() {
                let cell = match rows.value(row, column) {
                    Some(QueryValue::Null) => Cell::Null,
                    Some(QueryValue::NodeRef(value)) => Cell::Node(value.id().get()),
                    Some(QueryValue::RelRef(value)) => Cell::Relationship(value.id().get()),
                    Some(QueryValue::List(list)) => {
                        let mut path = Vec::new();
                        path.try_reserve_exact(list.len())
                            .map_err(|_| RuntimeError::Batch)?;
                        for index in 0..list.len() {
                            match list.get(index) {
                                Some(QueryValue::RelRef(value)) => path.push(value.id().get()),
                                _ => return Err(RuntimeError::Batch.into()),
                            }
                        }
                        Cell::Relationships(path)
                    }
                    _ => return Err(RuntimeError::Batch.into()),
                };
                cells.push((self.slots[column], cell));
            }
            output.push(cells);
        }
        let bytes = rows
            .rows()
            .checked_mul(rows.columns())
            .and_then(|cells| cells.checked_mul(size_of::<Cell>()))
            .ok_or(RuntimeError::Batch)?;
        FrozenOutput::new(output, rows.rows(), bytes, 0).map_err(Into::into)
    }
}

struct OracleConsumer<'a> {
    pattern: &'a TinyPattern,
}

type OracleExecution = Result<Execution<Vec<oracle::Row>>, RuntimeFailure<NativeExecutionError>>;

impl NativeReadConsumer<OracleExecution> for OracleConsumer<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<OracleExecution, TreeError> {
        let compiled = compile(self.pattern);
        let types = compiled
            .types
            .iter()
            .map(|names| {
                names
                    .iter()
                    .map(|name| GraphName::new(&compiled.names[*name]).expect("plan type name"))
                    .collect::<Vec<GraphName<'_>>>()
            })
            .collect::<Vec<Vec<GraphName<'_>>>>();
        let expressions = compiled
            .expressions
            .iter()
            .map(|expression| match expression {
                AbstractExpression::Slot(slot) => Expression::Slot(SlotId(*slot)),
                AbstractExpression::HasLabel { entity, name } => Expression::HasLabel {
                    entity: ExprId(*entity),
                    label: GraphName::new(&compiled.names[*name]).expect("plan label name"),
                },
                AbstractExpression::Equal { left, right } => Expression::Binary {
                    operation: BinaryExpression::Comparison(Comparison::Equal),
                    left: ExprId(*left),
                    right: ExprId(*right),
                },
                AbstractExpression::Not(operand) => Expression::Unary {
                    operation: UnaryExpression::Not,
                    operand: ExprId(*operand),
                },
            })
            .collect::<Vec<Expression<'_>>>();
        let operators = compiled
            .kinds
            .iter()
            .enumerate()
            .map(|(index, kind)| Operator {
                inputs: compiled.inputs[index].as_slice(),
                kind: match kind {
                    AbstractKind::Unit => OperatorKind::Unit,
                    AbstractKind::LookupNode { output, id } => OperatorKind::LookupNode {
                        output: SlotId(*output),
                        id: NodeId::new(*id).expect("plan node identity"),
                    },
                    AbstractKind::ScanNodes { output, label } => OperatorKind::ScanNodes {
                        output: SlotId(*output),
                        label: label
                            .map(|name| GraphName::new(&compiled.names[name]).expect("plan label")),
                    },
                    AbstractKind::Expand {
                        source,
                        node,
                        relationship,
                        direction: step,
                        pattern,
                    } => OperatorKind::Expand {
                        source: SlotId(*source),
                        node: SlotId(*node),
                        relationship: SlotId(*relationship),
                        direction: direction(*step),
                        relationship_types: types[index].as_slice(),
                        pattern: PatternId(*pattern),
                    },
                    AbstractKind::BoundedExpand {
                        source,
                        node,
                        relationships,
                        min,
                        max,
                        direction: step,
                        pattern,
                    } => OperatorKind::BoundedExpand {
                        source: SlotId(*source),
                        node: SlotId(*node),
                        relationships: SlotId(*relationships),
                        edge_predicate: None,
                        completed_edge_predicate: None,
                        min: *min,
                        max: *max,
                        direction: direction(*step),
                        relationship_types: types[index].as_slice(),
                        pattern: PatternId(*pattern),
                    },
                    AbstractKind::Join => OperatorKind::Join { predicate: None },
                    AbstractKind::Optional { predicate } => OperatorKind::OptionalApply {
                        predicate: predicate.map(ExprId),
                    },
                    AbstractKind::Filter { predicate } => OperatorKind::Filter(ExprId(*predicate)),
                    AbstractKind::Collect => OperatorKind::Collect,
                },
            })
            .collect::<Vec<Operator<'_>>>();
        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len()).map_err(|_| TreeError::Memory)?;
        for _ in 0..operators.len() {
            facts
                .push(NodeFacts::default())
                .map_err(|_| TreeError::Memory)?;
        }
        let mut regions = vec![
            RetainedRegion::vector(&operators).expect("operator region"),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .expect("facts region"),
        ];
        let mut owners = vec![RetainedAllocation::vector(&operators).expect("operator owner")];
        if !expressions.is_empty() {
            regions.push(RetainedRegion::vector(&expressions).expect("expression region"));
            owners.push(RetainedAllocation::vector(&expressions).expect("expression owner"));
        }
        for inputs in &compiled.inputs {
            if inputs.is_empty() {
                continue;
            }
            regions.push(RetainedRegion::vector(inputs).expect("input region"));
            owners.push(RetainedAllocation::vector(inputs).expect("input owner"));
        }
        for list in &types {
            if list.is_empty() {
                continue;
            }
            regions.push(RetainedRegion::vector(list).expect("type region"));
            owners.push(RetainedAllocation::vector(list).expect("type owner"));
        }
        for name in &compiled.names {
            if name.capacity() == 0 {
                continue;
            }
            regions.push(
                RetainedRegion::declared(name.as_ptr() as usize, name.capacity())
                    .expect("name region"),
            );
            owners.push(RetainedAllocation::string(name).expect("name owner"));
        }
        regions.sort();
        let retained_bytes = regions
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.end().saturating_sub(region.start()))
            })
            .ok_or(TreeError::Invalid("oracle plan footprint"))?;
        let mut external = memory
            .reserve_external_capacity()
            .map_err(|_| TreeError::Memory)?;
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .map_err(|_| TreeError::Memory)?;
        let description = PlanDescription {
            operators: operators.as_slice(),
            expressions: expressions.as_slice(),
            parameters: &[],
            root: compiled.root,
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).expect("oracle plan backing"),
                runtime.values(),
            )
            .expect("validate oracle plan");
        owners.push(facts_owner);
        let root_facts = plan.facts(compiled.root).expect("root facts");
        let slots = (0..root_facts.width())
            .map(|ordinal| root_facts.slot_at(ordinal).expect("root slot").0.0)
            .collect::<Vec<u32>>();
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).expect("oracle owners"),
            runtime.values(),
        )
        .expect("retain oracle plan")
        .admit_plan(&plan, runtime.values())
        .expect("admit oracle plan");
        let arena = ArenaCapacity {
            string_bytes: 256,
            list_cells: 256,
            node_ids: 128,
            relationship_ids: 256,
        };
        let mut source = NativePattern::new(
            view,
            &admitted,
            compiled.root,
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 64,
                    max_rows: 64,
                    payload_bytes: 8192,
                    variable: arena,
                },
                expression: ExpressionCapacity {
                    cells: 32,
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
            &mut FreezeOracleRows { slots },
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 64,
                batch_payload_bytes: 8192,
                result_payload_bytes: 16384,
                batch: arena,
                result: arena,
            },
        ))
    }
}

/// Executes one tiny pattern against the real native runtime.
pub(super) fn observed(fixture: &Fixture, pattern: &TinyPattern) -> oracle::Bag {
    let execution = fixture
        .store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            OracleConsumer { pattern },
        )
        .expect("admit real native view")
        .expect("execute native pattern");
    oracle::bag(execution.output)
}

/// Asserts that production and the independent oracle agree exactly, and
/// returns the shared bag.
pub(super) fn agreed(fixture: &Fixture, pattern: &TinyPattern) -> oracle::Bag {
    let expected = oracle::evaluate(&fixture.graph, pattern).expect("oracle evaluation");
    let observed = observed(fixture, pattern);
    assert_eq!(
        observed, expected,
        "native pattern disagreed with the independent oracle"
    );
    expected
}

// ---------------------------------------------------------------------------
// Tiny pattern helpers
// ---------------------------------------------------------------------------

pub(super) fn lookup(output: u32, id: u128) -> TinyPattern {
    TinyPattern::LookupNode {
        input: Box::new(TinyPattern::Unit),
        output,
        id,
    }
}

pub(super) fn scan(output: u32) -> TinyPattern {
    TinyPattern::ScanNodes {
        input: Box::new(TinyPattern::Unit),
        output,
        label: None,
    }
}

pub(super) fn expand(
    input: TinyPattern,
    source: u32,
    node: u32,
    relationship: u32,
    step: TinyDirection,
    pattern: u32,
) -> TinyPattern {
    TinyPattern::Expand {
        input: Box::new(input),
        source,
        node,
        relationship,
        direction: step,
        relationship_types: Vec::new(),
        pattern,
    }
}

pub(super) fn bounded(
    input: TinyPattern,
    source: u32,
    node: u32,
    relationships: u32,
    min: u8,
    max: u8,
    step: TinyDirection,
    pattern: u32,
) -> TinyPattern {
    TinyPattern::BoundedExpand {
        input: Box::new(input),
        source,
        node,
        relationships,
        min,
        max,
        direction: step,
        relationship_types: Vec::new(),
        pattern,
    }
}

pub(super) fn row(cells: &[(u32, Cell)]) -> oracle::Row {
    let mut row = cells.to_vec();
    row.sort();
    row
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

#[test]
fn native_pattern_oracle_parallel_and_self_edges() {
    let fixture = fixture(
        "oracle-parallel",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS"), (0, 0, "LINKS")],
    );
    let pattern = expand(scan(0), 0, 1, 2, TinyDirection::Out, 0);
    let bag = agreed(&fixture, &pattern);
    let (a, b) = (fixture.node(0), fixture.node(1));
    let mut expected = oracle::bag(vec![
        row(&[
            (0, Cell::Node(a)),
            (1, Cell::Node(b)),
            (2, Cell::Relationship(fixture.relationship(0))),
        ]),
        row(&[
            (0, Cell::Node(a)),
            (1, Cell::Node(b)),
            (2, Cell::Relationship(fixture.relationship(1))),
        ]),
        row(&[
            (0, Cell::Node(a)),
            (1, Cell::Node(a)),
            (2, Cell::Relationship(fixture.relationship(2))),
        ]),
    ]);
    expected.sort();
    assert_eq!(bag, expected, "parallel and self relationships");
    let from_lookup = expand(lookup(0, a), 0, 1, 2, TinyDirection::Out, 0);
    assert_eq!(
        agreed(&fixture, &from_lookup),
        bag,
        "a lookup start and a scan start agree on the rows they share"
    );
    fixture.close();
}

#[test]
fn native_pattern_oracle_undirected_self_loop_once() {
    let fixture = fixture(
        "oracle-undirected",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS"), (0, 0, "LINKS")],
    );
    let pattern = expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0);
    let bag = agreed(&fixture, &pattern);
    let self_loop = fixture.relationship(2);
    let visits = bag
        .iter()
        .filter(|row| {
            row.iter()
                .any(|(_, cell)| *cell == Cell::Relationship(self_loop))
        })
        .count();
    assert_eq!(visits, 1, "an undirected self-loop is visited exactly once");
    assert_eq!(bag.len(), 5, "two parallel edges seen from both endpoints");
    fixture.close();
}

#[test]
fn native_pattern_oracle_zero_hops() {
    let fixture = fixture("oracle-zero-hops", &[&[], &[]], &[(0, 1, "LINKS")]);
    let pattern = bounded(scan(0), 0, 1, 2, 0, 0, TinyDirection::Out, 0);
    let bag = agreed(&fixture, &pattern);
    let mut expected = oracle::bag(
        (0..2)
            .map(|index| {
                let node = fixture.node(index);
                row(&[
                    (0, Cell::Node(node)),
                    (1, Cell::Node(node)),
                    (2, Cell::Relationships(Vec::new())),
                ])
            })
            .collect(),
    );
    expected.sort();
    assert_eq!(bag, expected, "depth zero emits [start, start, []]");
    fixture.close();
}

#[test]
fn native_pattern_oracle_variable_relationship_lists() {
    let fixture = fixture(
        "oracle-variable",
        &[&[], &[], &[]],
        &[
            (0, 1, "LINKS"),
            (1, 2, "LINKS"),
            (2, 0, "LINKS"),
            (0, 1, "LINKS"),
        ],
    );
    for (min, max) in [(0_u8, 2_u8), (1, 1), (2, 3)] {
        let pattern = bounded(scan(0), 0, 1, 2, min, max, TinyDirection::Out, 0);
        let bag = agreed(&fixture, &pattern);
        for row in &bag {
            let length = row
                .iter()
                .find_map(|(slot, cell)| match (slot, cell) {
                    (2, Cell::Relationships(path)) => Some(path.len()),
                    _ => None,
                })
                .expect("path cell");
            assert!(
                length >= usize::from(min) && length <= usize::from(max),
                "path length {length} outside [{min}, {max}]"
            );
        }
        assert!(!bag.is_empty(), "variable-length bag for [{min}, {max}]");
    }
    fixture.close();
}

#[test]
fn native_pattern_oracle_repeated_nodes_no_repeated_relationship() {
    let fixture = fixture(
        "oracle-repeated",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS")],
    );
    let pattern = expand(
        expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0),
        1,
        3,
        4,
        TinyDirection::Undirected,
        0,
    );
    let bag = agreed(&fixture, &pattern);
    for row in &bag {
        let first = row
            .iter()
            .find_map(|(slot, cell)| (*slot == 2).then(|| cell.clone()))
            .expect("first relationship");
        let second = row
            .iter()
            .find_map(|(slot, cell)| (*slot == 4).then(|| cell.clone()))
            .expect("second relationship");
        assert_ne!(first, second, "one pattern never reuses a relationship");
    }
    assert!(!bag.is_empty(), "two-step bag is populated");
    let distinct = TinyPattern::Filter {
        input: Box::new(pattern),
        predicate: Predicate::Not(Box::new(Predicate::Same { left: 2, right: 4 })),
    };
    assert_eq!(
        agreed(&fixture, &distinct),
        bag,
        "an explicit distinct-relationship filter removes nothing"
    );
    fixture.close();
}

#[test]
fn native_pattern_oracle_subsequent_match_reuse() {
    let fixture = fixture(
        "oracle-subsequent",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS")],
    );
    let same = expand(
        expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0),
        1,
        3,
        4,
        TinyDirection::Undirected,
        0,
    );
    let subsequent = expand(
        expand(scan(0), 0, 1, 2, TinyDirection::Undirected, 0),
        1,
        3,
        4,
        TinyDirection::Undirected,
        1,
    );
    let same_bag = agreed(&fixture, &same);
    let subsequent_bag = agreed(&fixture, &subsequent);
    let reused = subsequent_bag
        .iter()
        .filter(|row| {
            let first = row
                .iter()
                .find(|(slot, _)| *slot == 2)
                .map(|(_, cell)| cell);
            let second = row
                .iter()
                .find(|(slot, _)| *slot == 4)
                .map(|(_, cell)| cell);
            first == second
        })
        .count();
    assert!(
        reused > 0,
        "a subsequent pattern match gets a fresh uniqueness set"
    );
    assert!(
        subsequent_bag.len() > same_bag.len(),
        "the fresh set admits rows the first pattern rejects"
    );
    let reused_rows = TinyPattern::Filter {
        input: Box::new(subsequent),
        predicate: Predicate::Same { left: 2, right: 4 },
    };
    assert_eq!(
        agreed(&fixture, &reused_rows).len(),
        reused,
        "the reused-relationship rows are exactly the rows the filter keeps"
    );
    fixture.close();
}

#[test]
fn native_pattern_oracle_optional_attached_where() {
    let fixture = fixture(
        "oracle-optional",
        &[&[], &["Tag"], &[]],
        &[(0, 1, "LINKS"), (0, 2, "LINKS")],
    );
    let pattern = TinyPattern::Optional {
        left: Box::new(scan(0)),
        right: Box::new(expand(TinyPattern::Anchor, 0, 1, 2, TinyDirection::Out, 1)),
        predicate: Some(Predicate::HasLabel {
            slot: 1,
            label: String::from("Tag"),
        }),
    };
    let bag = agreed(&fixture, &pattern);
    let mut expected = oracle::bag(vec![
        row(&[
            (0, Cell::Node(fixture.node(0))),
            (1, Cell::Node(fixture.node(1))),
            (2, Cell::Relationship(fixture.relationship(0))),
        ]),
        row(&[
            (0, Cell::Node(fixture.node(1))),
            (1, Cell::Null),
            (2, Cell::Null),
        ]),
        row(&[
            (0, Cell::Node(fixture.node(2))),
            (1, Cell::Null),
            (2, Cell::Null),
        ]),
    ]);
    expected.sort();
    assert_eq!(
        bag, expected,
        "an unmatched left row keeps one row with null new slots"
    );
    let negated = TinyPattern::Optional {
        left: Box::new(scan(0)),
        right: Box::new(expand(TinyPattern::Anchor, 0, 1, 2, TinyDirection::Out, 1)),
        predicate: Some(Predicate::Not(Box::new(Predicate::HasLabel {
            slot: 1,
            label: String::from("Tag"),
        }))),
    };
    let negated_bag = agreed(&fixture, &negated);
    let mut negated_expected = oracle::bag(vec![
        row(&[
            (0, Cell::Node(fixture.node(0))),
            (1, Cell::Node(fixture.node(2))),
            (2, Cell::Relationship(fixture.relationship(1))),
        ]),
        row(&[
            (0, Cell::Node(fixture.node(1))),
            (1, Cell::Null),
            (2, Cell::Null),
        ]),
        row(&[
            (0, Cell::Node(fixture.node(2))),
            (1, Cell::Null),
            (2, Cell::Null),
        ]),
    ]);
    negated_expected.sort();
    assert_eq!(
        negated_bag, negated_expected,
        "the attached WHERE selects the other endpoint under negation"
    );
    fixture.close();
}

#[test]
fn native_pattern_oracle_disconnected_patterns() {
    let fixture = fixture(
        "oracle-disconnected",
        &[&[], &[]],
        &[(0, 1, "LINKS"), (0, 1, "LINKS")],
    );
    let independent = TinyPattern::Join {
        left: Box::new(expand(scan(0), 0, 1, 2, TinyDirection::Out, 0)),
        right: Box::new(expand(scan(3), 3, 4, 5, TinyDirection::Out, 1)),
    };
    let bag = agreed(&fixture, &independent);
    assert_eq!(bag.len(), 4, "disjoint patterns form the cross product");
    let shared_scope = TinyPattern::Join {
        left: Box::new(expand(scan(0), 0, 1, 2, TinyDirection::Out, 0)),
        right: Box::new(expand(scan(3), 3, 4, 5, TinyDirection::Out, 0)),
    };
    let scoped = agreed(&fixture, &shared_scope);
    assert_eq!(
        scoped.len(),
        2,
        "uniqueness inside one pattern survives the join"
    );
    fixture.close();
}

#[test]
fn native_pattern_oracle_start_join_permutations() {
    let fixture = fixture(
        "oracle-permutations",
        &[&[], &[], &[]],
        &[
            (0, 1, "LINKS"),
            (1, 2, "LINKS"),
            (0, 1, "LINKS"),
            (2, 0, "LINKS"),
        ],
    );
    let forward_left = expand(scan(0), 0, 1, 2, TinyDirection::Out, 0);
    let reverse_left = expand(scan(1), 1, 0, 2, TinyDirection::In, 0);
    let forward_right = expand(scan(1), 1, 3, 4, TinyDirection::Out, 0);
    let reverse_right = expand(scan(3), 3, 1, 4, TinyDirection::In, 0);
    let permutations = [
        TinyPattern::Join {
            left: Box::new(forward_left.clone()),
            right: Box::new(forward_right.clone()),
        },
        TinyPattern::Join {
            left: Box::new(forward_right.clone()),
            right: Box::new(forward_left.clone()),
        },
        TinyPattern::Join {
            left: Box::new(reverse_left.clone()),
            right: Box::new(forward_right.clone()),
        },
        TinyPattern::Join {
            left: Box::new(forward_left.clone()),
            right: Box::new(reverse_right.clone()),
        },
        TinyPattern::Join {
            left: Box::new(reverse_right),
            right: Box::new(reverse_left),
        },
    ];
    let mut bags = Vec::new();
    for pattern in &permutations {
        bags.push(agreed(&fixture, pattern));
    }
    let first = bags.first().expect("one permutation");
    assert!(!first.is_empty(), "permutation bag is populated");
    for bag in &bags {
        assert_eq!(
            bag, first,
            "every legal start and build-side choice yields the identical bag"
        );
    }
    fixture.close();
}
