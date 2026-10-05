//! Directed real-search composition receipts shared by libtests and the runner.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "cfg-only directed proof fixtures fail at the violated contract"
)]
use super::entry_probe::{Backing, control, options, run_plan};
use super::search_adapter::NativeSearchAdapter;
use super::*;
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{OpenOptions, Store};
use crate::property_graph::query::completed::{
    ActualTier, CandidateCoverage, CompletedGraphResult, ScorePrecision, SearchReport, Value,
};
use crate::property_graph::query::pattern::{SearchHit, SearchInvocation};
use crate::property_graph::query::plan::{
    AggregateExpression, Direction, ExprId, Expression, Literal, Operator, PatternId, Projection,
    SearchCallId, SearchMode, SearchOutputs, SearchRequest, SlotId,
};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphDeleteMode,
    GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue, RelId,
};

struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("ze64-search-{}-{serial:020}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze64-probe".into(),
        model_version: "1".into(),
        weights_digest: vec![64],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}
fn key(kind: EntityKind, name: &str) -> ApplicationKey<'_> {
    ApplicationKey::new(kind, "ze64-probe", name).unwrap()
}
fn node_write(
    store: &Store,
    name: &str,
    point: Option<[f32; 2]>,
    p: i64,
    replace: Option<NodeId>,
) -> NodeId {
    let document = tower();
    let mut props = [GraphProperty::new(
        GraphName::new("p").unwrap(),
        PropertyValue::new(PropertyData::I64(p)).unwrap(),
    )];
    let image = CanonicalContents::node(
        &mut [],
        &mut props,
        Some("amber"),
        point
            .as_ref()
            .map(|p| CanonicalEmbedding::new(&document, p).unwrap()),
    )
    .unwrap();
    let result = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Node, name),
                revision: GraphRevision::new(if replace.is_some() { 2 } else { 1 }).unwrap(),
                operation: replace.map_or(StructuredOperation::Create, |n| {
                    StructuredOperation::Put(EntityId::Node(n))
                }),
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .unwrap();
    let EntityId::Node(n) = result[0].entity else {
        panic!("node receipt")
    };
    n
}
fn edge(store: &Store, name: &str, source: NodeId, target: NodeId) -> RelId {
    let result = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Relationship, name),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Existing(source),
                    target: NodeRef::Existing(target),
                    relationship_type: GraphName::new("R").unwrap(),
                    properties: &[],
                }),
            }],
            &control(),
        )
        .unwrap();
    let EntityId::Relationship(r) = result[0].entity else {
        panic!("edge receipt")
    };
    r
}

struct Fixture {
    store: Store,
    directory: Directory,
    seed: NodeId,
    active: NodeId,
    neighbor: NodeId,
    old_edge: RelId,
}
impl Fixture {
    fn new(value: i64) -> Self {
        let directory = Directory::new();
        let store = Store::create_native_graph(
            directory.path().join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            Some(tower()),
        )
        .unwrap();
        let seed = node_write(&store, "seed", Some([0.0, 0.0]), value, None);
        let neighbor = node_write(&store, "neighbor", None, value, None);
        let old_edge = edge(&store, "old", seed, neighbor);
        store.checkpoint_native_graph(&control()).unwrap();
        let admission = store.admit_native_graph_maintenance().unwrap();
        store
            .commit_native_graph_maintenance(&admission, &control())
            .unwrap();
        drop(admission);
        let active = node_write(&store, "active", Some([1.0, 1.0]), value, None);
        edge(&store, "active-edge", active, neighbor);
        Self {
            store,
            directory,
            seed,
            active,
            neighbor,
            old_edge,
        }
    }
    fn publish(&self, value: i64) {
        node_write(
            &self.store,
            "seed",
            Some([10.0, 10.0]),
            value,
            Some(self.seed),
        );
        node_write(&self.store, "neighbor", None, value, Some(self.neighbor));
        self.store
            .apply_native_graph(
                &[StructuredWrite {
                    key: key(EntityKind::Relationship, "old"),
                    revision: GraphRevision::new(2).unwrap(),
                    operation: StructuredOperation::Delete(
                        EntityId::Relationship(self.old_edge),
                        GraphDeleteMode::Restrict,
                    ),
                    image: None,
                }],
                &control(),
            )
            .unwrap();
    }
}

/// A wrapper retains the producer report and commits between seed ranking and
/// graph pulling. It never substitutes a scripted hit or report.
struct Observed<'a> {
    adapter: NativeSearchAdapter<'a>,
    fixture: &'a Fixture,
    publish: Option<i64>,
    report: Option<SearchReport>,
}
impl<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g> for Observed<'_> {
    fn search<'s>(
        &mut self,
        view: &'s GraphReadView<'s, 'v, 'm, 'g>,
        invocation: &SearchInvocation<'_, '_, 'v, 'm, 'g>,
        hits: &mut QueryArena<'m, 'g, SearchHit>,
        runtime: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<SearchReport, NativeExecutionError> {
        let report = self.adapter.search(view, invocation, hits, runtime)?;
        self.report = Some(report);
        if let Some(value) = self.publish.take() {
            std::thread::scope(|scope| scope.spawn(|| self.fixture.publish(value)).join()).unwrap();
        }

        Ok(report)
    }
}

