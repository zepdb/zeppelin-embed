#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use super::super::Value;
use super::*;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{
    CancelToken, Deadline, ManualMonotonicClock, OpenOptions, QueryControl, Store,
    SystemMonotonicClock,
};
use crate::property_graph::query::Arithmetic;
use crate::property_graph::query::expression::ExpressionCapacity;
use crate::property_graph::query::expression::{ExpressionError, ExpressionFailure};
use crate::property_graph::query::plan::{
    BinaryExpression, Direction, ExprId, Expression, Literal, NodeFacts, Operator, OperatorKind,
    PatternId, PlanBacking, PlanDescription, PlanFootprint, Projection, RetainedRegion, SlotId,
    VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{ArenaCapacity, RuntimeLimits, WorkKind};
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[derive(Default)]
struct NativeResultMapVfs {
    calls: AtomicU64,
    fail_at: AtomicU64,
    fires: AtomicU64,
}

impl NativeResultMapVfs {
    fn arm_next(&self) {
        self.fail_at
            .store(self.calls.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    }

    fn disarm(&self) {
        self.fail_at.store(0, Ordering::SeqCst);
    }
}

impl Vfs for NativeResultMapVfs {
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
            return Err(std::io::Error::other(
                "scheduled native-result entity map fault",
            ));
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

macro_rules! run_native_result {
    (@execute $view:expr, $runtime:expr, $plan:expr, $columns:expr,
     $pattern:expr, $execution:expr, $before_copy:expr) => {
        execute_native_result_observed(
            $view,
            $runtime,
            $plan,
            &[],
            $columns,
            $pattern,
            $execution,
            $before_copy,
        )
    };
    (@execute $view:expr, $runtime:expr, $plan:expr, $columns:expr,
     $pattern:expr, $execution:expr) => {
        execute_native_result(
            $view,
            $runtime,
            $plan,
            &[],
            $columns,
            $pattern,
            $execution,
        )
    };
    ($view:expr, $runtime:expr, $operators:ident, $expressions:ident,
     $regions:expr, $owners:expr, $columns:expr $(, before_copy = $before_copy:expr)?) => {{
        let memory = $runtime.memory();
        let mut facts = QueryArena::new(memory, $operators.len()).expect("result fact arena");
        for _ in 0..$operators.len() {
            facts.push(NodeFacts::default()).expect("result fact slot");
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
            .expect("retained result plan bytes");
        let mut external = memory
            .reserve_external_capacity()
            .expect("external result plan backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("result plan validation backing");
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
            .expect("validate result plan");
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
        .expect("retain result plan")
        .admit_plan(&plan, $runtime.values())
        .expect("admit result plan");
        run_native_result!(@execute
            $view,
            $runtime,
            &admitted,
            $columns,
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 32,
                    payload_bytes: 16 * 1024,
                    variable: ArenaCapacity {
                        string_bytes: 8192,
                        list_cells: 512,
                        node_ids: 128,
                        relationship_ids: 128,
                    },
                },
                expression: ExpressionCapacity {
                    cells: 128,
                    string_bytes: 8192,
                },
            },
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 32,
                batch_payload_bytes: 16 * 1024,
                result_payload_bytes: 16 * 1024,
                batch: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 512,
                    node_ids: 128,
                    relationship_ids: 128,
                },
                result: ArenaCapacity {
                    string_bytes: 8192,
                    list_cells: 512,
                    node_ids: 128,
                    relationship_ids: 128,
                },
            }
            $(, $before_copy)?
        )
    }};
}

struct ActualRowsConsumer;

impl
    NativeReadConsumer<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
    > for ActualRowsConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
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
                expression: ExprId(1),
            },
        ];
        let expressions = [
            Expression::Slot(SlotId(0)),
            Expression::Literal(Literal::I64(42)),
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
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
        ];
        let mut facts = QueryArena::new(memory, operators.len()).expect("fact arena");
        for _ in 0..operators.len() {
            facts.push(NodeFacts::default()).expect("fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
            RetainedRegion::slice(&scan_inputs).unwrap(),
            RetainedRegion::slice(&project_inputs).unwrap(),
            RetainedRegion::slice(&projections).unwrap(),
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
            .expect("validate native result plan");
        let owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::array(&expressions).unwrap(),
            facts_owner,
            RetainedAllocation::array(&scan_inputs).unwrap(),
            RetainedAllocation::array(&project_inputs).unwrap(),
            RetainedAllocation::array(&projections).unwrap(),
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            runtime.values(),
        )
        .expect("retain native result plan")
        .admit_plan(&plan, runtime.values())
        .expect("admit native result plan");
        let column_names = [
            GraphName::new("entity").unwrap(),
            GraphName::new("answer").unwrap(),
        ];
        Ok(execute_native_result(
            view,
            runtime,
            &admitted,
            &[],
            &column_names,
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 4,
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
        ))
    }
}

#[test]
fn native_result_actual_rows_survive_close() {
    let directory = tempfile::tempdir().expect("native result store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        None,
    )
    .expect("create native result graph");
    let node = CanonicalContents::node(&mut [], &mut [], None, None).expect("fixture node");
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "native-result", "one").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("publish native result fixture");
    let expected = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("native result fixture receipt kind"),
    };
    let result = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ActualRowsConsumer,
        )
        .expect("admit actual native result")
        .expect("materialize actual native result");
    store.close().expect("close native result store");
    drop(store);

    assert_eq!(result.metadata().rows, 1);
    let pools = result.pools();
    assert_eq!(pools.columns.len(), 2);
    assert_eq!(result.string(pools.columns[0].name), Some("entity"));
    assert_eq!(result.string(pools.columns[1].name), Some("answer"));
    let Value::Node(index) = *result.cell(0, 0).expect("owned node cell") else {
        panic!("owned node cell kind");
    };
    assert_eq!(pools.nodes[index as usize].id, expected);
    assert_eq!(result.cell(0, 1), Some(&Value::I64(42)));
}

