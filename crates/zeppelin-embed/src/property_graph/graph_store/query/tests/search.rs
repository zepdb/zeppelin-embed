//! Real ranked sources through the public structured query seam.
use super::*;
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::Store;
use crate::property_graph::CanonicalEmbedding;
use crate::property_graph::query::completed::{
    ActualTier, CandidateCoverage, LegState, SearchKind,
};
use crate::property_graph::query::plan::{SearchCallId, SearchMode, SearchOutputs, SearchRequest};

fn tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze64".into(),
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

struct SearchFixture {
    directory: tempfile::TempDir,
    store: Store,
    nodes: [NodeId; 5],
}
impl SearchFixture {
    fn create() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let document = tower();
        let store = Store::create_graph(
            directory.path().join("native"),
            store_options().with_epoch(crate::epoch::StoreEpoch {
                embedding: crate::epoch::EmbeddingEpoch {
                    query: document.clone(),
                    document: document.clone(),
                    alignment_digest: vec![],
                },
                tokenizer: crate::fts::tokenizer::TokenizerConfig::text_default().epoch(),
            }),
            Some(document.clone()),
        )
        .unwrap();
        // vector-only, text-only, dual, present nonmatching text, graph-only.
        let points = [
            Some([1.0, 1.0]),
            None,
            Some([0.0, 0.0]),
            Some([5.0, 5.0]),
            None,
        ];
        let texts = [
            None,
            Some("amber birch"),
            Some("amber"),
            Some("birch"),
            None,
        ];
        let contents: Vec<_> = points
            .iter()
            .zip(texts)
            .map(|(point, text)| {
                CanonicalContents::node(
                    &mut [],
                    &mut [],
                    text,
                    point
                        .as_ref()
                        .map(|p| CanonicalEmbedding::new(&document, p).unwrap()),
                )
                .unwrap()
            })
            .collect();
        let writes: Vec<_> = contents
            .iter()
            .enumerate()
            .map(|(i, image)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze64", ["a", "b", "c", "d", "e"][i])
                    .unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(image)),
            })
            .collect();
        let receipt = store.graph_apply(&writes, &control()).unwrap();
        let nodes = std::array::from_fn(|i| match receipt.receipts()[i].entity {
            EntityId::Node(n) => n,
            _ => panic!("node receipt"),
        });
        use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
        let documents = nodes
            .iter()
            .zip(points)
            .zip(texts)
            .filter(|((_, vector), text)| vector.is_some() || text.is_some())
            .map(|((id, vector), text)| {
                let document = IngestDocument::new(
                    DocumentVersion::new(DocId::new(id.get()), Revision::new(1)),
                    vector.unwrap_or([1000.0; 2]).to_vec(),
                );
                text.map_or(document.clone(), |text| document.with_text(text))
            })
            .collect();
        store
            .ingest(IngestBatch::new(documents).with_epoch(store.epoch_identity().unwrap()))
            .unwrap();
        Self {
            directory,
            store,
            nodes,
        }
    }
}

