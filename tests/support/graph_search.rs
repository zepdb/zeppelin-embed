//! ZE-65 primitive inputs and PG13 truth, independent of ranking/compiler code.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::result_large_err
)]
use std::collections::{BTreeMap, BTreeSet};
use zeppelin_embed::graph_commit_recovery_test_support::{Fixture, ProbeStore, document};
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, GraphQueryOptions, Value,
};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use zeppelin_embed::property_graph::*;
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
use zeppelin_embed_cypher::{CompileLimits, execute};

pub const ABSOLUTE: f64 = 1e-6;
pub const RELATIVE: f64 = 1e-6;
pub fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
pub struct Corpus {
    pub dir: tempfile::TempDir,
    pub store: Option<ProbeStore>,
    pub model: oracle::Graph,
}
impl Corpus {
    pub fn new() -> Self {
        Self::with_vfs(std::sync::Arc::new(StdVfs))
    }
    pub fn with_vfs(vfs: std::sync::Arc<dyn zeppelin_embed::vfs::Vfs>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture {
            store: 65,
            rank: 0,
            text: String::new(),
            coordinates: [0.0; 2],
        };
        let store = ProbeStore::create(&dir.path().join("graph"), &fixture, vfs);
        let mut this = Self {
            dir,
            store: Some(store),
            model: oracle::Graph::default(),
        };
        // IDs: project, alice, meeting, item, entity, eligible duals, excluded
        // nearest vector, text only, vector only, absent/empty/whitespace text.
        for (key, label, text, vector) in [
            ("project", "Project", None, None),
            ("alice", "Person", None, None),
            ("meeting", "Meeting", Some("cedar"), None),
            ("item", "Item", Some("amber"), None),
            ("entity", "Entity", None, None),
            ("a", "Eligible", Some("amber cedar"), Some([1.0, 0.0])),
            ("b", "Eligible", Some("amber"), Some([2.0, 0.0])),
            (
                "outside",
                "Outside",
                Some("amber amber amber"),
                Some([0.0, 0.0]),
            ),
            ("text", "Eligible", Some("cedar"), None),
            ("vector", "Eligible", None, Some([2.0, 0.0])),
            ("empty", "Eligible", Some(""), None),
            ("space", "Eligible", Some("   "), None),
        ] {
            this.node(key, label, text, vector, 1, oracle::Operation::Create);
        }
        for (key, source, target, ty) in [
            ("project-meeting", 3, 1, "FOR_PROJECT"),
            ("participation", 2, 3, "PARTICIPATED_IN"),
            ("about", 4, 1, "ABOUT"),
            ("support", 4, 6, "SUPPORTED_BY"),
            ("chunk-a", 3, 6, "HAS_CHUNK"),
            ("parallel-a", 3, 6, "HAS_CHUNK"),
            ("chunk-b", 3, 7, "HAS_CHUNK"),
            ("mention-a", 6, 5, "MENTIONS"),
            ("mention-b", 7, 5, "MENTIONS"),
            ("self", 6, 6, "MENTIONS"),
            ("item-edge", 3, 4, "HAS_ITEM"),
        ] {
            this.edge(key, source, target, ty);
        }
        if let Ok(path) = std::env::var("ZE65_CORPUS_OUTPUT") {
            std::fs::write(path, format!("{:#?}", this.snapshot())).unwrap();
        }
        this
    }
    pub fn graph(&self) -> &GraphStore {
        self.store.as_ref().unwrap().graph()
    }
    pub fn snapshot(&self) -> oracle::Snapshot {
        self.model.snapshot()
    }
    pub fn node(
        &mut self,
        key: &str,
        label: &str,
        text: Option<&str>,
        vector: Option<[f32; 2]>,
        revision: u64,
        operation: oracle::Operation,
    ) {
        let props = BTreeMap::from([
            (
                "name".into(),
                oracle::Property::Scalar(oracle::Scalar::String(key.into())),
            ),
            (
                "excerpt".into(),
                oracle::Property::Scalar(oracle::Scalar::String(key.into())),
            ),
            (
                "timestamp".into(),
                oracle::Property::Scalar(oracle::Scalar::I64(65)),
            ),
        ]);
        let k = oracle::Key {
            kind: oracle::Kind::Node,
            namespace: "ze65".into(),
            value: key.into(),
        };
        let expected = self
            .model
            .history
            .get(&k)
            .map_or(oracle::Expectation::Absent, |h| {
                if operation == oracle::Operation::Recreate {
                    oracle::Expectation::Deletion(h.last.revision)
                } else {
                    oracle::Expectation::Entity(h.id)
                }
            });
        let mutation = oracle::Mutation {
            key: k,
            operation,
            revision,
            expected,
            detach: false,
            image: Some(oracle::Image::Node {
                labels: BTreeSet::from([label.into()]),
                properties: props,
                text: text.map(str::to_owned),
                vector: vector.map(|v| v.map(f32::to_bits).to_vec()),
            }),
        };
        self.apply(&mutation);
    }
    pub fn edge(&mut self, key: &str, source: u128, target: u128, ty: &str) {
        self.apply(&oracle::Mutation {
            key: oracle::Key {
                kind: oracle::Kind::Relationship,
                namespace: "ze65".into(),
                value: key.into(),
            },
            operation: oracle::Operation::Create,
            revision: 1,
            expected: oracle::Expectation::Absent,
            detach: false,
            image: Some(oracle::Image::Relationship {
                source,
                target,
                relationship_type: ty.into(),
                properties: BTreeMap::new(),
            }),
        });
    }
    pub fn apply(&mut self, m: &oracle::Mutation) {
        let expected = self.model.apply(std::slice::from_ref(m)).unwrap();
        let entity = match m.expected {
            oracle::Expectation::Entity(id) => Some(match m.key.kind {
                oracle::Kind::Node => EntityId::Node(NodeId::new(id).unwrap()),
                oracle::Kind::Relationship => EntityId::Relationship(RelId::new(id).unwrap()),
            }),
            _ => None,
        };
        let operation = match m.operation {
            oracle::Operation::Create => StructuredOperation::Create,
            oracle::Operation::Put => StructuredOperation::Put(entity.unwrap()),
            oracle::Operation::Delete => StructuredOperation::Delete(
                entity.unwrap(),
                if m.detach {
                    GraphDeleteMode::Detach
                } else {
                    GraphDeleteMode::Restrict
                },
            ),
            oracle::Operation::Recreate => {
                let oracle::Expectation::Deletion(r) = m.expected else {
                    panic!("deletion revision")
                };
                StructuredOperation::Recreate(GraphRevision::new(r).unwrap())
            }
        };
        let key = ApplicationKey::new(
            if m.key.kind == oracle::Kind::Node {
                EntityKind::Node
            } else {
                EntityKind::Relationship
            },
            &m.key.namespace,
            &m.key.value,
        )
        .unwrap();
        let write = |image| {
            self.graph()
                .apply_batch(
                    &[StructuredWrite {
                        key,
                        revision: GraphRevision::new(m.revision).unwrap(),
                        operation,
                        image,
                    }],
                    &control(),
                )
                .unwrap()
        };
        let actual = match &m.image {
            Some(oracle::Image::Node {
                labels,
                properties,
                text,
                vector,
            }) => {
                let mut labels: Vec<_> =
                    labels.iter().map(|s| GraphName::new(s).unwrap()).collect();
                let mut properties: Vec<_> = properties
                    .iter()
                    .map(|(k, v)| {
                        GraphProperty::new(
                            GraphName::new(k).unwrap(),
                            PropertyValue::new(match v {
                                oracle::Property::Scalar(oracle::Scalar::String(s)) => {
                                    PropertyData::String(s)
                                }
                                oracle::Property::Scalar(oracle::Scalar::I64(i)) => {
                                    PropertyData::I64(*i)
                                }
                                _ => panic!("fixture property"),
                            })
                            .unwrap(),
                        )
                    })
                    .collect();
                let coords: Option<Vec<_>> = vector
                    .as_ref()
                    .map(|v| v.iter().map(|b| f32::from_bits(*b)).collect());
                let tower = document();
                let content = CanonicalContents::node(
                    &mut labels,
                    &mut properties,
                    text.as_deref(),
                    coords
                        .as_ref()
                        .map(|v| CanonicalEmbedding::new(&tower, v).unwrap()),
                )
                .unwrap();
                write(Some(WriteImage::Node(&content)))
            }
            Some(oracle::Image::Relationship {
                source,
                target,
                relationship_type,
                ..
            }) => write(Some(WriteImage::Relationship {
                source: NodeRef::Existing(NodeId::new(*source).unwrap()),
                target: NodeRef::Existing(NodeId::new(*target).unwrap()),
                relationship_type: GraphName::new(relationship_type).unwrap(),
                properties: &[],
            })),
            None => write(None),
        };
        assert!(
            matches!(actual.outcome(),GraphWriteOutcome::Committed {generation} if generation.get()==expected.generation)
        );
        assert_eq!(
            actual.receipts()[0].entity,
            match m.key.kind {
                oracle::Kind::Node => EntityId::Node(NodeId::new(expected.receipts[0].id).unwrap()),
                oracle::Kind::Relationship =>
                    EntityId::Relationship(RelId::new(expected.receipts[0].id).unwrap()),
            }
        );
    }
    pub fn run(&self, q: &str) -> CompletedGraphResult {
        execute(
            self.graph().statement_store(),
            &control(),
            &GraphQueryOptions::default(),
            q,
            &[],
            CompileLimits::default(),
        )
        .unwrap()
    }
    pub fn reopen(&mut self) {
        assert_eq!(self.store.take().unwrap().release(), 0);
        self.store = Some(ProbeStore::open(&self.dir.path().join("graph")).unwrap());
    }
}
impl Drop for Corpus {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            assert_eq!(store.close(), 0);
        }
    }
}
fn cell(r: &CompletedGraphResult, v: Value) -> oracle::Cell {
    match v {
        Value::Null => oracle::Cell::Null,
        Value::Bool(v) => oracle::Cell::Bool(v),
        Value::I64(v) => oracle::Cell::I64(v),
        Value::F64(v) => oracle::Cell::Score(v),
        Value::String(s) => oracle::Cell::String(r.string(s).unwrap().into()),
        Value::Node(i) => oracle::Cell::Node(r.pools().nodes[i as usize].id.get()),
        Value::Relationship(i) => {
            oracle::Cell::Relationship(r.pools().relationships[i as usize].id.get())
        }
        Value::List { children, .. } => oracle::Cell::List(
            r.pools().children[children.start as usize..(children.start + children.len) as usize]
                .iter()
                .map(|i| cell(r, r.pools().values[i.0 as usize]))
                .collect(),
        ),
    }
}
pub fn observe(r: &CompletedGraphResult) -> Vec<oracle::Row> {
    (0..r.metadata().rows as usize)
        .map(|row| {
            (0..r.pools().columns.len())
                .map(|col| cell(r, *r.cell(row, col).unwrap()))
                .collect()
        })
        .collect()
}
// Independent structured eligible-domain vector plan. No compiler plan used.
pub fn structured_plan(store: &GraphStore, k: i64) -> CompletedGraphResult {
    let label = String::from("Eligible");
    let coordinates = vec![ExprId(0), ExprId(1)];
    let expressions = vec![
        Expression::Literal(Literal::F64(0.0)),
        Expression::Literal(Literal::F64(0.0)),
        Expression::List(&coordinates),
        Expression::Literal(Literal::I64(k)),
        Expression::Slot(SlotId(0)),
        Expression::Aggregate {
            operation: AggregateExpression::Collect { distinct: true },
            operand: Some(ExprId(4)),
        },
        Expression::Slot(SlotId(1)),
        Expression::Slot(SlotId(2)),
        Expression::Slot(SlotId(3)),
    ];
    let aggregate = vec![Projection {
        slot: SlotId(1),
        expression: ExprId(5),
    }];
    let projection = vec![
        Projection {
            slot: SlotId(10),
            expression: ExprId(7),
        },
        Projection {
            slot: SlotId(11),
            expression: ExprId(8),
        },
    ];
    let edges: Vec<Vec<_>> = (0..5)
        .map(|i| {
            if i == 0 {
                vec![]
            } else {
                vec![PlanNodeId(i - 1)]
            }
        })
        .collect();
    let operators = vec![
        Operator {
            inputs: &edges[0],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &edges[1],
            kind: OperatorKind::ScanNodes {
                output: SlotId(0),
                label: Some(GraphName::new(&label).unwrap()),
            },
        },
        Operator {
            inputs: &edges[2],
            kind: OperatorKind::Aggregate {
                keys: &[],
                aggregates: &aggregate,
            },
        },
        Operator {
            inputs: &edges[3],
            kind: OperatorKind::Search {
                call: SearchCallId(0),
                request: SearchRequest::Vector {
                    vector: ExprId(2),
                    k: ExprId(3),
                    mode: SearchMode::Exact,
                    eligible: Some(ExprId(6)),
                },
                outputs: SearchOutputs {
                    node: Some(SlotId(2)),
                    distance: Some(SlotId(3)),
                    ..Default::default()
                },
            },
        },
        Operator {
            inputs: &edges[4],
            kind: OperatorKind::Project(&projection),
        },
    ];
    let mut backing = GraphPlanBacking::default();
    backing.string(&label).unwrap();
    backing.vec(&coordinates).unwrap();
    backing.vec(&aggregate).unwrap();
    backing.vec(&projection).unwrap();
    for edge in &edges {
        backing.vec(edge).unwrap();
    }
    store
        .query(
            &control(),
            &GraphQueryOptions::default(),
            &GraphQueryPlan {
                operators: &operators,
                expressions: &expressions,
                parameters: &Vec::new(),
                eager_searches: &vec![PlanNodeId(3)],
                root: PlanNodeId(4),
                backing: &backing,
                bindings: &[],
                columns: &["node", "distance"],
            },
        )
        .unwrap()
}
impl Corpus {
    pub fn ann_cohort(&mut self) {
        let tower = document();
        let coordinates: Vec<_> = (0..96).map(|i| [i as f32 / 16.0, 1.0]).collect();
        let keys: Vec<_> = (0..96).map(|i| format!("ann-{i}")).collect();
        let mut labels: Vec<_> = (0..96)
            .map(|_| vec![GraphName::new("Ann").unwrap()])
            .collect();
        let mut props: Vec<Vec<GraphProperty<'_>>> = (0..96).map(|_| vec![]).collect();
        let contents: Vec<_> = labels
            .iter_mut()
            .zip(props.iter_mut())
            .zip(&coordinates)
            .map(|((l, p), v)| {
                CanonicalContents::node(
                    l,
                    p,
                    None,
                    Some(CanonicalEmbedding::new(&tower, v).unwrap()),
                )
                .unwrap()
            })
            .collect();
        let writes: Vec<_> = keys
            .iter()
            .zip(&contents)
            .map(|(key, c)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze65", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(c)),
            })
            .collect();
        let mutations: Vec<_> = keys
            .iter()
            .zip(&coordinates)
            .map(|(key, v)| oracle::Mutation {
                key: oracle::Key {
                    kind: oracle::Kind::Node,
                    namespace: "ze65".into(),
                    value: key.clone(),
                },
                operation: oracle::Operation::Create,
                revision: 1,
                expected: oracle::Expectation::Absent,
                detach: false,
                image: Some(oracle::Image::Node {
                    labels: BTreeSet::from(["Ann".into()]),
                    properties: BTreeMap::new(),
                    text: None,
                    vector: Some(v.map(f32::to_bits).to_vec()),
                }),
            })
            .collect();
        let expected = self.model.apply(&mutations).unwrap();
        let actual = self.graph().apply_batch(&writes, &control()).unwrap();
        assert!(
            matches!(actual.outcome(),GraphWriteOutcome::Committed {generation} if generation.get()==expected.generation)
        );
    }
}
/// Exhaustive primitive full-index truth, including members not expanded by
/// application queries. Ordered f64 arithmetic never uses a product scorer.
pub fn check_search_snapshot(c: &Corpus) -> Result<(), String> {
    let snapshot = c.snapshot();
    let mut vector: Vec<_> = snapshot
        .nodes
        .iter()
        .filter_map(|n| {
            n.vector.as_ref().map(|v| {
                let distance = v.iter().fold(0.0, |sum, b| {
                    let v = f64::from(f32::from_bits(*b));
                    sum + v * v
                });
                (distance, n.id)
            })
        })
        .collect();
    vector.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let expected: Vec<_> = vector
        .into_iter()
        .map(|(d, id)| vec![oracle::Cell::Node(id), oracle::Cell::Score(d.to_bits())])
        .collect();
    let actual =
        c.run("CALL ze.vector_search([0,0],4096,'exact') YIELD node,distance RETURN node,distance");
    oracle::compare_scored_rows(&expected, &observe(&actual), ABSOLUTE, RELATIVE)?;
    let expected = oracle::query(
        &snapshot,
        &oracle::Query::LexicalEvidence {
            terms: vec!["amber".into()],
            phrase: false,
            k: 4096,
        },
    )?;
    let actual=c.run("CALL ze.text_search('amber',4096) YIELD node,score RETURN node,node.name,ze.stored_text(node) IS NOT NULL,ze.stored_text(node),score");
    oracle::compare_scored_rows(&expected, &observe(&actual), ABSOLUTE, RELATIVE)?;
    let mut expected: Vec<_> = snapshot
        .nodes
        .iter()
        .map(|n| {
            let property = |name: &str| match n.properties.get(name) {
                Some(oracle::Property::Scalar(oracle::Scalar::String(s))) => {
                    oracle::Cell::String(s.clone())
                }
                _ => oracle::Cell::Null,
            };
            vec![
                oracle::Cell::Node(n.id),
                property("name"),
                property("excerpt"),
                n.text
                    .clone()
                    .map_or(oracle::Cell::Null, oracle::Cell::String),
            ]
        })
        .collect();
    expected.sort_by_key(|r| match r[0] {
        oracle::Cell::Node(id) => id,
        _ => 0,
    });
    let actual = c.run("MATCH (n) RETURN n,n.name,n.excerpt,ze.stored_text(n) ORDER BY n");
    oracle::compare_rows(&expected, &observe(&actual), true)?;
    for n in actual.pools().nodes {
        let expected = snapshot
            .nodes
            .iter()
            .find(|truth| truth.id == n.id.get())
            .ok_or("unknown copied node")?;
        if n.revision.get() != expected.revision || n.generation.get() != expected.generation {
            return Err("copied node version differs from primitive history".into());
        }
    }
    let mut expected: Vec<_> = snapshot
        .relationships
        .iter()
        .map(|e| {
            vec![
                oracle::Cell::Relationship(e.id),
                oracle::Cell::Node(e.source),
                oracle::Cell::Node(e.target),
                oracle::Cell::String(e.relationship_type.clone()),
            ]
        })
        .collect();
    expected.sort();
    let actual = c.run("MATCH (a)-[r]->(b) RETURN r,a,b,type(r) ORDER BY r");
    oracle::compare_rows(&expected, &observe(&actual), true)
}