struct RecordsConsumer {
    source: NodeId,
}

impl
    NativeReadConsumer<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
    > for RecordsConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        {
            let mut resources =
                crate::property_graph::storage::tree::directory::TreeResources::for_query(runtime)?;
            let node = view
                .lookup_node(self.source, &mut resources)?
                .expect("record accessor source");
            let count = node.record().canonical().property_count();
            assert!(node.record().property_at(0, &mut resources).is_ok());
            assert!(node.record().property_at(count, &mut resources).is_err());
        }
        let source_label = String::from("Source");
        let scan_inputs = [PlanNodeId(0)];
        let expand_inputs = [PlanNodeId(1)];
        let expressions: [Expression<'_>; 0] = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &scan_inputs,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: Some(GraphName::new(&source_label).unwrap()),
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
        ];
        let columns = [
            GraphName::new("source").unwrap(),
            GraphName::new("target").unwrap(),
            GraphName::new("edge").unwrap(),
        ];
        Ok(run_native_result!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_inputs).unwrap(),
                RetainedRegion::slice(&expand_inputs).unwrap(),
                RetainedRegion::declared(source_label.as_ptr() as usize, source_label.capacity())
                    .unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_inputs).unwrap(),
                RetainedAllocation::array(&expand_inputs).unwrap(),
                RetainedAllocation::string(&source_label).unwrap(),
            ],
            &columns
        ))
    }
}

fn text(result: &super::super::CompletedGraphResult, span: Span) -> String {
    result.string(span).expect("owned result text").to_owned()
}

fn native_failure(
    result: Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
    message: &str,
) -> RuntimeFailure<NativeResultError> {
    match result {
        Err(failure) => failure,
        Ok(_) => panic!("{message}"),
    }
}

#[test]
fn native_result_records_properties_and_provenance() {
    let directory = tempfile::tempdir().expect("native record result store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(192 * 1024 * 1024),
        None,
    )
    .expect("create native record graph");
    let strings: [&str; 0] = [];
    let bools = [false, true];
    let integers: [i64; 0] = [];
    let floats = [f64::from_bits(0x7ff8_0000_0000_0042), -0.0];
    let mut labels = [
        GraphName::new("z-last").unwrap(),
        GraphName::new("Source").unwrap(),
    ];
    let mut properties = [
        GraphProperty::new(
            GraphName::new("z-string").unwrap(),
            PropertyValue::new(PropertyData::String("v\0é")).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("a-bool").unwrap(),
            PropertyValue::new(PropertyData::Bool(true)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("m-i64").unwrap(),
            PropertyValue::new(PropertyData::I64(i64::MIN)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("b-f64").unwrap(),
            PropertyValue::new(PropertyData::F64(-0.0)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("c-empty").unwrap(),
            PropertyValue::new(PropertyData::EmptyList { count: 0 }).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("d-strings").unwrap(),
            PropertyValue::new(PropertyData::Strings(&strings)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("e-bools").unwrap(),
            PropertyValue::new(PropertyData::Bools(&bools)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("f-ints").unwrap(),
            PropertyValue::new(PropertyData::Integers(&integers)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("g-floats").unwrap(),
            PropertyValue::new(PropertyData::Floats(&floats)).unwrap(),
        ),
    ];
    let source_image =
        CanonicalContents::node(&mut labels, &mut properties, None, None).expect("source image");
    let target_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("target image");
    let rel_properties = [GraphProperty::new(
        GraphName::new("edge\0weight").unwrap(),
        PropertyValue::new(PropertyData::I64(9)).unwrap(),
    )];
    let receipts = crate::property_graph::with_local_refs(|refs| {
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "n\0s", "").unwrap(),
                        revision: GraphRevision::new(7).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&source_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "records", "target").unwrap(),
                        revision: GraphRevision::new(4).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&target_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "", "r\0").unwrap(),
                        revision: GraphRevision::new(9).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).unwrap()),
                            target: NodeRef::Local(refs.node(1).unwrap()),
                            relationship_type: GraphName::new("Z_LINK").unwrap(),
                            properties: &rel_properties,
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("publish native records")
    });
    let source = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("source receipt"),
    };
    let target = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("target receipt"),
    };
    let edge = match receipts[2].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("edge receipt"),
    };
    let unrelated = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "records", "later").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&unrelated)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("later unrelated publication");

    let result = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            crate::property_graph::query::MAX_QUERY_BYTES,
            128,
            RecordsConsumer { source },
        )
        .expect("admit native records")
        .expect("materialize native records");
    let pools = result.pools();
    assert_eq!(result.metadata().rows, 1);
    assert_eq!(pools.nodes.len(), 2);
    assert_eq!(pools.relationships.len(), 1);
    let Value::Node(source_index) = *result.cell(0, 0).unwrap() else {
        panic!("source cell kind")
    };
    let Value::Node(target_index) = *result.cell(0, 1).unwrap() else {
        panic!("target cell kind")
    };
    let Value::Relationship(edge_index) = *result.cell(0, 2).unwrap() else {
        panic!("edge cell kind")
    };
    let source_record = pools.nodes[source_index as usize];
    let target_record = pools.nodes[target_index as usize];
    let edge_record = pools.relationships[edge_index as usize];
    assert_eq!((source_record.id, target_record.id), (source, target));
    assert_eq!(edge_record.id, edge);
    assert_eq!((edge_record.source, edge_record.target), (source, target));
    assert_eq!(source_record.revision, GraphRevision::new(7).unwrap());
    assert_eq!(edge_record.revision, GraphRevision::new(9).unwrap());
    assert_eq!(source_record.generation, receipts[0].generation);
    assert_eq!(edge_record.generation, receipts[2].generation);
    assert_eq!(source_record.text, None);
    assert_eq!(source_record.vector, None);
    let key = source_record.key.expect("source key");
    assert_eq!(text(&result, key.namespace), "n\0s");
    assert_eq!(text(&result, key.value), "");
    let edge_key = edge_record.key.expect("edge key");
    assert_eq!(text(&result, edge_key.namespace), "");
    assert_eq!(text(&result, edge_key.value), "r\0");
    assert_eq!(text(&result, edge_record.relationship_type), "Z_LINK");
    let label_names = pools.names[source_record.labels.start as usize
        ..(source_record.labels.start + source_record.labels.len) as usize]
        .iter()
        .map(|span| text(&result, *span))
        .collect::<Vec<_>>();
    assert_eq!(label_names, vec!["Source", "z-last"]);
    let properties = &pools.properties[source_record.properties.start as usize
        ..(source_record.properties.start + source_record.properties.len) as usize];
    let property_names = properties
        .iter()
        .map(|property| text(&result, property.name))
        .collect::<Vec<_>>();
    assert_eq!(
        property_names,
        vec![
            "a-bool",
            "b-f64",
            "c-empty",
            "d-strings",
            "e-bools",
            "f-ints",
            "g-floats",
            "m-i64",
            "z-string"
        ]
    );
    assert_eq!(
        pools.values[properties[0].value.0 as usize],
        Value::Bool(true)
    );
    assert_eq!(
        pools.values[properties[1].value.0 as usize],
        Value::F64((-0.0f64).to_bits())
    );
    assert!(matches!(
        pools.values[properties[2].value.0 as usize],
        Value::List {
            element: super::super::ListKind::Empty,
            children: Span { len: 0, .. }
        }
    ));
    assert!(matches!(
        pools.values[properties[3].value.0 as usize],
        Value::List {
            element: super::super::ListKind::String,
            children: Span { len: 0, .. }
        }
    ));
    assert_eq!(
        pools.values[properties[7].value.0 as usize],
        Value::I64(i64::MIN)
    );
    let Value::String(string) = pools.values[properties[8].value.0 as usize] else {
        panic!("stored string kind")
    };
    assert_eq!(text(&result, string), "v\0é");
    store.close().expect("close native record store");
}