fn search(
    store: &Store,
    kind: SearchKind,
    k: i64,
) -> Result<CompletedGraphResult, GraphStoreError> {
    search_with_eligibility(store, kind, k, false, SearchMode::Exact)
}
fn search_with_eligibility(
    store: &Store,
    kind: SearchKind,
    k: i64,
    empty: bool,
    mode: SearchMode,
) -> Result<CompletedGraphResult, GraphStoreError> {
    let unit = vec![PlanNodeId(0)];
    let source = vec![PlanNodeId(1)];
    let coordinates = vec![ExprId(0), ExprId(1)];
    let text = String::from("amber");
    let mut expressions = Vec::new();
    if kind != SearchKind::Lexical {
        expressions.extend([
            Expression::Literal(Literal::F64(0.0)),
            Expression::Literal(Literal::F64(0.0)),
            Expression::List(&coordinates),
        ]);
    }
    let query = ExprId(expressions.len() as u32);
    if kind != SearchKind::Vector {
        expressions.push(Expression::Literal(Literal::String(&text)));
    }
    let count = ExprId(expressions.len() as u32);
    expressions.push(Expression::Literal(Literal::I64(k)));
    let eligible = if empty {
        let id = ExprId(expressions.len() as u32);
        expressions.push(Expression::List(&[]));
        Some(id)
    } else {
        None
    };
    let request = match kind {
        SearchKind::Vector => SearchRequest::Vector {
            vector: ExprId(2),
            k: count,
            mode,
            eligible,
            options: Default::default(),
        },
        SearchKind::Lexical => SearchRequest::Text {
            query,
            k: count,
            eligible,
            options: Default::default(),
        },
        SearchKind::Hybrid => SearchRequest::Hybrid {
            vector: ExprId(2),
            text: query,
            k: count,
            mode,
            eligible,
            options: Default::default(),
        },
    };
    let mut projections = Vec::new();
    let width = if kind == SearchKind::Hybrid { 4 } else { 2 };
    for i in 0..width {
        let expression = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(SlotId(i)));
        projections.push(Projection {
            slot: SlotId(10 + i),
            expression,
        });
    }
    let outputs = SearchOutputs {
        node: Some(SlotId(0)),
        distance: (kind == SearchKind::Vector).then_some(SlotId(1)),
        score: (kind != SearchKind::Vector).then_some(SlotId(1)),
        vector_distance: (kind == SearchKind::Hybrid).then_some(SlotId(2)),
        lexical_score: (kind == SearchKind::Hybrid).then_some(SlotId(3)),
    };
    let operators = vec![
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &unit,
            kind: OperatorKind::Search {
                call: SearchCallId(0),
                request,
                outputs,
            },
        },
        Operator {
            inputs: &source,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let eager = vec![PlanNodeId(1)];
    let parameters = Vec::new();
    let mut backing = GraphPlanBacking::default();
    backing.vec(&unit).unwrap();
    backing.vec(&source).unwrap();
    backing.vec(&coordinates).unwrap();
    backing.vec(&projections).unwrap();
    backing.string(&text).unwrap();
    store.graph_query(
        &control(),
        &GraphQueryOptions::default(),
        &GraphQueryPlan {
            operators: &operators,
            expressions: &expressions,
            parameters: &parameters,
            eager_searches: &eager,
            root: PlanNodeId(2),
            backing: &backing,
            bindings: &[],
            columns: if width == 4 {
                &["n", "score", "distance", "lexical"]
            } else {
                &["n", "score"]
            },
        },
    )
}
fn node(result: &CompletedGraphResult, row: usize, col: usize) -> NodeId {
    let Value::Node(index) = result.cell(row, col).unwrap() else {
        panic!("node cell")
    };
    result.pools().nodes[*index as usize].id
}
fn score(result: &CompletedGraphResult, row: usize, col: usize) -> Option<f64> {
    match result.cell(row, col).unwrap() {
        Value::F64(bits) => Some(f64::from_bits(*bits)),
        Value::Null => None,
        other => panic!("score {other:?}"),
    }
}
#[test]
fn ze64_public_search_sources_execute_and_report() {
    let fixture = SearchFixture::create();
    for kind in [SearchKind::Vector, SearchKind::Lexical, SearchKind::Hybrid] {
        let result = search(&fixture.store, kind, 4).unwrap();
        assert_eq!(result.pools().reports.len(), 1);
        let report = result.pools().reports[0];
        assert_eq!(report.kind, kind);
        assert_eq!(report.generation, result.metadata().generation);
        assert_eq!(report.coverage, CandidateCoverage::Exact);
        match kind {
            SearchKind::Vector => {
                assert_eq!(report.actual_tier, Some(ActualTier::Exact));
                let got: Vec<_> = (0..3)
                    .map(|i| (node(&result, i, 0), score(&result, i, 1)))
                    .collect();
                assert_eq!(
                    got,
                    vec![
                        (fixture.nodes[2], Some(0.0)),
                        (fixture.nodes[0], Some(2.0)),
                        (fixture.nodes[3], Some(50.0))
                    ]
                );
            }
            SearchKind::Lexical => {
                assert_eq!(result.metadata().rows, 2);
                assert_eq!(node(&result, 0, 0), fixture.nodes[2]);
                assert_eq!(node(&result, 1, 0), fixture.nodes[1]);
                assert!(score(&result, 0, 1).unwrap() > score(&result, 1, 1).unwrap());
                assert_eq!(report.lexical_leg, LegState::Nonempty);
                // Store statistics include all four indexed documents:
                // N=4, df=2, avgdl=1, tf=1, k1=1.2, b=0.75.
                for (row, denominator) in [(0, 2.2), (1, 3.1)] {
                    let expected = 2.0_f64.ln() * 2.2 / denominator;
                    assert!((score(&result, row, 1).unwrap() - expected).abs() < 1e-12);
                }
            }
            SearchKind::Hybrid => {
                assert_eq!(result.metadata().rows, 4);
                assert_eq!(node(&result, 0, 0), fixture.nodes[2]);
                assert_eq!(score(&result, 0, 2), Some(0.0));
                assert!(score(&result, 0, 3).unwrap() > 0.0);
                assert!(report.cross_score_complete);
            }
        }
    }
}