fn query<S: for<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g>>(
    store: &Store,
    adapter: &mut S,
    mode: SearchMode,
    aggregate: bool,
) -> Result<CompletedGraphResult, GraphQueryError> {
    store.execute_graph_query(
        &control(),
        &options(16),
        Some(adapter),
        |runtime, executor| {
            let inputs: Vec<Vec<_>> = (0..4).map(|i| vec![PlanNodeId(i)]).collect();
            let vector = vec![ExprId(0), ExprId(1)];
            let property = String::from("p");
            let mut expressions = vec![
                Expression::Literal(Literal::F64(0.0)),
                Expression::Literal(Literal::F64(0.0)),
                Expression::List(&vector),
                Expression::Literal(Literal::I64(2)),
            ];
            let mut project = Vec::new();
            let mut aggregates = Vec::new();
            let mut operators = vec![
                Operator {
                    inputs: &[],
                    kind: OperatorKind::Unit,
                },
                Operator {
                    inputs: &inputs[0],
                    kind: OperatorKind::Search {
                        call: SearchCallId(0),
                        request: SearchRequest::Vector {
                            vector: ExprId(2),
                            k: ExprId(3),
                            mode,
                            eligible: None,
                            options: Default::default(),
                        },
                        outputs: SearchOutputs {
                            node: Some(SlotId(0)),
                            distance: Some(SlotId(1)),
                            ..SearchOutputs::default()
                        },
                    },
                },
                Operator {
                    inputs: &inputs[1],
                    kind: OperatorKind::Expand {
                        source: SlotId(0),
                        node: SlotId(2),
                        relationship: SlotId(3),
                        direction: Direction::Outgoing,
                        relationship_types: &[],
                        pattern: PatternId(0),
                    },
                },
            ];
            if aggregate {
                expressions.push(Expression::Aggregate {
                    operation: AggregateExpression::Count { distinct: false },
                    operand: None,
                });
                aggregates.push(Projection {
                    slot: SlotId(10),
                    expression: ExprId(4),
                });
                operators.push(Operator {
                    inputs: &inputs[2],
                    kind: OperatorKind::Aggregate {
                        keys: &[],
                        aggregates: &aggregates,
                    },
                });
            } else {
                for slot in [0, 2, 1] {
                    let e = ExprId(expressions.len() as u32);
                    expressions.push(Expression::Slot(SlotId(slot)));
                    project.push(Projection {
                        slot: SlotId(10 + project.len() as u32),
                        expression: e,
                    });
                }
                expressions.push(Expression::Property {
                    entity: ExprId(5),
                    name: GraphName::new(&property).unwrap(),
                });
                project.push(Projection {
                    slot: SlotId(13),
                    expression: ExprId(7),
                });
                operators.push(Operator {
                    inputs: &inputs[2],
                    kind: OperatorKind::Project(&project),
                });
            }
            let eager = vec![PlanNodeId(1)];
            let mut backing = Backing::default();
            for input in &inputs {
                backing.vec(input)?;
            }
            backing.vec(&vector)?;
            backing.vec(&project)?;
            backing.vec(&aggregates)?;
            backing.string(&property)?;
            run_plan(
                runtime,
                executor,
                &operators,
                &expressions,
                &eager,
                &backing,
                if aggregate {
                    &["count"]
                } else {
                    &["seed", "m", "distance", "p"]
                },
            )
        },
    )
}
fn id(result: &CompletedGraphResult, row: usize, col: usize) -> NodeId {
    let Value::Node(i) = *result.cell(row, col).unwrap() else {
        panic!("node")
    };
    result.pools().nodes[i as usize].id
}
fn assert_old(result: &CompletedGraphResult, fixture: &Fixture, value: i64) {
    assert_eq!(result.metadata().rows, 2);
    assert_eq!(id(result, 0, 0), fixture.seed);
    assert_eq!(id(result, 1, 0), fixture.active);
    assert_eq!(id(result, 0, 1), fixture.neighbor);
    assert_eq!(id(result, 1, 1), fixture.neighbor);
    assert_eq!(result.cell(0, 2), Some(&Value::F64(0.0f64.to_bits())));
    assert_eq!(result.cell(1, 2), Some(&Value::F64(2.0f64.to_bits())));
    for row in 0..2 {
        assert_eq!(result.cell(row, 3), Some(&Value::I64(value)));
    }
    let copied = &result.pools().nodes;
    assert!(
        copied
            .iter()
            .filter(|n| n.id == fixture.neighbor)
            .all(|n| n.revision.get() == 1)
    );
    assert!(
        copied
            .iter()
            .all(|n| n.generation <= result.metadata().generation)
    );
    assert_eq!(
        result.pools().reports[0].generation,
        result.metadata().generation
    );
}

