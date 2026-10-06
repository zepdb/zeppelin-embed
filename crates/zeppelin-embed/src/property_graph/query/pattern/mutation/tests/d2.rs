//! ZE-52 slice D2: the `Mutate` occurrence for SET, REMOVE and label items.
//!
//! Every statement here runs through `Store::with_native_mutation`: a real
//! writer lease, a real read view, a real commit and, where it matters, a real
//! reopen. The plans are validated `GraphPlan`s built through the same
//! `execute_relational_plan!` admission every other occurrence suite uses.

use super::super::super::MutationScope;
use super::*;
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::native_graph::{
    NativeMutationConsumer, NativeMutationError, NativeMutationReport,
};
use crate::property_graph::query::plan::{BinaryExpression, SortKey, UnaryExpression};
use crate::property_graph::query::runtime::{Completion, RuntimeLimits};
use crate::property_graph::query::{Arithmetic, Comparison};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{
    GraphBatchReadView, StageError, StatementImages, WriteControl,
};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::{
    BatchDisposition, CanonicalEmbedding, GraphGeneration, NodeRef, RelId,
};
use std::cell::Cell;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Plan specifications
// ---------------------------------------------------------------------------

/// One expression, addressed by position in `Spec::expressions`.
#[derive(Clone, Copy)]
enum E {
    Slot(u32),
    Property(u32, &'static str),
    I64(i64),
    Null,
    Arithmetic(Arithmetic, u32, u32),
    Comparison(Comparison, u32, u32),
    /// The full-width identity text of the node an expression names.
    NodeIdText(u32),
}

/// One mutation item.
#[derive(Clone, Copy)]
enum M {
    Set(u32, &'static str, u32),
    Remove(u32, &'static str),
    Label(u32, &'static str, bool),
    Delete(u32),
    /// `DETACH DELETE`.
    DetachDelete(u32),
    /// `CreateNode(output, labels)`.
    CreateNode(u32, &'static [&'static str]),
    /// `CreateRelationship(output, source, target, type)`.
    CreateRelationship(u32, u32, u32, &'static str),
}

/// One operator kind.
#[derive(Clone)]
enum K {
    Unit,
    Scan(u32),
    LookupNode(u32, NodeId),
    LookupRelationship(u32, RelId),
    Eager,
    Mutate(Vec<M>),
    Project(Vec<(u32, u32)>),
    Filter(u32),
    Sort(Vec<(u32, bool)>),
    Join,
    /// `OptionalApply` with no predicate; the right input reads the left one
    /// as its anchor.
    Optional,
    /// `OffsetLimit(offset, limit)`.
    Limit(u64, Option<u64>),
    Collect,
}

/// A complete plan as owned data; the last operator is the root.
#[derive(Clone)]
struct Spec {
    operators: Vec<(Vec<u32>, K)>,
    expressions: Vec<E>,
}

type EntityValueRows = ([Option<(u128, i64)>; PATTERN_ROWS], usize);

/// Freezes `(entity, i64)` rows in emission order. The entity may be a node
/// or a relationship.
struct FreezeEntityValues;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeEntityValues {
    type Output = EntityValueRows;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > PATTERN_ROWS || rows.columns() != 2 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; PATTERN_ROWS];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            let entity = match rows.value(row, 0) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                Some(QueryValue::RelRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let value = match rows.value(row, 1) {
                Some(QueryValue::I64(value)) => value,
                _ => return Err(RuntimeError::Batch.into()),
            };
            *destination = Some((entity, value));
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<(u128, i64)>(),
            0,
        )
        .map_err(Into::into)
    }
}

/// Materializes `spec` over borrowed plan storage and runs it, with a writer
/// scope when `mutation` is given and read-only otherwise.
macro_rules! run_spec {
    ($spec:expr, $view:expr, $runtime:expr, $pattern_rows:expr; $($run:tt)+) => {{
        let spec: &Spec = $spec;
        let mut names = String::new();
        let mut name = |value: &str| {
            let start = names.len();
            names.push_str(value);
            (start, value.len())
        };
        let expression_names: Vec<Option<(usize, usize)>> = spec
            .expressions
            .iter()
            .map(|expression| match expression {
                E::Property(_, value) => Some(name(value)),
                _ => None,
            })
            .collect();
        let mutation_names: Vec<Vec<Vec<(usize, usize)>>> = spec
            .operators
            .iter()
            .map(|(_, kind)| match kind {
                K::Mutate(items) => items
                    .iter()
                    .map(|item| match item {
                        M::Set(_, value, _)
                        | M::Remove(_, value)
                        | M::Label(_, value, _)
                        | M::CreateRelationship(_, _, _, value) => vec![name(value)],
                        M::CreateNode(_, labels) => labels.iter().map(|label| name(label)).collect(),
                        M::Delete(_) | M::DetachDelete(_) => Vec::new(),
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        let names = names;
        let named = |range: Option<(usize, usize)>| {
            let (start, len) = range.expect("named plan item");
            GraphName::new(&names[start..start + len]).unwrap()
        };
        let first = |ranges: &Vec<(usize, usize)>| named(ranges.first().copied());
        // One label list per mutation item, in operator then item order.
        let label_lists: Vec<Vec<GraphName<'_>>> = mutation_names
            .iter()
            .flatten()
            .zip(spec.operators.iter().flat_map(|(_, kind)| match kind {
                K::Mutate(items) => items.clone(),
                _ => Vec::new(),
            }))
            .map(|(ranges, item)| match item {
                M::CreateNode(..) => ranges.iter().map(|range| named(Some(*range))).collect(),
                _ => Vec::new(),
            })
            .collect();
        let mut label_list = label_lists.iter();
        let inputs: Vec<Vec<PlanNodeId>> = spec
            .operators
            .iter()
            .map(|(inputs, _)| inputs.iter().map(|input| PlanNodeId(*input)).collect())
            .collect();
        let projections: Vec<Vec<Projection>> = spec
            .operators
            .iter()
            .map(|(_, kind)| match kind {
                K::Project(items) => items
                    .iter()
                    .map(|(slot, expression)| Projection {
                        slot: SlotId(*slot),
                        expression: ExprId(*expression),
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        let sort_keys: Vec<Vec<SortKey>> = spec
            .operators
            .iter()
            .map(|(_, kind)| match kind {
                K::Sort(keys) => keys
                    .iter()
                    .map(|(expression, descending)| SortKey {
                        expression: ExprId(*expression),
                        descending: *descending,
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        let mutations: Vec<Vec<Mutation<'_>>> = spec
            .operators
            .iter()
            .zip(&mutation_names)
            .map(|((_, kind), ranges)| match kind {
                K::Mutate(items) => items
                    .iter()
                    .zip(ranges)
                    .map(|(item, range)| {
                        let labels = label_list.next().expect("one label list per item");
                        match *item {
                            M::Set(entity, _, value) => Mutation::SetProperty {
                                entity: ExprId(entity),
                                name: first(range),
                                value: ExprId(value),
                            },
                            M::Remove(entity, _) => Mutation::RemoveProperty {
                                entity: ExprId(entity),
                                name: first(range),
                            },
                            M::Label(entity, _, present) => Mutation::SetLabel {
                                entity: ExprId(entity),
                                label: first(range),
                                present,
                            },
                            M::Delete(entity) => Mutation::Delete {
                                entity: ExprId(entity),
                                detach: false,
                            },
                            M::DetachDelete(entity) => Mutation::Delete {
                                entity: ExprId(entity),
                                detach: true,
                            },
                            M::CreateNode(output, _) => Mutation::CreateNode {
                                output: SlotId(output),
                                labels: labels.as_slice(),
                            },
                            M::CreateRelationship(output, source, target, _) => {
                                Mutation::CreateRelationship {
                                    output: SlotId(output),
                                    source: ExprId(source),
                                    target: ExprId(target),
                                    relationship_type: first(range),
                                }
                            }
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        let expressions: Vec<Expression<'_>> = spec
            .expressions
            .iter()
            .zip(&expression_names)
            .map(|(expression, range)| match *expression {
                E::Slot(slot) => Expression::Slot(SlotId(slot)),
                E::Property(entity, _) => Expression::Property {
                    entity: ExprId(entity),
                    name: named(*range),
                },
                E::I64(value) => Expression::Literal(Literal::I64(value)),
                E::Null => Expression::Literal(Literal::Null),
                E::Arithmetic(operation, left, right) => Expression::Binary {
                    operation: BinaryExpression::Arithmetic(operation),
                    left: ExprId(left),
                    right: ExprId(right),
                },
                E::Comparison(operation, left, right) => Expression::Binary {
                    operation: BinaryExpression::Comparison(operation),
                    left: ExprId(left),
                    right: ExprId(right),
                },
                E::NodeIdText(operand) => Expression::Unary {
                    operation: UnaryExpression::NodeIdText,
                    operand: ExprId(operand),
                },
            })
            .collect();
        let operators: Vec<Operator<'_>> = spec
            .operators
            .iter()
            .enumerate()
            .map(|(index, (_, kind))| Operator {
                inputs: &inputs[index],
                kind: match kind {
                    K::Unit => OperatorKind::Unit,
                    K::Scan(slot) => OperatorKind::ScanNodes {
                        output: SlotId(*slot),
                        label: None,
                    },
                    K::LookupNode(slot, id) => OperatorKind::LookupNode {
                        output: SlotId(*slot),
                        id: *id,
                    },
                    K::LookupRelationship(slot, id) => OperatorKind::LookupRelationship {
                        output: SlotId(*slot),
                        id: *id,
                    },
                    K::Eager => OperatorKind::Eager,
                    K::Mutate(_) => OperatorKind::Mutate(&mutations[index]),
                    K::Project(_) => OperatorKind::Project(&projections[index]),
                    K::Filter(predicate) => OperatorKind::Filter(ExprId(*predicate)),
                    K::Sort(_) => OperatorKind::Sort(&sort_keys[index]),
                    K::Join => OperatorKind::Join { predicate: None },
                    K::Optional => OperatorKind::OptionalApply { predicate: None },
                    K::Limit(offset, limit) => OperatorKind::OffsetLimit {
                        offset: *offset,
                        limit: *limit,
                    },
                    K::Collect => OperatorKind::Collect,
                },
            })
            .collect();
        // A plan that names nothing (a DELETE-only statement) has no name
        // backing to retain.
        let mut regions = Vec::new();
        let mut owners = Vec::new();
        if names.capacity() != 0 {
            regions.push(
                RetainedRegion::declared(names.as_ptr() as usize, names.capacity()).unwrap(),
            );
            owners.push(RetainedAllocation::string(&names).unwrap());
        }
        retain(&inputs, &mut regions, &mut owners);
        retain(&projections, &mut regions, &mut owners);
        retain(&sort_keys, &mut regions, &mut owners);
        retain(&mutations, &mut regions, &mut owners);
        retain(&label_lists, &mut regions, &mut owners);
        run_spec!(@$($run)+, $view, $runtime, operators, expressions, regions, owners, $pattern_rows)
    }};
    (@read, $view:expr, $runtime:expr, $operators:ident, $expressions:ident, $regions:ident,
     $owners:ident, $pattern_rows:expr) => {
        execute_relational_plan!(
            $view,
            $runtime,
            $operators,
            $expressions,
            $regions,
            $owners,
            &mut FreezeEntityValues,
            pattern_capacity($pattern_rows),
            execution_capacity(PATTERN_ROWS),
            EagerExecutionFailure::Build,
            |error| EagerExecutionFailure::Run(Box::new(error)),
            |source| source,
            vector
        )
    };
    (@mutate $scope:expr, $view:expr, $runtime:expr, $operators:ident, $expressions:ident,
     $regions:ident, $owners:ident, $pattern_rows:expr) => {
        execute_relational_plan!(
            mutation = $scope;
            $view,
            $runtime,
            $operators,
            $expressions,
            $regions,
            $owners,
            &mut FreezeEntityValues,
            pattern_capacity($pattern_rows),
            execution_capacity(PATTERN_ROWS),
            EagerExecutionFailure::Build,
            |error| EagerExecutionFailure::Run(Box::new(error)),
            vector
        )
    };
    (@result $overlay:expr, $images:expr, $columns:expr, $view:expr, $runtime:expr,
     $operators:ident, $expressions:ident, $regions:ident, $owners:ident, $pattern_rows:expr) => {
        execute_relational_plan!(
            @admit $runtime, $operators, $expressions, $regions, $owners, vector,
            (admitted, root) => {
                let _ = root;
                crate::property_graph::query::completed::execute_native_mutation_result(
                    $view,
                    $runtime,
                    &admitted,
                    &[],
                    $columns,
                    pattern_capacity($pattern_rows),
                    execution_capacity(PATTERN_ROWS),
                    $overlay,
                    $images,
                )
            }
        )
    };
    (@read_result $columns:expr, $view:expr, $runtime:expr, $operators:ident,
     $expressions:ident, $regions:ident, $owners:ident, $pattern_rows:expr) => {
        execute_relational_plan!(
            @admit $runtime, $operators, $expressions, $regions, $owners, vector,
            (admitted, root) => {
                let _ = root;
                crate::property_graph::query::completed::execute_native_result(
                    $view,
                    $runtime,
                    &admitted,
                    &[],
                    $columns,
                    pattern_capacity($pattern_rows),
                    execution_capacity(PATTERN_ROWS),
                )
            }
        )
    };
}

/// Declares every nonempty plan vector as a retained region with its owner.
fn retain<'a, T>(
    vectors: &'a [Vec<T>],
    regions: &mut Vec<RetainedRegion>,
    owners: &mut Vec<RetainedAllocation<'a>>,
) {
    for vector in vectors.iter().filter(|vector| vector.capacity() != 0) {
        regions.push(RetainedRegion::vector(vector).unwrap());
        owners.push(RetainedAllocation::vector(vector).unwrap());
    }
}

/// Runs one `Spec` as a mutation statement. `refused` records whether the
/// pattern was refused at build time, before any row was pulled.
struct SpecMutation<'a> {
    spec: &'a Spec,
    refused: &'a Cell<bool>,
}

impl NativeMutationConsumer<EntityValueRows> for SpecMutation<'_> {
    fn consume<'lease, 'm, 'g, 'w, 'i>(
        &mut self,
        view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        overlay: GraphBatchReadView<'w, 'static>,
        images: &'w StatementImages<'i>,
        _control: &mut WriteControl<'_>,
    ) -> Result<(EntityValueRows, GraphBatchReadView<'w, 'static>), NativeExecutionError> {
        let scope = MutationScope::new(overlay, images);
        let (result, overlay) = run_spec!(self.spec, view, runtime, PATTERN_ROWS; mutate scope);
        match result {
            Ok(execution) => Ok((execution.output, overlay.ok_or(RuntimeError::Batch)?)),
            Err(EagerExecutionFailure::Build(error)) => {
                self.refused.set(true);
                Err(error)
            }
            Err(EagerExecutionFailure::Run(failure)) => Err(failure.error),
        }
    }
}

/// Runs one `Spec` read-only through `with_native_read`.
struct SpecRead<'a> {
    spec: &'a Spec,
}

impl NativeReadConsumer<Result<EntityValueRows, EagerExecutionFailure>> for SpecRead<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Result<EntityValueRows, EagerExecutionFailure>, TreeError> {
        let result = run_spec!(self.spec, view, runtime, PATTERN_ROWS; read);
        Ok(result.map(|execution| execution.output))
    }
}

// ---------------------------------------------------------------------------
// Stores, fixtures and public-path observations
// ---------------------------------------------------------------------------

fn options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

struct D2Store {
    _directory: tempfile::TempDir,
    path: PathBuf,
    store: Store,
    document: Option<EmbeddingTower>,
}

impl D2Store {
    fn create(document: Option<EmbeddingTower>) -> Self {
        let directory = tempfile::tempdir().expect("d2 store directory");
        let path = directory.path().join("native");
        let store = Store::create_native_graph(&path, options(), document.clone())
            .expect("create d2 native store");
        Self {
            _directory: directory,
            path,
            store,
            document,
        }
    }

    fn reopen(mut self) -> Self {
        self.store.close().expect("close d2 store");
        self.store = Store::open_native_graph(&self.path, options(), self.document.clone())
            .expect("reopen d2 store");
        self
    }

    fn generation(&self) -> u64 {
        self.store
            .admit_native_read()
            .expect("native read lease")
            .bundle()
            .base()
            .generation
            .get()
    }
}

fn node(receipt: &crate::property_graph::staging::ItemReceipt) -> NodeId {
    match receipt.entity {
        EntityId::Node(id) => id,
        EntityId::Relationship(_) => panic!("node receipt"),
    }
}

/// Three nodes carrying `p` = 1, 2 and 3, returned in that order.
fn three_nodes(store: &D2Store) -> [NodeId; 3] {
    let receipts = crate::property_graph::with_local_refs(|_| {
        let p = GraphName::new("p").unwrap();
        let mut one = [GraphProperty::new(
            p,
            PropertyValue::new(PropertyData::I64(1)).unwrap(),
        )];
        let mut two = [GraphProperty::new(
            p,
            PropertyValue::new(PropertyData::I64(2)).unwrap(),
        )];
        let mut three = [GraphProperty::new(
            p,
            PropertyValue::new(PropertyData::I64(3)).unwrap(),
        )];
        let one = CanonicalContents::node(&mut [], &mut one, None, None).unwrap();
        let two = CanonicalContents::node(&mut [], &mut two, None, None).unwrap();
        let three = CanonicalContents::node(&mut [], &mut three, None, None).unwrap();
        let requests =
            [("one", &one), ("two", &two), ("three", &three)].map(|(key, image)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "d2", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(image)),
            });
        store
            .store
            .apply_native_graph(&requests, &control())
            .expect("publish three nodes")
    });
    [node(&receipts[0]), node(&receipts[1]), node(&receipts[2])]
}

fn document() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze52-d2-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x52, 0xd2],
        dims: 3,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}

/// Exact bit patterns the rich node must round-trip: a NaN payload, a signed
/// zero and a vector coordinate with a nonzero low mantissa.
const NAN_BITS: u64 = 0x7ff8_0000_0000_0001;
const NEGATIVE_ZERO_BITS: u64 = 0x8000_0000_0000_0000;
const COORDINATES: [u32; 3] = [0x3f80_0046, 0x8000_0000, 0x4000_0001];
const RICH_TEXT: &str = "stored d2 text";

/// The rich node's owned contents: every property kind, typed and untyped
/// empties, exact float bits, labels, text and a vector.
struct Rich {
    labels: Vec<&'static str>,
    properties: Vec<(&'static str, RichValue)>,
}

#[derive(Clone)]
enum RichValue {
    String(&'static str),
    I64(i64),
    F64(u64),
    Bool(bool),
    Strings(Vec<&'static str>),
    Integers(Vec<i64>),
    Floats(Vec<u64>),
    Bools(Vec<bool>),
    TypedEmpty,
    UntypedEmpty,
}

impl Rich {
    fn original() -> Self {
        Self {
            labels: vec!["A", "B"],
            properties: vec![
                ("b", RichValue::Bool(true)),
                ("e", RichValue::UntypedEmpty),
                ("f", RichValue::F64(NAN_BITS)),
                (
                    "fs",
                    RichValue::Floats(vec![NEGATIVE_ZERO_BITS, 1.5_f64.to_bits()]),
                ),
                ("bs", RichValue::Bools(vec![true, false])),
                ("is", RichValue::Integers(vec![1, -2, i64::MIN])),
                ("p", RichValue::I64(7)),
                ("q", RichValue::String("queue")),
                ("s", RichValue::String("same")),
                ("ss", RichValue::Strings(vec!["x", "", "y\0z"])),
                ("te", RichValue::TypedEmpty),
            ],
        }
    }

    fn set(&mut self, name: &'static str, value: RichValue) {
        self.remove(name);
        self.properties.push((name, value));
    }

    fn remove(&mut self, name: &str) {
        self.properties.retain(|(existing, _)| *existing != name);
    }

    /// The exact canonical bytes these contents encode to.
    fn canonical(&self, document: &EmbeddingTower) -> Vec<u8> {
        let floats: Vec<Vec<f64>> = self
            .properties
            .iter()
            .map(|(_, value)| match value {
                RichValue::Floats(bits) => bits.iter().map(|bits| f64::from_bits(*bits)).collect(),
                _ => Vec::new(),
            })
            .collect();
        let mut properties: Vec<GraphProperty<'_>> = self
            .properties
            .iter()
            .zip(&floats)
            .map(|((name, value), floats)| {
                let data = match value {
                    RichValue::String(value) => PropertyData::String(value),
                    RichValue::I64(value) => PropertyData::I64(*value),
                    RichValue::F64(bits) => PropertyData::F64(f64::from_bits(*bits)),
                    RichValue::Bool(value) => PropertyData::Bool(*value),
                    RichValue::Strings(values) => PropertyData::Strings(values),
                    RichValue::Integers(values) => PropertyData::Integers(values),
                    RichValue::Floats(_) => PropertyData::Floats(floats),
                    RichValue::Bools(values) => PropertyData::Bools(values),
                    RichValue::TypedEmpty => PropertyData::Integers(&[]),
                    RichValue::UntypedEmpty => PropertyData::EmptyList { count: 0 },
                };
                GraphProperty::new(
                    GraphName::new(name).unwrap(),
                    PropertyValue::new(data).unwrap(),
                )
            })
            .collect();
        let mut labels: Vec<GraphName<'_>> = self
            .labels
            .iter()
            .map(|label| GraphName::new(label).unwrap())
            .collect();
        let coordinates = COORDINATES.map(f32::from_bits);
        let embedding = CanonicalEmbedding::new(document, &coordinates).unwrap();
        let image = CanonicalContents::node(
            &mut labels,
            &mut properties,
            Some(RICH_TEXT),
            Some(embedding),
        )
        .unwrap();
        let mut bytes = Vec::new();
        image.write_to(&mut bytes, &mut || Ok(())).unwrap();
        bytes
    }

    /// Commits these contents as one new keyed node.
    fn commit(&self, store: &D2Store) -> NodeId {
        let document = store.document.as_ref().expect("rich store document");
        let bytes = self.canonical(document);
        let receipts = crate::property_graph::with_local_refs(|_| {
            let floats: Vec<Vec<f64>> = self
                .properties
                .iter()
                .map(|(_, value)| match value {
                    RichValue::Floats(bits) => {
                        bits.iter().map(|bits| f64::from_bits(*bits)).collect()
                    }
                    _ => Vec::new(),
                })
                .collect();
            let mut properties: Vec<GraphProperty<'_>> = self
                .properties
                .iter()
                .zip(&floats)
                .map(|((name, value), floats)| {
                    let data = match value {
                        RichValue::String(value) => PropertyData::String(value),
                        RichValue::I64(value) => PropertyData::I64(*value),
                        RichValue::F64(bits) => PropertyData::F64(f64::from_bits(*bits)),
                        RichValue::Bool(value) => PropertyData::Bool(*value),
                        RichValue::Strings(values) => PropertyData::Strings(values),
                        RichValue::Integers(values) => PropertyData::Integers(values),
                        RichValue::Floats(_) => PropertyData::Floats(floats),
                        RichValue::Bools(values) => PropertyData::Bools(values),
                        RichValue::TypedEmpty => PropertyData::Integers(&[]),
                        RichValue::UntypedEmpty => PropertyData::EmptyList { count: 0 },
                    };
                    GraphProperty::new(
                        GraphName::new(name).unwrap(),
                        PropertyValue::new(data).unwrap(),
                    )
                })
                .collect();
            let mut labels: Vec<GraphName<'_>> = self
                .labels
                .iter()
                .map(|label| GraphName::new(label).unwrap())
                .collect();
            let coordinates = COORDINATES.map(f32::from_bits);
            let embedding = CanonicalEmbedding::new(document, &coordinates).unwrap();
            let image = CanonicalContents::node(
                &mut labels,
                &mut properties,
                Some(RICH_TEXT),
                Some(embedding),
            )
            .unwrap();
            store
                .store
                .apply_native_graph(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "d2", "rich").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&image)),
                    }],
                    &control(),
                )
                .expect("publish rich node")
        });
        let id = node(&receipts[0]);
        assert_eq!(
            records(store, &[id], &[]),
            vec![(1, bytes)],
            "the rich fixture must commit exactly the contents it encodes"
        );
        id
    }
}

/// Reads each named record's installed revision and exact canonical bytes
/// through the public read view.
struct ReadRecords<'a> {
    nodes: &'a [NodeId],
    relationships: &'a [RelId],
}

fn read_payload<S: crate::property_graph::storage::tree::directory::BlockSource>(
    payload: crate::property_graph::storage::stream::PayloadSlice<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<Vec<u8>, TreeError> {
    let mut bytes = vec![0_u8; usize::try_from(payload.len()).unwrap()];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let end = (offset + 64 * 1024).min(bytes.len());
        let read = payload.read_at(offset as u64, &mut bytes[offset..end], resources)?;
        assert_eq!(read, end - offset);
        offset = end;
    }
    Ok(bytes)
}

impl NativeReadConsumer<Vec<(u64, Vec<u8>)>> for ReadRecords<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Vec<(u64, Vec<u8>)>, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        let mut observed = Vec::new();
        for node in self.nodes {
            let found = view
                .lookup_node(*node, &mut resources)?
                .ok_or(TreeError::Invalid("d2 node is not live"))?;
            let record = found.record();
            observed.push((
                record.revision().get(),
                read_payload(record.canonical_bytes(), &mut resources)?,
            ));
        }
        for relationship in self.relationships {
            let found = view
                .lookup_relationship(*relationship, &mut resources)?
                .ok_or(TreeError::Invalid("d2 relationship is not live"))?;
            let record = found.record();
            observed.push((
                record.revision().get(),
                read_payload(record.canonical_bytes(), &mut resources)?,
            ));
        }
        Ok(observed)
    }
}

fn records(store: &D2Store, nodes: &[NodeId], relationships: &[RelId]) -> Vec<(u64, Vec<u8>)> {
    store
        .store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReadRecords {
                nodes,
                relationships,
            },
        )
        .expect("read d2 records")
}

fn revisions(store: &D2Store, nodes: &[NodeId]) -> Vec<u64> {
    records(store, nodes, &[])
        .into_iter()
        .map(|(revision, _)| revision)
        .collect()
}

const IMAGES: usize = 16;

/// Runs `spec` as one mutation statement.
fn mutate(
    store: &D2Store,
    spec: &Spec,
    image_capacity: usize,
) -> (
    Result<(EntityValueRows, NativeMutationReport), NativeMutationError>,
    bool,
) {
    let refused = Cell::new(false);
    let outcome = store.store.with_native_mutation(
        &control(),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        16,
        16,
        image_capacity,
        SpecMutation {
            spec,
            refused: &refused,
        },
    );
    (outcome, refused.get())
}

fn committed(
    outcome: (
        Result<(EntityValueRows, NativeMutationReport), NativeMutationError>,
        bool,
    ),
) -> (Vec<(u128, i64)>, NativeMutationReport) {
    match outcome.0 {
        Ok((rows, report)) => (
            rows.0.iter().take(rows.1).flatten().copied().collect(),
            report,
        ),
        Err(error) => panic!("mutation statement must commit: {error}"),
    }
}

fn read(store: &D2Store, spec: &Spec) -> Vec<(u128, i64)> {
    let rows = store
        .store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            SpecRead { spec },
        )
        .expect("admit d2 read")
        .unwrap_or_else(|error| panic!("d2 read must execute: {error}"));
    rows.0.iter().take(rows.1).flatten().copied().collect()
}

fn sorted(mut rows: Vec<(u128, i64)>) -> Vec<(u128, i64)> {
    rows.sort_unstable();
    rows
}

fn pairs(nodes: &[NodeId], values: &[i64]) -> Vec<(u128, i64)> {
    sorted(
        nodes
            .iter()
            .zip(values)
            .map(|(node, value)| (node.get(), *value))
            .collect(),
    )
}

/// `Unit -> ScanNodes(n) -> Project(n, n.p) -> Collect`, read-only.
fn scan_p() -> Spec {
    Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Project(vec![(100, 0), (101, 1)])),
            (vec![2], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Property(0, "p")],
    }
}

