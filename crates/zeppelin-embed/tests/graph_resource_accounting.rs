#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::query::completed::*;
use zeppelin_embed::property_graph::query::resources::QueryMemory;
use zeppelin_embed::property_graph::query::runtime::{
    RetainedView, RuntimeContext, RuntimeLimits, WorkKind,
};
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{GraphGeneration, NodeId, StoreInstanceId};

use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use zeppelin_embed::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphPlanBacking,
    GraphProperty, GraphQueryPlan, GraphRevision, GraphStore, GraphStoreError, PropertyData,
    PropertyValue,
};
struct View(QueryView);
impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.0
    }
    fn check_active(&self) -> Result<(), QueryError> {
        Ok(())
    }
}
fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn store_options() -> OpenOptions {
    OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024)
}

/// A graph store with three nodes whose `p` values are `base + 1`,
/// `base + 2` and `base + 3`, in node-ID order.
struct Fixture {
    _directory: tempfile::TempDir,
    store: GraphStore,
}

impl Fixture {
    fn create(base: i64) -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = GraphStore::create(directory.path().join("native"), store_options(), None)
            .expect("graph store");
        let values = [base + 1, base + 2, base + 3];
        let p = GraphName::new("p").expect("property name");
        let mut properties = values.map(|value| {
            let value = PropertyValue::new(PropertyData::I64(value)).expect("property value");
            [GraphProperty::new(p, value)]
        });
        let mut images = Vec::new();
        for property in &mut properties {
            images
                .push(CanonicalContents::node(&mut [], property, None, None).expect("node image"));
        }
        let keys = ["one", "two", "three"];
        let mut requests = Vec::new();
        for (key, image) in keys.iter().zip(&images) {
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze66-s2", key).expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(image)),
            });
        }
        let result = store
            .apply_batch(&requests, &control())
            .expect("fixture batch");
        let mut nodes = Vec::new();
        for receipt in result.receipts() {
            match receipt.entity {
                EntityId::Node(node) => nodes.push(node),
                EntityId::Relationship(_) => panic!("fixture receipt names a relationship"),
            }
        }
        let _: [NodeId; 3] = nodes.try_into().expect("three fixture receipts");
        Self {
            _directory: directory,
            store,
        }
    }
}

/// `MATCH (n) RETURN n, n.p`, through `GraphStore::query`.
fn read_p_with_options(
    store: &GraphStore,
    control: &QueryControl,
    options: &GraphQueryOptions,
) -> Result<CompletedGraphResult, Box<GraphStoreError>> {
    let p = String::from("p");
    let name = GraphName::new(&p).expect("name");
    let unit = vec![PlanNodeId(0)];
    let scan = vec![PlanNodeId(1)];
    let projections = vec![Projection {
        slot: SlotId(11),
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
    let eager: Vec<PlanNodeId> = Vec::new();
    let parameters: Vec<Parameter<'_>> = Vec::new();
    let mut backing = GraphPlanBacking::default();
    backing.string(&p).expect("backing string");
    backing.vec(&unit).expect("backing unit");
    backing.vec(&scan).expect("backing scan");
    backing.vec(&projections).expect("backing projections");
    let plan = GraphQueryPlan {
        operators: &operators,
        expressions: &expressions,
        parameters: &parameters,
        eager_searches: &eager,
        root: PlanNodeId(2),
        backing: &backing,
        bindings: &[],
        columns: &["p"],
    };
    store.query(control, options, &plan).map_err(Box::new)
}

#[test]
fn ze76_public_work_matches_independent_deltas() {
    let fixture = Fixture::create(10);
    let result =
        read_p_with_options(&fixture.store, &control(), &GraphQueryOptions::default()).unwrap();
    assert_eq!(result.metadata().rows, 3);
    for (row, expected) in [11, 12, 13].into_iter().enumerate() {
        assert_eq!(result.cell(row, 0), Some(&Value::I64(expected)));
    }
    // Exactly one indexed property selection per input node. Native I64
    // encoding is one tag byte plus eight scalar bytes.
    assert_eq!(result.metadata().counters.get(WorkKind::PropertyValues), 3);
    assert_eq!(result.metadata().counters.get(WorkKind::PropertyBytes), 27);
}

#[test]
fn ze76_oversize_and_control_failures_release_without_partial_effects() {
    let fixture = Fixture::create(10);
    let resources = fixture.store.resources().unwrap();
    let baseline = resources.reserved_bytes().unwrap();
    let options = GraphQueryOptions::default()
        .with_result_row_limit(1)
        .unwrap();
    assert!(read_p_with_options(&fixture.store, &control(), &options).is_err());
    let token = CancelToken::new();
    token.cancel();
    assert!(
        read_p_with_options(
            &fixture.store,
            &QueryControl::Cancel(token),
            &GraphQueryOptions::default()
        )
        .is_err()
    );
    let disk = || {
        let mut files: Vec<_> = std::fs::read_dir(fixture._directory.path().join("native"))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), entry.metadata().unwrap().len())
            })
            .collect();
        files.sort();
        files
    };
    let before = disk();
    let text = "x".repeat(5 << 20);
    let first = CanonicalContents::node(&mut [], &mut [], Some(&text), None).unwrap();
    let second = CanonicalContents::node(&mut [], &mut [], Some(&text), None).unwrap();
    let writes =
        [(&first, "oversize-a"), (&second, "oversize-b")].map(|(image, key)| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "ze76", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(image)),
        });
    let error = fixture.store.apply_batch(&writes, &control()).unwrap_err();
    assert!(error.nothing_committed());
    assert_eq!(disk(), before);
    assert_eq!(resources.reserved_bytes().unwrap(), baseline);
    let result =
        read_p_with_options(&fixture.store, &control(), &GraphQueryOptions::default()).unwrap();
    // Unified creation publishes 1; the fixture's single batch publishes 2.
    assert_eq!(result.metadata().generation.get(), 2);
    assert_eq!(result.metadata().rows, 3);
    assert_eq!(result.cell(0, 0), Some(&Value::I64(11)));
}

