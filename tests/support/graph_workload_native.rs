//! Pointer-free tooling records consumed by the C public ABI adapter.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use zeppelin_embed::property_graph::query::Comparison;
use zeppelin_embed::property_graph::{GraphQueryPlan, query::plan::*};
use zeppelin_embed_bench::harness_json::{Value, json};
#[derive(Default)]
struct Job {
    bytes: Vec<u8>,
    values: Vec<Value>,
    names: Vec<Value>,
    expressions: Vec<Value>,
    inputs: Vec<u32>,
    children: Vec<u32>,
    projections: Vec<Value>,
    sort: Vec<Value>,
    searches: Vec<Value>,
    operators: Vec<Value>,
    eager: Vec<u32>,
    ids: BTreeMap<u32, u32>,
}
impl Job {
    fn name(&mut self, s: &str) -> (u32, u32) {
        let r = (self.bytes.len() as u32, s.len() as u32);
        self.bytes.extend_from_slice(s.as_bytes());
        r
    }
    fn projections(&mut self, p: &[Projection]) -> (u32, u32) {
        let r = (self.projections.len() as u32, p.len() as u32);
        self.projections.extend(
            p.iter()
                .map(|p| json!([p.slot.0, self.ids[&p.expression.0]])),
        );
        r
    }
}
pub fn write_plan(plan: &GraphQueryPlan<'_>, out: &mut impl Write) -> Result<(), String> {
    let mut j = Job::default();
    let mut pending = Vec::new();
    for op in plan.operators {
        match op.kind {
            OperatorKind::Filter(e) => pending.push(e.0),
            OperatorKind::Project(p) => pending.extend(p.iter().map(|p| p.expression.0)),
            OperatorKind::Sort(k) => pending.extend(k.iter().map(|k| k.expression.0)),
            OperatorKind::Search { request, .. } => match request {
                SearchRequest::Vector { vector, k, .. } => pending.extend([vector.0, k.0]),
                SearchRequest::Text { query, k, .. } => pending.extend([query.0, k.0]),
                SearchRequest::Hybrid {
                    vector, text, k, ..
                } => pending.extend([vector.0, text.0, k.0]),
            },
            _ => {}
        }
    }
    let mut used = BTreeSet::new();
    while let Some(i) = pending.pop() {
        if !used.insert(i) {
            continue;
        }
        match plan.expressions.get(i as usize).ok_or("expression index")? {
            Expression::Unary { operand, .. } => pending.push(operand.0),
            Expression::Property { entity, .. } => pending.push(entity.0),
            Expression::Binary { left, right, .. } => pending.extend([left.0, right.0]),
            Expression::List(c) => pending.extend(c.iter().map(|e| e.0)),
            _ => {}
        }
    }
    j.ids = used
        .iter()
        .enumerate()
        .map(|(n, i)| (*i, n as u32))
        .collect();
    let ids = j.ids.clone();
    for i in used {
        let e = plan.expressions.get(i as usize).ok_or("expression index")?;
        let mut a = [0_u32; 11];
        match *e {
            Expression::Literal(l) => {
                a[6] = j.values.len() as u32;
                let (tag, integer, bits, range) = match l {
                    Literal::I64(v) => (2, v, 0, (0, 0)),
                    Literal::F64(v) => (3, 0, v.to_bits(), (0, 0)),
                    Literal::String(v) => (4, 0, 0, j.name(v)),
                    _ => return Err("unsupported native workload literal".into()),
                };
                j.values.push(json!([tag, integer, bits, range.0, range.1]));
            }
            Expression::Slot(s) => {
                a[0] = 1;
                a[6] = s.0;
            }
            Expression::Unary { operation, operand } => {
                a[0] = 3;
                a[1] = match operation {
                    UnaryExpression::IsNotNull => 4,
                    UnaryExpression::StoredText => 8,
                    UnaryExpression::NodeIdText => 9,
                    _ => return Err("unsupported workload unary".into()),
                };
                a[2] = ids[&operand.0];
            }
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Equal),
                left,
                right,
            } => {
                a[0] = 4;
                a[1] = 3;
                a[2] = ids[&left.0];
                a[3] = ids[&right.0];
            }
            Expression::Property { entity, name } => {
                a[0] = 5;
                a[2] = ids[&entity.0];
                (a[7], a[8]) = j.name(name.as_str());
            }
            Expression::List(children) => {
                a[0] = 7;
                a[9] = j.children.len() as u32;
                a[10] = children.len() as u32;
                j.children.extend(children.iter().map(|e| ids[&e.0]));
            }
            Expression::Aggregate {
                operation: AggregateExpression::Collect { distinct },
                operand,
            } => {
                a[0] = 8;
                a[1] = 1;
                a[4] = u32::from(operand.is_some());
                a[2] = operand.map_or(0, |e| e.0);
                a[5] = u32::from(distinct);
            }
            _ => return Err("unsupported native workload expression".into()),
        }
        j.expressions.push(json!(a));
    }
    for op in plan.operators {
        if matches!(op.kind, OperatorKind::Unit) {
            continue;
        }
        let mut a = [0_u64; 24];
        let mut id = 0_u128;
        let mut inputs = op
            .inputs
            .iter()
            .filter(|i| i.0 != 0)
            .map(|i| i.0 - 1)
            .collect::<Vec<_>>();
        match op.kind {
            OperatorKind::Unit => {
                a[0] = 0;
                inputs.clear();
            }
            OperatorKind::LookupNode { output, id: n } => {
                a[0] = 10;
                a[2] = u64::from(output.0);
                id = n.get();
                inputs.clear();
            }
            OperatorKind::Expand {
                source,
                node,
                relationship,
                direction,
                relationship_types,
                pattern,
            } => {
                a[0] = 13;
                a[1] = u64::from(source.0);
                a[2] = u64::from(node.0);
                a[3] = u64::from(relationship.0);
                a[4] = if matches!(direction, Direction::Incoming) {
                    1
                } else {
                    0
                };
                a[5] = u64::from(pattern.0);
                a[6] = j.names.len() as u64;
                a[7] = relationship_types.len() as u64;
                for n in relationship_types {
                    let r = j.name(n.as_str());
                    j.names.push(json!([r.0, r.1]));
                }
            }
            OperatorKind::BoundedExpand {
                source,
                node,
                relationships,
                direction,
                relationship_types,
                pattern,
                min,
                max,
                ..
            } => {
                a[0] = 14;
                a[1] = u64::from(source.0);
                a[2] = u64::from(node.0);
                a[3] = u64::from(relationships.0);
                a[4] = if matches!(direction, Direction::Incoming) {
                    1
                } else {
                    0
                };
                a[5] = u64::from(pattern.0);
                a[6] = j.names.len() as u64;
                a[7] = relationship_types.len() as u64;
                a[8] = u64::from(min);
                a[9] = u64::from(max);
                for n in relationship_types {
                    let r = j.name(n.as_str());
                    j.names.push(json!([r.0, r.1]));
                }
            }
            OperatorKind::Aggregate { .. } => {
                a[0] = 20;
                a[1] = 13;
                a[10] = 15;
            }
            OperatorKind::Filter(e) => {
                a[0] = 18;
                a[11] = u64::from(ids[&e.0]);
                a[12] = 1;
            }
            OperatorKind::Sort(keys) => {
                a[0] = 3;
                a[13] = j.sort.len() as u64;
                a[14] = keys.len() as u64;
                j.sort.extend(
                    keys.iter()
                        .map(|k| json!([ids[&k.expression.0], u32::from(k.descending)])),
                );
            }
            OperatorKind::OffsetLimit { offset, limit } => {
                a[0] = 9;
                a[15] = offset;
                a[16] = limit.unwrap_or(0);
                a[17] = u64::from(limit.is_some());
            }
            OperatorKind::Project(p) => {
                a[0] = 16;
                let r = j.projections(p);
                a[18] = u64::from(r.0);
                a[19] = u64::from(r.1);
            }
            OperatorKind::Search {
                call,
                request,
                outputs,
            } => {
                a[0] = 8;
                a[20] = j.searches.len() as u64;
                let (kind, v, t, k, mode, eligible) = match request {
                    SearchRequest::Vector {
                        vector,
                        k,
                        mode,
                        eligible,
                        ..
                    } => (0, Some(vector), None, k, Some(mode), eligible),
                    SearchRequest::Text {
                        query, k, eligible, ..
                    } => (1, None, Some(query), k, None, eligible),
                    SearchRequest::Hybrid {
                        vector,
                        text,
                        k,
                        mode,
                        eligible,
                        ..
                    } => (2, Some(vector), Some(text), k, Some(mode), eligible),
                };
                let set = eligible.map(|_| 15);
                if set.is_none() {
                    inputs.clear();
                }
                let options = request.options();
                j.searches.push(json!([
                    kind,
                    call.0,
                    u32::from(v.is_some()),
                    v.map_or(0, |e| ids[&e.0]),
                    u32::from(t.is_some()),
                    t.map_or(0, |e| ids[&e.0]),
                    ids[&k.0],
                    u32::from(mode.is_some()),
                    if matches!(mode, Some(SearchMode::Exact)) {
                        1
                    } else {
                        0
                    },
                    u32::from(set.is_some()),
                    set.unwrap_or(0),
                    outputs.node.map_or(0, |s| s.0),
                    outputs.distance.or(outputs.score).map_or(0, |s| s.0),
                    u32::from(outputs.vector_distance.is_some()),
                    outputs.vector_distance.map_or(0, |s| s.0),
                    u32::from(outputs.lexical_score.is_some()),
                    outputs.lexical_score.map_or(0, |s| s.0),
                    u32::from(options.alpha.is_some()),
                    options.alpha.unwrap_or(0.0)
                ]));
            }
            _ => return Err("unsupported native workload operator".into()),
        }
        a[21] = (id >> 64) as u64;
        a[22] = id as u64;
        a[23] = j.inputs.len() as u64;
        j.operators
            .push(json!({"fields":a,"input_count":inputs.len()}));
        j.inputs.extend(inputs);
    }
    j.eager = plan.eager_searches.iter().map(|e| e.0 - 1).collect();
    writeln!(out, "ZE77JOB1").map_err(|e| e.to_string())?;
    writeln!(out, "R {}", plan.root.0 - 1).map_err(|e| e.to_string())?;
    write!(out, "B ").map_err(|e| e.to_string())?;
    for b in &j.bytes {
        write!(out, "{b:02x}").map_err(|e| e.to_string())?;
    }
    writeln!(out).map_err(|e| e.to_string())?;
    for (tag, rows) in [
        ('V', j.values),
        ('N', j.names),
        ('E', j.expressions),
        ('P', j.projections),
        ('T', j.sort),
        ('S', j.searches),
    ] {
        for row in rows {
            write!(out, "{tag}").map_err(|e| e.to_string())?;
            for v in row.as_array().ok_or("row array")? {
                write!(out, " {v}").map_err(|e| e.to_string())?;
            }
            writeln!(out).map_err(|e| e.to_string())?;
        }
    }
    for (tag, rows) in [('I', j.inputs), ('H', j.children), ('G', j.eager)] {
        for v in rows {
            writeln!(out, "{tag} {v}").map_err(|e| e.to_string())?;
        }
    }
    for row in j.operators {
        write!(out, "O").map_err(|e| e.to_string())?;
        for v in row["fields"].as_array().ok_or("operator fields")? {
            write!(out, " {v}").map_err(|e| e.to_string())?;
        }
        writeln!(out, " {}", row["input_count"]).map_err(|e| e.to_string())?;
    }
    Ok(())
}