struct ListsConsumer;

impl
    NativeReadConsumer<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
    > for ListsConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let text = String::from("query\0é");
        let scan_inputs = [PlanNodeId(0)];
        let expand_inputs = [PlanNodeId(1)];
        let project_inputs = [PlanNodeId(2)];
        let inner_items = [
            ExprId(0),
            ExprId(2),
            ExprId(3),
            ExprId(4),
            ExprId(5),
            ExprId(6),
            ExprId(7),
        ];
        let outer_items = [ExprId(8), ExprId(0), ExprId(2), ExprId(8)];
        let expressions = [
            Expression::Slot(SlotId(0)),
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(2)),
            Expression::Literal(Literal::Null),
            Expression::Literal(Literal::Bool(true)),
            Expression::Literal(Literal::I64(i64::MIN)),
            Expression::Literal(Literal::F64(f64::from_bits(0x7ff8_0000_0000_0042))),
            Expression::Literal(Literal::String(&text)),
            Expression::List(&inner_items),
            Expression::List(&outer_items),
        ];
        let projections = [
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
            Projection {
                slot: SlotId(13),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(14),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(15),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(16),
                expression: ExprId(4),
            },
            Projection {
                slot: SlotId(17),
                expression: ExprId(5),
            },
            Projection {
                slot: SlotId(18),
                expression: ExprId(6),
            },
            Projection {
                slot: SlotId(19),
                expression: ExprId(7),
            },
            Projection {
                slot: SlotId(20),
                expression: ExprId(9),
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
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
        ];
        let columns = [
            "source",
            "target",
            "edge",
            "source-again",
            "edge-again",
            "null",
            "bool",
            "integer",
            "float",
            "string",
            "nested",
        ]
        .map(|name| GraphName::new(name).unwrap());
        Ok(run_native_result!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_inputs).unwrap(),
                RetainedRegion::slice(&expand_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&inner_items).unwrap(),
                RetainedRegion::slice(&outer_items).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
                RetainedRegion::declared(text.as_ptr() as usize, text.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_inputs).unwrap(),
                RetainedAllocation::array(&expand_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&inner_items).unwrap(),
                RetainedAllocation::array(&outer_items).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
                RetainedAllocation::string(&text).unwrap(),
            ],
            &columns
        ))
    }
}