#[test]
fn ze76_eligibility_distinguishes_examined_and_unique_ids() {
    use zeppelin_embed::property_graph::NodeId;
    use zeppelin_embed::property_graph::query::eligibility::EligibleNodeSet;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        OpenOptions::new().with_max_resident_bytes(8 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 1024 * 1024).unwrap();
    let view = View(QueryView::new(
        StoreInstanceId::new(9).unwrap(),
        GraphGeneration::new(0),
    ));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
    let a = view.0.node(NodeId::new(1).unwrap());
    let b = view.0.node(NodeId::new(2).unwrap());
    let set = EligibleNodeSet::build(&mut context, 2, [a, b, a, b]).unwrap();
    assert_eq!(set.ids_for(&view.0).unwrap().len(), 2);
    assert_eq!(context.counters().get(WorkKind::EligibilityEntries), 4);
    assert_eq!(
        context.counters().get(WorkKind::EligibilityUniqueEntries),
        2
    );
}

#[test]
fn ze76_public_replay_counts_canonical_byte_pairs() {
    use zeppelin_embed::property_graph::staging::{
        StructuredOperation, StructuredWrite, WriteImage,
    };
    use zeppelin_embed::property_graph::{
        ApplicationKey, CanonicalContents, EntityKind, GraphRevision, GraphStore,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = GraphStore::create(
        dir.path().join("graph"),
        OpenOptions::new().with_max_resident_bytes(16 * 1024 * 1024),
        None,
    )
    .unwrap();
    let resources = store.resources().unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let requests = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "ze76", "a").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    drop(
        store
            .apply_batch(&requests, &QueryControl::Cancel(CancelToken::new()))
            .unwrap(),
    );
    let before = resources.work_ledger().unwrap();
    drop(
        store
            .apply_batch(&requests, &QueryControl::Cancel(CancelToken::new()))
            .unwrap(),
    );
    let after = resources.work_ledger().unwrap();
    // ZGCI(4), version(2), node tag(1), labels/properties counts(8 each),
    // absent text(1), absent embedding(1): 25 original canonical bytes.
    assert_eq!(
        after.canonical_comparison_bytes - before.canonical_comparison_bytes,
        25
    );
    assert_eq!(
        after.canonical_encoding_bytes - before.canonical_encoding_bytes,
        25
    );
    assert_eq!(after.wal_appends - before.wal_appends, 0);
}