/// `Unit -> ScanNodes(n) -> Eager -> Mutate(items) -> Project(n, <value>) ->
/// Collect` where `value` is expression 1 onward as the caller supplies.
fn scan_mutate(items: Vec<M>, projected: u32, expressions: Vec<E>) -> Spec {
    Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(items)),
            (vec![3], K::Project(vec![(100, 0), (101, projected)])),
            (vec![4], K::Collect),
        ],
        expressions,
    }
}

/// `SET n.p = n.p + 1`, projecting `(n, n.p)`.
fn increment_p() -> Spec {
    scan_mutate(
        vec![M::Set(0, "p", 3)],
        1,
        vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(1),
            E::Arithmetic(Arithmetic::Add, 1, 2),
        ],
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A SET over a scanned bag reads each node's value, stages the increment,
/// and publishes it; a second statement reads the first one's result.
#[test]
fn ze52_slice_d2_set_property_reads_progressively_and_publishes() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();

    let (rows, report) = committed(mutate(&store, &increment_p(), IMAGES));
    assert_eq!(sorted(rows), pairs(&nodes, &[2, 3, 4]));
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.admitted.get(), before);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));
    assert_eq!(read(&store, &scan_p()), pairs(&nodes, &[2, 3, 4]));

    let (rows, report) = committed(mutate(&store, &increment_p(), IMAGES));
    assert_eq!(sorted(rows), pairs(&nodes, &[3, 4, 5]));
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 2));

    let store = store.reopen();
    assert_eq!(store.generation(), before + 2);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[3, 4, 5]));
    assert_eq!(revisions(&store, &nodes), vec![3, 3, 3]);
    store.store.close().expect("close d2 store");
}

