#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::query::{QueryView, ValueContext};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
fn validate(description: PlanDescription<'_>) -> Result<(), PlanError> {
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut facts = vec![NodeFacts::default(); description.operators.len()];
    validate_plan(
        description,
        &mut facts,
        PlanFootprint::declared(24 * 1024 * 1024),
        &mut context,
    )
    .map(|_| ())
}
#[test]
fn typed_plan_revalidates_shared_expressions_after_with_scope() {
    let expressions = [
        Expression::Literal(Literal::Bool(true)),
        Expression::Slot(SlotId(70_000)),
    ];
    let original = [Projection {
        slot: SlotId(70_000),
        expression: ExprId(0),
    }];
    let renamed = [Projection {
        slot: SlotId(2),
        expression: ExprId(1),
    }];
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::Project(&original),
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Filter(ExprId(1)),
        },
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::With(&renamed),
        },
        Operator {
            inputs: &[PlanNodeId(3)],
            kind: OperatorKind::Filter(ExprId(1)),
        },
    ];
    macro_rules! describe {
        ($operators:expr) => {
            PlanDescription {
                operators: $operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(4),
                eager_searches: &[],
            }
        };
    }
    assert_eq!(validate(describe!(&operators)), Err(PlanError::Scope));
    operators[3].kind = OperatorKind::With(&original);
    assert_eq!(validate(describe!(&operators)), Ok(()));
    operators[0].inputs = &[PlanNodeId(4)];
    assert_eq!(validate(describe!(&operators)), Err(PlanError::Cycle));
    operators[0].inputs = &[];
    operators[2].inputs = &[PlanNodeId(99)];
    assert_eq!(validate(describe!(&operators)), Err(PlanError::Reference));
}

#[test]
fn expression_dags_and_parameter_bindings_reject_cycles_types_and_entities() {
    use zeppelin_embed::property_graph::NodeId;
    use zeppelin_embed::property_graph::query::QueryValue;
    let parameters = [Parameter {
        name: "flag",
        kinds: ValueKinds::BOOL.union(ValueKinds::NULL),
    }];
    let mut expressions = [
        Expression::Parameter(ParameterId(0)),
        Expression::Unary {
            operation: UnaryExpression::Not,
            operand: ExprId(0),
        },
    ];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::Filter(ExprId(1)),
        },
    ];
    macro_rules! describe {
        ($expressions:expr) => {
            PlanDescription {
                operators: &operators,
                expressions: $expressions,
                parameters: &parameters,
                root: PlanNodeId(1),
                eager_searches: &[],
            }
        };
    }
    assert_eq!(validate(describe!(&expressions)), Ok(()));
    expressions[0] = Expression::Unary {
        operation: UnaryExpression::Not,
        operand: ExprId(1),
    };
    assert_eq!(validate(describe!(&expressions)), Err(PlanError::Cycle));
    expressions[0] = Expression::Literal(Literal::I64(1));
    assert_eq!(validate(describe!(&expressions)), Err(PlanError::Type));
    expressions[0] = Expression::Parameter(ParameterId(1));
    assert_eq!(validate(describe!(&expressions)), Err(PlanError::Parameter));
    expressions[0] = Expression::Parameter(ParameterId(0));
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut facts = vec![NodeFacts::default(); operators.len()];
    let plan = validate_plan(
        describe!(&expressions),
        &mut facts,
        PlanFootprint::declared(24 * 1024 * 1024),
        &mut context,
    )
    .unwrap();
    assert_eq!(
        plan.validate_parameters(
            &[ParameterBinding {
                name: "flag",
                value: QueryValue::Null
            }],
            &mut context
        ),
        Ok(())
    );
    for bindings in [
        vec![],
        vec![ParameterBinding {
            name: "wrong",
            value: QueryValue::Bool(true),
        }],
        vec![ParameterBinding {
            name: "flag",
            value: QueryValue::I64(1),
        }],
        vec![
            ParameterBinding {
                name: "flag",
                value: QueryValue::Bool(true)
            };
            2
        ],
        vec![ParameterBinding {
            name: "flag",
            value: view.node(NodeId::new(1).unwrap()),
        }],
    ] {
        assert_eq!(
            plan.validate_parameters(&bindings, &mut context),
            Err(PlanError::Parameter)
        );
    }
}

#[test]
fn pattern_plans_pin_full_ids_path_bounds_and_optional_null_extension() {
    use zeppelin_embed::property_graph::NodeId;
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(100_000),
                id: NodeId::new((1 << 100) + 9).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::BoundedExpand {
                source: SlotId(100_000),
                node: SlotId(2),
                relationships: SlotId(3),
                min: 0,
                max: 16,
                direction: Direction::Incoming,
                completed_edge_predicate: None,
                edge_predicate: None,
                relationship_types: &[],
                pattern: PatternId(7),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1), PlanNodeId(2)],
            kind: OperatorKind::OptionalApply { predicate: None },
        },
    ];
    macro_rules! describe {
        () => {
            PlanDescription {
                operators: &operators,
                expressions: &[],
                parameters: &[],
                root: PlanNodeId(3),
                eager_searches: &[],
            }
        };
    }
    let mut facts = vec![NodeFacts::default(); 4];
    let plan = validate_plan(
        describe!(),
        &mut facts,
        PlanFootprint::declared(24 * 1024 * 1024),
        &mut context,
    )
    .unwrap();
    let facts = plan.facts(PlanNodeId(3)).unwrap();
    assert_eq!(facts.slot(SlotId(100_000)), Some(ValueKinds::NODE));
    assert_eq!(
        facts.slot(SlotId(2)),
        Some(ValueKinds::NODE.union(ValueKinds::NULL))
    );
    assert_eq!(
        facts.slot(SlotId(3)),
        Some(ValueKinds::LIST.union(ValueKinds::NULL))
    );
    assert!(facts.classification().reads());
    assert!(facts.barriers().optional());
    operators[2].kind = OperatorKind::BoundedExpand {
        source: SlotId(100_000),
        node: SlotId(2),
        relationships: SlotId(3),
        min: 0,
        max: 17,
        direction: Direction::Outgoing,
        completed_edge_predicate: None,
        edge_predicate: None,
        relationship_types: &[],
        pattern: PatternId(7),
    };
    assert_eq!(validate(describe!()), Err(PlanError::PathBound));
    operators[2].kind = OperatorKind::Expand {
        source: SlotId(999),
        node: SlotId(2),
        relationship: SlotId(3),
        direction: Direction::Either,
        relationship_types: &[],
        pattern: PatternId(7),
    };
    assert_eq!(validate(describe!()), Err(PlanError::Scope));
    operators[2].kind = OperatorKind::Expand {
        source: SlotId(100_000),
        node: SlotId(2),
        relationship: SlotId(100_000),
        direction: Direction::Either,
        relationship_types: &[],
        pattern: PatternId(7),
    };
    assert_eq!(validate(describe!()), Err(PlanError::Scope));
}