#[test]
fn native_result_lists_bits_and_entity_identity() {
    let directory = tempfile::tempdir().expect("native list result store");
    let high = 1_u128 << 80;
    let first_node = NodeId::new(high - 1).unwrap();
    let first_relationship = RelId::new((1_u128 << 96) + 7).unwrap();
    let store = Store::create_native_graph_with_allocator_seed_for_test(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        None,
        first_node,
        first_relationship,
    )
    .expect("create native list graph");
    store.close().unwrap();
    drop(store);
    let store = Store::open_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        None,
    )
    .expect("ordinary reopen of empty seeded native graph");
    {
        let lease = store.admit_native_read().unwrap();
        let bundle = lease.bundle();
        assert_eq!(bundle.high_waters().node, high - 2);
        assert_eq!(
            bundle.high_waters().relationship,
            first_relationship.get() - 1
        );
        assert_eq!(
            bundle.base().generation,
            crate::property_graph::GraphGeneration::new(0)
        );
        assert_eq!(bundle.sequence(), 0);
        assert!(bundle.roots().references().iter().all(Option::is_none));
    }
    let empty_a = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let empty_b = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let receipts = crate::property_graph::with_local_refs(|refs| {
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "lists", "source").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&empty_a)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "lists", "target").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&empty_b)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "lists", "edge")
                            .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).unwrap()),
                            target: NodeRef::Local(refs.node(1).unwrap()),
                            relationship_type: GraphName::new("CONNECTS").unwrap(),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap()
    });
    let source = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("source"),
    };
    let target = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("target"),
    };
    let edge = match receipts[2].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("edge"),
    };
    assert_eq!(source, first_node);
    assert_eq!(target, NodeId::new(high).unwrap());
    assert_eq!(edge, first_relationship);
    let result = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ListsConsumer,
        )
        .unwrap()
        .expect("materialize native lists");
    let pools = result.pools();
    assert_eq!(result.metadata().rows, 1);
    assert_eq!(pools.nodes.len(), 2);
    assert_eq!(pools.relationships.len(), 1);
    let mut expected_nodes = vec![source, target];
    expected_nodes.sort_unstable();
    assert_eq!(
        pools.nodes.iter().map(|node| node.id).collect::<Vec<_>>(),
        expected_nodes
    );
    assert_eq!(pools.relationships[0].id, edge);
    assert_eq!(pools.relationships[0].source, source);
    assert_eq!(pools.relationships[0].target, target);
    assert_eq!(result.cell(0, 0), result.cell(0, 3));
    assert_eq!(result.cell(0, 2), result.cell(0, 4));
    assert_eq!(result.cell(0, 5), Some(&Value::Null));
    assert_eq!(result.cell(0, 6), Some(&Value::Bool(true)));
    assert_eq!(result.cell(0, 7), Some(&Value::I64(i64::MIN)));
    assert_eq!(result.cell(0, 8), Some(&Value::F64(0x7ff8_0000_0000_0042)));
    let Value::String(string) = *result.cell(0, 9).unwrap() else {
        panic!("string")
    };
    assert_eq!(result.string(string), Some("query\0é"));
    let Value::List { children, element } = *result.cell(0, 10).unwrap() else {
        panic!("outer list")
    };
    assert_eq!(element, super::super::ListKind::Query);
    assert_eq!(children.len, 4);
    let outer = &pools.children[children.start as usize..(children.start + children.len) as usize];
    for repeated in [outer[0], outer[3]] {
        assert!(matches!(
            pools.values[repeated.0 as usize],
            Value::List {
                children: Span { len: 7, .. },
                element: super::super::ListKind::Query,
            }
        ));
    }
    let parent_index = pools.values.len() - 1;
    assert!(outer.iter().all(|index| (index.0 as usize) < parent_index));
    store.close().unwrap();
    drop(store);

    let reopened = Store::open_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        None,
    )
    .expect("reopen populated high-id native graph");
    {
        let lease = reopened.admit_native_read().unwrap();
        let bundle = lease.bundle();
        assert_eq!(bundle.high_waters().node, high);
        assert_eq!(bundle.high_waters().relationship, first_relationship.get());
        assert_eq!(bundle.base().generation, receipts[2].generation);
        assert_eq!(bundle.sequence(), 1);
        assert!(bundle.roots().references().iter().any(Option::is_some));
    }
    let next_image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let next = reopened
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "lists", "after-reopen").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&next_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap()[0];
    assert_eq!(next.entity, EntityId::Node(NodeId::new(high + 1).unwrap()));
    reopened.close().unwrap();
}

struct PublicationConsumer {
    store: Option<std::sync::Arc<Store>>,
    node: NodeId,
}

impl
    NativeReadConsumer<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
    > for PublicationConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_inputs = [PlanNodeId(0)];
        let expressions: [Expression<'_>; 0] = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.node,
                },
            },
        ];
        let columns = [GraphName::new("entity").unwrap()];
        let mut publication = self.store.take();
        let node = self.node;
        Ok(run_native_result!(
            view,
            runtime,
            operators,
            expressions,
            vec![RetainedRegion::slice(&lookup_inputs).unwrap()],
            vec![RetainedAllocation::array(&lookup_inputs).unwrap()],
            &columns,
            before_copy = move |stage, _| {
                if stage == NativeCompletionStage::BeforeStaging
                    && let Some(store) = publication.take()
                {
                    let mut properties = [GraphProperty::new(
                        GraphName::new("version").unwrap(),
                        PropertyValue::new(PropertyData::I64(2)).unwrap(),
                    )];
                    let image = CanonicalContents::node(&mut [], &mut properties, None, None)
                        .expect("updated publication image");
                    store
                        .apply_native_graph(
                            &[StructuredWrite {
                                key: ApplicationKey::new(EntityKind::Node, "same-view", "node")
                                    .unwrap(),
                                revision: GraphRevision::new(2).unwrap(),
                                operation: StructuredOperation::Put(EntityId::Node(node)),
                                image: Some(WriteImage::Node(&image)),
                            }],
                            &QueryControl::Cancel(CancelToken::new()),
                        )
                        .expect("publish between rows and native result copy");
                }
                Ok(())
            }
        ))
    }
}

struct OwnerCapture;
impl NativeReadConsumer<NativeOwnerFingerprint> for OwnerCapture {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        _: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<NativeOwnerFingerprint, crate::property_graph::storage::tree::directory::TreeError>
    {
        Ok(NativeOwnerFingerprint::capture(runtime))
    }
}