/// Three input rows that all target one node each run `SET n.p = n.p + 1`.
/// `Mutate` is a barrier, so every row returns the final value; a streaming
/// executor would return 2, 3 and 4.
#[test]
fn ze52_slice_d2_repeated_target_rows_see_the_final_value() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let first = nodes[0];
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, first)),
            (vec![], K::Unit),
            (vec![2], K::Scan(1)),
            (vec![1, 3], K::Join),
            (vec![4], K::Eager),
            (vec![5], K::Mutate(vec![M::Set(0, "p", 3)])),
            (vec![6], K::Project(vec![(100, 0), (101, 1)])),
            (vec![7], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(1),
            E::Arithmetic(Arithmetic::Add, 1, 2),
        ],
    };

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![(first.get(), 4); 3]);
    assert_eq!(report.disposition, BatchDisposition::Changed);

    let store = store.reopen();
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[4, 2, 3]));
    // One statement changes the node once, however many rows targeted it.
    assert_eq!(revisions(&store, &[first]), vec![2]);
    store.store.close().expect("close d2 store");
}

/// Within one row, a later item reads what an earlier item staged:
/// `SET n.p = n.p + 1, n.q = n.p, n.r = n.q` stores the new `p` in both `q`
/// and `r`. The chain also proves each item rebuilds from the image the item
/// before it staged: `q` exists only in that staged image, never in the read
/// view, so an item that rebuilt from the read view would drop it.
#[test]
fn ze52_slice_d2_intra_row_items_read_earlier_items() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let spec = scan_mutate(
        vec![M::Set(0, "p", 3), M::Set(0, "q", 1), M::Set(0, "r", 4)],
        5,
        vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(1),
            E::Arithmetic(Arithmetic::Add, 1, 2),
            E::Property(0, "q"),
            E::Property(0, "r"),
        ],
    );

    let (rows, _) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(sorted(rows), pairs(&nodes, &[2, 3, 4]));

    let store = store.reopen();
    for name in ["p", "q", "r"] {
        let projected = Spec {
            expressions: vec![E::Slot(0), E::Property(0, name)],
            ..scan_p()
        };
        assert_eq!(
            sorted(read(&store, &projected)),
            pairs(&nodes, &[2, 3, 4]),
            "reopened {name}"
        );
    }
    store.store.close().expect("close d2 store");
}