use crate::property_graph::RelId;
use crate::property_graph::query::plan::{AggregateExpression, Direction, PatternId, SortKey};

impl SearchFixture {
    // Two parallel edges and one self-loop from the closest seed, plus one
    // edge from the next seed. The graph bag has four rows, not two seeds.
    fn edges(&self) -> [RelId; 4] {
        let r = GraphName::new("R").unwrap();
        let pairs = [(2, 1), (2, 1), (2, 2), (0, 1)];
        let keys = ["r0", "r1", "r2", "r3"];
        let writes: Vec<_> = pairs
            .iter()
            .zip(keys)
            .map(|((a, b), key)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze64", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: crate::property_graph::NodeRef::Existing(self.nodes[*a]),
                    target: crate::property_graph::NodeRef::Existing(self.nodes[*b]),
                    relationship_type: r,
                    properties: &[],
                }),
            })
            .collect();
        let result = self.store.graph_apply(&writes, &control()).unwrap();
        std::array::from_fn(|i| match result.receipts()[i].entity {
            EntityId::Relationship(r) => r,
            _ => panic!("edge receipt"),
        })
    }
}

// None is absent restriction; Some(false) collects duplicate edge destinations;
// Some(true) collects a real empty scan. Collection remains a global singleton.
fn expand_search(
    store: &Store,
    collect: Option<bool>,
    aggregate: bool,
    mode: SearchMode,
) -> Result<CompletedGraphResult, GraphStoreError> {
    let inputs: Vec<Vec<_>> = (0..9).map(|i| vec![PlanNodeId(i)]).collect();
    let coordinates = vec![ExprId(0), ExprId(1)];
    let mut expressions = vec![
        Expression::Literal(Literal::F64(0.0)),
        Expression::Literal(Literal::F64(0.0)),
        Expression::List(&coordinates),
        Expression::Literal(Literal::I64(2)),
    ];
    let mut operators = vec![Operator {
        inputs: &[],
        kind: OperatorKind::Unit,
    }];
    let label = String::from("missing");
    let mut aggregates = Vec::new();
    let mut eligible = None;
    if let Some(empty) = collect {
        operators.push(Operator {
            inputs: &inputs[0],
            kind: OperatorKind::ScanNodes {
                output: SlotId(5),
                label: if empty {
                    Some(GraphName::new(&label).unwrap())
                } else {
                    None
                },
            },
        });
        if !empty {
            operators.push(Operator {
                inputs: &inputs[1],
                kind: OperatorKind::Expand {
                    source: SlotId(5),
                    node: SlotId(6),
                    relationship: SlotId(7),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(1),
                },
            });
        }
        let operand = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(if empty { SlotId(5) } else { SlotId(6) }));
        let collected = ExprId(expressions.len() as u32);
        expressions.push(Expression::Aggregate {
            operation: AggregateExpression::Collect { distinct: true },
            operand: Some(operand),
        });
        aggregates.push(Projection {
            slot: SlotId(8),
            expression: collected,
        });
        operators.push(Operator {
            inputs: &inputs[operators.len() - 1],
            kind: OperatorKind::Aggregate {
                keys: &[],
                aggregates: &aggregates,
            },
        });
        eligible = Some(ExprId(expressions.len() as u32));
        expressions.push(Expression::Slot(SlotId(8)));
    }
    let search_id = PlanNodeId(operators.len() as u32);
    operators.push(Operator {
        inputs: &inputs[operators.len() - 1],
        kind: OperatorKind::Search {
            call: SearchCallId(0),
            request: SearchRequest::Vector {
                vector: ExprId(2),
                k: ExprId(3),
                mode,
                eligible,
                options: Default::default(),
            },
            outputs: SearchOutputs {
                node: Some(SlotId(0)),
                distance: Some(SlotId(1)),
                ..SearchOutputs::default()
            },
        },
    });
    operators.push(Operator {
        inputs: &inputs[operators.len() - 1],
        kind: OperatorKind::Expand {
            source: SlotId(0),
            node: SlotId(2),
            relationship: SlotId(3),
            direction: Direction::Outgoing,
            relationship_types: &[],
            pattern: PatternId(2),
        },
    });
    let mut projections = Vec::new();
    let mut groups = Vec::new();
    let mut counts = Vec::new();
    let sort = vec![SortKey {
        expression: ExprId(expressions.len() as u32 + 2),
        descending: false,
    }];
    if aggregate {
        let key = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(SlotId(2)));
        groups.push(Projection {
            slot: SlotId(10),
            expression: key,
        });
        let count = ExprId(expressions.len() as u32);
        expressions.push(Expression::Aggregate {
            operation: AggregateExpression::Count { distinct: false },
            operand: None,
        });
        counts.push(Projection {
            slot: SlotId(11),
            expression: count,
        });
        operators.push(Operator {
            inputs: &inputs[operators.len() - 1],
            kind: OperatorKind::Aggregate {
                keys: &groups,
                aggregates: &counts,
            },
        });
        expressions.push(Expression::Slot(SlotId(11)));
        operators.push(Operator {
            inputs: &inputs[operators.len() - 1],
            kind: OperatorKind::Sort(&sort),
        });
    } else {
        for slot in [0, 2, 1] {
            let expression = ExprId(expressions.len() as u32);
            expressions.push(Expression::Slot(SlotId(slot)));
            projections.push(Projection {
                slot: SlotId(10 + projections.len() as u32),
                expression,
            });
        }
        operators.push(Operator {
            inputs: &inputs[operators.len() - 1],
            kind: OperatorKind::Project(&projections),
        });
    }
    let eager = vec![search_id];
    let parameters = Vec::new();
    let mut backing = GraphPlanBacking::default();
    for input in &inputs {
        backing.vec(input).unwrap();
    }
    for p in [&projections, &aggregates, &groups, &counts] {
        backing.vec(p).unwrap();
    }
    backing.vec(&coordinates).unwrap();
    backing.vec(&sort).unwrap();
    backing.string(&label).unwrap();
    store.graph_query(
        &control(),
        &GraphQueryOptions::default(),
        &GraphQueryPlan {
            operators: &operators,
            expressions: &expressions,
            parameters: &parameters,
            eager_searches: &eager,
            root: PlanNodeId(operators.len() as u32 - 1),
            backing: &backing,
            bindings: &[],
            columns: if aggregate {
                &["m", "count"]
            } else {
                &["seed", "m", "distance"]
            },
        },
    )
}