struct OwnerReject(NativeOwnerFingerprint);
impl NativeReadConsumer<Result<(), NativeResultError>> for OwnerReject {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        _: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<(), NativeResultError>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        Ok(self.0.validate(runtime))
    }
}

struct ControlledLookupConsumer<C> {
    node: NodeId,
    action: C,
    wait_for_close: bool,
}

impl<C>
    NativeReadConsumer<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
    > for ControlledLookupConsumer<C>
where
    C: FnMut(
        NativeCompletionStage,
        crate::property_graph::query::runtime::WorkCounters,
    ) -> Result<(), NativeResultError>,
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_inputs = [PlanNodeId(0)];
        let expressions: [Expression<'_>; 0] = [];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.node,
                },
            },
        ];
        let columns = [GraphName::new("entity").unwrap()];
        Ok(run_native_result!(
            view,
            runtime,
            operators,
            expressions,
            vec![RetainedRegion::slice(&lookup_inputs).unwrap()],
            vec![RetainedAllocation::array(&lookup_inputs).unwrap()],
            &columns,
            before_copy = |stage, counters| {
                (self.action)(stage, counters)?;
                if self.wait_for_close && stage == NativeCompletionStage::BeforeDestination {
                    view.wait_until_cancelled_for_test()
                        .map_err(NativeExecutionError::Tree)
                        .map_err(NativeResultError::Native)?;
                }
                Ok(())
            }
        ))
    }
}

struct ReleaseMeasured<C> {
    consumer: C,
    released: Arc<AtomicBool>,
}

impl<T, C: NativeReadConsumer<T>> NativeReadConsumer<T> for ReleaseMeasured<C> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<T, crate::property_graph::storage::tree::directory::TreeError> {
        let baseline = runtime.memory().reserved_bytes();
        let result = self.consumer.consume(view, runtime);
        self.released.store(
            runtime.memory().reserved_bytes() == baseline,
            Ordering::SeqCst,
        );
        result
    }
}

struct AdmissionProbe;

impl NativeReadConsumer<()> for AdmissionProbe {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        _: &GraphReadView<'s, 'lease, 'm, 'g>,
        _: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), crate::property_graph::storage::tree::directory::TreeError> {
        Ok(())
    }
}

struct ArithmeticFailureConsumer;

impl
    NativeReadConsumer<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
    > for ArithmeticFailureConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<super::super::CompletedGraphResult, RuntimeFailure<NativeResultError>>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let scan_inputs = [PlanNodeId(0)];
        let project_inputs = [PlanNodeId(1)];
        let projections = [Projection {
            slot: SlotId(10),
            expression: ExprId(2),
        }];
        let expressions = [
            Expression::Literal(Literal::I64(1)),
            Expression::Literal(Literal::I64(0)),
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Divide),
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
        let columns = [GraphName::new("quotient").unwrap()];
        Ok(run_native_result!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
            ],
            &columns
        ))
    }
}

#[test]
fn native_result_same_view_and_owner_rejection() {
    let directory = tempfile::tempdir().expect("native same-view result store");
    let store = std::sync::Arc::new(
        Store::create_native_graph(
            directory.path().join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(128 * 1024 * 1024),
            None,
        )
        .expect("create same-view graph"),
    );
    let mut initial_properties = [GraphProperty::new(
        GraphName::new("version").unwrap(),
        PropertyValue::new(PropertyData::I64(1)).unwrap(),
    )];
    let initial = CanonicalContents::node(&mut [], &mut initial_properties, None, None).unwrap();
    let receipt = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "same-view", "node").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&initial)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap()[0];
    let node = match receipt.entity {
        EntityId::Node(id) => id,
        _ => panic!("node"),
    };
    let old = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PublicationConsumer {
                store: Some(std::sync::Arc::clone(&store)),
                node,
            },
        )
        .unwrap()
        .expect("old admission materialization");
    let fresh = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            PublicationConsumer { store: None, node },
        )
        .unwrap()
        .expect("fresh admission materialization");
    let old_node = old.pools().nodes[0];
    let fresh_node = fresh.pools().nodes[0];
    assert_eq!(old_node.revision, GraphRevision::new(1).unwrap());
    assert_eq!(old_node.generation, receipt.generation);
    assert_eq!(fresh_node.revision, GraphRevision::new(2).unwrap());
    assert!(fresh_node.generation > old_node.generation);
    let old_property = old.pools().properties[old_node.properties.start as usize];
    let fresh_property = fresh.pools().properties[fresh_node.properties.start as usize];
    assert_eq!(
        old.pools().values[old_property.value.0 as usize],
        Value::I64(1)
    );
    assert_eq!(
        fresh.pools().values[fresh_property.value.0 as usize],
        Value::I64(2)
    );

    let first_owner = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            1024 * 1024,
            8,
            OwnerCapture,
        )
        .unwrap();
    let rejected = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            1024 * 1024,
            8,
            OwnerReject(first_owner),
        )
        .unwrap();
    assert!(matches!(
        rejected,
        Err(NativeResultError::Completed(CompletedError::Source(
            super::super::SourceError::ForeignView
        )))
    ));
}

