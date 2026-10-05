//! Shared tooling runner. Structured plans are authored in the manifest, never compiled from Cypher.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[path = "../../crates/zeppelin-embed-ffi/tests/common/mod.rs"]
mod common;
#[path = "../../crates/zeppelin-embed-cypher/tests/support/tck.rs"]
pub mod tck;
use common::graph::*;
use common::sized_zeroed;
use std::collections::{BTreeMap, BTreeSet};
use tck::V;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl};
use zeppelin_embed::property_graph::query::completed::{CompletedGraphResult, GraphQueryOptions};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::{GraphName, GraphPlanBacking, GraphQueryPlan, GraphStore};
use zeppelin_embed_bench::harness_json::{Value as Json, json};
use zeppelin_embed_ffi::*;

pub fn manifest() -> Json {
    zeppelin_embed_bench::harness_json::from_str(include_str!(
        "../../bindings/fixtures/graph_profile_parity_v1.json"
    ))
    .unwrap()
}
fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
fn rust_cypher(store: &GraphStore, query: &str) -> Result<CompletedGraphResult, String> {
    zeppelin_embed_cypher::execute(
        store.statement_store(),
        &control(),
        &GraphQueryOptions::default(),
        query,
        &[],
        zeppelin_embed_cypher::CompileLimits::default(),
    )
    .map_err(statement_error)
}
fn number(j: &Json, name: &str) -> u32 {
    j[name].as_u64().unwrap() as u32
}
fn indices(j: &Json, key: &str) -> Vec<usize> {
    j[key]
        .as_array()
        .map(|a| a.iter().map(|n| n.as_u64().unwrap() as usize).collect())
        .unwrap_or_default()
}
fn rust_structured(
    store: &GraphStore,
    plan: &Json,
    columns: &[&str],
) -> Result<CompletedGraphResult, String> {
    let strings: Vec<String> = plan["expressions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["value"].as_str().unwrap_or("").to_owned())
        .collect();
    let children: Vec<Vec<ExprId>> = plan["expressions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            indices(e, "items")
                .iter()
                .map(|n| ExprId(*n as u32))
                .collect()
        })
        .collect();
    let expr: Vec<Expression<'_>> = plan["expressions"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, e)| match e["kind"].as_str().unwrap() {
            "literal" => Expression::Literal(match e["type"].as_str().unwrap() {
                "null" => Literal::Null,
                "float" => Literal::F64(e["value"].as_f64().unwrap()),
                "string" => Literal::String(&strings[i]),
                "integer" => Literal::I64(e["value"].as_i64().unwrap()),
                "bool" => Literal::Bool(e["value"].as_bool().unwrap()),
                _ => panic!("unsupported authored literal"),
            }),
            "list" => Expression::List(&children[i]),
            "slot" => Expression::Slot(SlotId(number(e, "value"))),
            "count" | "collect" => Expression::Aggregate {
                operation: if e["kind"] == "count" {
                    AggregateExpression::Count { distinct: false }
                } else {
                    AggregateExpression::Collect { distinct: false }
                },
                operand: e["operand"].as_u64().map(|i| ExprId(i as u32)),
            },
            _ => panic!("unsupported authored expression"),
        })
        .collect();
    let projections: Vec<Projection> = plan["projections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| Projection {
            slot: SlotId(number(p, "slot")),
            expression: ExprId(number(p, "expression")),
        })
        .collect();
    let ops = plan["operators"].as_array().unwrap();
    let inputs: Vec<Vec<PlanNodeId>> = ops
        .iter()
        .map(|o| {
            indices(o, "inputs")
                .into_iter()
                .map(|n| PlanNodeId(n as u32))
                .collect()
        })
        .collect();
    let lists: Vec<Vec<Projection>> = ops
        .iter()
        .map(|o| {
            indices(o, "projections")
                .iter()
                .map(|i| projections[*i])
                .collect()
        })
        .collect();
    let aggregates: Vec<Vec<Projection>> = ops
        .iter()
        .map(|o| {
            indices(o, "aggregates")
                .iter()
                .map(|i| projections[*i])
                .collect()
        })
        .collect();
    let labels: Vec<String> = ops
        .iter()
        .map(|o| o["label"].as_str().unwrap_or("").to_owned())
        .collect();
    let mutations = vec![Mutation::CreateNode {
        output: SlotId(3),
        labels: &[],
    }];
    let operators: Vec<Operator<'_>> = ops
        .iter()
        .enumerate()
        .map(|(i, o)| Operator {
            inputs: &inputs[i],
            kind: match o["kind"].as_str().unwrap() {
                "unit" => OperatorKind::Unit,
                "scan" => OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: if labels[i].is_empty() {
                        None
                    } else {
                        Some(GraphName::new(&labels[i]).unwrap())
                    },
                },
                "project" => OperatorKind::Project(&lists[i]),
                "with" => OperatorKind::With(&lists[i]),
                "eager" => OperatorKind::Eager,
                "mutate" => OperatorKind::Mutate(&mutations),
                "limit" => OperatorKind::OffsetLimit {
                    offset: 0,
                    limit: Some(o["limit"].as_u64().unwrap()),
                },
                "aggregate" => OperatorKind::Aggregate {
                    keys: &lists[i],
                    aggregates: &aggregates[i],
                },
                "optional" => OperatorKind::OptionalApply {
                    predicate: Some(ExprId(number(o, "predicate"))),
                },
                "expand" => OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(2),
                    relationship: SlotId(1),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
                "bounded" => OperatorKind::BoundedExpand {
                    source: SlotId(0),
                    node: SlotId(2),
                    relationships: SlotId(1),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                    min: number(o, "min") as u8,
                    max: number(o, "max") as u8,
                    edge_predicate: None,
                    completed_edge_predicate: None,
                },
                _ => panic!("unsupported authored operator"),
            },
        })
        .collect();
    let mut backing = GraphPlanBacking::default();
    for s in &strings {
        backing.string(s).unwrap();
    }
    for c in &children {
        backing.vec(c).unwrap();
    }
    for label in &labels {
        backing.string(label).unwrap();
    }
    for i in &inputs {
        backing.vec(i).unwrap();
    }
    for p in &lists {
        backing.vec(p).unwrap();
    }
    for p in &aggregates {
        backing.vec(p).unwrap();
    }
    backing.vec(&mutations).unwrap();
    let parameters = vec![];
    let searches = vec![];
    store
        .query(
            &control(),
            &GraphQueryOptions::default(),
            &GraphQueryPlan {
                operators: &operators,
                expressions: &expr,
                parameters: &parameters,
                eager_searches: &searches,
                root: PlanNodeId(number(plan, "root")),
                backing: &backing,
                bindings: &[],
                columns,
            },
        )
        .map_err(|e| format!("{e:?}"))
}
fn c_cypher(handle: ZeGraphHandle, query: &str) -> (ZeErrorCode, ZeGraphResponse) {
    let mut r = empty_response();
    let q = cypher_request(query.as_bytes(), &[], None);
    let code = ze_graph_cypher(handle, &q, &mut r);
    (code, r)
}
fn c_structured(handle: ZeGraphHandle, authored: &Json) -> (ZeErrorCode, ZeGraphResponse) {
    let mut adapted = authored.clone();
    if adapted["operators"][0]["kind"] == "unit" && adapted["operators"][1]["kind"] == "scan" {
        adapted["operators"].as_array_mut().unwrap().remove(0);
        adapted["root"] = json!(authored["root"].as_u64().unwrap() - 1);
        for o in adapted["operators"].as_array_mut().unwrap() {
            if o["kind"] == "scan" {
                o["inputs"] = json!([]);
            } else {
                for i in o["inputs"].as_array_mut().unwrap() {
                    *i = json!(i.as_u64().unwrap() - 1);
                }
            }
        }
    }
    let p = &adapted;
    let mut pool: ZeGraphValuePool = sized_zeroed();
    let mut values = Vec::new();
    let mut expressions = Vec::new();
    let mut expression_children = Vec::new();
    let mut literal_bytes = Vec::new();
    for e in p["expressions"].as_array().unwrap() {
        let mut expression: ZeGraphExpression = sized_zeroed();
        match e["kind"].as_str().unwrap() {
            "literal" => {
                let mut v: ZeGraphValue = sized_zeroed();
                match e["type"].as_str().unwrap() {
                    "null" => {}
                    "float" => {
                        v.tag = 3;
                        v.floating = e["value"].as_f64().unwrap();
                    }
                    "string" => {
                        v.tag = 4;
                        let s = e["value"].as_str().unwrap();
                        v.range = ZeGraphRange {
                            start: literal_bytes.len() as u32,
                            count: s.len() as u32,
                        };
                        literal_bytes.extend_from_slice(s.as_bytes());
                    }
                    "integer" => {
                        v.tag = 2;
                        v.integer = e["value"].as_i64().unwrap();
                    }
                    "bool" => {
                        v.tag = 1;
                        v.boolean = u32::from(e["value"].as_bool().unwrap());
                    }
                    _ => panic!("literal"),
                };
                expression.value = values.len() as u32;
                values.push(v);
            }
            "list" => {
                expression.kind = 7;
                let children = indices(e, "items");
                expression.children = ZeGraphRange {
                    start: expression_children.len() as u32,
                    count: children.len() as u32,
                };
                expression_children.extend(children.iter().map(|i| *i as u32));
            }
            "slot" => {
                expression.kind = 1;
                expression.value = number(e, "value");
            }
            "count" | "collect" => {
                expression.kind = 8;
                expression.operation = if e["kind"] == "count" { 0 } else { 1 };
                if let Some(n) = e["operand"].as_u64() {
                    expression.has_operand = 1;
                    expression.left = n as u32;
                }
            }
            _ => panic!("expression"),
        };
        expressions.push(expression);
    }
    pool.values = values.as_ptr();
    pool.value_count = values.len();
    let projections: Vec<ZeGraphProjection> = p["projections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| {
            let mut v: ZeGraphProjection = sized_zeroed();
            v.slot = number(j, "slot");
            v.expression = number(j, "expression");
            v
        })
        .collect();
    let mut operators = Vec::new();
    let mut inputs = Vec::new();
    let mut selected = Vec::new();
    let mut bytes = literal_bytes;
    for o in p["operators"].as_array().unwrap() {
        let mut v: ZeGraphOperator = sized_zeroed();
        v.inputs = ZeGraphRange {
            start: if indices(o, "inputs").is_empty() {
                0
            } else {
                inputs.len() as u32
            },
            count: indices(o, "inputs").len() as u32,
        };
        inputs.extend(indices(o, "inputs").iter().map(|i| *i as u32));
        for (key, dest) in [
            ("projections", &mut v.projections),
            ("aggregates", &mut v.aggregates),
        ] {
            let list = indices(o, key);
            *dest = ZeGraphRange {
                start: if list.is_empty() {
                    0
                } else {
                    selected.len() as u32
                },
                count: list.len() as u32,
            };
            selected.extend(list.iter().map(|i| projections[*i]));
        }
        v.kind = match o["kind"].as_str().unwrap() {
            "unit" => 0,
            "scan" => 4,
            "eager" => 5,
            "mutate" => 6,
            "aggregate" => 7,
            "limit" => 9,
            "expand" => 13,
            "bounded" => 14,
            "optional" => 15,
            "project" => 16,
            "with" => 17,
            _ => panic!("operator"),
        };
        if v.kind == 4 {
            v.inputs = ZeGraphRange { start: 0, count: 0 };
            if let Some(label) = o["label"].as_str() {
                v.has_name = 1;
                v.name = ZeGraphRange {
                    start: bytes.len() as u32,
                    count: label.len() as u32,
                };
                bytes.extend_from_slice(label.as_bytes());
            }
        }
        if v.kind == 6 {
            v.mutations = ZeGraphRange { start: 0, count: 1 };
        }
        if v.kind == 9 {
            v.has_limit = 1;
            v.limit = o["limit"].as_u64().unwrap();
        }
        if v.kind == 13 || v.kind == 14 {
            v.node_slot = 2;
            v.relationship_slot = 1;
            if v.kind == 14 {
                v.path_min = number(o, "min");
                v.path_max = number(o, "max");
            }
        }
        if v.kind == 15 {
            v.predicate = ZeGraphOptionalIndex {
                present: 1,
                index: number(o, "predicate"),
            };
        }
        operators.push(v);
    }
    let mut mutation: ZeGraphMutation = sized_zeroed();
    mutation.output = 3;
    pool.bytes = bytes.as_ptr();
    pool.byte_count = bytes.len();
    let mut plan: ZeGraphPlan = sized_zeroed();
    plan.root = number(p, "root");
    plan.pool = &pool;
    plan.operators = operators.as_ptr();
    plan.operator_count = operators.len();
    plan.inputs = inputs.as_ptr();
    plan.input_count = inputs.len();
    plan.expressions = expressions.as_ptr();
    plan.expression_count = expressions.len();
    plan.projections = selected.as_ptr();
    plan.projection_count = selected.len();
    plan.mutations = &mutation;
    plan.mutation_count = 1;
    plan.expression_children = expression_children.as_ptr();
    plan.expression_child_count = expression_children.len();
    let mut q: ZeGraphQueryRequest = sized_zeroed();
    q.plan = &plan;
    let mut r = empty_response();
    let code = ze_graph_query(handle, &q, &mut r);
    (code, r)
}
unsafe fn slice<'a, T>(p: *const T, n: usize) -> &'a [T] {
    if n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(p, n) }
    }
}
fn c_text(p: &ZeGraphValuePool, r: ZeGraphRange) -> String {
    let b = unsafe { slice(p.bytes, p.byte_count) };
    String::from_utf8(b[r.start as usize..(r.start + r.count) as usize].to_vec()).unwrap()
}
fn c_properties(p: &ZeGraphValuePool, r: ZeGraphRange) -> BTreeMap<String, V> {
    (unsafe { slice(p.properties, p.property_count) })
        [r.start as usize..(r.start + r.count) as usize]
        .iter()
        .map(|v| (c_text(p, v.name), c_value(p, v.value)))
        .collect()
}
fn c_value(p: &ZeGraphValuePool, i: u32) -> V {
    let v = unsafe { slice(p.values, p.value_count) }[i as usize];
    match v.tag {
        0 => V::Null,
        1 => V::Bool(v.boolean != 0),
        2 => V::Int(v.integer),
        3 => V::Float(v.floating),
        4 => V::Str(c_text(p, v.range)),
        7 => V::List(
            unsafe { slice(p.children, p.child_count) }
                [v.range.start as usize..(v.range.start + v.range.count) as usize]
                .iter()
                .map(|i| c_value(p, *i))
                .collect(),
        ),
        5 => {
            let n = unsafe { slice(p.nodes, p.node_count) }[v.entity_index as usize];
            let mut labels = unsafe { slice(p.names, p.name_count) }
                [n.labels.start as usize..(n.labels.start + n.labels.count) as usize]
                .iter()
                .map(|r| c_text(p, *r))
                .collect::<Vec<_>>();
            labels.sort();
            V::Node(labels, c_properties(p, n.properties))
        }
        6 => {
            let r =
                unsafe { slice(p.relationships, p.relationship_count) }[v.entity_index as usize];
            V::Rel(
                c_text(p, r.relationship_type),
                c_properties(p, r.properties),
            )
        }
        _ => panic!("unknown public value tag {}", v.tag),
    }
}
fn c_table(r: &ZeGraphResponse) -> (Vec<String>, Vec<Vec<V>>) {
    let cells = unsafe { slice(r.cells, r.cell_count) };
    let rows = (0..r.row_count)
        .map(|row| {
            (0..r.column_count)
                .map(|col| c_value(&r.pool, cells[row * r.column_count + col]))
                .collect()
        })
        .collect();
    (column_names(r), rows)
}
#[derive(Debug, PartialEq)]
struct State {
    nodes: BTreeSet<String>,
    rels: BTreeSet<String>,
    labels: BTreeSet<String>,
    props: Vec<(bool, String, String, V)>,
}
fn state(
    mut query: impl FnMut(&str) -> Result<(Vec<String>, Vec<Vec<V>>), String>,
) -> Result<State, String> {
    let mut s = State {
        nodes: BTreeSet::new(),
        rels: BTreeSet::new(),
        labels: BTreeSet::new(),
        props: vec![],
    };
    let mut bytes = 0;
    for (rel, q) in [
        (false, "MATCH (n) RETURN ze.node_id(n), n"),
        (true, "MATCH ()-[r]->() RETURN ze.relationship_id(r), r"),
    ] {
        for row in query(q)?.1 {
            bytes += format!("{row:?}").len();
            if bytes > 24 * 1024 * 1024 {
                return Err("24 MiB snapshot cap exceeded".into());
            }
            match row.as_slice() {
                [V::Str(id), V::Node(labels, props)] if !rel => {
                    s.nodes.insert(id.clone());
                    s.labels.extend(labels.iter().cloned());
                    s.props.extend(
                        props
                            .iter()
                            .map(|(k, v)| (false, id.clone(), k.clone(), v.clone())),
                    );
                }
                [V::Str(id), V::Rel(_, props)] if rel => {
                    s.rels.insert(id.clone());
                    s.props.extend(
                        props
                            .iter()
                            .map(|(k, v)| (true, id.clone(), k.clone(), v.clone())),
                    );
                }
                _ => return Err(format!("invalid snapshot {row:?}")),
            }
        }
    }
    Ok(s)
}
fn effects(a: &State, b: &State) -> Vec<usize> {
    vec![
        b.nodes.difference(&a.nodes).count(),
        a.nodes.difference(&b.nodes).count(),
        b.rels.difference(&a.rels).count(),
        a.rels.difference(&b.rels).count(),
        b.labels.difference(&a.labels).count(),
        a.labels.difference(&b.labels).count(),
        b.props.iter().filter(|p| !a.props.contains(p)).count(),
        a.props.iter().filter(|p| !b.props.contains(p)).count(),
    ]
}
fn canonical(rows: &[Vec<V>], ordered: bool) -> Vec<String> {
    let mut r = rows.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>();
    if !ordered {
        r.sort();
    }
    r
}
pub fn run_local(case: &Json, path: &str) -> Result<Json, String> {
    let structured = path.ends_with("structured");
    if structured && case["structured"].is_null() {
        return Err(format!(
            "{}: independent structured translation not implemented",
            case["id"]
        ));
    }
    let expected = case["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|v| tck::parse_value(v.as_str().unwrap()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let header = case["header"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect::<Vec<_>>();
    let changed = case["effects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_u64().unwrap() != 0);
    let expected_outcome = if changed {
        2
    } else if case["write"].as_bool().unwrap_or(false) {
        4
    } else {
        0
    };
    let params = parameter_values(case);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("graph");
    let (actual, delta, metadata, column_kinds) = if path.starts_with("rust") {
        let store = GraphStore::create(
            &root,
            OpenOptions::new().with_max_resident_bytes(256 << 20),
            None,
        )
        .map_err(|e| e.to_string())?;
        for setup in case["setup"].as_array().unwrap() {
            rust_cypher(&store, setup.as_str().unwrap())?;
        }
        let generation = rust_cypher(&store, "RETURN 1")?.metadata().generation;
        let before = state(|q| rust_cypher(&store, q).map(|r| tck::actual_table(&r)))?;
        let r = if structured {
            rust_structured(&store, &case["structured"], &header)
        } else {
            rust_cypher_parameters(&store, case["query"].as_str().unwrap(), &params)
        };
        if !case["error"].is_null() {
            let error = r.err().ok_or("expected compile rejection")?;
            if !error.contains(case["error"]["rust"].as_str().unwrap()) {
                return Err(error);
            }
            if let Some(message) = case["error"]["message"].as_str()
                && !error.contains(message)
            {
                return Err("wrong rejection reason".into());
            }
            if error.contains("nothing_committed=false") {
                return Err(
                    "indeterminate write: recovery required before observing TCK state".into(),
                );
            }
            if rust_cypher(&store, "RETURN 1")?.metadata().generation != generation {
                return Err("rejection changed generation".into());
            }
            let after = state(|q| rust_cypher(&store, q).map(|r| tck::actual_table(&r)))?;
            if before != after {
                return Err("rejection mutated state".into());
            }
            let observed_stage = if structured && error.contains("Admission(Plan(") {
                "typed-plan-validation"
            } else if error.starts_with("Compile(") {
                "compile"
            } else {
                "runtime"
            };
            if case["error"]["stage"].as_str() != Some(observed_stage) {
                return Err("wrong Rust error stage".into());
            }
            store.close().map_err(|e| e.to_string())?;
            let reopened = GraphStore::open(
                &root,
                OpenOptions::new().with_max_resident_bytes(256 << 20),
                None,
            )
            .map_err(|e| e.to_string())?;
            if state(|q| rust_cypher(&reopened, q).map(|r| tck::actual_table(&r)))? != after {
                return Err("rejected state changed on reopen".into());
            }
            reopened.close().map_err(|e| e.to_string())?;
            return Ok(
                json!({"case":case["id"],"path":path,"state":"focused GREEN","error":error,"stage":case["error"]["stage"],"effects":[0,0,0,0,0,0,0,0],"released":true,"reopened":true}),
            );
        }
        let r = r?;
        use zeppelin_embed::property_graph::query::completed::Outcome;
        let expected_generation = generation.get() + u64::from(changed);
        let actual_outcome = match r.metadata().outcome {
            Outcome::Read => 0,
            Outcome::Replayed => 3,
            Outcome::NoOp => 4,
            Outcome::Committed { changed: g } => {
                if g.get() != expected_generation {
                    return Err("wrong changed generation".into());
                }
                2
            }
        };
        if actual_outcome != expected_outcome
            || r.metadata().generation != generation
            || rust_cypher(&store, "RETURN 1")?.metadata().generation.get() != expected_generation
        {
            return Err("wrong Rust disposition/admitted/final generation".into());
        }
        let metadata = format!("{:?}", r.metadata());
        let after = state(|q| rust_cypher(&store, q).map(|r| tck::actual_table(&r)))?;
        store.close().map_err(|e| e.to_string())?;
        let reopened = GraphStore::open(
            &root,
            OpenOptions::new().with_max_resident_bytes(256 << 20),
            None,
        )
        .map_err(|e| e.to_string())?;
        if state(|q| rust_cypher(&reopened, q).map(|r| tck::actual_table(&r)))? != after {
            return Err("reopen changed state".into());
        }
        reopened.close().map_err(|e| e.to_string())?;
        (
            tck::actual_table(&r),
            effects(&before, &after),
            metadata,
            r.pools()
                .columns
                .iter()
                .map(|c| kind_mask(c.kinds))
                .collect::<Vec<_>>(),
        )
    } else {
        let (code, h) = graph_open(&root, MODE_CREATE);
        if code != ZeErrorCode::ZeOk {
            return Err(format!("C open {code:?}"));
        }
        let query = |q: &str| -> Result<(Vec<String>, Vec<Vec<V>>), String> {
            let (code, mut r) = c_cypher(h, q);
            let result = if code == ZeErrorCode::ZeOk {
                Ok(c_table(&r))
            } else {
                Err(format!("C {code:?}"))
            };
            assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
            result
        };
        for setup in case["setup"].as_array().unwrap() {
            query(setup.as_str().unwrap())?;
        }
        let generation = c_generation(h);
        let before = state(query)?;
        let (code, mut r) = if structured {
            c_structured(h, &case["structured"])
        } else {
            CParameters::new(&params).query(h, case["query"].as_str().unwrap())
        };
        if r.disposition == 5 {
            ze_graph_response_free(&mut r);
            ze_graph_close(h);
            return Err(
                "indeterminate C write: recovery required before observing TCK state".into(),
            );
        }
        if !case["error"].is_null() {
            if c_generation(h) != generation {
                return Err("C rejection changed generation".into());
            }
            let error = format!("{code:?}");
            let after = state(query)?;
            assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
            assert_eq!(ze_graph_close(h), ZeErrorCode::ZeOk);
            if error != case["error"]["c"].as_str().unwrap() {
                return Err(format!(
                    "expected public C error {}, got {error}",
                    case["error"]["c"]
                ));
            }
            if before != after {
                return Err("C rejection mutated state".into());
            }
            let (code, reopened) = graph_open(&root, MODE_READ_WRITE);
            assert_eq!(code, ZeErrorCode::ZeOk);
            let durable = state(|q| {
                let (code, mut r) = c_cypher(reopened, q);
                assert_eq!(code, ZeErrorCode::ZeOk);
                let table = c_table(&r);
                assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
                Ok(table)
            })?;
            assert_eq!(ze_graph_close(reopened), ZeErrorCode::ZeOk);
            if durable != after {
                return Err("C rejected state changed on reopen".into());
            }
            return Ok(
                json!({"case":case["id"],"path":path,"state":"focused GREEN","error":error,"stage":case["error"]["stage"],"effects":[0,0,0,0,0,0,0,0],"released":true,"reopened":true}),
            );
        }
        if code != ZeErrorCode::ZeOk {
            let error = format!("{code:?}: {}", last_error(h.token));
            ze_graph_response_free(&mut r);
            ze_graph_close(h);
            return Err(error);
        }
        if r.disposition != expected_outcome
            || r.has_admitted_generation != 1
            || r.admitted_generation != generation
            || r.has_changed_generation != u32::from(changed)
            || (changed && r.changed_generation != generation + 1)
            || c_generation(h) != generation + u64::from(changed)
        {
            ze_graph_response_free(&mut r);
            ze_graph_close(h);
            return Err("wrong C disposition/admitted/changed/final generation".into());
        }
        let after = state(query)?;
        let metadata = format!(
            "disposition={};admitted={};changed={}",
            r.disposition, r.admitted_generation, r.changed_generation
        );
        assert_eq!(ze_graph_close(h), ZeErrorCode::ZeOk);
        let actual = c_table(&r);
        let column_kinds = unsafe { slice(r.columns, r.column_count) }
            .iter()
            .map(|c| c.kinds)
            .collect::<Vec<_>>();
        assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
        let (code, h) = graph_open(&root, MODE_READ_WRITE);
        assert_eq!(code, ZeErrorCode::ZeOk);
        let reopened = state(|q| {
            let (code, mut r) = c_cypher(h, q);
            assert_eq!(code, ZeErrorCode::ZeOk);
            let table = c_table(&r);
            assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
            Ok(table)
        })?;
        assert_eq!(ze_graph_close(h), ZeErrorCode::ZeOk);
        if reopened != after {
            return Err("C reopen changed state".into());
        }
        (actual, effects(&before, &after), metadata, column_kinds)
    };
    let expected_names = if path == "c-structured" {
        let mapping = case["column_mapping"]
            .as_object()
            .ok_or("missing declared C column mapping")?;
        let mut names = mapping.keys().cloned().collect::<Vec<_>>();
        names.sort_by_key(|name| name.strip_prefix("slot_").unwrap().parse::<u32>().unwrap());
        if names
            .iter()
            .map(|n| mapping[n].as_str().unwrap())
            .collect::<Vec<_>>()
            != header
        {
            return Err("declared C column mapping differs from Cypher aliases".into());
        }
        names
    } else {
        header.iter().map(|s| s.to_string()).collect()
    };
    if let Some(declared) = case["column_kinds"].as_array() {
        let declared = declared
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect::<Vec<_>>();
        if declared != column_kinds {
            return Err(format!(
                "column kind masks {column_kinds:?} expected {declared:?}"
            ));
        }
    }
    if !expected_names.is_empty() && actual.0 != expected_names {
        return Err(format!(
            "columns {:?} expected {expected_names:?}",
            actual.0
        ));
    }
    let ordered = case["ordered"].as_bool().unwrap();
    let mut expected = expected;
    let mut observed = actual.1.clone();
    if case["mode"] == "bag-lists-unordered" {
        for row in expected.iter_mut().chain(observed.iter_mut()) {
            row.iter_mut().for_each(V::sort_lists);
        }
    }
    use zeppelin_embed_adversarial_oracle::graph_profile::{Observation, compare_observation};
    let oracle_expected = Observation {
        columns: header
            .iter()
            .map(|name| (name.to_string(), "declared".into()))
            .collect(),
        rows: expected
            .iter()
            .map(|row| row.iter().map(oracle_cell).collect())
            .collect(),
        ordered,
        error: None,
        disposition: "separately checked".into(),
        generation: 0,
        provenance: vec![],
        effects: case["effects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as usize)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap(),
    };
    let mut oracle_observed = oracle_expected.clone();
    oracle_observed.rows = observed
        .iter()
        .map(|row| row.iter().map(oracle_cell).collect())
        .collect();
    oracle_observed.effects = delta.clone().try_into().unwrap();
    compare_observation(&oracle_expected, &oracle_observed)?;
    let expected_rows = canonical(&expected, ordered);
    let rows = canonical(&observed, ordered);
    if rows != expected_rows {
        return Err(format!(
            "{} {path}: rows {rows:?} expected {expected_rows:?}",
            case["id"]
        ));
    }
    let declared = case["effects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_u64().unwrap() as usize)
        .collect::<Vec<_>>();
    if delta != declared {
        return Err(format!("effects {delta:?} expected {declared:?}"));
    }
    Ok(
        json!({"case":case["id"],"path":path,"state":"focused GREEN","columns":actual.0,"column_kinds":column_kinds,"rows":rows,"expected_rows":expected_rows,"effects":delta,"metadata":metadata,"reopened":true,"released":true}),
    )
}

fn tck_text(v: &V) -> String {
    let props = |p: &BTreeMap<String, V>| {
        format!(
            "{{{}}}",
            p.iter()
                .map(|(k, v)| format!("{k}: {}", tck_text(v)))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    match v {
        V::Null => "null".into(),
        V::Bool(b) => b.to_string(),
        V::Int(i) => i.to_string(),
        V::Float(f) => format!("{f:?}"),
        V::Str(s) => format!("'{s}'"),
        V::List(l) => format!(
            "[{}]",
            l.iter().map(tck_text).collect::<Vec<_>>().join(", ")
        ),
        V::Node(labels, p) => format!(
            "({}{})",
            labels.iter().map(|l| format!(":{l}")).collect::<String>(),
            props(p)
        ),
        V::Rel(kind, p) => format!("[:{kind}{}]", props(p)),
    }
}
pub fn original_cases() -> Vec<Json> {
    let mut cases = Vec::new();
    for (write, text) in [
        (
            false,
            include_str!(
                "../../crates/zeppelin-embed-cypher/tests/fixtures/read-tck-execution.txt"
            ),
        ),
        (
            true,
            include_str!(
                "../../crates/zeppelin-embed-cypher/tests/fixtures/write-tck-execution.txt"
            ),
        ),
    ] {
        for s in tck::scenarios(text) {
            let (header, rows, ordered, error, mode) = match &s.expect {
                tck::Expect::Table { mode, header, rows } => (
                    header.clone(),
                    rows.clone(),
                    mode == "ordered",
                    Json::Null,
                    mode.clone(),
                ),
                tck::Expect::Empty => (vec![], vec![], false, Json::Null, "bag".into()),
                tck::Expect::RejectProfile => {
                    let kind = if [
                        "clauses/match/Match4.feature [2]",
                        "clauses/match/Match4.feature [5]",
                        "clauses/match/Match4.feature [8]",
                        "clauses/match/Match7.feature [12]",
                    ]
                    .contains(&s.coordinate.as_str())
                    {
                        "InvalidRange"
                    } else {
                        "Unsupported"
                    };
                    (
                        vec![],
                        vec![],
                        false,
                        json!({"rust":kind,"c":if kind=="InvalidRange"{"ZeErrInvalidArgument"}else{"ZeErrQueryUnsupported"},"stage":"compile"}),
                        "bag".into(),
                    )
                }
                tck::Expect::CompileError(e) => {
                    let (rust, c) = if e.contains("InvalidParameterUse") {
                        ("InvalidParameterUse", "ZeErrParameter")
                    } else {
                        ("RelationshipUniqueness", "ZeErrInvalidArgument")
                    };
                    (
                        vec![],
                        vec![],
                        false,
                        json!({"rust":rust,"c":c,"stage":"compile"}),
                        "bag".into(),
                    )
                }
                tck::Expect::RuntimeError { .. } => (
                    vec![],
                    vec![],
                    false,
                    json!({"rust":"Constraint","c":"ZeErrEndpoint","stage":"runtime"}),
                    "bag".into(),
                ),
                tck::Expect::LocalExample(v) => {
                    let Some(v) = v else {
                        continue;
                    };
                    (
                        vec!["result".into()],
                        vec![vec![v.clone()]],
                        false,
                        Json::Null,
                        "bag".into(),
                    )
                }
            };
            let effects = [
                "+nodes",
                "-nodes",
                "+relationships",
                "-relationships",
                "+labels",
                "-labels",
                "+properties",
                "-properties",
            ]
            .iter()
            .map(|name| s.side_effects.get(*name).copied().unwrap_or(0))
            .collect::<Vec<_>>();
            let rows = rows
                .iter()
                .map(|r| r.iter().map(tck_text).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            cases.push(json!({"id":s.coordinate,"write":write,"setup":s.setup,"parameters":s.parameters.iter().map(|(k,v)|json!([k,tck_text(v)])).collect::<Vec<_>>(),"query":s.query,"header":header,"rows":rows,"ordered":ordered,"mode":mode,"error":error,"effects":effects,"structured":null}));
        }
    }
    cases
}
fn parameter_values(case: &Json) -> Vec<(String, V)> {
    case["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p[0].as_str().unwrap().to_owned(),
                tck::parse_value(p[1].as_str().unwrap()),
            )
        })
        .collect()
}
fn scalar(v: &V) -> zeppelin_embed::property_graph::query::QueryValue<'_> {
    use zeppelin_embed::property_graph::query::QueryValue as Q;
    match v {
        V::Null => Q::Null,
        V::Bool(v) => Q::Bool(*v),
        V::Int(v) => Q::I64(*v),
        V::Float(v) => Q::F64(*v),
        V::Str(v) => Q::String(v),
        _ => panic!("scalar parameter required"),
    }
}
fn rust_cypher_parameters(
    store: &GraphStore,
    query: &str,
    values: &[(String, V)],
) -> Result<CompletedGraphResult, String> {
    use zeppelin_embed::property_graph::query::{QueryList, QueryView, ValueContext};
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let c = control();
    let mut context = ValueContext::new(&view, &c, 1_000_000).unwrap();
    let items = values
        .iter()
        .map(|(_, v)| {
            if let V::List(items) = v {
                items.iter().map(scalar).collect::<Vec<_>>()
            } else {
                vec![]
            }
        })
        .collect::<Vec<_>>();
    let bindings = values
        .iter()
        .zip(&items)
        .map(|((name, v), items)| ParameterBinding {
            name,
            value: if matches!(v, V::List(_)) {
                zeppelin_embed::property_graph::query::QueryValue::List(
                    QueryList::new(items, &mut context).unwrap(),
                )
            } else {
                scalar(v)
            },
        })
        .collect::<Vec<_>>();
    zeppelin_embed_cypher::execute(
        store.statement_store(),
        &c,
        &GraphQueryOptions::default(),
        query,
        &bindings,
        zeppelin_embed_cypher::CompileLimits::default(),
    )
    .map_err(statement_error)
}
#[derive(Default)]
struct CParameters {
    bytes: Vec<u8>,
    values: Vec<ZeGraphValue>,
    children: Vec<u32>,
    bindings: Vec<ZeGraphParameterValue>,
}
impl CParameters {
    fn text(&mut self, s: &str) -> ZeGraphRange {
        let r = ZeGraphRange {
            start: self.bytes.len() as u32,
            count: s.len() as u32,
        };
        self.bytes.extend_from_slice(s.as_bytes());
        r
    }
    fn value(&mut self, v: &V) -> u32 {
        let mut c: ZeGraphValue = sized_zeroed();
        match v {
            V::Null => {}
            V::Bool(v) => {
                c.tag = 1;
                c.boolean = u32::from(*v);
            }
            V::Int(v) => {
                c.tag = 2;
                c.integer = *v;
            }
            V::Float(v) => {
                c.tag = 3;
                c.floating = *v;
            }
            V::Str(v) => {
                c.tag = 4;
                c.range = self.text(v);
            }
            V::List(list) => {
                let children = list.iter().map(|v| self.value(v)).collect::<Vec<_>>();
                c.tag = 7;
                c.range = ZeGraphRange {
                    start: self.children.len() as u32,
                    count: children.len() as u32,
                };
                self.children.extend(children);
            }
            _ => panic!("entity parameter"),
        };
        let i = self.values.len() as u32;
        self.values.push(c);
        i
    }
    fn new(values: &[(String, V)]) -> Self {
        let mut p = Self::default();
        for (name, value) in values {
            let name = p.text(name);
            let value = p.value(value);
            let mut binding: ZeGraphParameterValue = sized_zeroed();
            binding.name = name;
            binding.value = value;
            p.bindings.push(binding);
        }
        p
    }
    fn query(&self, h: ZeGraphHandle, text: &str) -> (ZeErrorCode, ZeGraphResponse) {
        let mut pool: ZeGraphValuePool = sized_zeroed();
        pool.bytes = self.bytes.as_ptr();
        pool.byte_count = self.bytes.len();
        pool.values = self.values.as_ptr();
        pool.value_count = self.values.len();
        pool.children = self.children.as_ptr();
        pool.child_count = self.children.len();
        let q = cypher_request(text.as_bytes(), &self.bindings, Some(&pool));
        let mut r = empty_response();
        let code = ze_graph_cypher(h, &q, &mut r);
        (code, r)
    }
}

pub fn high_id_roundtrip() -> Result<(), String> {
    use zeppelin_embed::property_graph::{NodeId, RelId};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("graph");
    let store = GraphStore::create_with_allocator_seed_for_test(
        &root,
        OpenOptions::new().with_max_resident_bytes(256 << 20),
        NodeId::new(7).unwrap(),
        RelId::new(11).unwrap(),
    )
    .map_err(|e| e.to_string())?;
    rust_cypher(&store, "CREATE (:A {s: 'low'})-[:R]->(:B)")?;
    let high_node = 7 + (1_u128 << 64);
    let high_rel = 11 + (1_u128 << 64);
    store
        .jump_allocators_for_test(
            NodeId::new(high_node).unwrap(),
            RelId::new(high_rel).unwrap(),
            &control(),
        )
        .map_err(|e| e.to_string())?;
    rust_cypher(&store, "CREATE (:A {s: 'high'})-[:R]->(:B)")?;
    let nodes = rust_cypher(&store, "MATCH (n) RETURN n, ze.node_id(n)")?;
    let rels = rust_cypher(&store, "MATCH ()-[r]->() RETURN r, ze.relationship_id(r)")?;
    let mut ids = nodes
        .pools()
        .nodes
        .iter()
        .map(|n| n.id.get())
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, [7, 8, high_node, high_node + 1]);
    let mut ids = rels
        .pools()
        .relationships
        .iter()
        .map(|r| r.id.get())
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, [11, high_rel]);
    store.close().map_err(|e| e.to_string())?;
    // Results still own their strings and graph objects after their store closes.
    assert_eq!(tck::actual_table(&nodes).1.len(), 4);
    assert_eq!(tck::actual_table(&rels).1.len(), 2);
    let (code, h) = graph_open(&root, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let (code, mut r) = c_cypher(h, "MATCH (n) RETURN n, ze.node_id(n)");
    assert_eq!(code, ZeErrorCode::ZeOk);
    let before = c_table(&r);
    let mut ids = unsafe { slice(r.pool.nodes, r.pool.node_count) }
        .iter()
        .map(|n| (u128::from(n.id.high) << 64) | u128::from(n.id.low))
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, [7, 8, high_node, high_node + 1]);
    let (code, mut relationships) = c_cypher(h, "MATCH ()-[r]->() RETURN r, ze.relationship_id(r)");
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut ids = unsafe {
        slice(
            relationships.pool.relationships,
            relationships.pool.relationship_count,
        )
    }
    .iter()
    .map(|r| (u128::from(r.id.high) << 64) | u128::from(r.id.low))
    .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, [11, high_rel]);
    let (code, mut changed) = c_cypher(
        h,
        &format!("MATCH (n) WHERE ze.node_id(n) = '{high_node:032x}' SET n.s = 'changed'"),
    );
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(changed.disposition, 2);
    assert_eq!(ze_graph_response_free(&mut changed), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_close(h), ZeErrorCode::ZeOk);
    assert_eq!(c_table(&r), before);
    assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
    assert_eq!(
        ze_graph_response_free(&mut relationships),
        ZeErrorCode::ZeOk
    );
    let reopened = GraphStore::open(
        &root,
        OpenOptions::new().with_max_resident_bytes(256 << 20),
        None,
    )
    .map_err(|e| e.to_string())?;
    let result = rust_cypher(
        &reopened,
        &format!("MATCH (n) WHERE ze.node_id(n) = '{high_node:032x}' RETURN n.s AS s"),
    )?;
    assert_eq!(
        tck::actual_table(&result).1,
        vec![vec![V::Str("changed".into())]]
    );
    reopened.close().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn primitive(v: &V) -> Json {
    match v {
        V::Null => json!({"type":0,"value":null}),
        V::Bool(v) => json!({"type":1,"value":v}),
        V::Int(v) => json!({"type":2,"value":v}),
        V::Float(v) => json!({"type":3,"value":v}),
        V::Str(v) => json!({"type":4,"value":v}),
        V::List(v) => json!({"type":7,"value":v.iter().map(primitive).collect::<Vec<_>>()}),
        V::Node(labels, props) => {
            json!({"type":5,"value":{"labels":labels,"properties":props.iter().map(|(k,v)|(k.clone(),primitive(v))).collect::<BTreeMap<_,_>>()}})
        }
        V::Rel(kind, props) => {
            json!({"type":6,"value":{"kind":kind,"properties":props.iter().map(|(k,v)|(k.clone(),primitive(v))).collect::<BTreeMap<_,_>>()}})
        }
    }
}

fn oracle_cell(v: &V) -> zeppelin_embed_adversarial_oracle::graph_profile::Cell {
    use zeppelin_embed_adversarial_oracle::graph_profile::Cell as C;
    match v {
        V::Null => C::Null,
        V::Bool(v) => C::Boolean(*v),
        V::Int(v) => C::Integer(*v),
        V::Float(v) => C::Float(v.to_bits()),
        V::Str(v) => C::String(v.clone()),
        V::List(v) => C::List(v.iter().map(oracle_cell).collect()),
        V::Node(labels, p) => C::NodeValue(
            labels.clone(),
            p.iter().map(|(k, v)| (k.clone(), oracle_cell(v))).collect(),
        ),
        V::Rel(kind, p) => C::RelationshipValue(
            kind.clone(),
            p.iter().map(|(k, v)| (k.clone(), oracle_cell(v))).collect(),
        ),
    }
}

fn statement_error(error: zeppelin_embed_cypher::StatementError) -> String {
    match &error {
        zeppelin_embed_cypher::StatementError::Compile(e) => format!("{error:?};kind={:?}", e.kind),
        zeppelin_embed_cypher::StatementError::Query(e) => format!(
            "{error:?};kind={:?};nothing_committed={}",
            e.kind(),
            e.nothing_committed()
        ),
    }
}

fn c_generation(handle: ZeGraphHandle) -> u64 {
    let (code, mut r) = c_cypher(handle, "RETURN 1");
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(r.disposition, 0);
    assert_eq!(r.has_admitted_generation, 1);
    let generation = r.admitted_generation;
    assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
    generation
}

fn kind_mask(kinds: ValueKinds) -> u32 {
    [
        (ValueKinds::NULL, 1),
        (ValueKinds::BOOL, 2),
        (ValueKinds::I64, 4),
        (ValueKinds::F64, 8),
        (ValueKinds::STRING, 16),
        (ValueKinds::NODE, 32),
        (ValueKinds::REL, 64),
        (ValueKinds::LIST, 128),
    ]
    .iter()
    .filter(|(kind, _)| kinds.contains(*kind))
    .map(|(_, bit)| *bit)
    .sum()
}
