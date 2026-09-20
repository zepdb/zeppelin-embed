//! Literal mutation compiler-to-plan oracles; these do not execute graph writes.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod support;

use zeppelin_embed::lifecycle::{CancelToken, Deadline, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{QueryValue, QueryView, ValueContext, plan::*, resources::*},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{
    CompileLimits, ErrorKind, ResourceError, compile_mutation_in, compile_read_in,
};

fn with_memory(run: impl FnOnce(&QueryMemory<'_>, &mut ValueContext<'_>)) {
    let path = support::unique_temp_dir("ze140-mutation-lowering");
    std::fs::create_dir(&path).unwrap();
    let store = Store::open(
        &path,
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let baseline = memory.reserved_bytes();
    run(&memory, &mut context);
    assert_eq!(memory.reserved_bytes(), baseline);
    drop(memory);
    drop(shared);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[derive(Debug, PartialEq)]
enum ObservedExpression {
    Null,
    Bool(bool),
    I64(i64),
    F64(u64),
    String(String),
    Slot(u32),
    Parameter(u32),
    List(Vec<ObservedExpression>),
    Property(Box<ObservedExpression>, String),
    HasLabel(Box<ObservedExpression>, String),
    Unary(String, Box<ObservedExpression>),
    Binary(String, Box<ObservedExpression>, Box<ObservedExpression>),
    Aggregate(String, Option<Box<ObservedExpression>>),
}

#[derive(Debug, PartialEq)]
enum ObservedMutation {
    CreateNode {
        output: u32,
        labels: Vec<String>,
    },
    CreateRelationship {
        output: u32,
        source: ObservedExpression,
        target: ObservedExpression,
        relationship_type: String,
    },
    SetProperty {
        entity: ObservedExpression,
        name: String,
        value: ObservedExpression,
    },
    RemoveProperty {
        entity: ObservedExpression,
        name: String,
    },
    SetLabel {
        entity: ObservedExpression,
        label: String,
        present: bool,
    },
    Delete {
        entity: ObservedExpression,
        detach: bool,
    },
}

fn expression(description: PlanDescription<'_>, id: ExprId) -> ObservedExpression {
    match description.expressions[id.0 as usize] {
        Expression::Literal(Literal::Null) => ObservedExpression::Null,
        Expression::Literal(Literal::Bool(value)) => ObservedExpression::Bool(value),
        Expression::Literal(Literal::I64(value)) => ObservedExpression::I64(value),
        Expression::Literal(Literal::F64(value)) => ObservedExpression::F64(value.to_bits()),
        Expression::Literal(Literal::String(value)) => ObservedExpression::String(value.to_owned()),
        Expression::Slot(slot) => ObservedExpression::Slot(slot.0),
        Expression::Parameter(parameter) => ObservedExpression::Parameter(parameter.0),
        Expression::List(values) => ObservedExpression::List(
            values
                .iter()
                .map(|id| expression(description, *id))
                .collect(),
        ),
        Expression::Property { entity, name } => ObservedExpression::Property(
            Box::new(expression(description, entity)),
            name.as_str().to_owned(),
        ),
        Expression::HasLabel { entity, label } => ObservedExpression::HasLabel(
            Box::new(expression(description, entity)),
            label.as_str().to_owned(),
        ),
        Expression::Unary { operation, operand } => ObservedExpression::Unary(
            format!("{operation:?}"),
            Box::new(expression(description, operand)),
        ),
        Expression::Binary {
            operation,
            left,
            right,
        } => ObservedExpression::Binary(
            format!("{operation:?}"),
            Box::new(expression(description, left)),
            Box::new(expression(description, right)),
        ),
        Expression::Aggregate { operation, operand } => ObservedExpression::Aggregate(
            format!("{operation:?}"),
            operand.map(|id| Box::new(expression(description, id))),
        ),
    }
}

fn mutations(description: PlanDescription<'_>) -> Vec<ObservedMutation> {
    let mut observed = Vec::new();
    for operator in description.operators {
        let OperatorKind::Mutate(items) = operator.kind else {
            continue;
        };
        for item in items {
            observed.push(match item {
                Mutation::CreateNode { output, labels } => ObservedMutation::CreateNode {
                    output: output.0,
                    labels: labels.iter().map(|name| name.as_str().to_owned()).collect(),
                },
                Mutation::CreateRelationship {
                    output,
                    source,
                    target,
                    relationship_type,
                } => ObservedMutation::CreateRelationship {
                    output: output.0,
                    source: expression(description, *source),
                    target: expression(description, *target),
                    relationship_type: relationship_type.as_str().to_owned(),
                },
                Mutation::SetProperty {
                    entity,
                    name,
                    value,
                } => ObservedMutation::SetProperty {
                    entity: expression(description, *entity),
                    name: name.as_str().to_owned(),
                    value: expression(description, *value),
                },
                Mutation::RemoveProperty { entity, name } => ObservedMutation::RemoveProperty {
                    entity: expression(description, *entity),
                    name: name.as_str().to_owned(),
                },
                Mutation::SetLabel {
                    entity,
                    label,
                    present,
                } => ObservedMutation::SetLabel {
                    entity: expression(description, *entity),
                    label: label.as_str().to_owned(),
                    present: *present,
                },
                Mutation::Delete { entity, detach } => ObservedMutation::Delete {
                    entity: expression(description, *entity),
                    detach: *detach,
                },
            });
        }
    }
    observed
}

fn operator_names(description: PlanDescription<'_>) -> Vec<&'static str> {
    description
        .operators
        .iter()
        .map(|operator| match operator.kind {
            OperatorKind::Unit => "Unit",
            OperatorKind::Eager => "Eager",
            OperatorKind::Mutate(_) => "Mutate",
            OperatorKind::Project(_) => "Project",
            OperatorKind::With(_) => "With",
            OperatorKind::Filter(_) => "Filter",
            OperatorKind::ScanNodes { .. } => "ScanNodes",
            OperatorKind::Expand { .. } => "Expand",
            OperatorKind::OptionalApply { .. } => "OptionalApply",
            OperatorKind::Aggregate { .. } => "Aggregate",
            OperatorKind::Distinct => "Distinct",
            OperatorKind::Sort(_) => "Sort",
            OperatorKind::OffsetLimit { .. } => "OffsetLimit",
            _ => "Other",
        })
        .collect()
}

#[test]
fn mutation_entry_is_one_shot_and_rejects_read_route() {
    with_memory(|memory, context| {
        let mut calls = 0;
        compile_mutation_in(
            "CREATE (n) RETURN n",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| {
                calls += 1;
                let plan = mutation.plan().description();
                assert_eq!(plan.operators.len(), 4);
                assert!(matches!(plan.operators[0].kind, OperatorKind::Unit));
                assert!(matches!(plan.operators[1].kind, OperatorKind::Eager));
                assert!(matches!(plan.operators[2].kind, OperatorKind::Mutate(_)));
                assert!(matches!(plan.operators[3].kind, OperatorKind::Project(_)));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls, 1);

        let mut calls = 0;
        let error = compile_mutation_in(
            "RETURN 1",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |_, _| {
                calls += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(calls, 0);

        let mut calls = 0;
        let error = compile_read_in(
            "CREATE (n) RETURN n",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |_, _| {
                calls += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(calls, 0);
    });
}

#[test]
fn create_endpoint_dependencies_preserve_textual_property_order() {
    with_memory(|memory, context| {
        let query = "CREATE (a)-[r:R]->(b {x:type(r)}) RETURN b.x";
        let (operators, items, spans) = compile_mutation_in(
            query,
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| {
                let description = mutation.plan().description();
                Ok((
                    operator_names(description),
                    mutations(description),
                    mutation
                        .mutation_spans()
                        .iter()
                        .map(|span| mutation.source()[span.start..span.end].to_owned())
                        .collect::<Vec<_>>(),
                ))
            },
        )
        .unwrap();
        assert_eq!(operators, ["Unit", "Eager", "Mutate", "Project"]);
        assert_eq!(
            items,
            [
                ObservedMutation::CreateNode {
                    output: 0,
                    labels: vec![],
                },
                ObservedMutation::CreateNode {
                    output: 2,
                    labels: vec![],
                },
                ObservedMutation::CreateRelationship {
                    output: 1,
                    source: ObservedExpression::Slot(0),
                    target: ObservedExpression::Slot(2),
                    relationship_type: "R".to_owned(),
                },
                ObservedMutation::SetProperty {
                    entity: ObservedExpression::Slot(2),
                    name: "x".to_owned(),
                    value: ObservedExpression::Unary(
                        "RelType".to_owned(),
                        Box::new(ObservedExpression::Slot(1)),
                    ),
                },
            ]
        );
        assert_eq!(spans, ["(a)", "(b {x:type(r)})", "-[r:R]->", "x:type(r)"]);

        let query = "CREATE (a {p:1})-[r:R {p:a.p,q:type(r)}]->(b {x:r.p,y:a.p}) RETURN b.x";
        let items = compile_mutation_in(
            query,
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| Ok(mutations(mutation.plan().description())),
        )
        .unwrap();
        assert_eq!(
            items,
            [
                ObservedMutation::CreateNode {
                    output: 0,
                    labels: vec![],
                },
                ObservedMutation::SetProperty {
                    entity: ObservedExpression::Slot(0),
                    name: "p".to_owned(),
                    value: ObservedExpression::I64(1),
                },
                ObservedMutation::CreateNode {
                    output: 2,
                    labels: vec![],
                },
                ObservedMutation::CreateRelationship {
                    output: 1,
                    source: ObservedExpression::Slot(0),
                    target: ObservedExpression::Slot(2),
                    relationship_type: "R".to_owned(),
                },
                ObservedMutation::SetProperty {
                    entity: ObservedExpression::Slot(1),
                    name: "p".to_owned(),
                    value: ObservedExpression::Property(
                        Box::new(ObservedExpression::Slot(0)),
                        "p".to_owned(),
                    ),
                },
                ObservedMutation::SetProperty {
                    entity: ObservedExpression::Slot(1),
                    name: "q".to_owned(),
                    value: ObservedExpression::Unary(
                        "RelType".to_owned(),
                        Box::new(ObservedExpression::Slot(1)),
                    ),
                },
                ObservedMutation::SetProperty {
                    entity: ObservedExpression::Slot(2),
                    name: "x".to_owned(),
                    value: ObservedExpression::Property(
                        Box::new(ObservedExpression::Slot(1)),
                        "p".to_owned(),
                    ),
                },
                ObservedMutation::SetProperty {
                    entity: ObservedExpression::Slot(2),
                    name: "y".to_owned(),
                    value: ObservedExpression::Property(
                        Box::new(ObservedExpression::Slot(0)),
                        "p".to_owned(),
                    ),
                },
            ]
        );

        for (query, expected) in [
            (
                "CREATE (n {p:1,q:n.p}) RETURN n.q",
                vec![
                    ObservedMutation::CreateNode {
                        output: 0,
                        labels: vec![],
                    },
                    ObservedMutation::SetProperty {
                        entity: ObservedExpression::Slot(0),
                        name: "p".to_owned(),
                        value: ObservedExpression::I64(1),
                    },
                    ObservedMutation::SetProperty {
                        entity: ObservedExpression::Slot(0),
                        name: "q".to_owned(),
                        value: ObservedExpression::Property(
                            Box::new(ObservedExpression::Slot(0)),
                            "p".to_owned(),
                        ),
                    },
                ],
            ),
            (
                "CREATE (a)-[r:R]->(b), (b)-[s:S {from:type(r)}]->(c) RETURN c",
                vec![
                    ObservedMutation::CreateNode {
                        output: 0,
                        labels: vec![],
                    },
                    ObservedMutation::CreateNode {
                        output: 2,
                        labels: vec![],
                    },
                    ObservedMutation::CreateRelationship {
                        output: 1,
                        source: ObservedExpression::Slot(0),
                        target: ObservedExpression::Slot(2),
                        relationship_type: "R".to_owned(),
                    },
                    ObservedMutation::CreateNode {
                        output: 4,
                        labels: vec![],
                    },
                    ObservedMutation::CreateRelationship {
                        output: 3,
                        source: ObservedExpression::Slot(2),
                        target: ObservedExpression::Slot(4),
                        relationship_type: "S".to_owned(),
                    },
                    ObservedMutation::SetProperty {
                        entity: ObservedExpression::Slot(3),
                        name: "from".to_owned(),
                        value: ObservedExpression::Unary(
                            "RelType".to_owned(),
                            Box::new(ObservedExpression::Slot(1)),
                        ),
                    },
                ],
            ),
            (
                "CREATE (a)<-[r:R]-(b {x:type(r)}) RETURN b.x",
                vec![
                    ObservedMutation::CreateNode {
                        output: 0,
                        labels: vec![],
                    },
                    ObservedMutation::CreateNode {
                        output: 2,
                        labels: vec![],
                    },
                    ObservedMutation::CreateRelationship {
                        output: 1,
                        source: ObservedExpression::Slot(2),
                        target: ObservedExpression::Slot(0),
                        relationship_type: "R".to_owned(),
                    },
                    ObservedMutation::SetProperty {
                        entity: ObservedExpression::Slot(2),
                        name: "x".to_owned(),
                        value: ObservedExpression::Unary(
                            "RelType".to_owned(),
                            Box::new(ObservedExpression::Slot(1)),
                        ),
                    },
                ],
            ),
        ] {
            let actual = compile_mutation_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |mutation, _| Ok(mutations(mutation.plan().description())),
            )
            .unwrap();
            assert_eq!(actual, expected, "{query}");
        }
    });
}

#[test]
fn create_lowers_all_shapes_slots_names_and_directions() {
    with_memory(|memory, context| {
        for (query, expected) in [
            (
                "CREATE (:A:B)",
                vec![ObservedMutation::CreateNode {
                    output: 0,
                    labels: vec!["A".to_owned(), "B".to_owned()],
                }],
            ),
            (
                "CREATE (n {p:1,q:null,e:[]}) RETURN n",
                vec![
                    ObservedMutation::CreateNode {
                        output: 0,
                        labels: vec![],
                    },
                    ObservedMutation::SetProperty {
                        entity: ObservedExpression::Slot(0),
                        name: "p".to_owned(),
                        value: ObservedExpression::I64(1),
                    },
                    ObservedMutation::SetProperty {
                        entity: ObservedExpression::Slot(0),
                        name: "q".to_owned(),
                        value: ObservedExpression::Null,
                    },
                    ObservedMutation::SetProperty {
                        entity: ObservedExpression::Slot(0),
                        name: "e".to_owned(),
                        value: ObservedExpression::List(vec![]),
                    },
                ],
            ),
            (
                "CREATE (a)<-[r:`τύπος`]-(b) RETURN a,r,b",
                vec![
                    ObservedMutation::CreateNode {
                        output: 0,
                        labels: vec![],
                    },
                    ObservedMutation::CreateNode {
                        output: 2,
                        labels: vec![],
                    },
                    ObservedMutation::CreateRelationship {
                        output: 1,
                        source: ObservedExpression::Slot(2),
                        target: ObservedExpression::Slot(0),
                        relationship_type: "τύπος".to_owned(),
                    },
                ],
            ),
            (
                "CREATE (a)-[r:R]->(a), (a)-[s:S]->(b), (a)-[t:S]->(b) RETURN a,r,s,t,b",
                vec![
                    ObservedMutation::CreateNode {
                        output: 0,
                        labels: vec![],
                    },
                    ObservedMutation::CreateRelationship {
                        output: 1,
                        source: ObservedExpression::Slot(0),
                        target: ObservedExpression::Slot(0),
                        relationship_type: "R".to_owned(),
                    },
                    ObservedMutation::CreateNode {
                        output: 3,
                        labels: vec![],
                    },
                    ObservedMutation::CreateRelationship {
                        output: 2,
                        source: ObservedExpression::Slot(0),
                        target: ObservedExpression::Slot(3),
                        relationship_type: "S".to_owned(),
                    },
                    ObservedMutation::CreateRelationship {
                        output: 4,
                        source: ObservedExpression::Slot(0),
                        target: ObservedExpression::Slot(3),
                        relationship_type: "S".to_owned(),
                    },
                ],
            ),
            (
                "MATCH (n) WITH n AS a CREATE (a)-[r:R]->(b) RETURN a,r,b",
                vec![
                    ObservedMutation::CreateNode {
                        output: 3,
                        labels: vec![],
                    },
                    ObservedMutation::CreateRelationship {
                        output: 2,
                        source: ObservedExpression::Slot(1),
                        target: ObservedExpression::Slot(3),
                        relationship_type: "R".to_owned(),
                    },
                ],
            ),
            (
                "CREATE (a) CREATE (a)-[r:R]->(b) RETURN a,r,b",
                vec![
                    ObservedMutation::CreateNode {
                        output: 0,
                        labels: vec![],
                    },
                    ObservedMutation::CreateNode {
                        output: 2,
                        labels: vec![],
                    },
                    ObservedMutation::CreateRelationship {
                        output: 1,
                        source: ObservedExpression::Slot(0),
                        target: ObservedExpression::Slot(2),
                        relationship_type: "R".to_owned(),
                    },
                ],
            ),
        ] {
            let actual = compile_mutation_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |mutation, _| Ok(mutations(mutation.plan().description())),
            )
            .unwrap();
            assert_eq!(actual, expected, "{query}");
        }
    });
}

#[test]
fn mutation_items_preserve_set_remove_delete_and_detach_order() {
    with_memory(|memory, context| {
        let query = "MATCH (n)-[r:R]->(m) SET n.p=1,r.q=null,n:A:B,n:A REMOVE n.p,r.q,n:A:B DELETE r,n DETACH DELETE m,r";
        let (items, spans, dynamic) = compile_mutation_in(
            query,
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| {
                Ok((
                    mutations(mutation.plan().description()),
                    mutation
                        .mutation_spans()
                        .iter()
                        .map(|span| mutation.source()[span.start..span.end].to_owned())
                        .collect::<Vec<_>>(),
                    mutation.requires_deleted_runtime_validation(),
                ))
            },
        )
        .unwrap();
        assert_eq!(
            items,
            [
                ObservedMutation::SetProperty {
                    entity: ObservedExpression::Slot(0),
                    name: "p".to_owned(),
                    value: ObservedExpression::I64(1),
                },
                ObservedMutation::SetProperty {
                    entity: ObservedExpression::Slot(1),
                    name: "q".to_owned(),
                    value: ObservedExpression::Null,
                },
                ObservedMutation::SetLabel {
                    entity: ObservedExpression::Slot(0),
                    label: "A".to_owned(),
                    present: true,
                },
                ObservedMutation::SetLabel {
                    entity: ObservedExpression::Slot(0),
                    label: "B".to_owned(),
                    present: true,
                },
                ObservedMutation::SetLabel {
                    entity: ObservedExpression::Slot(0),
                    label: "A".to_owned(),
                    present: true,
                },
                ObservedMutation::RemoveProperty {
                    entity: ObservedExpression::Slot(0),
                    name: "p".to_owned(),
                },
                ObservedMutation::RemoveProperty {
                    entity: ObservedExpression::Slot(1),
                    name: "q".to_owned(),
                },
                ObservedMutation::SetLabel {
                    entity: ObservedExpression::Slot(0),
                    label: "A".to_owned(),
                    present: false,
                },
                ObservedMutation::SetLabel {
                    entity: ObservedExpression::Slot(0),
                    label: "B".to_owned(),
                    present: false,
                },
                ObservedMutation::Delete {
                    entity: ObservedExpression::Slot(1),
                    detach: false,
                },
                ObservedMutation::Delete {
                    entity: ObservedExpression::Slot(0),
                    detach: false,
                },
                ObservedMutation::Delete {
                    entity: ObservedExpression::Slot(2),
                    detach: true,
                },
                ObservedMutation::Delete {
                    entity: ObservedExpression::Slot(1),
                    detach: true,
                },
            ]
        );
        assert_eq!(
            spans,
            [
                "n.p=1", "r.q=null", "n:A:B", "n:A:B", "n:A", "n.p", "r.q", "n:A:B", "n:A:B", "r",
                "n", "m", "r",
            ]
        );
        assert!(
            !dynamic,
            "deletion targets alone do not imply an output obligation"
        );

        let items = compile_mutation_in(
            "OPTIONAL MATCH (a)-[r:R]->(b) DELETE a,r DETACH DELETE b,r",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| Ok(mutations(mutation.plan().description())),
        )
        .unwrap();
        assert_eq!(
            items,
            [
                ObservedMutation::Delete {
                    entity: ObservedExpression::Slot(0),
                    detach: false,
                },
                ObservedMutation::Delete {
                    entity: ObservedExpression::Slot(1),
                    detach: false,
                },
                ObservedMutation::Delete {
                    entity: ObservedExpression::Slot(2),
                    detach: true,
                },
                ObservedMutation::Delete {
                    entity: ObservedExpression::Slot(1),
                    detach: true,
                },
            ]
        );
    });
}

#[test]
fn mutation_clauses_keep_immediate_eager_and_downstream_projections() {
    with_memory(|memory, context| {
        let query = "MATCH (n) WITH n ORDER BY n.p SET n.p=1 SET n.p=n.p+1 REMOVE n.q WITH n WHERE n.p>0 RETURN DISTINCT n.p AS p ORDER BY p SKIP 1 LIMIT 0";
        compile_mutation_in(
            query,
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| {
                let description = mutation.plan().description();
                let mut mutation_count = 0;
                for (index, operator) in description.operators.iter().enumerate() {
                    if matches!(operator.kind, OperatorKind::Mutate(_)) {
                        mutation_count += 1;
                        assert_eq!(operator.inputs.len(), 1);
                        let eager = &description.operators[operator.inputs[0].0 as usize];
                        assert!(matches!(eager.kind, OperatorKind::Eager));
                        assert_eq!(eager.inputs.len(), 1);
                        assert_eq!(
                            mutation.operator_spans()[index],
                            mutation.operator_spans()[operator.inputs[0].0 as usize]
                        );
                    }
                }
                assert_eq!(mutation_count, 3);
                assert!(matches!(
                    description.operators[description.root.0 as usize].kind,
                    OperatorKind::OffsetLimit {
                        offset: 1,
                        limit: Some(0)
                    }
                ));
                let names = operator_names(description);
                assert!(names.contains(&"Sort"));
                assert!(names.contains(&"With"));
                assert!(names.contains(&"Filter"));
                assert!(names.contains(&"Distinct"));
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn mutation_rhs_keeps_fresh_properties_and_frozen_alias_slots() {
    with_memory(|memory, context| {
        let items = compile_mutation_in(
            "MATCH (n) WITH n,n.p AS old SET n.p=n.p+1,n.q=old+1",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| Ok(mutations(mutation.plan().description())),
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        let ObservedMutation::SetProperty { value: fresh, .. } = &items[0] else {
            panic!("first SET item")
        };
        assert_eq!(
            fresh,
            &ObservedExpression::Binary(
                "Arithmetic(Add)".to_owned(),
                Box::new(ObservedExpression::Property(
                    Box::new(ObservedExpression::Slot(1)),
                    "p".to_owned(),
                )),
                Box::new(ObservedExpression::I64(1)),
            )
        );
        let ObservedMutation::SetProperty { value: frozen, .. } = &items[1] else {
            panic!("second SET item")
        };
        assert_eq!(
            frozen,
            &ObservedExpression::Binary(
                "Arithmetic(Add)".to_owned(),
                Box::new(ObservedExpression::Slot(2)),
                Box::new(ObservedExpression::I64(1)),
            )
        );
    });
}

#[test]
fn mutation_no_return_and_limit_zero_keep_all_updates() {
    with_memory(|memory, context| {
        compile_mutation_in(
            "CREATE (n) SET n.p=1",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| {
                let description = mutation.plan().description();
                assert!(mutation.columns().is_empty());
                assert!(matches!(
                    description.operators[description.root.0 as usize].kind,
                    OperatorKind::Mutate(_)
                ));
                assert_eq!(mutations(description).len(), 2);
                Ok(())
            },
        )
        .unwrap();
        compile_mutation_in(
            "CREATE (n) SET n.p=1 RETURN n LIMIT 0",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| {
                let description = mutation.plan().description();
                assert_eq!(mutations(description).len(), 2);
                assert!(matches!(
                    description.operators[description.root.0 as usize].kind,
                    OperatorKind::OffsetLimit {
                        offset: 0,
                        limit: Some(0)
                    }
                ));
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn mutation_deleted_boundaries_keep_static_errors_and_dynamic_obligation() {
    with_memory(|memory, context| {
        for (query, offending) in [
            ("MATCH (n) DELETE n RETURN n", "n"),
            ("MATCH (n) DELETE n RETURN n.p", "n.p"),
            ("MATCH (n) DELETE n SET n.p=1", "n.p=1"),
        ] {
            let mut calls = 0;
            let error = compile_mutation_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |_, _| {
                    calls += 1;
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::DeletedEntity, "{query}");
            assert_eq!(
                &query[error.span.start..error.span.end],
                offending,
                "{query}"
            );
            assert_eq!(calls, 0);
        }

        for query in [
            "OPTIONAL MATCH (n) DELETE n RETURN n",
            "MATCH (n),(m) DELETE n RETURN m",
            "MATCH (n) WITH [n] AS ns,n DELETE n RETURN ns[0]",
            "MATCH ()-[r]->(), ()-[rs:R*1..2]->() DELETE r RETURN rs",
        ] {
            let required = compile_mutation_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |mutation, _| Ok(mutation.requires_deleted_runtime_validation()),
            )
            .unwrap();
            assert!(required, "{query}");
        }

        for query in [
            "CREATE (n) RETURN n",
            "MATCH (n) WITH n,n.p AS old DELETE n RETURN old",
            "MATCH (n) WITH n,count(*) AS count DELETE n RETURN count",
            "MATCH (n) DELETE n RETURN 1",
        ] {
            let required = compile_mutation_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |mutation, _| Ok(mutation.requires_deleted_runtime_validation()),
            )
            .unwrap();
            assert!(!required, "{query}");
        }
    });
}

#[test]
fn mutation_whole_statement_rejects_unsupported_suffixes_and_read_write_search() {
    with_memory(|memory, context| {
        for query in [
            "CREATE (a)-[:R]-(b)",
            "CREATE (a)-[:R]->(b) MATCH (c) RETURN c",
            "CALL ze.text_search('x',1) YIELD node CREATE (n)",
            "CREATE (n) RETURN n trailing",
        ] {
            let mut calls = 0;
            let error = compile_mutation_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |_, _| {
                    calls += 1;
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(
                matches!(error.kind, ErrorKind::Unsupported | ErrorKind::Syntax),
                "{query}: {error:?}"
            );
            assert_eq!(calls, 0, "{query}");
        }
    });
}

#[test]
fn mutation_metadata_owns_exact_source_spans_columns_and_parameters() {
    use zeppelin_embed::property_graph::query::plan::ParameterBinding;

    with_memory(|memory, context| {
        let source = "/* λ */ CREATE (n:`标签` {`π`:$value}) RETURN n AS `rés`;";
        let parameters = [ParameterBinding {
            name: "value",
            value: QueryValue::F64(f64::from_bits(0x7ff8_0000_0000_0042)),
        }];
        compile_mutation_in(
            source,
            &parameters,
            CompileLimits::default(),
            memory,
            context,
            |mutation, _| {
                assert_eq!(mutation.source(), source);
                assert_ne!(mutation.source().as_ptr(), source.as_ptr());
                assert_eq!(mutation.columns().len(), 1);
                assert_eq!(mutation.columns()[0].name, "rés");
                assert_eq!(mutation.columns()[0].slot, SlotId(1));
                assert_eq!(mutation.columns()[0].kinds, ValueKinds::NODE);
                assert_eq!(mutation.parameters().len(), 1);
                assert_eq!(mutation.parameters()[0].name, "value");
                let QueryValue::F64(value) = mutation.parameters()[0].value else {
                    panic!("F64 parameter")
                };
                assert_eq!(value.to_bits(), 0x7ff8_0000_0000_0042);
                let items = mutations(mutation.plan().description());
                assert_eq!(
                    items,
                    [
                        ObservedMutation::CreateNode {
                            output: 0,
                            labels: vec!["标签".to_owned()],
                        },
                        ObservedMutation::SetProperty {
                            entity: ObservedExpression::Slot(0),
                            name: "π".to_owned(),
                            value: ObservedExpression::Parameter(0),
                        },
                    ]
                );
                assert_eq!(
                    mutation
                        .mutation_spans()
                        .iter()
                        .map(|span| &mutation.source()[span.start..span.end])
                        .collect::<Vec<_>>(),
                    ["(n:`标签` {`π`:$value})", "`π`:$value"]
                );
                for span in mutation
                    .expression_spans()
                    .iter()
                    .chain(mutation.operator_spans())
                    .chain(mutation.mutation_spans())
                {
                    assert!(mutation.source().get(span.start..span.end).is_some());
                }
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn mutation_owner_inventory_proves_full_capacity_and_facts() {
    with_memory(|memory, context| {
        let bindings = [ParameterBinding {
            name: "seed",
            value: QueryValue::I64(7),
        }];
        compile_mutation_in(
            "CREATE (a:A {seed:$seed})-[r:R]->(b {p:1}) RETURN a,r,b",
            &bindings,
            CompileLimits::default(),
            memory,
            context,
            |mutation, context| {
                assert_eq!(mutation.owners().len(), 35);
                let mut charge = memory.reserve_external_capacity().unwrap();
                charge
                    .reserve_additional(
                        std::mem::size_of::<Vec<RetainedAllocation<'_>>>()
                            + 35 * std::mem::size_of::<RetainedAllocation<'_>>(),
                    )
                    .unwrap();
                let mut complete = Vec::new();
                complete.try_reserve_exact(mutation.owners().len()).unwrap();
                complete.extend_from_slice(mutation.owners());
                let inputs = QueryInputs::reserve(
                    memory,
                    RetentionInventory::vector(&complete).unwrap(),
                    context,
                )
                .unwrap();
                inputs
                    .verify_span(mutation.source().as_bytes(), context)
                    .unwrap();
                inputs.verify_span(mutation.columns(), context).unwrap();
                inputs
                    .verify_span(mutation.expression_spans(), context)
                    .unwrap();
                inputs
                    .verify_span(mutation.operator_spans(), context)
                    .unwrap();
                inputs
                    .verify_span(mutation.mutation_spans(), context)
                    .unwrap();
                inputs.verify_span(mutation.parameters(), context).unwrap();
                let runtime = inputs.admit_plan(mutation.plan(), context).unwrap();
                assert_eq!(
                    runtime
                        .plan()
                        .facts(runtime.plan().description().root)
                        .unwrap()
                        .width(),
                    3
                );

                let foreign_path = support::unique_temp_dir("ze140-foreign-memory");
                std::fs::create_dir(&foreign_path).unwrap();
                let foreign_store = Store::open(
                    &foreign_path,
                    OpenOptions::new().with_max_resident_bytes(64 * 1024 * 1024),
                )
                .unwrap();
                {
                    let foreign_shared = GraphResources::from_store(&foreign_store).unwrap();
                    let foreign_memory =
                        QueryMemory::new(&foreign_shared, 8 * 1024 * 1024).unwrap();
                    let foreign_baseline = foreign_memory.reserved_bytes();
                    assert!(matches!(
                        QueryInputs::reserve(
                            &foreign_memory,
                            RetentionInventory::vector(&complete).unwrap(),
                            context,
                        ),
                        Err(MemoryError::UnprovedInput)
                    ));
                    assert_eq!(foreign_memory.reserved_bytes(), foreign_baseline);
                }
                drop(foreign_store);
                std::fs::remove_dir_all(foreign_path).unwrap();

                let operators = [Operator {
                    inputs: &[],
                    kind: OperatorKind::Unit,
                }];
                let mut unproved_facts = vec![NodeFacts::default()];
                let mut unproved_regions = vec![
                    RetainedRegion::slice(&operators).unwrap(),
                    RetainedRegion::vector(&unproved_facts).unwrap(),
                ];
                unproved_regions.sort();
                let unproved = GraphPlan::validate(
                    PlanDescription {
                        operators: &operators,
                        expressions: &[],
                        parameters: &[],
                        root: PlanNodeId(0),
                        eager_searches: &[],
                    },
                    &mut unproved_facts,
                    PlanFootprint::declared(memory.reserved_bytes()),
                    PlanBacking::vector(&unproved_regions).unwrap(),
                    context,
                )
                .unwrap();
                let unproved_owners = [RetainedAllocation::array(&operators).unwrap()];
                let inputs = QueryInputs::reserve(
                    memory,
                    RetentionInventory::array(&unproved_owners),
                    context,
                )
                .unwrap();
                assert!(matches!(
                    inputs.admit_plan(&unproved, context),
                    Err(MemoryError::UnprovedInput)
                ));

                let mut missing_parameter_metadata = complete.clone();
                missing_parameter_metadata.remove(18);
                let inputs = QueryInputs::reserve(
                    memory,
                    RetentionInventory::vector(&missing_parameter_metadata).unwrap(),
                    context,
                )
                .unwrap();
                assert!(matches!(
                    inputs.verify_span(mutation.parameters(), context),
                    Err(MemoryError::UnprovedInput)
                ));

                let mut missing_mutations = complete.clone();
                missing_mutations.remove(29);
                let inputs = QueryInputs::reserve(
                    memory,
                    RetentionInventory::vector(&missing_mutations).unwrap(),
                    context,
                )
                .unwrap();
                assert!(matches!(
                    inputs.admit_plan(mutation.plan(), context),
                    Err(MemoryError::Plan(_)) | Err(MemoryError::UnprovedInput)
                ));

                let mut missing_spans = complete;
                missing_spans.remove(30);
                let inputs = QueryInputs::reserve(
                    memory,
                    RetentionInventory::vector(&missing_spans).unwrap(),
                    context,
                )
                .unwrap();
                assert!(matches!(
                    inputs.verify_span(mutation.mutation_spans(), context),
                    Err(MemoryError::UnprovedInput)
                ));
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn mutation_lowering_limits_cancel_timeout_memory_and_work_before_consumer() {
    let path = support::unique_temp_dir("ze140-mutation-controls");
    std::fs::create_dir(&path).unwrap();
    let store = Store::open(
        &path,
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let query = "CREATE (a:A)-[r:R]->(b {p:type(r)}) RETURN b LIMIT 0";

    {
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
        let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
        token.cancel();
        let baseline = memory.reserved_bytes();
        let mut calls = 0;
        let error = compile_mutation_in(
            query,
            &[],
            CompileLimits::default(),
            &memory,
            &mut context,
            |_, _| {
                calls += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Cancelled));
        assert_eq!(calls, 0);
        assert_eq!(memory.reserved_bytes(), baseline);
    }

    {
        let control =
            QueryControl::Deadline(Deadline::after(std::time::Duration::from_millis(2)).unwrap());
        let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
        let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let baseline = memory.reserved_bytes();
        let mut calls = 0;
        let error = compile_mutation_in(
            query,
            &[],
            CompileLimits::default(),
            &memory,
            &mut context,
            |_, _| {
                calls += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Timeout));
        assert_eq!(calls, 0);
        assert_eq!(memory.reserved_bytes(), baseline);
    }

    let control = QueryControl::Cancel(CancelToken::new());
    let memory = QueryMemory::new(&shared, 1024).unwrap();
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let baseline = memory.reserved_bytes();
    let mut calls = 0;
    let error = compile_mutation_in(
        query,
        &[],
        CompileLimits::default(),
        &memory,
        &mut context,
        |_, _| {
            calls += 1;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Memory));
    assert_eq!(calls, 0);
    assert_eq!(memory.reserved_bytes(), baseline);
    drop(memory);

    let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
    let mut context = ValueContext::new(&view, &control, 1).unwrap();
    let baseline = memory.reserved_bytes();
    let mut calls = 0;
    let error = compile_mutation_in(
        query,
        &[],
        CompileLimits::default(),
        &memory,
        &mut context,
        |_, _| {
            calls += 1;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Resource(ResourceError::WorkLimit));
    assert_eq!(calls, 0);
    assert_eq!(context.work(), 1);
    assert_eq!(memory.reserved_bytes(), baseline);

    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let limits = CompileLimits {
        text_bytes: query.len() - 1,
        ..CompileLimits::default()
    };
    let mut calls = 0;
    let error = compile_mutation_in(query, &[], limits, &memory, &mut context, |_, _| {
        calls += 1;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::Limit(zeppelin_embed_cypher::LimitKind::TextBytes)
    );
    assert_eq!(calls, 0);

    drop(memory);
    drop(shared);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}