/// `SET n.s = n.s` rebuilds the whole image from the read view and stages
/// it. Finalization compares canonical bytes exactly, so a `NoOp` proves the
/// rebuild is lossless: every label, property kind and bit pattern, the text
/// and the vector. A real change then publishes exactly one edited field.
#[test]
fn ze52_slice_d2_unchanged_set_is_noop_and_rebuild_is_lossless() {
    let store = D2Store::create(Some(document()));
    let mut rich = Rich::original();
    let id = rich.commit(&store);
    let before = store.generation();
    let lookup = |items: Vec<M>, expressions: Vec<E>| Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, id)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(items)),
            (vec![3], K::Project(vec![(100, 0), (101, 1)])),
            (vec![4], K::Collect),
        ],
        expressions,
    };

    let same = lookup(
        vec![M::Set(0, "s", 2)],
        vec![E::Slot(0), E::Property(0, "p"), E::Property(0, "s")],
    );
    let (rows, report) = committed(mutate(&store, &same, IMAGES));
    assert_eq!(rows, vec![(id.get(), 7)]);
    assert_eq!(report.disposition, BatchDisposition::NoOp);
    assert_eq!(report.changed, None);
    assert_eq!(store.generation(), before);

    let change = lookup(vec![M::Set(0, "p", 1)], vec![E::Slot(0), E::I64(42)]);
    let (rows, report) = committed(mutate(&store, &change, IMAGES));
    assert_eq!(rows, vec![(id.get(), 42)]);
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));

    let store = store.reopen();
    rich.set("p", RichValue::I64(42));
    let expected = rich.canonical(store.document.as_ref().unwrap());
    assert_eq!(records(&store, &[id], &[]), vec![(2, expected)]);
    store.store.close().expect("close d2 store");
}