#[test]
fn typed_scalar_expression_forms_validate_entity_and_operand_kinds() {
    use zeppelin_embed::property_graph::query::{Arithmetic, Comparison, StringPredicate};
    use zeppelin_embed::property_graph::{GraphName, NodeId};
    let expressions = [
        Expression::Slot(SlotId(1)),
        Expression::Unary {
            operation: UnaryExpression::StoredText,
            operand: ExprId(0),
        },
        Expression::Unary {
            operation: UnaryExpression::NodeIdText,
            operand: ExprId(0),
        },
        Expression::Property {
            entity: ExprId(0),
            name: GraphName::new("p").unwrap(),
        },
        Expression::HasLabel {
            entity: ExprId(0),
            label: GraphName::new("L").unwrap(),
        },
        Expression::Unary {
            operation: UnaryExpression::Labels,
            operand: ExprId(0),
        },
        Expression::Unary {
            operation: UnaryExpression::Size,
            operand: ExprId(5),
        },
        Expression::Literal(Literal::I64(1)),
        Expression::Binary {
            operation: BinaryExpression::Arithmetic(Arithmetic::Add),
            left: ExprId(6),
            right: ExprId(7),
        },
        Expression::List(&[ExprId(2), ExprId(3), ExprId(4), ExprId(8)]),
        Expression::Binary {
            operation: BinaryExpression::Index,
            left: ExprId(9),
            right: ExprId(7),
        },
        Expression::Binary {
            operation: BinaryExpression::Comparison(Comparison::Equal),
            left: ExprId(10),
            right: ExprId(1),
        },
        Expression::Binary {
            operation: BinaryExpression::String(StringPredicate::Contains),
            left: ExprId(1),
            right: ExprId(2),
        },
        Expression::Binary {
            operation: BinaryExpression::And,
            left: ExprId(11),
            right: ExprId(12),
        },
    ];
    let projection = [Projection {
        slot: SlotId(9),
        expression: ExprId(13),
    }];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Project(&projection),
        },
    ];
    let description = PlanDescription {
        operators: &operators,
        expressions: &expressions,
        parameters: &[],
        root: PlanNodeId(2),
        eager_searches: &[],
    };
    assert_eq!(validate(description), Ok(()));
    let mut wrong = expressions;
    wrong[1] = Expression::Unary {
        operation: UnaryExpression::RelIdText,
        operand: ExprId(0),
    };
    assert_eq!(
        validate(PlanDescription {
            expressions: &wrong,
            ..description
        }),
        Err(PlanError::Type)
    );
    wrong[1] = Expression::Unary {
        operation: UnaryExpression::RelType,
        operand: ExprId(0),
    };
    assert_eq!(
        validate(PlanDescription {
            expressions: &wrong,
            ..description
        }),
        Err(PlanError::Type)
    );
    wrong[1] = Expression::Unary {
        operation: UnaryExpression::Negate,
        operand: ExprId(0),
    };
    assert_eq!(
        validate(PlanDescription {
            expressions: &wrong,
            ..description
        }),
        Err(PlanError::Type)
    );
}

#[test]
fn eager_search_obligations_survive_limit_zero_and_require_singleton_sources() {
    use zeppelin_embed::property_graph::NodeId;
    let expressions = [
        Expression::Literal(Literal::String("needle")),
        Expression::Literal(Literal::I64(3)),
        Expression::Slot(SlotId(1)),
        Expression::Aggregate {
            operation: AggregateExpression::Collect { distinct: true },
            operand: Some(ExprId(2)),
        },
    ];
    let aggregate = [Projection {
        slot: SlotId(2),
        expression: ExprId(3),
    }];
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Aggregate {
                keys: &[],
                aggregates: &aggregate,
            },
        },
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::Search {
                call: SearchCallId(0),
                request: SearchRequest::Text {
                    query: ExprId(0),
                    k: ExprId(1),
                    eligible: None,
                    options: Default::default(),
                },
                outputs: SearchOutputs {
                    node: Some(SlotId(3)),
                    score: Some(SlotId(4)),
                    ..SearchOutputs::default()
                },
            },
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::OffsetLimit {
                offset: 0,
                limit: Some(0),
            },
        },
    ];
    macro_rules! describe {
        ($eager:expr) => {
            PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(4),
                eager_searches: $eager,
            }
        };
    }
    assert_eq!(validate(describe!(&[PlanNodeId(3)])), Ok(()));
    assert_eq!(validate(describe!(&[])), Err(PlanError::Search));
    assert_eq!(
        validate(PlanDescription {
            operators: &operators[..4],
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(3),
            eager_searches: &[]
        }),
        Err(PlanError::Search)
    );
    assert_eq!(
        validate(describe!(&[PlanNodeId(3), PlanNodeId(3)])),
        Err(PlanError::Search)
    );
    operators[3].inputs = &[PlanNodeId(1)];
    assert_eq!(
        validate(describe!(&[PlanNodeId(3)])),
        Err(PlanError::Search)
    );
    operators[3].inputs = &[PlanNodeId(2)];
    operators[2].kind = OperatorKind::Aggregate {
        keys: &[Projection {
            slot: SlotId(1),
            expression: ExprId(2),
        }],
        aggregates: &aggregate,
    };
    assert_eq!(
        validate(describe!(&[PlanNodeId(3)])),
        Err(PlanError::Search)
    );
    operators[2].kind = OperatorKind::Project(&aggregate);
    assert_eq!(
        validate(describe!(&[PlanNodeId(3)])),
        Err(PlanError::Aggregate)
    );
}

#[test]
fn mutations_require_eager_input_and_reject_following_reads_or_search() {
    use zeppelin_embed::property_graph::{GraphName, NodeId};
    let expressions = [
        Expression::Slot(SlotId(1)),
        Expression::Literal(Literal::I64(7)),
    ];
    let changes = [Mutation::SetProperty {
        entity: ExprId(0),
        name: GraphName::new("p").unwrap(),
        value: ExprId(1),
    }];
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Eager,
        },
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::Mutate(&changes),
        },
        Operator {
            inputs: &[PlanNodeId(3)],
            kind: OperatorKind::OffsetLimit {
                offset: 0,
                limit: Some(0),
            },
        },
    ];
    macro_rules! describe {
        () => {
            PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(4),
                eager_searches: &[],
            }
        };
    }
    assert_eq!(validate(describe!()), Ok(()));
    let no_barrier = [
        operators[0],
        operators[1],
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Mutate(&changes),
        },
    ];
    assert_eq!(
        validate(PlanDescription {
            operators: &no_barrier,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(2),
            eager_searches: &[]
        }),
        Err(PlanError::Barrier)
    );
    operators[3].inputs = &[PlanNodeId(1)];
    assert_eq!(validate(describe!()), Err(PlanError::Barrier));
    operators[3].inputs = &[PlanNodeId(2)];
    operators[4].kind = OperatorKind::ScanNodes {
        output: SlotId(9),
        label: None,
    };
    assert_eq!(validate(describe!()), Err(PlanError::ReadAfterWrite));
    operators[4].kind = OperatorKind::Expand {
        source: SlotId(1),
        node: SlotId(2),
        relationship: SlotId(3),
        direction: Direction::Outgoing,
        relationship_types: &[],
        pattern: PatternId(2),
    };
    assert_eq!(validate(describe!()), Err(PlanError::ReadAfterWrite));
}

#[test]
fn correlated_optional_preserves_an_existing_relationship_binding() {
    use zeppelin_embed::property_graph::NodeId;
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Expand {
                source: SlotId(1),
                node: SlotId(2),
                relationship: SlotId(3),
                direction: Direction::Outgoing,
                relationship_types: &[],
                pattern: PatternId(1),
            },
        },
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::Expand {
                source: SlotId(2),
                node: SlotId(4),
                relationship: SlotId(5),
                direction: Direction::Outgoing,
                relationship_types: &[],
                pattern: PatternId(2),
            },
        },
        Operator {
            inputs: &[PlanNodeId(2), PlanNodeId(3)],
            kind: OperatorKind::OptionalApply { predicate: None },
        },
    ];
    assert_eq!(
        validate(PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(4),
            eager_searches: &[]
        }),
        Ok(())
    );
}