#[test]
fn ze64_application_shapes_preserve_scores_and_bags() {
    let fixture = SearchFixture::create();
    fixture.edges();
    let expanded = expand_search(&fixture.store, None, false, SearchMode::Exact).unwrap();
    assert_eq!(expanded.metadata().rows, 4);
    let rows: Vec<_> = (0..4)
        .map(|i| {
            (
                node(&expanded, i, 0),
                node(&expanded, i, 1),
                score(&expanded, i, 2),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (fixture.nodes[2], fixture.nodes[1], Some(0.0)),
            (fixture.nodes[2], fixture.nodes[1], Some(0.0)),
            (fixture.nodes[2], fixture.nodes[2], Some(0.0)),
            (fixture.nodes[0], fixture.nodes[1], Some(2.0))
        ]
    );
    let eligible = expand_search(&fixture.store, Some(false), false, SearchMode::Exact).unwrap();
    // The Store indexes both eligible documents; duplicate graph edges
    // preserve the three c rows after seed ranking.
    assert_eq!(eligible.metadata().rows, 3);
    assert!((0..3).all(|i| node(&eligible, i, 0) == fixture.nodes[2]));
    assert_eq!(eligible.pools().reports[0].candidate_count, 2);
    let hybrid = search(&fixture.store, SearchKind::Hybrid, 4).unwrap();
    for i in 0..4 {
        let n = node(&hybrid, i, 0);
        if n == fixture.nodes[1] {
            assert_eq!(score(&hybrid, i, 2), Some(2_000_000.0));
            assert!(score(&hybrid, i, 3).unwrap() > 0.0);
        }
        if n == fixture.nodes[0] {
            assert_eq!(score(&hybrid, i, 2), Some(2.0));
            assert_eq!(score(&hybrid, i, 3), Some(0.0));
        }
        if n == fixture.nodes[3] {
            assert_eq!(score(&hybrid, i, 2), Some(50.0));
            assert_eq!(score(&hybrid, i, 3), Some(0.0));
        }
        assert_ne!(n, fixture.nodes[4]);
    }
    let grouped = expand_search(&fixture.store, None, true, SearchMode::Exact).unwrap();
    assert_eq!(grouped.metadata().rows, 2);
    assert_eq!(grouped.cell(0, 1), Some(&Value::I64(1)));
    assert_eq!(grouped.cell(1, 1), Some(&Value::I64(3)));
    assert_eq!(grouped.pools().reports, expanded.pools().reports);
}
#[test]
fn ze64_empty_collect_differs_from_absent_restriction() {
    let fixture = SearchFixture::create();
    fixture.edges();
    let none = expand_search(&fixture.store, Some(true), false, SearchMode::Exact).unwrap();
    assert_eq!(none.metadata().rows, 0);
    assert_eq!(none.pools().reports.len(), 1);
    assert_eq!(
        none.pools().reports[0].vector_leg,
        LegState::NoEligibleMembers
    );
    assert_eq!(
        expand_search(&fixture.store, None, false, SearchMode::Exact)
            .unwrap()
            .metadata()
            .rows,
        4
    );
}

fn two_calls(
    store: &Store,
    aggregate: bool,
    limit_zero: bool,
    empty_input: bool,
) -> CompletedGraphResult {
    let inputs = [
        vec![PlanNodeId(0)],
        vec![PlanNodeId(2)],
        vec![PlanNodeId(1), PlanNodeId(3)],
        vec![PlanNodeId(4)],
        vec![PlanNodeId(5)],
        vec![PlanNodeId(6)],
    ];
    let text = String::from("amber");
    let coordinates = vec![ExprId(0), ExprId(1)];
    let mut expressions = vec![
        Expression::Literal(Literal::F64(0.0)),
        Expression::Literal(Literal::F64(0.0)),
        Expression::List(&coordinates),
        Expression::Literal(Literal::I64(2)),
        Expression::Literal(Literal::String(&text)),
    ];
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
                    mode: SearchMode::Exact,
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
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &inputs[1],
            kind: OperatorKind::Search {
                call: SearchCallId(1),
                request: SearchRequest::Text {
                    query: ExprId(4),
                    k: ExprId(3),
                    eligible: None,
                    options: Default::default(),
                },
                outputs: SearchOutputs {
                    node: Some(SlotId(2)),
                    score: Some(SlotId(3)),
                    ..SearchOutputs::default()
                },
            },
        },
        Operator {
            inputs: &inputs[2],
            kind: OperatorKind::Join { predicate: None },
        },
    ];
    let mut projection = Vec::new();
    if empty_input {
        expressions.push(Expression::Literal(Literal::Bool(false)));
        operators.push(Operator {
            inputs: &inputs[3],
            kind: OperatorKind::Filter(ExprId(5)),
        });
    }
    let expression = ExprId(expressions.len() as u32);
    expressions.push(if aggregate {
        Expression::Aggregate {
            operation: AggregateExpression::Count { distinct: false },
            operand: None,
        }
    } else {
        Expression::Literal(Literal::I64(7))
    });
    projection.push(Projection {
        slot: SlotId(10),
        expression,
    });
    let tail_inputs = vec![PlanNodeId(operators.len() as u32 - 1)];
    operators.push(Operator {
        inputs: &tail_inputs,
        kind: if aggregate {
            OperatorKind::Aggregate {
                keys: &[],
                aggregates: &projection,
            }
        } else {
            OperatorKind::Project(&projection)
        },
    });
    let limit_inputs = vec![PlanNodeId(operators.len() as u32 - 1)];
    operators.push(Operator {
        inputs: &limit_inputs,
        kind: OperatorKind::OffsetLimit {
            offset: 0,
            limit: limit_zero.then_some(0),
        },
    });
    let eager = vec![PlanNodeId(1), PlanNodeId(3)];
    let parameters = Vec::new();
    let mut backing = GraphPlanBacking::default();
    for input in &inputs {
        backing.vec(input).unwrap();
    }
    backing.vec(&tail_inputs).unwrap();
    backing.vec(&limit_inputs).unwrap();
    backing.vec(&coordinates).unwrap();
    backing.vec(&projection).unwrap();
    backing.string(&text).unwrap();
    store
        .graph_query(
            &control(),
            &GraphQueryOptions::default(),
            &GraphQueryPlan {
                operators: &operators,
                expressions: &expressions,
                parameters: &parameters,
                eager_searches: &eager,
                root: PlanNodeId(operators.len() as u32 - 1),
                backing: &backing,
                bindings: &[],
                columns: &["value"],
            },
        )
        .unwrap()
}
#[test]
fn ze64_explicit_calls_keep_reports_after_projection_and_aggregation() {
    let fixture = SearchFixture::create();
    let projected = two_calls(&fixture.store, false, false, false);
    assert_eq!(projected.metadata().rows, 4);
    assert!((0..4).all(|i| projected.cell(i, 0) == Some(&Value::I64(7))));
    let aggregated = two_calls(&fixture.store, true, false, false);
    assert_eq!(aggregated.metadata().rows, 1);
    assert_eq!(aggregated.cell(0, 0), Some(&Value::I64(4)));
    assert_eq!(projected.pools().reports, aggregated.pools().reports);
    assert_eq!(projected.pools().reports.len(), 2);
    assert_eq!(projected.pools().reports[0].call, SearchCallId(0));
    assert_eq!(projected.pools().reports[0].kind, SearchKind::Vector);
    assert_eq!(projected.pools().reports[1].call, SearchCallId(1));
    assert_eq!(projected.pools().reports[1].kind, SearchKind::Lexical);
}
#[test]
fn ze64_eager_reports_survive_limit_zero_and_empty_input() {
    let fixture = SearchFixture::create();
    for (limit, empty) in [(true, false), (false, true)] {
        let result = two_calls(&fixture.store, false, limit, empty);
        assert_eq!(result.metadata().rows, 0);
        assert_eq!(result.pools().reports.len(), 2);
        assert_eq!(
            result
                .metadata()
                .counters
                .get(crate::property_graph::query::runtime::WorkKind::SearchInvocations),
            2
        );
    }
}
#[test]
fn ze64_invalid_search_plans_publish_nothing() {
    let fixture = SearchFixture::create();
    let before = snapshot(&fixture.directory.path().join("native"));
    for writes in [false, true] {
        let text = String::from("amber");
        let property = String::from("p");
        let inputs: Vec<Vec<_>> = (0..4).map(|i| vec![PlanNodeId(i)]).collect();
        let mut expressions = vec![
            Expression::Literal(Literal::String(&text)),
            Expression::Literal(Literal::I64(2)),
        ];
        let mut operators = vec![Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        }];
        if !writes {
            operators.push(Operator {
                inputs: &inputs[0],
                kind: OperatorKind::ScanNodes {
                    output: SlotId(5),
                    label: None,
                },
            });
        }
        let search_id = PlanNodeId(operators.len() as u32);
        operators.push(Operator {
            inputs: &inputs[operators.len() - 1],
            kind: OperatorKind::Search {
                call: SearchCallId(0),
                request: SearchRequest::Text {
                    query: ExprId(0),
                    k: ExprId(1),
                    eligible: None,
                    options: Default::default(),
                },
                outputs: SearchOutputs {
                    node: Some(SlotId(0)),
                    ..SearchOutputs::default()
                },
            },
        });
        let mutations = vec![Mutation::SetProperty {
            entity: ExprId(2),
            name: GraphName::new(&property).unwrap(),
            value: ExprId(3),
        }];
        if writes {
            expressions.push(Expression::Slot(SlotId(0)));
            expressions.push(Expression::Literal(Literal::I64(9)));
            operators.push(Operator {
                inputs: &inputs[operators.len() - 1],
                kind: OperatorKind::Eager,
            });
            operators.push(Operator {
                inputs: &inputs[operators.len() - 1],
                kind: OperatorKind::Mutate(&mutations),
            });
        }
        let eager = vec![search_id];
        let parameters = Vec::new();
        let mut backing = GraphPlanBacking::default();
        for input in &inputs {
            backing.vec(input).unwrap();
        }
        backing.string(&text).unwrap();
        backing.string(&property).unwrap();
        backing.vec(&mutations).unwrap();
        let error = refused(
            fixture.store.graph_query(
                &control(),
                &GraphQueryOptions::default(),
                &GraphQueryPlan {
                    operators: &operators,
                    expressions: &expressions,
                    parameters: &parameters,
                    eager_searches: &eager,
                    root: PlanNodeId(operators.len() as u32 - 1),
                    backing: &backing,
                    bindings: &[],
                    columns: if writes { &[] } else { &["n", "input"] },
                },
            ),
            "invalid search",
        );
        assert_eq!(error.kind(), GraphStoreErrorKind::InvalidRequest);
        assert!(error.nothing_committed());
        assert!(
            error
                .to_string()
                .contains(if writes { "ReadWriteSearch" } else { "Search" }),
            "{error}"
        );
        assert_eq!(snapshot(&fixture.directory.path().join("native")), before);
    }
}

