//! Independently authored application operator chains and complete PG13 columns.
use super::graph_search::*;
use zeppelin_embed::lifecycle::Store;
use zeppelin_embed::property_graph::query::completed::{CompletedGraphResult, GraphQueryOptions};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::{GraphName, GraphPlanBacking, GraphQueryPlan};
pub const APPLICATIONS: [&str; 3] = [
    "MATCH (item:Item)-[:ABOUT]->(project:Project) MATCH (item)-[:SUPPORTED_BY]->(chunk) MATCH (meeting)-[:HAS_CHUNK]->(chunk) RETURN item,item.name,meeting,meeting.name,chunk,chunk.excerpt ORDER BY meeting.timestamp DESC,item,meeting,chunk LIMIT 20",
    "CALL ze.vector_search([0,0],20,'exact') YIELD node AS chunk,distance MATCH (meeting)-[:HAS_CHUNK]->(chunk) MATCH (chunk)-[:MENTIONS]->(entity) RETURN chunk,distance,chunk.excerpt,meeting,meeting.name,entity,entity.name ORDER BY distance,chunk,meeting,entity",
    "MATCH (person:Person)-[:PARTICIPATED_IN]->(meeting)-[:FOR_PROJECT]->(project:Project) MATCH (meeting)-[:HAS_CHUNK]->(chunk) WITH collect(DISTINCT chunk) AS eligible CALL ze.vector_search([0,0],20,'exact',eligible) YIELD node AS chunk,distance MATCH (meeting)-[:HAS_CHUNK]->(chunk) RETURN chunk,distance,chunk.excerpt,meeting ORDER BY distance,chunk,meeting",
];
pub fn structured_application(store: &Store, shape: usize) -> CompletedGraphResult {
    let names: Vec<String> = [
        "Item",
        "Person",
        "ABOUT",
        "SUPPORTED_BY",
        "HAS_CHUNK",
        "MENTIONS",
        "PARTICIPATED_IN",
        "FOR_PROJECT",
        "name",
        "excerpt",
        "timestamp",
    ]
    .iter()
    .map(|s| (*s).into())
    .collect();
    let n = |i: usize| GraphName::new(&names[i]).unwrap();
    let types: Vec<Vec<_>> = (2..8).map(|i| vec![n(i)]).collect();
    let expand = |source, node, rel, t: usize, direction, pattern| OperatorKind::Expand {
        source: SlotId(source),
        node: SlotId(node),
        relationship: SlotId(rel),
        relationship_types: &types[t],
        direction,
        pattern: PatternId(pattern),
    };
    let coordinates = vec![ExprId(0), ExprId(1)];
    let mut expressions = vec![
        Expression::Literal(Literal::F64(0.0)),
        Expression::Literal(Literal::F64(0.0)),
        Expression::List(&coordinates),
        Expression::Literal(Literal::I64(20)),
    ];
    let mut kinds = vec![OperatorKind::Unit];
    let mut aggregates = vec![];
    let mut eager = vec![];
    if shape == 0 {
        expressions.clear();
        kinds.push(OperatorKind::ScanNodes {
            output: SlotId(0),
            label: Some(n(0)),
        });
        kinds.push(expand(0, 8, 9, 0, Direction::Outgoing, 0));
        kinds.push(expand(0, 2, 10, 1, Direction::Outgoing, 1));
        kinds.push(expand(2, 1, 11, 2, Direction::Incoming, 2));
    } else {
        let eligible = if shape == 2 {
            kinds.push(OperatorKind::ScanNodes {
                output: SlotId(8),
                label: Some(n(1)),
            });
            kinds.push(expand(8, 9, 10, 4, Direction::Outgoing, 0));
            kinds.push(expand(9, 11, 12, 5, Direction::Outgoing, 1));
            kinds.push(expand(9, 13, 14, 2, Direction::Outgoing, 2));
            expressions.push(Expression::Slot(SlotId(13)));
            expressions.push(Expression::Aggregate {
                operation: AggregateExpression::Collect { distinct: true },
                operand: Some(ExprId(4)),
            });
            aggregates.push(Projection {
                slot: SlotId(15),
                expression: ExprId(5),
            });
            kinds.push(OperatorKind::Aggregate {
                keys: &[],
                aggregates: &aggregates,
            });
            expressions.push(Expression::Slot(SlotId(15)));
            Some(ExprId(6))
        } else {
            None
        };
        eager.push(PlanNodeId(kinds.len() as u32));
        kinds.push(OperatorKind::Search {
            call: SearchCallId(0),
            request: SearchRequest::Vector {
                vector: ExprId(2),
                k: ExprId(3),
                mode: SearchMode::Exact,
                eligible,
                options: Default::default(),
            },
            outputs: SearchOutputs {
                node: Some(SlotId(2)),
                distance: Some(SlotId(3)),
                ..Default::default()
            },
        });
        kinds.push(expand(2, 1, 10, 2, Direction::Incoming, 3));
        if shape == 1 {
            kinds.push(expand(2, 4, 11, 3, Direction::Outgoing, 4));
        }
    }
    let fields: Vec<(u32, Option<usize>)> = match shape {
        0 => vec![
            (0, None),
            (0, Some(8)),
            (1, None),
            (1, Some(8)),
            (2, None),
            (2, Some(9)),
        ],
        1 => vec![
            (2, None),
            (3, None),
            (2, Some(9)),
            (1, None),
            (1, Some(8)),
            (4, None),
            (4, Some(8)),
        ],
        _ => vec![(2, None), (3, None), (2, Some(9)), (1, None)],
    };
    let mut projections = vec![];
    for (i, (slot, property)) in fields.into_iter().enumerate() {
        let mut expression = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(SlotId(slot)));
        if let Some(key) = property {
            expressions.push(Expression::Property {
                entity: expression,
                name: n(key),
            });
            expression = ExprId(expressions.len() as u32 - 1);
        }
        projections.push(Projection {
            slot: SlotId(20 + i as u32),
            expression,
        });
    }
    let sort_slots = if shape == 0 {
        vec![
            (1, Some(10), true),
            (0, None, false),
            (1, None, false),
            (2, None, false),
        ]
    } else {
        let mut v = vec![(3, None, false), (2, None, false), (1, None, false)];
        if shape == 1 {
            v.push((4, None, false));
        }
        v
    };
    let mut sort = vec![];
    for (slot, property, descending) in sort_slots {
        let mut expression = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(SlotId(slot)));
        if let Some(key) = property {
            expressions.push(Expression::Property {
                entity: expression,
                name: n(key),
            });
            expression = ExprId(expressions.len() as u32 - 1);
        }
        sort.push(SortKey {
            expression,
            descending,
        });
    }
    kinds.push(OperatorKind::Sort(&sort));
    kinds.push(OperatorKind::Project(&projections));
    let edges: Vec<Vec<_>> = (0..kinds.len())
        .map(|i| {
            if i == 0 {
                vec![]
            } else {
                vec![PlanNodeId(i as u32 - 1)]
            }
        })
        .collect();
    let operators: Vec<_> = kinds
        .into_iter()
        .enumerate()
        .map(|(i, kind)| Operator {
            inputs: &edges[i],
            kind,
        })
        .collect();
    let mut backing = GraphPlanBacking::default();
    for s in &names {
        backing.string(s).unwrap();
    }
    for t in &types {
        backing.vec(t).unwrap();
    }
    for edge in &edges {
        backing.vec(edge).unwrap();
    }
    backing.vec(&coordinates).unwrap();
    backing.vec(&aggregates).unwrap();
    backing.vec(&projections).unwrap();
    backing.vec(&sort).unwrap();
    store
        .graph_query(
            &control(),
            &GraphQueryOptions::default(),
            &GraphQueryPlan {
                operators: &operators,
                expressions: &expressions,
                parameters: &Vec::new(),
                eager_searches: &eager,
                root: PlanNodeId(operators.len() as u32 - 1),
                backing: &backing,
                bindings: &[],
                columns: if shape == 0 {
                    &[
                        "item",
                        "name",
                        "meeting",
                        "meeting_name",
                        "chunk",
                        "excerpt",
                    ]
                } else if shape == 1 {
                    &[
                        "chunk",
                        "distance",
                        "excerpt",
                        "meeting",
                        "name",
                        "entity",
                        "entity_name",
                    ]
                } else {
                    &["chunk", "distance", "excerpt", "meeting"]
                },
            },
        )
        .unwrap()
}