#[test]
fn relational_barriers_preserve_declared_order_until_a_join_or_distinct() {
    use zeppelin_embed::property_graph::NodeId;
    let expressions = [Expression::Slot(SlotId(1)), Expression::Slot(SlotId(2))];
    let sort = [
        SortKey {
            expression: ExprId(0),
            descending: false,
        },
        SortKey {
            expression: ExprId(1),
            descending: true,
        },
    ];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(2),
                id: NodeId::new(2).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1), PlanNodeId(2)],
            kind: OperatorKind::Join { predicate: None },
        },
        Operator {
            inputs: &[PlanNodeId(3)],
            kind: OperatorKind::Sort(&sort),
        },
        Operator {
            inputs: &[PlanNodeId(4)],
            kind: OperatorKind::OffsetLimit {
                offset: 2,
                limit: Some(5),
            },
        },
        Operator {
            inputs: &[PlanNodeId(5)],
            kind: OperatorKind::Eager,
        },
        Operator {
            inputs: &[PlanNodeId(6)],
            kind: OperatorKind::Distinct,
        },
    ];
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut facts = vec![NodeFacts::default(); operators.len()];
    let plan = validate_plan(
        PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(7),
            eager_searches: &[],
        },
        &mut facts,
        PlanFootprint::declared(24 * 1024 * 1024),
        &mut context,
    )
    .unwrap();
    assert!(!plan.facts(PlanNodeId(3)).unwrap().ordered());
    assert!(plan.facts(PlanNodeId(6)).unwrap().ordered());
    assert!(!plan.facts(PlanNodeId(7)).unwrap().ordered());
    assert!(
        plan.facts(PlanNodeId(7))
            .unwrap()
            .barriers()
            .contains(Barrier::Distinct)
    );
    assert!(
        plan.facts(PlanNodeId(7))
            .unwrap()
            .barriers()
            .contains(Barrier::Sort)
    );
}

#[test]
fn mutation_items_bind_new_entities_in_order_and_validate_every_rhs() {
    use zeppelin_embed::property_graph::GraphName;
    let labels = [GraphName::new("L").unwrap()];
    let expressions = [
        Expression::Slot(SlotId(1)),
        Expression::Slot(SlotId(2)),
        Expression::Slot(SlotId(3)),
        Expression::Literal(Literal::I64(1)),
    ];
    let items = [
        Mutation::CreateNode {
            output: SlotId(1),
            labels: &labels,
        },
        Mutation::CreateNode {
            output: SlotId(2),
            labels: &[],
        },
        Mutation::CreateRelationship {
            output: SlotId(3),
            source: ExprId(0),
            target: ExprId(1),
            relationship_type: GraphName::new("R").unwrap(),
        },
        Mutation::SetProperty {
            entity: ExprId(2),
            name: GraphName::new("p").unwrap(),
            value: ExprId(3),
        },
        Mutation::RemoveProperty {
            entity: ExprId(2),
            name: GraphName::new("p").unwrap(),
        },
        Mutation::SetLabel {
            entity: ExprId(0),
            label: GraphName::new("M").unwrap(),
            present: true,
        },
        Mutation::Delete {
            entity: ExprId(2),
            detach: false,
        },
    ];
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::Eager,
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Mutate(&items),
        },
    ];
    macro_rules! describe {
        () => {
            PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(2),
                eager_searches: &[],
            }
        };
    }
    assert_eq!(validate(describe!()), Ok(()));
    let mut invalid = items;
    invalid.swap(0, 2);
    operators[2].kind = OperatorKind::Mutate(&invalid);
    assert_eq!(validate(describe!()), Err(PlanError::Scope));
}

#[test]
fn detach_delete_accepts_node_relationship_and_null_targets() {
    use zeppelin_embed::property_graph::GraphName;

    let validate_items = |expressions: &[Expression<'_>], items: &[Mutation<'_>]| {
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &[PlanNodeId(0)],
                kind: OperatorKind::Eager,
            },
            Operator {
                inputs: &[PlanNodeId(1)],
                kind: OperatorKind::Mutate(items),
            },
        ];
        validate(PlanDescription {
            operators: &operators,
            expressions,
            parameters: &[],
            root: PlanNodeId(2),
            eager_searches: &[],
        })
    };
    let validate_node = |detach| {
        let expressions = [Expression::Slot(SlotId(1))];
        let items = [
            Mutation::CreateNode {
                output: SlotId(1),
                labels: &[],
            },
            Mutation::Delete {
                entity: ExprId(0),
                detach,
            },
        ];
        validate_items(&expressions, &items)
    };
    let validate_relationship = |detach| {
        let expressions = [
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(2)),
            Expression::Slot(SlotId(3)),
        ];
        let items = [
            Mutation::CreateNode {
                output: SlotId(1),
                labels: &[],
            },
            Mutation::CreateNode {
                output: SlotId(2),
                labels: &[],
            },
            Mutation::CreateRelationship {
                output: SlotId(3),
                source: ExprId(0),
                target: ExprId(1),
                relationship_type: GraphName::new("R").unwrap(),
            },
            Mutation::Delete {
                entity: ExprId(2),
                detach,
            },
        ];
        validate_items(&expressions, &items)
    };

    for detach in [false, true] {
        assert_eq!(validate_node(detach), Ok(()));
        assert_eq!(validate_relationship(detach), Ok(()));

        let null = [Expression::Literal(Literal::Null)];
        let delete_null = [Mutation::Delete {
            entity: ExprId(0),
            detach,
        }];
        assert_eq!(validate_items(&null, &delete_null), Ok(()));

        for expression in [
            Expression::Literal(Literal::Bool(false)),
            Expression::Literal(Literal::I64(1)),
            Expression::Literal(Literal::String("not an entity")),
        ] {
            let scalar = [expression];
            let delete_scalar = [Mutation::Delete {
                entity: ExprId(0),
                detach,
            }];
            assert_eq!(
                validate_items(&scalar, &delete_scalar),
                Err(PlanError::Type)
            );
        }

        let list = [
            Expression::Literal(Literal::I64(1)),
            Expression::List(&[ExprId(0)]),
        ];
        let delete_list = [Mutation::Delete {
            entity: ExprId(1),
            detach,
        }];
        assert_eq!(validate_items(&list, &delete_list), Err(PlanError::Type));
    }
}

#[test]
fn search_plans_preserve_request_intent_and_nullable_hybrid_outputs() {
    let expressions = [
        Expression::Literal(Literal::F64(1.0)),
        Expression::List(&[ExprId(0)]),
        Expression::Literal(Literal::String("query")),
        Expression::Literal(Literal::I64(4)),
    ];
    for mode in [
        SearchMode::Default,
        SearchMode::Auto,
        SearchMode::Exact,
        SearchMode::Scan,
    ] {
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &[PlanNodeId(0)],
                kind: OperatorKind::Search {
                    call: SearchCallId(0),
                    request: SearchRequest::Hybrid {
                        vector: ExprId(1),
                        text: ExprId(2),
                        k: ExprId(3),
                        mode,
                        eligible: None,
                        options: Default::default(),
                    },
                    outputs: SearchOutputs {
                        node: Some(SlotId(1)),
                        distance: None,
                        score: Some(SlotId(2)),
                        vector_distance: Some(SlotId(3)),
                        lexical_score: Some(SlotId(4)),
                    },
                },
            },
        ];
        assert_eq!(
            validate(PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(1),
                eager_searches: &[PlanNodeId(1)],
            }),
            Ok(())
        );
    }
}

