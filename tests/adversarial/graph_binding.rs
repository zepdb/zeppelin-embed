//! PG11 production compile/bind seam; no graph execution/writer claim.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::property_graph::query::{
    QueryValue,
    plan::{ParameterBinding, ValueKinds},
};
use zeppelin_embed_adversarial_oracle::graph_binding::{
    self as oracle, Case, Column, Kind, Observation, Outcome,
};
use zeppelin_embed_cypher::{
    BoundSearchMode, BoundSearchRequest, CompileLimits, ErrorKind, ResourceError, Resources,
    compile_with, parse_with,
};
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.binding.scope",
    "property-graph.binding.profile",
    "property-graph.binding.bits",
    "property-graph.binding.modes",
    "property-graph.binding.cancel.fire",
    "property-graph.binding.budget.fire",
    "property-graph.binding.same-seed-control",
];
#[derive(Default, Debug)]
pub struct ProbeReport {
    pub cases: usize,
    pub fault_fires: usize,
    pub clean_controls: usize,
}
#[derive(Default)]
struct Faults {
    polls: usize,
    charges: usize,
    fail_poll: usize,
    fail_charge: usize,
    fires: usize,
}
impl Resources for Faults {
    fn checkpoint(&mut self) -> Result<(), ResourceError> {
        self.polls += 1;
        if self.polls == self.fail_poll {
            self.fires += 1;
            return Err(ResourceError::Cancelled);
        }
        Ok(())
    }
    fn charge(&mut self, _: usize) -> Result<(), ResourceError> {
        self.charges += 1;
        if self.charges == self.fail_charge {
            self.fires += 1;
            return Err(ResourceError::Memory);
        }
        Ok(())
    }
}
fn kind(value: ValueKinds) -> Result<Kind, String> {
    Ok(if value == ValueKinds::I64 {
        Kind::Integer
    } else if value == ValueKinds::F64 {
        Kind::Float
    } else if value == ValueKinds::STRING {
        Kind::Text
    } else if value == ValueKinds::NODE {
        Kind::Node
    } else if value == ValueKinds::NODE.union(ValueKinds::NULL) {
        Kind::NullableNode
    } else if value == ValueKinds::LIST {
        Kind::List
    } else {
        return Err(format!("PG11 unexpected kinds {value:?}"));
    })
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::binding_probe", seed);
    let name = format!("v_{}", rng.next_u64());
    let bits = 0x7ff8000000000000 | (rng.next_u64() & 0x0007ffffffffffff);
    let mut report = ProbeReport::default();
    for mode in 0..4_u8 {
        for case in oracle::CASES {
            let spelling = ["default", "auto", "exact", "scan"][mode as usize];
            let query = match case {
                Case::Alias => format!("WITH 7 AS old WITH old AS {name}, 'λ' AS text RETURN *"),
                Case::Optional => {
                    "MATCH (base) OPTIONAL MATCH (base)-->(maybe) RETURN base, maybe".into()
                }
                Case::Path => "MATCH ()-[edges:R*0..2]->() RETURN edges".into(),
                Case::Writes => "MATCH (n) DELETE n RETURN count(n) AS total".into(),
                Case::Search => format!(
                    "CALL ze.vector_search([1,2.0], 1, '{spelling}') YIELD node AS hit, distance AS score RETURN hit, score"
                ),
                Case::Parameter => format!("RETURN $value AS {name}"),
                Case::Unknown => "CREATE (n) RETURN missing".into(),
                Case::Duplicate => "RETURN 1 AS x, 2 AS x".into(),
                Case::WrongType => "WITH 1 AS n MATCH (n) RETURN n".into(),
                Case::Unsupported => "CREATE (n) UNWIND [1] AS x RETURN x".into(),
                Case::BadVector => {
                    "CALL ze.vector_search(['x'], 1, 'exact') YIELD node RETURN node".into()
                }
                Case::Deleted => "MATCH (n) DELETE n RETURN collect(n)".into(),
            };
            let parameter = [ParameterBinding {
                name: "value",
                value: QueryValue::F64(f64::from_bits(bits)),
            }];
            let parameters = if matches!(case, Case::Parameter) {
                &parameter[..]
            } else {
                &[]
            };
            let mut observed = Observation {
                outcome: Outcome::Bound,
                columns: vec![],
                modes: vec![],
                parameter_bits: vec![],
                consumer_calls: 0,
            };
            let mut invalid_kind = None;
            let result = compile_with(
                &query,
                parameters,
                CompileLimits::default(),
                &mut Faults::default(),
                |bound| {
                    observed.consumer_calls += 1;
                    for column in bound.columns() {
                        match kind(column.kinds) {
                            Ok(kind) => observed.columns.push(Column {
                                name: column.name.into(),
                                kind,
                            }),
                            Err(error) => invalid_kind = Some(error),
                        }
                    }
                    for call in bound.calls() {
                        let mode = match call.request {
                            BoundSearchRequest::Vector { mode, .. }
                            | BoundSearchRequest::Hybrid { mode, .. } => Some(mode),
                            BoundSearchRequest::Text { .. } => None,
                        };
                        observed.modes.push(match mode {
                            Some(BoundSearchMode::Default) => 0,
                            Some(BoundSearchMode::Auto) => 1,
                            Some(BoundSearchMode::Exact) => 2,
                            Some(BoundSearchMode::Scan) => 3,
                            None => 255,
                        });
                    }
                    for parameter in bound.parameters() {
                        if let QueryValue::F64(value) = parameter.value {
                            observed.parameter_bits.push(value.to_bits());
                        }
                    }
                    Ok(())
                },
            );
            if let Some(error) = invalid_kind {
                return Err(error);
            }
            if let Err(error) = result {
                observed.outcome = match error.kind {
                    ErrorKind::UnknownVariable => Outcome::Unknown,
                    ErrorKind::DuplicateVariable => Outcome::Duplicate,
                    ErrorKind::Type => Outcome::Type,
                    ErrorKind::Unsupported => Outcome::Unsupported,
                    ErrorKind::SearchContext => Outcome::Search,
                    ErrorKind::DeletedEntity => Outcome::Deleted,
                    _ => return Err(format!("PG11 unexpected bind error {error:?}")),
                };
            }
            oracle::check(*case, &name, bits, mode, &observed)?;
            report.cases += 1;
        }
    }
    for key in &REQUIRED_COVERAGE[..4] {
        coverage.hit(*key);
    }
    let query = format!("MATCH (n) WITH n AS {name} RETURN ze.node_id({name}) AS id");
    let mut syntax = Faults::default();
    drop(
        parse_with(&query, CompileLimits::default(), &mut syntax)
            .map_err(|e| format!("PG11 {e:?}"))?,
    );
    for cancel in [true, false] {
        let mut faults = Faults {
            fail_poll: if cancel { syntax.polls + 4 } else { 0 },
            fail_charge: if cancel { 0 } else { syntax.charges + 2 },
            ..Faults::default()
        };
        let mut calls = 0;
        let result = compile_with(&query, &[], CompileLimits::default(), &mut faults, |_| {
            calls += 1;
            Ok(())
        });
        let expected = if cancel {
            ResourceError::Cancelled
        } else {
            ResourceError::Memory
        };
        if !matches!(result,Err(error) if error.kind==ErrorKind::Resource(expected)) {
            return Err("PG11 scheduled fault did not refuse compilation".into());
        }
        oracle::check_fault(cancel, cancel, faults.fires, calls)?;
        let mut clean = Faults::default();
        let mut clean_calls = 0;
        compile_with(&query, &[], CompileLimits::default(), &mut clean, |_| {
            clean_calls += 1;
            Ok(())
        })
        .map_err(|e| format!("PG11 control {e:?}"))?;
        if clean.fires != 0 || clean_calls != 1 {
            return Err("PG11 invalid clean control".into());
        }
        report.fault_fires += faults.fires;
        report.clean_controls += 1;
        report.cases += 2;
        coverage.hit(if cancel {
            REQUIRED_COVERAGE[4]
        } else {
            REQUIRED_COVERAGE[5]
        });
        coverage.hit(REQUIRED_COVERAGE[6]);
    }
    Ok(report)
}
