//! Directed compiler/core seam proof, not a general read lowerer or TCK runner.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{QueryView, ValueContext, plan::*, resources::*},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{CompileLimits, compile_in};
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("ze-55-core-lowering-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn mark(
    id: ExprId,
    expressions: &[Expression<'_>],
    seen: &mut [bool; MAX_PLAN_NODES],
    depth: usize,
) {
    assert!(depth <= MAX_PLAN_DEPTH);
    if seen[id.0 as usize] {
        return;
    }
    seen[id.0 as usize] = true;
    match expressions[id.0 as usize] {
        Expression::Unary { operand, .. } => mark(operand, expressions, seen, depth + 1),
        Expression::Binary { left, right, .. } => {
            mark(left, expressions, seen, depth + 1);
            mark(right, expressions, seen, depth + 1);
        }
        Expression::List(values) => {
            for value in values {
                mark(*value, expressions, seen, depth + 1);
            }
        }
        Expression::Literal(_) => {}
        _ => panic!("fixture is a closed scalar RETURN; operator lowering belongs ZE56"),
    }
}
fn region<T>(arena: &QueryArena<'_, '_, T>) -> RetainedRegion {
    RetainedRegion::declared(arena.as_slice().as_ptr() as usize, arena.heap_bytes()).unwrap()
}
#[test]
fn closed_return_lowering_uses_copied_accounted_owners_and_validated_fact_vec() {
    let directory = Scratch::new();
    let store = Store::open(
        &directory.0,
        OpenOptions::new().with_max_resident_bytes(4 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let before = shared.reserved_bytes().unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    {
        let memory = QueryMemory::new(&shared, 1024 * 1024).unwrap();
        let baseline = memory.reserved_bytes();
        compile_in("RETURN -9223372036854775808 AS lo, 9223372036854775807 AS hi, 'λ\\u0000' AS text, [] AS empty, [1,2] AS list, 1+2*3 AS number, [null] = [null] AS nullable, 1 < 'a' AS incomparable", &[], CompileLimits::default(), &memory, &control, |bound| {
            let mut seen = [false; MAX_PLAN_NODES];
            let mut remap = [None; MAX_PLAN_NODES];
            for column in bound.columns() { mark(column.expression, bound.expressions(), &mut seen, 1); }
            let mut text_bytes = 0;
            let mut list_elements = 0;
            let mut count = 0;
            for (index, expression) in bound.expressions().iter().enumerate() {
                if !seen[index] { continue; }
                remap[index] = Some(ExprId(count)); count += 1;
                match expression { Expression::Literal(Literal::String(s)) => text_bytes += s.len(), Expression::List(v) => list_elements += v.len(), _ => {} }
            }
            let mut bytes = QueryArena::<u8>::new(&memory, text_bytes.max(1)).unwrap();
            let mut links = QueryArena::<ExprId>::new(&memory, list_elements.max(1)).unwrap();
            for (index, expression) in bound.expressions().iter().enumerate() {
                if !seen[index] { continue; }
                match expression {
                    Expression::Literal(Literal::String(s)) => for b in s.bytes() { control.checkpoint().unwrap(); bytes.push(b).unwrap(); },
                    Expression::List(v) => for id in *v { control.checkpoint().unwrap(); links.push(remap[id.0 as usize].unwrap()).unwrap(); },
                    _ => {}
                }
            }
            let mut expressions = QueryArena::<Expression<'_>>::new(&memory, count as usize).unwrap();
            let mut text_offset = 0; let mut link_offset = 0;
            for (index, expression) in bound.expressions().iter().enumerate() {
                if !seen[index] { continue; }
                control.checkpoint().unwrap();
                let copied = match *expression {
                    Expression::Literal(Literal::String(s)) => {
                        let value = std::str::from_utf8(&bytes.as_slice()[text_offset..text_offset+s.len()]).unwrap(); text_offset += s.len();
                        assert_ne!(value.as_ptr(), s.as_ptr());
                        Expression::Literal(Literal::String(value))
                    }
                    Expression::List(v) => { let values = &links.as_slice()[link_offset..link_offset+v.len()]; link_offset += v.len(); Expression::List(values) }
                    Expression::Binary { operation, left, right } => Expression::Binary { operation, left: remap[left.0 as usize].unwrap(), right: remap[right.0 as usize].unwrap() },
                    Expression::Unary { operation, operand } => Expression::Unary { operation, operand: remap[operand.0 as usize].unwrap() },
                    Expression::Literal(value) => Expression::Literal(value),
                    _ => panic!("closed RETURN fixture"),
                };
                expressions.push(copied).unwrap();
            }
            let mut projections = QueryArena::new(&memory, bound.columns().len()).unwrap();
            for column in bound.columns() { projections.push(Projection { slot: column.slot, expression: remap[column.expression.0 as usize].unwrap() }).unwrap(); }
            let mut inputs = QueryArena::new(&memory, 1).unwrap(); inputs.push(PlanNodeId(0)).unwrap();
            let mut operators = QueryArena::new(&memory, 2).unwrap();
            operators.push(Operator { inputs: &[], kind: OperatorKind::Unit }).unwrap();
            operators.push(Operator { inputs: inputs.as_slice(), kind: OperatorKind::Project(projections.as_slice()) }).unwrap();
            // Actual facts Vec and validator scratch remain reserved separately
            // while every copied IR arena and the frontend AST still coexist.
            let mut charge = memory.reserve_external_capacity().unwrap();
            charge.reserve_additional(2*std::mem::size_of::<NodeFacts>() + 7*std::mem::size_of::<RetainedRegion>() + VALIDATION_SCRATCH_BYTES + std::mem::size_of::<GraphPlan<'_, '_>>()).unwrap();
            let mut facts = Vec::new(); facts.try_reserve_exact(2).unwrap();
            if facts.capacity() > 2 { charge.reserve_additional((facts.capacity()-2)*std::mem::size_of::<NodeFacts>()).unwrap(); }
            facts.resize_with(2, NodeFacts::default);
            let mut regions = [region(&bytes), region(&links), region(&expressions), region(&projections), region(&inputs), region(&operators), RetainedRegion::vector(&facts).unwrap()];
            regions.sort_unstable();
            let footprint = VALIDATION_SCRATCH_BYTES + std::mem::size_of_val(&regions) + regions.iter().map(|r| r.end()-r.start()).sum::<usize>();
            let description = PlanDescription { operators: operators.as_slice(), expressions: expressions.as_slice(), parameters: &[], root: PlanNodeId(1), eager_searches: &[] };
            let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
            let plan = GraphPlan::validate_with_fact_vec(description, &mut facts, PlanFootprint::declared(footprint), PlanBacking::new(&regions, std::mem::size_of_val(&regions)).unwrap(), &mut context).unwrap();
            assert_eq!(plan.facts(PlanNodeId(1)).unwrap().width(), bound.columns().len());
            for column in bound.columns() { assert_eq!(plan.facts(PlanNodeId(1)).unwrap().slot(column.slot), Some(column.kinds)); }
            assert_eq!(shared.reserved_bytes().unwrap(), before + memory.reserved_bytes() as u64);
            Ok(())
        }).unwrap();
        assert_eq!(memory.reserved_bytes(), baseline);
    }
    assert_eq!(shared.reserved_bytes().unwrap(), before);
    store.close().unwrap();
}

#[test]
fn bound_id_extensions_reach_core_without_truncating_any_identity_bits() {
    use zeppelin_embed::property_graph::{
        NodeId, RelId,
        query::{QueryValue, plan::UnaryExpression},
    };
    use zeppelin_embed_cypher::{Budget, compile_with};
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let node = view.node(NodeId::new(0xfedc_ba98_7654_3210_0123_4567_89ab_cdef).unwrap());
    let rel = view.relationship(RelId::new(0x8000_0000_0000_0001_ffff_ffff_ffff_ffff).unwrap());
    compile_with(
        "MATCH (n)-[r]->() RETURN ze.node_id(n) AS n, ze.relationship_id(r) AS r",
        &[],
        CompileLimits::default(),
        &mut Budget::default(),
        |bound| {
            for (column, value, expected) in [
                (0, node, "fedcba98765432100123456789abcdef"),
                (1, rel, "8000000000000001ffffffffffffffff"),
            ] {
                let mut output = [0; 32];
                let value = match bound.expressions()[bound.columns()[column].expression.0 as usize]
                {
                    Expression::Unary {
                        operation: UnaryExpression::NodeIdText,
                        ..
                    } => value.node_id_text(&mut output, &mut context).unwrap(),
                    Expression::Unary {
                        operation: UnaryExpression::RelIdText,
                        ..
                    } => value
                        .relationship_id_text(&mut output, &mut context)
                        .unwrap(),
                    _ => panic!("ID extension did not lower to its typed core operation"),
                };
                let QueryValue::String(value) = value else {
                    panic!("ID text")
                };
                assert_eq!(value, expected);
            }
            Ok(())
        },
    )
    .unwrap();
}

#[test]
fn bound_ieee_scalars_and_empty_lists_keep_core_arithmetic_and_property_rules() {
    use zeppelin_embed::property_graph::{
        PropertyData,
        query::{
            PropertyScratch, QueryError, QueryList, QueryValue, Truth, plan::BinaryExpression,
        },
    };
    use zeppelin_embed_cypher::{Budget, compile_with};
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let nan = QueryValue::F64(f64::from_bits(0x7ff8_0000_0000_1234));
    let parameters = [ParameterBinding {
        name: "p",
        value: nan,
    }];
    compile_with(
        "RETURN $p = $p AS equality, $p + 1.0 AS arithmetic",
        &parameters,
        CompileLimits::default(),
        &mut Budget::default(),
        |bound| {
            for column in bound.columns() {
                let Expression::Binary { operation, .. } =
                    bound.expressions()[column.expression.0 as usize]
                else {
                    panic!("typed binary")
                };
                match operation {
                    BinaryExpression::Comparison(operation) => assert_eq!(
                        nan.predicate(nan, operation, &mut context).unwrap(),
                        Truth::False
                    ),
                    BinaryExpression::Arithmetic(operation) => assert!(matches!(
                        nan.arithmetic(QueryValue::F64(1.0), operation),
                        Err(QueryError::ArithmeticDomain)
                    )),
                    _ => panic!("exact scalar operation"),
                }
            }
            let assignment = bound.parameters()[0]
                .value
                .to_property(PropertyScratch::None, &mut context)
                .unwrap();
            let Some(PropertyData::F64(value)) = assignment.data() else {
                panic!("F64 assignment")
            };
            assert_eq!(value.to_bits(), 0x7ff8_0000_0000_1234);
            Ok(())
        },
    )
    .unwrap();
    for (query, left, right, expected) in [
        (
            "RETURN 1e308 * 1e308",
            QueryValue::F64(1e308),
            QueryValue::F64(1e308),
            QueryError::ArithmeticOverflow,
        ),
        (
            "RETURN 9223372036854775807 + 1",
            QueryValue::I64(i64::MAX),
            QueryValue::I64(1),
            QueryError::ArithmeticOverflow,
        ),
        (
            "RETURN 1.0 / -0.0",
            QueryValue::F64(1.0),
            QueryValue::F64(-0.0),
            QueryError::DivisionByZero,
        ),
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                let Expression::Binary {
                    operation: BinaryExpression::Arithmetic(operation),
                    ..
                } = bound.expressions()[bound.columns()[0].expression.0 as usize]
                else {
                    panic!("arithmetic lowering")
                };
                assert_eq!(left.arithmetic(right, operation).unwrap_err(), expected);
                Ok(())
            },
        )
        .unwrap();
    }
    compile_with(
        "RETURN [] AS empty, null AS missing",
        &[],
        CompileLimits::default(),
        &mut Budget::default(),
        |bound| {
            for column in bound.columns() {
                let value = match bound.expressions()[column.expression.0 as usize] {
                    Expression::List([]) => {
                        QueryValue::List(QueryList::new(&[], &mut context).unwrap())
                    }
                    Expression::Literal(Literal::Null) => QueryValue::Null,
                    _ => panic!("empty/null lowering"),
                };
                let property = value
                    .to_property(PropertyScratch::None, &mut context)
                    .unwrap();
                if column.name == "empty" {
                    assert!(matches!(
                        property.data(),
                        Some(PropertyData::EmptyList { count: 0 })
                    ));
                } else {
                    assert!(property.data().is_none());
                }
            }
            Ok(())
        },
    )
    .unwrap();
}

#[test]
fn unary_numeric_binding_matches_core_property_kind_mask() {
    use zeppelin_embed::property_graph::GraphName;
    use zeppelin_embed_cypher::{Budget, compile_with};
    for query in [
        "MATCH (n) RETURN +n.p AS number",
        "MATCH (n) RETURN -n.p AS number",
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                let column = bound.columns()[0];
                let Expression::Unary { operation, .. } =
                    bound.expressions()[column.expression.0 as usize]
                else {
                    panic!("numeric unary binding");
                };
                let expressions = [
                    Expression::Slot(SlotId(0)),
                    Expression::Property {
                        entity: ExprId(0),
                        name: GraphName::new("p").unwrap(),
                    },
                    Expression::Unary {
                        operation,
                        operand: ExprId(1),
                    },
                ];
                let projection = [Projection {
                    slot: SlotId(1),
                    expression: ExprId(2),
                }];
                let operators = [
                    Operator {
                        inputs: &[],
                        kind: OperatorKind::Unit,
                    },
                    Operator {
                        inputs: &[PlanNodeId(0)],
                        kind: OperatorKind::ScanNodes {
                            output: SlotId(0),
                            label: None,
                        },
                    },
                    Operator {
                        inputs: &[PlanNodeId(1)],
                        kind: OperatorKind::Project(&projection),
                    },
                ];
                let view =
                    QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
                let control = QueryControl::Cancel(CancelToken::new());
                let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
                let mut facts = [
                    NodeFacts::default(),
                    NodeFacts::default(),
                    NodeFacts::default(),
                ];
                // Structural/type agreement only. This raw-slice validation grants
                // no runtime ownership certificate or execution authority.
                let mut regions = [
                    RetainedRegion::slice(&operators).unwrap(),
                    RetainedRegion::slice(&expressions).unwrap(),
                    RetainedRegion::slice(&projection).unwrap(),
                    RetainedRegion::slice(operators[1].inputs).unwrap(),
                    RetainedRegion::slice(operators[2].inputs).unwrap(),
                    RetainedRegion::slice(&facts).unwrap(),
                    RetainedRegion::slice("p".as_bytes()).unwrap(),
                ];
                regions.sort_unstable();
                let plan = GraphPlan::validate(
                    PlanDescription {
                        operators: &operators,
                        expressions: &expressions,
                        parameters: &[],
                        root: PlanNodeId(2),
                        eager_searches: &[],
                    },
                    &mut facts,
                    PlanFootprint::declared(1024 * 1024),
                    PlanBacking::new(&regions, std::mem::size_of_val(&regions)).unwrap(),
                    &mut context,
                )
                .unwrap();
                let numeric = ValueKinds::NULL
                    .union(ValueKinds::I64)
                    .union(ValueKinds::F64);
                assert_eq!(
                    plan.facts(PlanNodeId(2)).unwrap().slot(SlotId(1)),
                    Some(numeric)
                );
                assert_eq!(column.kinds, numeric);
                Ok(())
            },
        )
        .unwrap();
    }
}