#[test]
fn search_plan_outputs_reject_empty_wrong_duplicate_and_input_colliding_slots() {
    let expressions = [
        Expression::Literal(Literal::String("query")),
        Expression::Literal(Literal::I64(1)),
        Expression::Literal(Literal::I64(7)),
    ];
    let prior = [Projection {
        slot: SlotId(9),
        expression: ExprId(2),
    }];
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::Project(&prior),
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Search {
                call: SearchCallId(0),
                request: SearchRequest::Text {
                    query: ExprId(0),
                    k: ExprId(1),
                    eligible: None,
                    options: Default::default(),
                },
                outputs: SearchOutputs::default(),
            },
        },
    ];
    macro_rules! describe {
        () => {
            PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(2),
                eager_searches: &[PlanNodeId(2)],
            }
        };
    }
    assert_eq!(validate(describe!()), Err(PlanError::Search));
    operators[2].kind = OperatorKind::Search {
        call: SearchCallId(0),
        request: SearchRequest::Text {
            query: ExprId(0),
            k: ExprId(1),
            eligible: None,
            options: Default::default(),
        },
        outputs: SearchOutputs {
            distance: Some(SlotId(10)),
            ..SearchOutputs::default()
        },
    };
    assert_eq!(validate(describe!()), Err(PlanError::Search));
    operators[2].kind = OperatorKind::Search {
        call: SearchCallId(0),
        request: SearchRequest::Text {
            query: ExprId(0),
            k: ExprId(1),
            eligible: None,
            options: Default::default(),
        },
        outputs: SearchOutputs {
            node: Some(SlotId(10)),
            score: Some(SlotId(10)),
            ..SearchOutputs::default()
        },
    };
    assert_eq!(validate(describe!()), Err(PlanError::Scope));
    operators[2].kind = OperatorKind::Search {
        call: SearchCallId(0),
        request: SearchRequest::Text {
            query: ExprId(0),
            k: ExprId(1),
            eligible: None,
            options: Default::default(),
        },
        outputs: SearchOutputs {
            node: Some(SlotId(9)),
            ..SearchOutputs::default()
        },
    };
    assert_eq!(validate(describe!()), Err(PlanError::Scope));
}

#[test]
fn search_sources_and_evaluated_bounds_never_clamp_invalid_requests() {
    assert_eq!(
        (
            SearchBounds::new(4096, 65_536).unwrap().k(),
            SearchBounds::new(1, 0).unwrap().candidate_window()
        ),
        (4096, 0)
    );
    for (k, window) in [
        (0, 0),
        (-1, 1),
        (4097, 65_536),
        (i64::MAX, 1),
        (1, 65_537),
        (1, u64::MAX),
    ] {
        assert_eq!(SearchBounds::new(k, window), Err(PlanError::Search));
    }
    let expressions = [
        Expression::Literal(Literal::F64(1.0)),
        Expression::List(&[ExprId(0)]),
        Expression::Literal(Literal::String("query")),
        Expression::Literal(Literal::I64(4096)),
    ];
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::Search {
                call: SearchCallId(0),
                request: SearchRequest::Hybrid {
                    vector: ExprId(1),
                    text: ExprId(2),
                    k: ExprId(3),
                    mode: SearchMode::Exact,
                    eligible: None,
                    options: Default::default(),
                },
                outputs: SearchOutputs {
                    node: Some(SlotId(1)),
                    score: Some(SlotId(2)),
                    ..SearchOutputs::default()
                },
            },
        },
    ];
    macro_rules! describe {
        () => {
            PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(1),
                eager_searches: &[PlanNodeId(1)],
            }
        };
    }
    assert_eq!(validate(describe!()), Ok(()));
    operators[1].kind = OperatorKind::Search {
        call: SearchCallId(0),
        request: SearchRequest::Vector {
            vector: ExprId(1),
            k: ExprId(3),
            mode: SearchMode::Auto,
            eligible: None,
            options: Default::default(),
        },
        outputs: SearchOutputs {
            node: Some(SlotId(1)),
            distance: Some(SlotId(2)),
            ..SearchOutputs::default()
        },
    };
    assert_eq!(validate(describe!()), Err(PlanError::Unreachable)); // Unused text expression is not silently retained.
    let short = [expressions[0], expressions[1], expressions[3]];
    operators[1].kind = OperatorKind::Search {
        call: SearchCallId(0),
        request: SearchRequest::Vector {
            vector: ExprId(1),
            k: ExprId(2),
            mode: SearchMode::Auto,
            eligible: None,
            options: Default::default(),
        },
        outputs: SearchOutputs {
            node: Some(SlotId(1)),
            distance: Some(SlotId(2)),
            ..SearchOutputs::default()
        },
    };
    assert_eq!(
        validate(PlanDescription {
            expressions: &short,
            ..describe!()
        }),
        Ok(())
    );
}

#[test]
fn structured_lookup_uses_distinct_full_ids_and_symbolic_keys() {
    use zeppelin_embed::property_graph::{EntityKind, GraphName, RelId};
    let expressions = [
        Expression::Literal(Literal::String("external/key")),
        Expression::Slot(SlotId(2)),
        Expression::Unary {
            operation: UnaryExpression::RelType,
            operand: ExprId(1),
        },
    ];
    let projection = [Projection {
        slot: SlotId(3),
        expression: ExprId(2),
    }];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupKey {
                output: SlotId(1),
                namespace: GraphName::new("app").unwrap(),
                key: ExprId(0),
                kind: EntityKind::Node,
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::LookupRelationship {
                output: SlotId(2),
                id: RelId::new((1 << 100) + 9).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::Project(&projection),
        },
    ];
    assert_eq!(
        validate(PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(3),
            eager_searches: &[]
        }),
        Ok(())
    );
}

#[test]
fn plan_bounds_charge_actual_declarations_and_hide_unused_fact_capacity() {
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let operators = [Operator {
        inputs: &[],
        kind: OperatorKind::Unit,
    }];
    let description = PlanDescription {
        operators: &operators,
        expressions: &[],
        parameters: &[],
        root: PlanNodeId(0),
        eager_searches: &[],
    };
    let mut facts = vec![NodeFacts::default(); 2];
    for declared in [0, 24 * 1024 * 1024 + 1] {
        assert!(matches!(
            validate_plan(
                description,
                &mut facts,
                PlanFootprint::declared(declared),
                &mut context
            ),
            Err(PlanError::Footprint)
        ));
    }
    let plan = validate_plan(
        description,
        &mut facts,
        PlanFootprint::declared(24 * 1024 * 1024),
        &mut context,
    )
    .unwrap();
    assert!(plan.facts(PlanNodeId(1)).is_none());
    assert_eq!(plan.facts(PlanNodeId(0)).unwrap().width(), 0);
    assert_eq!(plan.footprint().retained_bytes(), 24 * 1024 * 1024);
    assert_eq!(plan.description().root, PlanNodeId(0));
}