fn snapshot(path: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect()
}

#[test]
fn ze64_empty_text_eligibility_retains_one_report() {
    let fixture = SearchFixture::create();
    let result = search_with_eligibility(
        &fixture.store,
        SearchKind::Lexical,
        1,
        true,
        SearchMode::Exact,
    )
    .unwrap();
    assert_eq!(result.metadata().rows, 0);
    assert_eq!(result.pools().reports.len(), 1);
    assert_eq!(
        result.pools().reports[0].lexical_leg,
        LegState::NoEligibleMembers
    );
    assert_eq!(result.pools().reports[0].candidate_count, 0);
}
#[test]
fn ze64_text_candidate_count_includes_matches_beyond_top_k() {
    let fixture = SearchFixture::create();
    let result = search(&fixture.store, SearchKind::Lexical, 1).unwrap();
    assert_eq!(result.metadata().rows, 1);
    assert_eq!(result.pools().reports[0].candidate_count, 2);
}
#[test]
fn ze64_public_vector_search_refuses_missing_space() {
    let fixture = Fixture::create(0);
    let error = refused(
        search(&fixture.store, SearchKind::Vector, 2),
        "missing space",
    );
    assert_eq!(error.kind(), GraphStoreErrorKind::Constraint);
    assert!(error.nothing_committed());
    assert!(error.to_string().contains("NoVectorSpace"));
}

