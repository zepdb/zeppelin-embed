use super::*;
/// Exact frontend mode; the later retrieval lowering preserves every variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundSearchMode {
    Default,
    Auto,
    Exact,
    Scan,
}
/// Eligibility provenance, separate from a list's ordinary value type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundEligibility {
    AllIndexed,
    Materialized {
        expression: ExprId,
        provenance: BoundEligibilityProvenance,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundEligibilityProvenance {
    LiteralEmpty,
    GlobalDistinctNodes,
}
#[derive(Clone, Copy, Debug)]
pub enum BoundSearchRequest {
    Vector {
        vector: ExprId,
        k: ExprId,
        mode: BoundSearchMode,
        eligible: BoundEligibility,
    },
    Text {
        query: ExprId,
        k: ExprId,
        eligible: BoundEligibility,
    },
    Hybrid {
        vector: ExprId,
        text: ExprId,
        k: ExprId,
        mode: BoundSearchMode,
        eligible: BoundEligibility,
    },
}
/// Source-ordered typed call facts. Runtime source/report scheduling is later-owned.
#[derive(Clone, Copy, Debug)]
pub struct BoundCall {
    pub syntax: AstId,
    pub id: SearchCallId,
    pub request: BoundSearchRequest,
    pub outputs: SearchOutputs,
}
fn rejected(span: Span, message: &'static str) -> ParseError {
    ParseError::new(ErrorKind::SearchContext, span, message)
}
impl<'a> Binder<'a, '_> {
    pub(super) fn call(&mut self, id: AstId, procedure: Procedure) -> Result<(), ParseError> {
        let clause = node(self.ast, id)?;
        if self.calls.len() == 8 {
            return Err(rejected(clause.span, "search call limit"));
        }
        let arguments = clause
            .children()
            .iter()
            .take_while(|id| {
                self.ast
                    .node(**id)
                    .is_some_and(|n| !matches!(n.kind, NodeKind::Yield { .. }))
            })
            .count();
        let (required, mode_index) = match procedure {
            Procedure::VectorSearch => (3, Some(2)),
            Procedure::TextSearch => (2, None),
            Procedure::HybridSearch => (4, Some(3)),
        };
        if arguments != required && arguments != required + 1 {
            return Err(rejected(clause.span, "search argument count"));
        }
        for index in 0..required {
            let root = child(clause, index)?;
            let info = self.expression(root)?;
            let expected = match (procedure, index) {
                (Procedure::VectorSearch | Procedure::HybridSearch, 0) => ValueKinds::LIST,
                (Procedure::TextSearch, 0) | (Procedure::HybridSearch, 1) => ValueKinds::STRING,
                _ if Some(index) == mode_index => ValueKinds::STRING,
                _ => ValueKinds::I64,
            };
            if info.row
                || info.invariant.is_none()
                || !info.kinds.contains(expected)
                || info.kinds.contains(ValueKinds::NULL)
            {
                return Err(rejected(
                    node(self.ast, root)?.span,
                    "search argument must be query-invariant and correctly typed",
                ));
            }
            if expected == ValueKinds::LIST {
                self.vector_elements(root)?;
            }
            if expected == ValueKinds::I64
                && let Some(QueryValue::I64(k)) = self.constant_value(root)?
                && !(1..=4096).contains(&k)
            {
                return Err(rejected(node(self.ast, root)?.span, "search k bound"));
            }
        }
        let mode = if let Some(index) = mode_index {
            let root = child(clause, index)?;
            Some(match self.constant_value(root)? {
                Some(QueryValue::String("default")) => BoundSearchMode::Default,
                Some(QueryValue::String("auto")) => BoundSearchMode::Auto,
                Some(QueryValue::String("exact")) => BoundSearchMode::Exact,
                Some(QueryValue::String("scan")) => BoundSearchMode::Scan,
                _ => return Err(rejected(node(self.ast, root)?.span, "invalid search mode")),
            })
        } else {
            None
        };
        let eligibility = if arguments == required {
            BoundEligibility::AllIndexed
        } else {
            let root = child(clause, required)?;
            let info = self.expression(root)?;
            let source = node(self.ast, self.ungroup(root)?)?;
            if source.kind == NodeKind::List && source.children().is_empty() {
                BoundEligibility::Materialized {
                    expression: self.invariant_argument(root)?,
                    provenance: BoundEligibilityProvenance::LiteralEmpty,
                }
            } else if self.singleton && info.eligible {
                BoundEligibility::Materialized {
                    expression: expr_id(root)?,
                    provenance: BoundEligibilityProvenance::GlobalDistinctNodes,
                }
            } else {
                return Err(rejected(
                    source.span,
                    "eligible input lacks singleton distinct-node provenance",
                ));
            }
        };
        let mut yielded = Vec::new();
        let mut outputs = SearchOutputs::default();
        for id in clause.children().iter().skip(arguments) {
            let item = node(self.ast, *id)?;
            let NodeKind::Yield { name, alias } = item.kind else {
                return Err(error(item.span, "invalid YIELD"));
            };
            let name = self
                .ast
                .text(name)
                .ok_or_else(|| error(item.span, "missing yield name"))?;
            let kinds = match (procedure, name) {
                (_, "node") => ValueKinds::NODE,
                (Procedure::VectorSearch, "distance")
                | (Procedure::TextSearch | Procedure::HybridSearch, "score") => ValueKinds::F64,
                (Procedure::HybridSearch, "vector_distance" | "lexical_score") => {
                    ValueKinds::F64.union(ValueKinds::NULL)
                }
                _ => return Err(rejected(item.span, "unknown search yield")),
            };
            for prior in &yielded {
                poll(self.resources, item.span)?;
                if *prior == name {
                    return Err(rejected(item.span, "duplicate search yield"));
                }
            }
            push(&mut yielded, name, self.resources, item.span)?;
            let alias = alias
                .map(|id| {
                    self.ast
                        .text(id)
                        .ok_or_else(|| error(item.span, "missing yield alias"))
                })
                .transpose()?
                .unwrap_or(name);
            for symbol in &self.scope {
                poll(self.resources, item.span)?;
                if symbol.name == alias {
                    return Err(ParseError::new(
                        ErrorKind::DuplicateVariable,
                        item.span,
                        "YIELD shadows an existing variable",
                    ));
                }
            }
            if self.scope.len() >= self.limits.columns {
                return Err(ParseError::new(
                    ErrorKind::Limit(LimitKind::Columns),
                    item.span,
                    "scope column limit",
                ));
            }
            let slot = self.new_slot(item.span)?;
            match name {
                "node" => outputs.node = Some(slot),
                "distance" => outputs.distance = Some(slot),
                "score" => outputs.score = Some(slot),
                "vector_distance" => outputs.vector_distance = Some(slot),
                "lexical_score" => outputs.lexical_score = Some(slot),
                _ => return Err(rejected(item.span, "unknown search yield")),
            }
            let info = Info {
                kinds,
                row: true,
                origin: if name == "node" { Some(slot) } else { None },
                eligible: false,
                constant: None,
                invariant: None,
            };
            push(
                &mut self.scope,
                Symbol {
                    name: alias,
                    slot,
                    info,
                },
                self.resources,
                item.span,
            )?;
            *self
                .facts
                .get_mut(id.0)
                .ok_or_else(|| error(item.span, "missing yield fact"))? = Some(info);
            *self
                .expressions
                .get_mut(id.0)
                .ok_or_else(|| error(item.span, "missing yield slot"))? = Expression::Slot(slot);
        }
        let request = match procedure {
            Procedure::VectorSearch => BoundSearchRequest::Vector {
                vector: self.invariant_argument(child(clause, 0)?)?,
                k: self.invariant_argument(child(clause, 1)?)?,
                mode: mode.ok_or_else(|| error(clause.span, "missing vector mode"))?,
                eligible: eligibility,
            },
            Procedure::TextSearch => BoundSearchRequest::Text {
                query: self.invariant_argument(child(clause, 0)?)?,
                k: self.invariant_argument(child(clause, 1)?)?,
                eligible: eligibility,
            },
            Procedure::HybridSearch => BoundSearchRequest::Hybrid {
                vector: self.invariant_argument(child(clause, 0)?)?,
                text: self.invariant_argument(child(clause, 1)?)?,
                k: self.invariant_argument(child(clause, 2)?)?,
                mode: mode.ok_or_else(|| error(clause.span, "missing hybrid mode"))?,
                eligible: eligibility,
            },
        };
        let call = BoundCall {
            syntax: id,
            id: SearchCallId(
                u32::try_from(self.calls.len())
                    .map_err(|_| error(clause.span, "call identity overflow"))?,
            ),
            request,
            outputs,
        };
        push(&mut self.calls, call, self.resources, clause.span)?;
        self.singleton = false;
        Ok(())
    }
    fn invariant_argument(&self, root: AstId) -> Result<ExprId, ParseError> {
        self.info(root)?
            .invariant
            .ok_or_else(|| {
                rejected(
                    node(self.ast, root).map_or(Span::default(), |n| n.span),
                    "search argument lacks invariant backing",
                )
            })
            .and_then(expr_id)
    }
    fn vector_elements(&mut self, root: AstId) -> Result<(), ParseError> {
        let source = self.info(root)?.constant.unwrap_or(root);
        let node = node(self.ast, source)?;
        if node.kind == NodeKind::List {
            for child in node.children() {
                poll(self.resources, node.span)?;
                let kinds = self.info(*child)?.kinds;
                if !kinds.contains(ValueKinds::I64) && !kinds.contains(ValueKinds::F64) {
                    return Err(rejected(node.span, "vector requires numeric elements"));
                }
            }
        } else if let Some(QueryValue::List(list)) = self.constant_value(root)? {
            for index in 0..list.len() {
                poll(self.resources, node.span)?;
                if !matches!(
                    list.get(index),
                    Some(QueryValue::I64(_) | QueryValue::F64(_))
                ) {
                    return Err(rejected(
                        node.span,
                        "vector parameter requires numeric elements",
                    ));
                }
            }
        }
        Ok(())
    }
    fn constant_value(&mut self, root: AstId) -> Result<Option<QueryValue<'a>>, ParseError> {
        let Some(root) = self.info(root)?.constant else {
            return Ok(None);
        };
        let node = node(self.ast, root)?;
        Ok(match node.kind {
            NodeKind::Integer(value) => Some(QueryValue::I64(value)),
            NodeKind::Float(value) => Some(QueryValue::F64(value)),
            NodeKind::Boolean(value) => Some(QueryValue::Bool(value)),
            NodeKind::Null => Some(QueryValue::Null),
            NodeKind::String(id) => Some(QueryValue::String(
                self.ast
                    .text(id)
                    .ok_or_else(|| error(node.span, "missing constant text"))?,
            )),
            NodeKind::Parameter(id) => Some(
                self.parameter(
                    self.ast
                        .text(id)
                        .ok_or_else(|| error(node.span, "missing parameter name"))?,
                    node.span,
                )?,
            ),
            _ => None,
        })
    }
}