#[test]
fn plan_hard_caps_accept_exact_nodes_depth_and_scope_width() {
    let edges: Vec<_> = (0..2047)
        .map(|index| [PlanNodeId(2 * index + 1), PlanNodeId(2 * index + 2)])
        .collect();
    let mut operators: Vec<_> = (0..4095)
        .map(|index| {
            if index < 2047 {
                Operator {
                    inputs: &edges[index],
                    kind: OperatorKind::Join { predicate: None },
                }
            } else {
                Operator {
                    inputs: &[],
                    kind: OperatorKind::Unit,
                }
            }
        })
        .collect();
    operators.push(Operator {
        inputs: &[PlanNodeId(0)],
        kind: OperatorKind::Collect,
    });
    assert_eq!(
        validate(PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(4095),
            eager_searches: &[]
        }),
        Ok(())
    );
    operators.push(Operator {
        inputs: &[PlanNodeId(4095)],
        kind: OperatorKind::Collect,
    });
    assert_eq!(
        validate(PlanDescription {
            operators: &operators,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(4096),
            eager_searches: &[]
        }),
        Err(PlanError::Limit)
    );
    let links: Vec<_> = (0..64).map(|i| [PlanNodeId(i)]).collect();
    let chain: Vec<_> = (0..65)
        .map(|i| Operator {
            inputs: if i == 0 { &[] } else { &links[i - 1] },
            kind: if i == 0 {
                OperatorKind::Unit
            } else {
                OperatorKind::Collect
            },
        })
        .collect();
    assert_eq!(
        validate(PlanDescription {
            operators: &chain[..64],
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(63),
            eager_searches: &[]
        }),
        Ok(())
    );
    assert_eq!(
        validate(PlanDescription {
            operators: &chain,
            expressions: &[],
            parameters: &[],
            root: PlanNodeId(64),
            eager_searches: &[]
        }),
        Err(PlanError::Limit)
    );
    let expression_chain: Vec<_> = (0..65)
        .map(|i| {
            if i == 0 {
                Expression::Literal(Literal::Bool(true))
            } else {
                Expression::Unary {
                    operation: UnaryExpression::Not,
                    operand: ExprId(i - 1),
                }
            }
        })
        .collect();
    let mut expression_ops = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::Filter(ExprId(63)),
        },
    ];
    assert_eq!(
        validate(PlanDescription {
            operators: &expression_ops,
            expressions: &expression_chain[..64],
            parameters: &[],
            root: PlanNodeId(1),
            eager_searches: &[]
        }),
        Ok(())
    );
    expression_ops[1].kind = OperatorKind::Filter(ExprId(64));
    assert_eq!(
        validate(PlanDescription {
            operators: &expression_ops,
            expressions: &expression_chain,
            parameters: &[],
            root: PlanNodeId(1),
            eager_searches: &[]
        }),
        Err(PlanError::Limit)
    );
    let columns: Vec<_> = (0..257)
        .map(|i| Projection {
            slot: SlotId(1_000_000 + i),
            expression: ExprId(0),
        })
        .collect();
    for (width, expected) in [(256, Ok(())), (257, Err(PlanError::Limit))] {
        let ops = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &[PlanNodeId(0)],
                kind: OperatorKind::With(&columns[..width]),
            },
        ];
        assert_eq!(
            validate(PlanDescription {
                operators: &ops,
                expressions: &expression_chain[..1],
                parameters: &[],
                root: PlanNodeId(1),
                eager_searches: &[]
            }),
            expected
        );
    }
}

#[test]
fn aliased_large_literal_backing_is_charged_once() {
    let text = "x".repeat(8 * 1024 * 1024);
    let expressions = [Expression::Literal(Literal::String(&text)); 3];
    let projection = [
        Projection {
            slot: SlotId(1),
            expression: ExprId(0),
        },
        Projection {
            slot: SlotId(2),
            expression: ExprId(1),
        },
        Projection {
            slot: SlotId(3),
            expression: ExprId(2),
        },
    ];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::Project(&projection),
        },
    ];
    let description = PlanDescription {
        operators: &operators,
        expressions: &expressions,
        parameters: &[],
        root: PlanNodeId(1),
        eager_searches: &[],
    };
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut facts = vec![NodeFacts::default(); 2];
    assert!(
        validate_plan(
            description,
            &mut facts,
            PlanFootprint::declared(9 * 1024 * 1024),
            &mut context
        )
        .is_ok()
    );
}

fn validate_plan<'a, 'facts>(
    description: PlanDescription<'a>,
    facts: &'facts mut [NodeFacts],
    footprint: PlanFootprint,
    context: &mut ValueContext<'_>,
) -> Result<GraphPlan<'a, 'facts>, PlanError> {
    let regions = retained_regions(description, facts);
    GraphPlan::validate(
        description,
        facts,
        footprint,
        PlanBacking::vector(&regions)?,
        context,
    )
}
fn retained_regions(description: PlanDescription<'_>, facts: &[NodeFacts]) -> Vec<RetainedRegion> {
    fn add<T>(regions: &mut Vec<RetainedRegion>, values: &[T]) {
        let region = RetainedRegion::slice(values).unwrap();
        if region.start() != region.end() {
            regions.push(region);
        }
    }
    let mut regions = Vec::new();
    add(&mut regions, description.operators);
    add(&mut regions, description.expressions);
    add(&mut regions, description.parameters);
    add(&mut regions, description.eager_searches);
    add(&mut regions, facts);
    for op in description.operators {
        add(&mut regions, op.inputs);
        match op.kind {
            OperatorKind::Project(items) | OperatorKind::With(items) => add(&mut regions, items),
            OperatorKind::Aggregate { keys, aggregates } => {
                add(&mut regions, keys);
                add(&mut regions, aggregates);
            }
            OperatorKind::Sort(keys) => add(&mut regions, keys),
            OperatorKind::ScanNodes {
                label: Some(name), ..
            }
            | OperatorKind::LookupKey {
                namespace: name, ..
            } => add(&mut regions, name.as_str().as_bytes()),
            OperatorKind::Expand {
                relationship_types, ..
            }
            | OperatorKind::BoundedExpand {
                relationship_types, ..
            } => {
                add(&mut regions, relationship_types);
                for name in relationship_types {
                    add(&mut regions, name.as_str().as_bytes());
                }
            }
            OperatorKind::Mutate(items) => {
                add(&mut regions, items);
                for item in items {
                    match item {
                        Mutation::SetProperty { name, .. }
                        | Mutation::RemoveProperty { name, .. }
                        | Mutation::SetLabel { label: name, .. }
                        | Mutation::CreateRelationship {
                            relationship_type: name,
                            ..
                        } => add(&mut regions, name.as_str().as_bytes()),
                        Mutation::CreateNode { labels, .. } => {
                            add(&mut regions, labels);
                            for label in *labels {
                                add(&mut regions, label.as_str().as_bytes());
                            }
                        }
                        Mutation::Delete { .. } => {}
                    }
                }
            }
            _ => {}
        }
    }
    for expression in description.expressions {
        match expression {
            Expression::Literal(Literal::String(text)) => add(&mut regions, text.as_bytes()),
            Expression::List(items) => add(&mut regions, items),
            Expression::Property { name, .. } | Expression::HasLabel { label: name, .. } => {
                add(&mut regions, name.as_str().as_bytes())
            }
            _ => {}
        }
    }
    for parameter in description.parameters {
        add(&mut regions, parameter.name.as_bytes());
    }
    regions.sort_unstable();
    let mut union: Vec<RetainedRegion> = Vec::new();
    for region in regions {
        if let Some(last) = union.last_mut()
            && region.start() <= last.end()
        {
            *last =
                RetainedRegion::declared(last.start(), region.end().max(last.end()) - last.start())
                    .unwrap();
        } else {
            union.push(region);
        }
    }
    union
}