#[test]
fn ze64_hybrid_window_limited_cross_scoring_is_reported_not_hidden() {
    let fixture = SearchFixture::create();
    let document = tower();
    let points: Vec<_> = (0..65).map(|i| [i as f32, 0.0]).collect();
    let names: Vec<_> = (0..65).map(|i| format!("window-{i}")).collect();
    let images: Vec<_> = points
        .iter()
        .map(|p| {
            CanonicalContents::node(
                &mut [],
                &mut [],
                Some("amber"),
                Some(CanonicalEmbedding::new(&document, p).unwrap()),
            )
            .unwrap()
        })
        .collect();
    let writes: Vec<_> = names
        .iter()
        .zip(&images)
        .map(|(name, image)| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "ze64", name).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(image)),
        })
        .collect();
    let receipts = fixture.store.graph_apply(&writes, &control()).unwrap();
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    let documents = receipts
        .receipts()
        .iter()
        .zip(&points)
        .map(|(receipt, point)| {
            let EntityId::Node(node) = receipt.entity else {
                panic!("node receipt")
            };
            IngestDocument::new(
                DocumentVersion::new(DocId::new(node.get()), Revision::new(1)),
                point.to_vec(),
            )
            .with_text("amber")
        })
        .collect();
    fixture
        .store
        .ingest(IngestBatch::new(documents).with_epoch(fixture.store.epoch_identity().unwrap()))
        .unwrap();
    let result = search_with_eligibility(
        &fixture.store,
        SearchKind::Hybrid,
        1,
        false,
        SearchMode::Exact,
    )
    .unwrap();
    let report = result.pools().reports[0];
    assert_eq!(result.metadata().rows, 1);
    assert_eq!(report.actual_tier, Some(ActualTier::Exact));
    assert_eq!(
        report.precision,
        crate::property_graph::query::completed::ScorePrecision::Original
    );
    assert!(report.candidate_count > 1 && report.candidate_count <= 69);
    assert_eq!(report.cross_scored_count, report.candidate_count);
    assert!(report.cross_score_complete);
    assert_ne!(report.normalization_version, 0);
    assert_ne!(report.rules_version, 0);
    // Store fusion refuses estimated scan scores; graph search preserves it.
    let error = search_with_eligibility(
        &fixture.store,
        SearchKind::Hybrid,
        1,
        false,
        SearchMode::Scan,
    )
    .err()
    .expect("estimated hybrid scores must refuse");
    assert!(error.to_string().contains("EstimatedVectorScore"));
}

#[test]
fn ze305_candidate_count_is_pre_top_k_population() {
    let fixture = SearchFixture::create();
    for mode in [SearchMode::Exact, SearchMode::Scan, SearchMode::Graph] {
        for (kind, count) in [(SearchKind::Vector, 4), (SearchKind::Lexical, 2)] {
            for k in [1, 20] {
                let result = search_with_eligibility(&fixture.store, kind, k, false, mode).unwrap();
                assert_eq!(result.metadata().rows, if k == 1 { 1 } else { count });
                assert_eq!(result.pools().reports[0].candidate_count, u64::from(count));
                let empty = search_with_eligibility(&fixture.store, kind, k, true, mode).unwrap();
                assert_eq!(empty.metadata().rows, 0);
                assert_eq!(empty.pools().reports[0].candidate_count, 0);
            }
        }
    }
}