#[test]
fn native_result_limits_and_no_partial_output() {
    let directory = tempfile::tempdir().expect("native result limit store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    let node_image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "limits", "small").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ActualRowsConsumer,
        )
        .unwrap()
        .expect("measure native result limits");
    let clean_copied = clean.metadata().counters.get(WorkKind::CopiedBytes);
    let clean_completed = clean.metadata().counters.get(WorkKind::CompletedBytes);
    let clean_peak = clean.metadata().peak_query_bytes;
    assert!(clean_copied > 1);
    assert!(clean_completed > 1);
    assert!(clean_peak > 1024);

    let memory_failure = native_failure(
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                clean_peak - 1,
                64,
                ActualRowsConsumer,
            )
            .expect("admit measured memory-limited result"),
        "destination overlap must exceed measured memory limit",
    );
    assert!(matches!(
        memory_failure.error,
        NativeResultError::Native(NativeExecutionError::Runtime(RuntimeError::Memory(_)))
            | NativeResultError::Native(NativeExecutionError::Tree(
                crate::property_graph::storage::tree::directory::TreeError::Runtime(
                    RuntimeError::Memory(_)
                )
            ))
            | NativeResultError::Completed(CompletedError::Runtime(RuntimeError::Memory(_)))
    ));

    let copied_limits = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, clean_copied - 1)
        .unwrap();
    let copied_failure = native_failure(
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                copied_limits,
                16 * 1024 * 1024,
                64,
                ActualRowsConsumer,
            )
            .unwrap(),
        "actual copied bytes must fire",
    );
    assert!(matches!(
        copied_failure.error,
        NativeResultError::Native(NativeExecutionError::Runtime(RuntimeError::Limit(
            WorkKind::CopiedBytes
        ))) | NativeResultError::Completed(CompletedError::Runtime(RuntimeError::Limit(
            WorkKind::CopiedBytes
        )))
    ));
    assert!(copied_failure.counters.get(WorkKind::CopiedBytes) > 0);

    let completed_limits = RuntimeLimits::default()
        .with_limit(WorkKind::CompletedBytes, clean_completed - 1)
        .unwrap();
    let completed_failure = native_failure(
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                completed_limits,
                16 * 1024 * 1024,
                64,
                ActualRowsConsumer,
            )
            .unwrap(),
        "driver completed bytes must fire",
    );
    assert!(matches!(
        completed_failure.error,
        NativeResultError::Native(NativeExecutionError::Runtime(RuntimeError::Limit(
            WorkKind::CompletedBytes
        )))
    ));
    assert_eq!(completed_failure.counters.get(WorkKind::CompletedBytes), 0);

    let large_text = "x".repeat(2 * 1024 * 1024 + 4096);
    let mut large_properties = [GraphProperty::new(
        GraphName::new("payload").unwrap(),
        PropertyValue::new(PropertyData::String(&large_text)).unwrap(),
    )];
    let large_image = CanonicalContents::node(&mut [], &mut large_properties, None, None).unwrap();
    for key in ["large-a", "large-b"] {
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "limits", key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&large_image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap();
    }
    let core_failure = native_failure(
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                crate::property_graph::query::MAX_QUERY_BYTES,
                64,
                ActualRowsConsumer,
            )
            .unwrap(),
        "represented core cap must reject large native property",
    );
    assert!(matches!(
        core_failure.error,
        NativeResultError::Completed(CompletedError::Limit)
    ));
}