#[test]
fn shared_relationship_origins_survive_lookup_joins_and_slot_renames() {
    use zeppelin_embed::property_graph::{NodeId, RelId};
    let expand = |node, pattern| OperatorKind::Expand {
        source: SlotId(1),
        node: SlotId(node),
        relationship: SlotId(3),
        direction: Direction::Outgoing,
        relationship_types: &[],
        pattern: PatternId(pattern),
    };
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: expand(2, 7),
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupRelationship {
                output: SlotId(3),
                id: RelId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(3), PlanNodeId(2)],
            kind: OperatorKind::Join { predicate: None },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: expand(4, 7),
        },
        Operator {
            inputs: &[PlanNodeId(4), PlanNodeId(5)],
            kind: OperatorKind::Join { predicate: None },
        },
    ];
    macro_rules! describe {
        () => {
            PlanDescription {
                operators: &operators,
                expressions: &[],
                parameters: &[],
                root: PlanNodeId(6),
                eager_searches: &[],
            }
        };
    }
    assert_eq!(validate(describe!()), Err(PlanError::Scope));
    operators[5].kind = expand(4, 8);
    assert_eq!(validate(describe!()), Ok(()));
}

#[test]
fn inner_join_accepts_and_narrows_compatible_optional_bindings() {
    use zeppelin_embed::property_graph::NodeId;
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Expand {
                source: SlotId(1),
                node: SlotId(2),
                relationship: SlotId(3),
                direction: Direction::Outgoing,
                relationship_types: &[],
                pattern: PatternId(7),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1), PlanNodeId(2)],
            kind: OperatorKind::OptionalApply { predicate: None },
        },
        Operator {
            inputs: &[PlanNodeId(3), PlanNodeId(2)],
            kind: OperatorKind::Join { predicate: None },
        },
    ];
    let description = PlanDescription {
        operators: &operators,
        expressions: &[],
        parameters: &[],
        root: PlanNodeId(4),
        eager_searches: &[],
    };
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut facts = vec![NodeFacts::default(); 5];
    let plan = validate_plan(
        description,
        &mut facts,
        PlanFootprint::declared(24 * 1024 * 1024),
        &mut context,
    )
    .unwrap();
    assert_eq!(
        plan.facts(PlanNodeId(4)).unwrap().slot(SlotId(2)),
        Some(ValueKinds::NODE)
    );
    assert_eq!(
        plan.facts(PlanNodeId(4)).unwrap().slot(SlotId(3)),
        Some(ValueKinds::REL)
    );
}

#[test]
fn retained_region_proof_rejects_gaps_overlap_overflow_and_uncharged_capacity() {
    let operators = [Operator {
        inputs: &[],
        kind: OperatorKind::Unit,
    }];
    let description = PlanDescription {
        operators: &operators,
        expressions: &[],
        parameters: &[],
        root: PlanNodeId(0),
        eager_searches: &[],
    };
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut facts = Vec::with_capacity(8);
    facts.push(NodeFacts::default());
    let mut regions = vec![
        RetainedRegion::slice(&operators).unwrap(),
        RetainedRegion::vector(&facts).unwrap(),
    ];
    regions.sort_unstable();
    let charged = |regions: &Vec<RetainedRegion>| {
        VALIDATION_SCRATCH_BYTES
            + regions.capacity() * std::mem::size_of::<RetainedRegion>()
            + regions.iter().map(|r| r.end() - r.start()).sum::<usize>()
    };
    let declared = charged(&regions);
    assert!(
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(declared),
            PlanBacking::vector(&regions).unwrap(),
            &mut context
        )
        .is_ok()
    );
    assert!(matches!(
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(declared - 1),
            PlanBacking::vector(&regions).unwrap(),
            &mut context
        ),
        Err(PlanError::Footprint)
    ));
    let full = regions[0];
    let split = full.start() + 1;
    let mut adjacent = regions.clone();
    adjacent[0] = RetainedRegion::declared(full.start(), 1).unwrap();
    adjacent.insert(
        1,
        RetainedRegion::declared(split, full.end() - split).unwrap(),
    );
    assert!(
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(charged(&adjacent)),
            PlanBacking::vector(&adjacent).unwrap(),
            &mut context
        )
        .is_ok()
    );
    let mut bad = adjacent.clone();
    bad[1] = RetainedRegion::declared(split - 1, full.end() - split + 1).unwrap();
    assert!(matches!(
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(24 * 1024 * 1024),
            PlanBacking::vector(&bad).unwrap(),
            &mut context
        ),
        Err(PlanError::Footprint)
    ));
    bad[1] = RetainedRegion::declared(split + 1, full.end() - split - 1).unwrap();
    assert!(matches!(
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(24 * 1024 * 1024),
            PlanBacking::vector(&bad).unwrap(),
            &mut context
        ),
        Err(PlanError::Footprint)
    ));
    regions.reverse();
    assert!(matches!(
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(24 * 1024 * 1024),
            PlanBacking::vector(&regions).unwrap(),
            &mut context
        ),
        Err(PlanError::Footprint)
    ));
    assert_eq!(
        RetainedRegion::declared(usize::MAX, 1),
        Err(PlanError::Footprint)
    );
    assert!(matches!(
        PlanBacking::new(&regions, 0),
        Err(PlanError::Footprint)
    ));
    assert!(matches!(
        PlanBacking::new(&regions, usize::MAX),
        Err(PlanError::Footprint)
    ));
    let mut self_overlap = Vec::with_capacity(4);
    self_overlap.push(RetainedRegion::vector(&self_overlap).unwrap());
    assert!(matches!(
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(24 * 1024 * 1024),
            PlanBacking::vector(&self_overlap).unwrap(),
            &mut context
        ),
        Err(PlanError::Footprint)
    ));
}

#[test]
fn renamed_origins_remain_checked_through_multiple_patterns() {
    use zeppelin_embed::property_graph::NodeId;
    let expressions = [Expression::Slot(SlotId(3))];
    let rename = [Projection {
        slot: SlotId(99),
        expression: ExprId(0),
    }];
    let mut operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::LookupNode {
                output: SlotId(1),
                id: NodeId::new(1).unwrap(),
            },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Expand {
                source: SlotId(1),
                node: SlotId(2),
                relationship: SlotId(3),
                direction: Direction::Outgoing,
                relationship_types: &[],
                pattern: PatternId(7),
            },
        },
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::With(&rename),
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Expand {
                source: SlotId(1),
                node: SlotId(4),
                relationship: SlotId(99),
                direction: Direction::Outgoing,
                relationship_types: &[],
                pattern: PatternId(8),
            },
        },
        Operator {
            inputs: &[PlanNodeId(3), PlanNodeId(4)],
            kind: OperatorKind::Join { predicate: None },
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::Expand {
                source: SlotId(1),
                node: SlotId(5),
                relationship: SlotId(99),
                direction: Direction::Outgoing,
                relationship_types: &[],
                pattern: PatternId(7),
            },
        },
        Operator {
            inputs: &[PlanNodeId(5), PlanNodeId(6)],
            kind: OperatorKind::Join { predicate: None },
        },
    ];
    macro_rules! describe {
        () => {
            PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(7),
                eager_searches: &[],
            }
        };
    }
    assert_eq!(validate(describe!()), Err(PlanError::Scope));
    operators[6].kind = OperatorKind::Expand {
        source: SlotId(1),
        node: SlotId(5),
        relationship: SlotId(99),
        direction: Direction::Outgoing,
        relationship_types: &[],
        pattern: PatternId(9),
    };
    assert_eq!(validate(describe!()), Ok(()));
}