pub(super) fn same_view(value: i64) {
    let fixture = Fixture::new(value);
    // Publication legitimately grows writer bookkeeping. Compare query cleanup
    // with the same publication on an independent store that ran no query.
    let clean = Fixture::new(value);
    clean.publish(value + 1);
    let baseline = clean.store.stats().unwrap().temporary_bytes;
    clean.store.close().unwrap();
    let mut adapter = Observed {
        adapter: NativeSearchAdapter::new(&fixture.store.tokenizer),
        fixture: &fixture,
        publish: Some(value + 1),
        report: None,
    };
    let result = query(&fixture.store, &mut adapter, SearchMode::Exact, false).unwrap();
    assert_old(&result, &fixture, value);
    assert_eq!(result.pools().reports, [adapter.report.unwrap()]);
    let mut next = NativeSearchAdapter::new(&fixture.store.tokenizer);
    let newer = query(&fixture.store, &mut next, SearchMode::Exact, false).unwrap();
    assert_eq!(newer.metadata().rows, 1);
    assert_eq!(id(&newer, 0, 0), fixture.active);
    assert_eq!(newer.cell(0, 3), Some(&Value::I64(value + 1)));
    assert!(newer.metadata().generation > result.metadata().generation);
    assert_eq!(fixture.store.stats().unwrap().temporary_bytes, baseline);
    fixture.store.close().unwrap();
    assert_old(&result, &fixture, value);
    let reopened = Store::open_native_graph(
        fixture.directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        Some(tower()),
    )
    .unwrap();
    let newer = query(
        &reopened,
        &mut NativeSearchAdapter::new(&reopened.tokenizer),
        SearchMode::Exact,
        false,
    )
    .unwrap();
    assert_eq!(newer.metadata().rows, 1);
    assert_eq!(newer.cell(0, 3), Some(&Value::I64(value + 1)));
    reopened.close().unwrap();
    assert_old(&result, &fixture, value);
}

pub(super) fn approximation(value: i64) {
    let fixture = Fixture::new(value);
    let baseline = fixture.store.stats().unwrap().temporary_bytes;
    let mut retained = Vec::new();
    for aggregate in [false, true] {
        let mut adapter = Observed {
            adapter: NativeSearchAdapter::new(&fixture.store.tokenizer),
            fixture: &fixture,
            publish: None,
            report: None,
        };
        let result = query(&fixture.store, &mut adapter, SearchMode::Auto, aggregate).unwrap();
        let report = adapter.report.unwrap();
        assert_eq!(report.actual_tier, Some(ActualTier::Graph));
        assert_eq!(report.precision, ScorePrecision::Original);
        assert_eq!(report.coverage, CandidateCoverage::Approximate);
        assert_eq!(result.pools().reports, [report]);
        if aggregate {
            assert_eq!(result.cell(0, 0), Some(&Value::I64(2)));
        } else {
            assert_old(&result, &fixture, value);
        }
        assert_eq!(fixture.store.stats().unwrap().temporary_bytes, baseline);
        retained.push((result, report));
    }
    fixture.store.close().unwrap();
    let reopened = Store::open_native_graph(
        fixture.directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        Some(tower()),
    )
    .unwrap();
    let baseline = reopened.stats().unwrap().temporary_bytes;
    let result = query(
        &reopened,
        &mut NativeSearchAdapter::new(&reopened.tokenizer),
        SearchMode::Auto,
        true,
    )
    .unwrap();
    assert_eq!(result.cell(0, 0), Some(&Value::I64(2)));
    assert_eq!(
        result.pools().reports[0].coverage,
        CandidateCoverage::Approximate
    );
    assert_eq!(reopened.stats().unwrap().temporary_bytes, baseline);
    reopened.close().unwrap();
    for (result, report) in retained {
        assert_eq!(result.pools().reports, [report]);
    }
}

pub(super) fn preparation_refusal() {
    let directory = Directory::new();
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    let before = store
        .admit_native_read()
        .unwrap()
        .bundle()
        .base()
        .generation;
    let baseline = store.stats().unwrap().temporary_bytes;
    let error = match query(
        &store,
        &mut NativeSearchAdapter::new(&store.tokenizer),
        SearchMode::Exact,
        false,
    ) {
        Ok(_) => panic!("missing vector space succeeded"),
        Err(e) => e,
    };
    assert_eq!(error.kind(), super::GraphQueryErrorKind::Constraint);
    assert!(error.nothing_committed());
    assert!(error.to_string().contains("NoVectorSpace"));
    assert_eq!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation,
        before
    );
    assert_eq!(store.stats().unwrap().temporary_bytes, baseline);
    store.close().unwrap();
}