#[test]
fn native_result_controls_and_late_errors_release() {
    let directory = tempfile::tempdir().expect("native result control store");
    let store = Arc::new(
        Store::create_native_graph(
            directory.path().join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(128 * 1024 * 1024),
            None,
        )
        .expect("create native result control graph"),
    );
    let node_image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let receipt = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "controls", "node").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap()[0];
    let node = match receipt.entity {
        EntityId::Node(id) => id,
        _ => panic!("control fixture node"),
    };

    let clean_release = Arc::new(AtomicBool::new(false));
    let clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseMeasured {
                consumer: ControlledLookupConsumer {
                    node,
                    action: |_, _| Ok(()),
                    wait_for_close: false,
                },
                released: Arc::clone(&clean_release),
            },
        )
        .unwrap()
        .expect("paired clean native result");
    assert_eq!(clean.metadata().rows, 1);
    assert!(clean_release.load(Ordering::SeqCst));

    let cancel = CancelToken::new();
    let cancel_action = cancel.clone();
    let cancel_fired = Arc::new(AtomicBool::new(false));
    let cancel_observed = Arc::clone(&cancel_fired);
    let cancel_copied = Arc::new(AtomicU64::new(0));
    let cancel_work = Arc::clone(&cancel_copied);
    let cancel_release = Arc::new(AtomicBool::new(false));
    let cancel_result = store.with_native_read(
        &QueryControl::Cancel(cancel),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        ReleaseMeasured {
            consumer: ControlledLookupConsumer {
                node,
                action:
                    move |stage, counters: crate::property_graph::query::runtime::WorkCounters| {
                        if stage == NativeCompletionStage::BeforeDestination {
                            cancel_work
                                .store(counters.get(WorkKind::CopiedBytes), Ordering::SeqCst);
                            cancel_observed.store(true, Ordering::SeqCst);
                            cancel_action.cancel();
                        }
                        Ok(())
                    },
                wait_for_close: false,
            },
            released: Arc::clone(&cancel_release),
        },
    );
    assert!(matches!(
        cancel_result,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(crate::property_graph::query::QueryError::Cancelled)
            )
        ))
    ));
    assert!(cancel_fired.load(Ordering::SeqCst));
    assert!(cancel_copied.load(Ordering::SeqCst) > 0);
    assert!(cancel_release.load(Ordering::SeqCst));

    let clock = Arc::new(ManualMonotonicClock::new());
    let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone()).unwrap();
    let timeout_fired = Arc::new(AtomicBool::new(false));
    let timeout_observed = Arc::clone(&timeout_fired);
    let timeout_copied = Arc::new(AtomicU64::new(0));
    let timeout_work = Arc::clone(&timeout_copied);
    let timeout_release = Arc::new(AtomicBool::new(false));
    let timeout_result = store.with_native_read(
        &QueryControl::Deadline(deadline),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        ReleaseMeasured {
            consumer: ControlledLookupConsumer {
                node,
                action:
                    move |stage, counters: crate::property_graph::query::runtime::WorkCounters| {
                        if stage == NativeCompletionStage::BeforeDestination {
                            timeout_work
                                .store(counters.get(WorkKind::CopiedBytes), Ordering::SeqCst);
                            timeout_observed.store(true, Ordering::SeqCst);
                            clock.advance(Duration::from_secs(2));
                        }
                        Ok(())
                    },
                wait_for_close: false,
            },
            released: Arc::clone(&timeout_release),
        },
    );
    assert!(matches!(
        timeout_result,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(crate::property_graph::query::QueryError::Timeout)
            )
        ))
    ));
    assert!(timeout_fired.load(Ordering::SeqCst));
    assert!(timeout_copied.load(Ordering::SeqCst) > 0);
    assert!(timeout_release.load(Ordering::SeqCst));

    let close_directory = tempfile::tempdir().expect("native result close control store");
    let close_store = Arc::new(
        Store::create_native_graph(
            close_directory.path().join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_reader_drain_timeout(Duration::ZERO)
                .with_max_resident_bytes(128 * 1024 * 1024),
            None,
        )
        .unwrap(),
    );
    let close_receipt = close_store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "controls", "close").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap()[0];
    let close_node = match close_receipt.entity {
        EntityId::Node(id) => id,
        _ => panic!("close fixture node"),
    };
    let close_action_store = Arc::clone(&close_store);
    let close_probe_store = Arc::clone(&close_store);
    let closing_observed = Arc::new(AtomicBool::new(false));
    let closing_receipt = Arc::clone(&closing_observed);
    let close_copied = Arc::new(AtomicU64::new(0));
    let close_work = Arc::clone(&close_copied);
    let close_release = Arc::new(AtomicBool::new(false));
    let (closed_tx, closed_rx) = std::sync::mpsc::sync_channel(1);
    let close_result = close_store.with_native_read(
        &QueryControl::Cancel(CancelToken::new()),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        64,
        ReleaseMeasured {
            consumer: ControlledLookupConsumer {
                node: close_node,
                action:
                    move |stage, counters: crate::property_graph::query::runtime::WorkCounters| {
                        if stage != NativeCompletionStage::BeforeDestination {
                            return Ok(());
                        }
                        close_work.store(counters.get(WorkKind::CopiedBytes), Ordering::SeqCst);
                        let store = Arc::clone(&close_action_store);
                        let sender = closed_tx.clone();
                        std::thread::spawn(move || {
                            let _ = sender.send(store.close());
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
                                closing_receipt.store(true, Ordering::SeqCst);
                                return Ok(());
                            }
                            Err(crate::lifecycle::native_graph::NativeGraphError::Read(
                                crate::property_graph::storage::tree::directory::TreeError::Runtime(
                                    RuntimeError::Value(
                                        crate::property_graph::query::QueryError::ReadCancelled,
                                    ),
                                ),
                            ))
                            | Ok(()) => std::thread::yield_now(),
                            result => panic!("unexpected close probe: {result:?}"),
                        }
                        }
                        panic!("native result store did not enter closing state")
                    },
                wait_for_close: true,
            },
            released: Arc::clone(&close_release),
        },
    );
    let close_is_read_cancelled = match close_result {
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Runtime(
                RuntimeError::Value(crate::property_graph::query::QueryError::ReadCancelled),
            ),
        )) => true,
        Ok(Err(failure)) => matches!(
            failure.error,
            NativeResultError::Native(NativeExecutionError::Runtime(RuntimeError::Value(
                crate::property_graph::query::QueryError::ReadCancelled
            ))) | NativeResultError::Native(NativeExecutionError::Tree(
                crate::property_graph::storage::tree::directory::TreeError::Runtime(
                    RuntimeError::Value(crate::property_graph::query::QueryError::ReadCancelled)
                )
            )) | NativeResultError::Completed(CompletedError::Runtime(RuntimeError::Value(
                crate::property_graph::query::QueryError::ReadCancelled
            )))
        ),
        Ok(Ok(_)) => panic!("close unexpectedly returned a completed result"),
        Err(error) => panic!("unexpected outer close error: {error:?}"),
    };
    assert!(close_is_read_cancelled);
    assert!(closing_observed.load(Ordering::SeqCst));
    assert!(close_copied.load(Ordering::SeqCst) > 0);
    assert!(close_release.load(Ordering::SeqCst));
    closed_rx.recv().unwrap().unwrap();

    let arithmetic_release = Arc::new(AtomicBool::new(false));
    let arithmetic_failure = native_failure(
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                ReleaseMeasured {
                    consumer: ArithmeticFailureConsumer,
                    released: Arc::clone(&arithmetic_release),
                },
            )
            .unwrap(),
        "actual native arithmetic must fail before completion",
    );
    assert_eq!(arithmetic_failure.operator, PlanNodeId(2));
    assert!(matches!(
        arithmetic_failure.error,
        NativeResultError::Native(NativeExecutionError::Expression(ExpressionError {
            expression: ExprId(2),
            failure: ExpressionFailure::Runtime(RuntimeError::Value(
                crate::property_graph::query::QueryError::DivisionByZero
            )),
        }))
    ));
    assert!(arithmetic_failure.counters.get(WorkKind::Scans) > 0);
    assert_eq!(arithmetic_failure.counters.get(WorkKind::CompletedRows), 0);
    assert!(arithmetic_release.load(Ordering::SeqCst));

    let storage_directory = tempfile::tempdir().expect("native result storage-error store");
    let fault_vfs = Arc::new(NativeResultMapVfs::default());
    let mut entropy = OsEntropy;
    let storage_store = Store::create_native_graph_with_infrastructure(
        storage_directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        None,
        fault_vfs.clone(),
        Arc::new(SystemMonotonicClock),
        &mut entropy,
    )
    .unwrap();
    for key in ["a", "b", "c"] {
        storage_store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "late-storage", key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&node_image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap();
    }
    let storage_work = Arc::new(AtomicU64::new(0));
    let observed_storage_work = Arc::clone(&storage_work);
    let arm_vfs = Arc::clone(&fault_vfs);
    let armed = Arc::new(AtomicBool::new(false));
    let arm_once = Arc::clone(&armed);
    let storage_release = Arc::new(AtomicBool::new(false));
    let storage_failure = native_failure(
        storage_store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                ReleaseMeasured {
                    consumer: super::test_support::materialize_with_source_observer(move |rows| {
                        if rows != 0 && !arm_once.swap(true, Ordering::SeqCst) {
                            observed_storage_work.store(rows as u64, Ordering::SeqCst);
                            arm_vfs.arm_next();
                        }
                        Ok(())
                    }),
                    released: Arc::clone(&storage_release),
                },
            )
            .unwrap(),
        "later real native entity read must preserve its storage error",
    );
    assert!(matches!(
        storage_failure.error,
        NativeResultError::Native(NativeExecutionError::Tree(
            crate::property_graph::storage::tree::directory::TreeError::Io(ref error)
        )) if error.kind() == std::io::ErrorKind::Other
    ));
    assert!(storage_work.load(Ordering::SeqCst) > 0);
    assert!(storage_failure.counters.get(WorkKind::RowsOut) > 0);
    assert_eq!(fault_vfs.fires.load(Ordering::SeqCst), 1);
    assert!(storage_release.load(Ordering::SeqCst));

    fault_vfs.disarm();
    let clean_storage_release = Arc::new(AtomicBool::new(false));
    let clean_storage = storage_store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            ReleaseMeasured {
                consumer: super::test_support::materialize_with_source_observer(|_| Ok(())),
                released: Arc::clone(&clean_storage_release),
            },
        )
        .unwrap()
        .expect("paired clean native entity read");
    assert_eq!(clean_storage.metadata().rows, 3);
    assert!(clean_storage_release.load(Ordering::SeqCst));
    storage_store.close().unwrap();
    store.close().unwrap();
}