#[test]
fn typed_pattern_contract_preserves_or_types_and_private_edge_scope() {
    use zeppelin_embed::property_graph::GraphName;
    let alternatives = [
        GraphName::new("FIRST").unwrap(),
        GraphName::new("SECOND").unwrap(),
        GraphName::new("FIRST").unwrap(),
    ];
    let expressions = [
        Expression::Literal(Literal::I64(7)),
        Expression::Slot(SlotId(u32::MAX)),
        Expression::Property {
            entity: ExprId(1),
            name: GraphName::new("weight").unwrap(),
        },
        Expression::Slot(SlotId(900)),
        Expression::Binary {
            operation: BinaryExpression::Comparison(
                zeppelin_embed::property_graph::query::Comparison::Equal,
            ),
            left: ExprId(2),
            right: ExprId(3),
        },
    ];
    let projection = [Projection {
        slot: SlotId(900),
        expression: ExprId(0),
    }];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &[PlanNodeId(0)],
            kind: OperatorKind::Project(&projection),
        },
        Operator {
            inputs: &[PlanNodeId(1)],
            kind: OperatorKind::ScanNodes {
                output: SlotId(1000),
                label: None,
            },
        },
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::BoundedExpand {
                source: SlotId(1000),
                node: SlotId(2000),
                relationships: SlotId(3000),
                min: 0,
                max: 16,
                direction: Direction::Outgoing,
                relationship_types: &alternatives,
                pattern: PatternId(4),
                completed_edge_predicate: None,
                edge_predicate: Some(EdgePredicate {
                    current_edge: SlotId(u32::MAX),
                    expression: ExprId(4),
                }),
            },
        },
    ];
    let description = PlanDescription {
        operators: &operators,
        expressions: &expressions,
        parameters: &[],
        root: PlanNodeId(3),
        eager_searches: &[],
    };
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut facts = vec![NodeFacts::default(); operators.len()];
    let plan = validate_plan(
        description,
        &mut facts,
        PlanFootprint::declared(1024 * 1024),
        &mut context,
    )
    .unwrap();
    let output = plan.facts(PlanNodeId(3)).unwrap();
    assert_eq!(output.width(), 4);
    assert_eq!(output.slot(SlotId(u32::MAX)), None);
    for (ordinal, id, kind) in [
        (0, 900, ValueKinds::I64),
        (1, 1000, ValueKinds::NODE),
        (2, 2000, ValueKinds::NODE),
        (3, 3000, ValueKinds::LIST),
    ] {
        assert_eq!(output.slot_at(ordinal), Some((SlotId(id), kind)));
    }
    assert_eq!(output.slot_at(4), None);
    assert_eq!(output.slot_at(usize::MAX), None);
    let (relationship_types, edge_predicate) = match operators[3].kind {
        OperatorKind::BoundedExpand {
            relationship_types,
            edge_predicate,
            ..
        } => Some((relationship_types, edge_predicate)),
        _ => None,
    }
    .unwrap();
    assert_eq!(
        relationship_types
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>(),
        ["FIRST", "SECOND", "FIRST"]
    );
    assert_eq!(edge_predicate.unwrap().current_edge, SlotId(u32::MAX));
}

fn edge_scope_operators<'a>(
    alternatives: &'a [zeppelin_embed::property_graph::GraphName<'a>],
    current_edge: SlotId,
    predicate: ExprId,
    max: u8,
) -> [Operator<'a>; 3] {
    [
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
                max,
                relationship_types: alternatives,
                direction: Direction::Either,
                pattern: PatternId(0),
                completed_edge_predicate: None,
                edge_predicate: Some(EdgePredicate {
                    current_edge,
                    expression: predicate,
                }),
            },
        },
    ]
}

#[test]
fn typed_pattern_predicate_rejects_aliases_foreign_scope_and_output_escape() {
    let expressions = [Expression::Literal(Literal::Bool(true))];
    for private in [7, 8, 9] {
        let operators = edge_scope_operators(&[], SlotId(private), ExprId(0), 1);
        assert_eq!(
            validate(PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(2),
                eager_searches: &[]
            }),
            Err(PlanError::Scope)
        );
    }
    for (expression, expected) in [
        (Expression::Slot(SlotId(42)), PlanError::Type),
        (Expression::Slot(SlotId(8)), PlanError::Scope),
        (Expression::Slot(SlotId(9)), PlanError::Scope),
        (Expression::Slot(SlotId(9000)), PlanError::Scope),
        (Expression::Literal(Literal::I64(1)), PlanError::Type),
    ] {
        let expressions = [expression];
        let operators = edge_scope_operators(&[], SlotId(42), ExprId(0), 0);
        assert_eq!(
            validate(PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(2),
                eager_searches: &[]
            }),
            Err(expected)
        );
    }
    let base = edge_scope_operators(&[], SlotId(42), ExprId(0), 0);
    let expressions = [
        Expression::Literal(Literal::Bool(true)),
        Expression::Slot(SlotId(42)),
    ];
    let operators = [
        base[0],
        base[1],
        base[2],
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::Filter(ExprId(1)),
        },
    ];
    assert_eq!(
        validate(PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(3),
            eager_searches: &[]
        }),
        Err(PlanError::Scope)
    );
    let missing = edge_scope_operators(&[], SlotId(42), ExprId(99), 0);
    assert_eq!(
        validate(PlanDescription {
            operators: &missing,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(2),
            eager_searches: &[],
        }),
        Err(PlanError::Reference)
    );
}

#[test]
fn reused_pattern_variables_use_fresh_candidates_equality_and_reprojection() {
    use zeppelin_embed::property_graph::query::Comparison;
    let expressions = [
        Expression::Slot(SlotId(7)),
        Expression::Slot(SlotId(10)),
        Expression::Binary {
            operation: BinaryExpression::Comparison(Comparison::Equal),
            left: ExprId(0),
            right: ExprId(1),
        },
        Expression::Slot(SlotId(9)),
        Expression::Slot(SlotId(11)),
        Expression::Binary {
            operation: BinaryExpression::Comparison(Comparison::Equal),
            left: ExprId(3),
            right: ExprId(4),
        },
        Expression::Binary {
            operation: BinaryExpression::And,
            left: ExprId(2),
            right: ExprId(5),
        },
        Expression::Slot(SlotId(8)),
    ];
    let projection = [
        Projection {
            slot: SlotId(7),
            expression: ExprId(0),
        },
        Projection {
            slot: SlotId(8),
            expression: ExprId(7),
        },
        Projection {
            slot: SlotId(9),
            expression: ExprId(3),
        },
    ];
    // MATCH (a)-[r]->(b) MATCH (b)<-[r]-(a): the second MATCH has its
    // own uniqueness scope and fresh candidate slots. Identity equality
    // constrains them; projection retains the existing a/r bindings.
    let mut operators = [
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
            kind: OperatorKind::Expand {
                source: SlotId(7),
                node: SlotId(8),
                relationship: SlotId(9),
                direction: Direction::Outgoing,
                relationship_types: &[],
                pattern: PatternId(0),
            },
        },
        Operator {
            inputs: &[PlanNodeId(2)],
            kind: OperatorKind::Expand {
                source: SlotId(8),
                node: SlotId(10),
                relationship: SlotId(11),
                direction: Direction::Incoming,
                relationship_types: &[],
                pattern: PatternId(1),
            },
        },
        Operator {
            inputs: &[PlanNodeId(3)],
            kind: OperatorKind::Filter(ExprId(6)),
        },
        Operator {
            inputs: &[PlanNodeId(4)],
            kind: OperatorKind::Project(&projection),
        },
    ];
    macro_rules! describe {
        ($operators:expr) => {
            PlanDescription {
                operators: $operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(5),
                eager_searches: &[],
            }
        };
    }
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let mut facts = vec![NodeFacts::default(); operators.len()];
    let plan = validate_plan(
        describe!(&operators),
        &mut facts,
        PlanFootprint::declared(1024 * 1024),
        &mut context,
    )
    .unwrap();
    let output = plan.facts(PlanNodeId(5)).unwrap();
    assert_eq!(output.width(), 3);
    for (ordinal, (slot, kinds)) in [
        (7, ValueKinds::NODE),
        (8, ValueKinds::NODE),
        (9, ValueKinds::REL),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(output.slot_at(ordinal), Some((SlotId(slot), kinds)));
    }
    assert_eq!(output.slot(SlotId(10)), None);
    assert_eq!(output.slot(SlotId(11)), None);
    if let OperatorKind::Expand { node, .. } = &mut operators[3].kind {
        *node = SlotId(7);
    }
    assert_eq!(validate(describe!(&operators)), Err(PlanError::Scope));
}