/// SET to null and REMOVE both remove a property; label items add and remove
/// labels. The reopened record is exactly the edited contents.
#[test]
fn ze52_slice_d2_remove_property_labels_and_null_value_reopen() {
    let store = D2Store::create(Some(document()));
    let mut rich = Rich::original();
    let id = rich.commit(&store);
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, id)),
            (vec![1], K::Eager),
            (
                vec![2],
                K::Mutate(vec![
                    M::Set(0, "q", 1),
                    M::Remove(0, "p"),
                    M::Label(0, "M", true),
                    M::Label(0, "A", false),
                    // Adding a label that is already present changes nothing.
                    M::Label(0, "B", true),
                ]),
            ),
            (vec![3], K::Project(vec![(100, 0), (101, 2)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Null, E::I64(0)],
    };

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![(id.get(), 0)]);
    assert_eq!(report.disposition, BatchDisposition::Changed);

    let store = store.reopen();
    rich.remove("q");
    rich.remove("p");
    rich.labels = vec!["B", "M"];
    let expected = rich.canonical(store.document.as_ref().unwrap());
    assert_eq!(records(&store, &[id], &[]), vec![(2, expected)]);
    store.store.close().expect("close d2 store");
}

/// Filter and Sort after a `Mutate` read the values it staged, not the read
/// view's: `SET n.p = 10 - n.p` then `WHERE n.p > 7 ORDER BY n.p DESC`.
#[test]
fn ze52_slice_d2_filter_and_sort_after_mutate_read_pending_values() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::Set(0, "p", 3)])),
            (vec![3], K::Filter(5)),
            (vec![4], K::Sort(vec![(1, true)])),
            (vec![5], K::Project(vec![(100, 0), (101, 1)])),
            (vec![6], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(10),
            E::Arithmetic(Arithmetic::Subtract, 2, 1),
            E::I64(7),
            E::Comparison(Comparison::Greater, 1, 4),
        ],
    };

    let (rows, _) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![(nodes[0].get(), 9), (nodes[1].get(), 8)]);

    let store = store.reopen();
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[9, 8, 7]));
    store.store.close().expect("close d2 store");
}

