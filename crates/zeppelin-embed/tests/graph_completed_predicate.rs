//! Independent ZE-126 review probes by /root/ze55_binder; retained for regression.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{QueryView, ValueContext, plan::*},
};
#[test]
fn independent_completed_scope_rejects_private_escape_target_and_prefix_list() {
    fn validate(description: PlanDescription<'_>) -> Result<(), PlanError> {
        let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
        let mut facts = vec![NodeFacts::default(); description.operators.len()];
        let mut regions = vec![
            RetainedRegion::slice(description.operators).unwrap(),
            RetainedRegion::slice(description.expressions).unwrap(),
            RetainedRegion::slice(&facts).unwrap(),
        ];
        for op in description.operators {
            if !op.inputs.is_empty() {
                regions.push(RetainedRegion::slice(op.inputs).unwrap());
            }
        }
        regions.sort_unstable();
        let mut union: Vec<RetainedRegion> = Vec::new();
        for region in regions {
            if let Some(last) = union.last_mut()
                && region.start() <= last.end()
            {
                *last = RetainedRegion::declared(
                    last.start(),
                    region.end().max(last.end()) - last.start(),
                )
                .unwrap();
            } else {
                union.push(region);
            }
        }
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(24 * 1024 * 1024),
            PlanBacking::vector(&union).unwrap(),
            &mut context,
        )
        .map(|_| ())
    }
    let expressions = [
        Expression::Slot(SlotId(9)),
        Expression::Unary {
            operation: UnaryExpression::Size,
            operand: ExprId(0),
        },
        Expression::Literal(Literal::I64(2)),
        Expression::Binary {
            operation: BinaryExpression::Comparison(
                zeppelin_embed::property_graph::query::Comparison::Equal,
            ),
            left: ExprId(1),
            right: ExprId(2),
        },
        Expression::Slot(SlotId(42)),
        Expression::Unary {
            operation: UnaryExpression::IsNotNull,
            operand: ExprId(4),
        },
        Expression::Binary {
            operation: BinaryExpression::And,
            left: ExprId(3),
            right: ExprId(5),
        },
        Expression::Slot(SlotId(8)),
        Expression::Unary {
            operation: UnaryExpression::IsNotNull,
            operand: ExprId(7),
        },
        Expression::Slot(SlotId(77)),
        Expression::Unary {
            operation: UnaryExpression::IsNotNull,
            operand: ExprId(9),
        },
        Expression::Slot(SlotId(7)),
        Expression::Unary {
            operation: UnaryExpression::IsNotNull,
            operand: ExprId(11),
        },
    ];
    for maximum in [0, 2] {
        for (predicate, private, prefix, escape, expected) in [
            (6, 42, false, false, Ok(())),
            (12, 42, false, false, Ok(())),
            (8, 42, false, false, Err(PlanError::Scope)),
            (10, 42, false, false, Err(PlanError::Scope)),
            (6, 42, true, false, Err(PlanError::Scope)),
            (6, 42, false, true, Err(PlanError::Scope)),
            (12, 7, false, false, Err(PlanError::Scope)),
            (12, 8, false, false, Err(PlanError::Scope)),
            (12, 9, false, false, Err(PlanError::Scope)),
        ] {
            let selected = if predicate == 6 {
                expressions[..7].to_vec()
            } else {
                vec![
                    expressions[predicate as usize - 1],
                    Expression::Unary {
                        operation: UnaryExpression::IsNotNull,
                        operand: ExprId(0),
                    },
                ]
            };
            let root_predicate = if predicate == 6 { 6 } else { 1 };
            let operators = [
                Operator {
                    inputs: &[],
                    kind: OperatorKind::Unit,
                },
                Operator {
                    inputs: &[PlanNodeId(0)],
                    kind: OperatorKind::ScanNodes {
                        output: SlotId(7),
                        label: None,
                    },
                },
                Operator {
                    inputs: &[PlanNodeId(1)],
                    kind: OperatorKind::BoundedExpand {
                        source: SlotId(7),
                        node: SlotId(8),
                        relationships: SlotId(9),
                        min: 0,
                        max: maximum,
                        relationship_types: &[],
                        direction: Direction::Either,
                        pattern: PatternId(0),
                        edge_predicate: prefix.then_some(EdgePredicate {
                            current_edge: SlotId(private),
                            expression: ExprId(root_predicate),
                        }),
                        completed_edge_predicate: (!prefix).then_some(CompletedEdgePredicate {
                            current_edge: SlotId(private),
                            expression: ExprId(root_predicate),
                        }),
                    },
                },
                Operator {
                    inputs: &[PlanNodeId(2)],
                    kind: OperatorKind::Filter(ExprId(5)),
                },
            ];
            assert_eq!(
                validate(PlanDescription {
                    operators: if escape { &operators } else { &operators[..3] },
                    expressions: &selected,
                    parameters: &[],
                    root: PlanNodeId(if escape { 3 } else { 2 }),
                    eager_searches: &[]
                }),
                expected,
                "max={maximum}, predicate={predicate}, private={private}, prefix={prefix}, escape={escape}"
            );
        }
    }
}