#[test]
fn typed_pattern_requires_array_and_every_alternative_name_backing() {
    use zeppelin_embed::property_graph::GraphName;
    let owned = ["first-type".to_owned(), "second-type".to_owned()];
    let names = [
        GraphName::new(&owned[0]).unwrap(),
        GraphName::new(&owned[1]).unwrap(),
    ];
    let operators = edge_scope_operators(&names, SlotId(42), ExprId(0), 1);
    let expressions = [Expression::Literal(Literal::Bool(true))];
    let description = PlanDescription {
        operators: &operators,
        expressions: &expressions,
        parameters: &[],
        root: PlanNodeId(2),
        eager_searches: &[],
    };
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    for missing in [
        RetainedRegion::slice(&names).unwrap(),
        RetainedRegion::slice(owned[0].as_bytes()).unwrap(),
        RetainedRegion::slice(owned[1].as_bytes()).unwrap(),
    ] {
        let mut facts = vec![NodeFacts::default(); operators.len()];
        let complete = retained_regions(description, &facts);
        let mut regions = Vec::new();
        for r in complete {
            if r.end() <= missing.start() || r.start() >= missing.end() {
                regions.push(r);
                continue;
            }
            if r.start() < missing.start() {
                regions.push(
                    RetainedRegion::declared(r.start(), missing.start() - r.start()).unwrap(),
                );
            }
            if r.end() > missing.end() {
                regions.push(
                    RetainedRegion::declared(missing.end(), r.end() - missing.end()).unwrap(),
                );
            }
        }
        let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
        assert!(matches!(
            GraphPlan::validate(
                description,
                &mut facts,
                PlanFootprint::declared(1024 * 1024),
                PlanBacking::vector(&regions).unwrap(),
                &mut context
            ),
            Err(PlanError::Footprint)
        ));
    }
    assert_eq!(validate(description), Ok(()));
}

#[test]
fn typed_pattern_zero_hop_validation_preserves_predicate_and_control_obligations() {
    use zeppelin_embed::property_graph::{GraphName, query::QueryError};
    let names = [
        GraphName::new("left").unwrap(),
        GraphName::new("right").unwrap(),
    ];
    let expressions = [Expression::Literal(Literal::Null)];
    let operators = edge_scope_operators(&names, SlotId(42), ExprId(0), 0);
    let description = PlanDescription {
        operators: &operators,
        expressions: &expressions,
        parameters: &[],
        root: PlanNodeId(2),
        eager_searches: &[],
    };
    assert_eq!(validate(description), Ok(()));
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let mut facts = vec![NodeFacts::default(); operators.len()];
    let regions = retained_regions(description, &facts);
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    GraphPlan::validate(
        description,
        &mut facts,
        PlanFootprint::declared(1024 * 1024),
        PlanBacking::vector(&regions).unwrap(),
        &mut context,
    )
    .unwrap();
    let work = context.work();
    assert!(work > 0);
    for limit in 0..work {
        let mut context = ValueContext::new(&view, &control, limit).unwrap();
        assert!(matches!(
            GraphPlan::validate(
                description,
                &mut facts,
                PlanFootprint::declared(1024 * 1024),
                PlanBacking::vector(&regions).unwrap(),
                &mut context
            ),
            Err(PlanError::Control(QueryError::WorkLimit))
        ));
        assert_eq!(context.work(), limit);
    }
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    token.cancel();
    assert!(matches!(
        GraphPlan::validate(
            description,
            &mut facts,
            PlanFootprint::declared(1024 * 1024),
            PlanBacking::vector(&regions).unwrap(),
            &mut context
        ),
        Err(PlanError::Control(QueryError::Cancelled))
    ));
}

#[test]
fn completed_edge_predicate_sees_full_list_and_private_edge_without_output_escape() {
    use zeppelin_embed::property_graph::GraphName;
    let expressions = [
        Expression::Slot(SlotId(9)),
        Expression::Unary {
            operation: UnaryExpression::Size,
            operand: ExprId(0),
        },
        Expression::Slot(SlotId(42)),
        Expression::Property {
            entity: ExprId(2),
            name: GraphName::new("x").unwrap(),
        },
        Expression::Binary {
            operation: BinaryExpression::Comparison(
                zeppelin_embed::property_graph::query::Comparison::Equal,
            ),
            left: ExprId(3),
            right: ExprId(1),
        },
    ];
    for maximum in [0, 2] {
        let mut operators = edge_scope_operators(&[], SlotId(42), ExprId(4), maximum);
        let OperatorKind::BoundedExpand {
            edge_predicate,
            completed_edge_predicate,
            ..
        } = &mut operators[2].kind
        else {
            unreachable!()
        };
        *edge_predicate = None;
        *completed_edge_predicate = Some(CompletedEdgePredicate {
            current_edge: SlotId(42),
            expression: ExprId(4),
        });
        let describe = |expressions| PlanDescription {
            operators: &operators,
            expressions,
            parameters: &[],
            root: PlanNodeId(2),
            eager_searches: &[],
        };
        assert_eq!(validate(describe(&expressions)), Ok(()));
        let mut invalid = expressions;
        // The fresh destination remains unavailable during the predicate.
        invalid[0] = Expression::Slot(SlotId(8));
        assert_eq!(validate(describe(&invalid)), Err(PlanError::Scope));
        // The existing source is visible, while expression typing still applies.
        let mut invalid = expressions;
        invalid[0] = Expression::Slot(SlotId(7));
        assert_eq!(validate(describe(&invalid)), Err(PlanError::Type));
    }
    for private in [7, 8, 9] {
        let mut operators = edge_scope_operators(&[], SlotId(42), ExprId(4), 2);
        let OperatorKind::BoundedExpand {
            edge_predicate,
            completed_edge_predicate,
            ..
        } = &mut operators[2].kind
        else {
            unreachable!()
        };
        *edge_predicate = None;
        *completed_edge_predicate = Some(CompletedEdgePredicate {
            current_edge: SlotId(private),
            expression: ExprId(4),
        });
        assert_eq!(
            validate(PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(2),
                eager_searches: &[]
            }),
            Err(PlanError::Scope)
        );
    }
}