/// SET works on a relationship property and keeps the relationship's type,
/// endpoints and other properties exactly.
#[test]
fn ze52_slice_d2_set_property_on_relationship() {
    let store = D2Store::create(None);
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let second = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let properties = [
            GraphProperty::new(
                GraphName::new("w").unwrap(),
                PropertyValue::new(PropertyData::I64(1)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("tag").unwrap(),
                PropertyValue::new(PropertyData::String("t")).unwrap(),
            ),
        ];
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "d2", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "d2", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "d2", "ab").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &properties,
                }),
            },
        ];
        store
            .store
            .apply_native_graph(&requests, &control())
            .expect("publish relationship fixture")
    });
    let (source, target) = (node(&receipts[0]), node(&receipts[1]));
    let EntityId::Relationship(relationship) = receipts[2].entity else {
        panic!("relationship receipt");
    };
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupRelationship(0, relationship)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::Set(0, "w", 3)])),
            (vec![3], K::Project(vec![(100, 0), (101, 1)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Property(0, "w"),
            E::I64(10),
            E::Arithmetic(Arithmetic::Add, 1, 2),
        ],
    };

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![(relationship.get(), 11)]);
    assert_eq!(report.disposition, BatchDisposition::Changed);

    let store = store.reopen();
    let mut properties = [
        GraphProperty::new(
            GraphName::new("w").unwrap(),
            PropertyValue::new(PropertyData::I64(11)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("tag").unwrap(),
            PropertyValue::new(PropertyData::String("t")).unwrap(),
        ),
    ];
    let image = CanonicalContents::relationship(
        source,
        target,
        GraphName::new("LINKS").unwrap(),
        &mut properties,
    )
    .unwrap();
    let mut expected = Vec::new();
    image.write_to(&mut expected, &mut || Ok(())).unwrap();
    assert_eq!(records(&store, &[], &[relationship]), vec![(2, expected)]);
    store.store.close().expect("close d2 store");
}