fn completed_represented_bytes(result: &super::super::CompletedGraphResult) -> usize {
    let pools = result.pools();
    [
        size_of::<super::super::CompletedGraphResult>(),
        std::mem::size_of_val(pools.values),
        std::mem::size_of_val(pools.bytes),
        std::mem::size_of_val(pools.columns),
        std::mem::size_of_val(pools.cells),
        std::mem::size_of_val(pools.children),
        std::mem::size_of_val(pools.names),
        std::mem::size_of_val(pools.properties),
        std::mem::size_of_val(pools.nodes),
        std::mem::size_of_val(pools.relationships),
        std::mem::size_of_val(pools.vectors),
        std::mem::size_of_val(pools.reports),
        std::mem::size_of_val(pools.receipts),
    ]
    .into_iter()
    .sum()
}

#[test]
fn native_result_final_counters_and_owned_copy() {
    let directory = tempfile::tempdir().expect("native result counters store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(128 * 1024 * 1024),
        None,
    )
    .unwrap();
    let node_image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "counters", "node").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();

    let mut expected_counters = None;
    for _ in 0..8 {
        let released = Arc::new(AtomicBool::new(false));
        let result = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                ReleaseMeasured {
                    consumer: ActualRowsConsumer,
                    released: Arc::clone(&released),
                },
            )
            .unwrap()
            .expect("repeat actual native result");
        let metadata = *result.metadata();
        let represented = completed_represented_bytes(&result);
        assert_eq!(metadata.rows, 1);
        assert_eq!(metadata.counters.get(WorkKind::CompletedRows), 1);
        assert_eq!(
            metadata.counters.get(WorkKind::CompletedBytes),
            represented as u64
        );
        assert_eq!(metadata.counters.get(WorkKind::CompletedAbiBytes), 0);
        assert!(metadata.counters.get(WorkKind::CopiedBytes) > represented as u64);
        assert!(metadata.counters.get(WorkKind::Scans) > 0);
        assert!(metadata.peak_query_bytes > represented);
        assert!(released.load(Ordering::SeqCst));
        if let Some(expected) = expected_counters {
            assert_eq!(metadata.counters, expected);
        } else {
            expected_counters = Some(metadata.counters);
        }
        drop(result);
    }
    store.close().unwrap();
}

#[test]
fn native_result_directed_probe_can_fire() {
    const RECEIPTS: [&str; 8] = [
        "copy",
        "identity",
        "same-view",
        "limit.fire",
        "control.fire",
        "late-error.fire",
        "release",
        "oracle.can-fire",
    ];
    let report = super::test_support::run_actual_probe(0x1565_eed)
        .expect("actual native result directed probe");
    assert_eq!(report.observations, report.expected);
    assert_eq!(report.receipts.len(), RECEIPTS.len());
    for (expected, (actual, count)) in RECEIPTS.into_iter().zip(&report.receipts) {
        assert_eq!(*actual, expected);
        assert!(*count > 0, "receipt did not fire: {actual}");
    }
    let mut perturbed = report.observations.clone();
    perturbed[0].1 ^= 1;
    assert_ne!(perturbed, report.expected);
    let paired = super::test_support::run_actual_probe(0x1565_eed)
        .expect("paired actual native result directed probe");
    assert_eq!(paired, report);
}