/// A statement that needs more images than the arena admits is refused with a
/// typed limit before anything commits, and releases everything it charged.
#[test]
fn ze52_slice_d2_image_capacity_limit_rejects_without_partial_commit() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let shared = GraphResources::from_store(&store.store).expect("graph resources");
    let baseline = shared.reserved_bytes().expect("baseline reservation");

    match mutate(&store, &increment_p(), 2) {
        (
            Err(NativeMutationError::Execution(NativeExecutionError::Stage(StageError::Limit))),
            false,
        ) => {}
        (Err(error), refused) => {
            panic!("expected a typed image limit, got {error} (refused {refused})")
        }
        (Ok((_, report)), _) => panic!("a two-image arena committed {:?}", report.changed),
    }
    assert_eq!(store.generation(), before);
    assert_eq!(
        shared.reserved_bytes().expect("reservation after"),
        baseline
    );
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[1, 2, 3]));

    // The same statement commits once the arena admits one image per row.
    let (rows, _) = committed(mutate(&store, &increment_p(), 3));
    assert_eq!(sorted(rows), pairs(&nodes, &[2, 3, 4]));
    store.store.close().expect("close d2 store");
}

/// Mutations require a writer scope, including after DETACH admission.
#[test]
fn mutation_without_writer_is_rejected_at_build() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let expressions = vec![E::Slot(0), E::Property(0, "p"), E::I64(1)];
    let supported = scan_mutate(vec![M::Set(0, "p", 2)], 1, expressions);
    let refused = store
        .store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            SpecRead { spec: &supported },
        )
        .expect("admit read-only mutate");
    match refused {
        Err(EagerExecutionFailure::Build(NativeExecutionError::Plan(PlanError::Reference))) => {}
        Err(other) => panic!("read-only Mutate must be refused by reference: {other}"),
        Ok(_) => panic!("read-only Mutate must not build"),
    }
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[1, 2, 3]));
    store.store.close().expect("close d2 store");
}

mod completed_result;
mod d3;
mod d4;
mod limit_zero;
