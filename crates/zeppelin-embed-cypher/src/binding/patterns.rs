use super::*;
use zeppelin_embed::property_graph::GraphName;
impl<'a> Binder<'a, '_> {
    pub(super) fn graph_name(&self, id: TextId, span: Span) -> Result<GraphName<'a>, ParseError> {
        GraphName::new(
            self.ast
                .text(id)
                .ok_or_else(|| error(span, "missing symbolic name"))?,
        )
        .map_err(|_| error(span, "invalid symbolic name"))
    }
    pub(super) fn patterns(
        &mut self,
        clause: &'a Node,
        optional: bool,
        create: bool,
    ) -> Result<(), ParseError> {
        self.singleton = false;
        let mut relationships = Vec::new();
        for pattern in clause.children() {
            let pattern = node(self.ast, *pattern)?;
            if pattern.kind == NodeKind::Predicate {
                let info = self.expression(child(pattern, 0)?)?;
                require(info.kinds, ValueKinds::BOOL, pattern.span)?;
                continue;
            }
            for id in pattern.children() {
                let part = node(self.ast, *id)?;
                poll(self.resources, part.span)?;
                let (name, mut kinds, relationship, bounded) = match part.kind {
                    NodeKind::NodePattern { variable } => {
                        (variable, ValueKinds::NODE, false, false)
                    }
                    NodeKind::RelationshipPattern {
                        variable, bounds, ..
                    } => (
                        variable,
                        if bounds.is_some() {
                            ValueKinds::LIST
                        } else {
                            ValueKinds::REL
                        },
                        true,
                        bounds.is_some(),
                    ),
                    _ => return Err(error(part.span, "invalid pattern member")),
                };
                let name = name
                    .map(|id| {
                        self.ast
                            .text(id)
                            .ok_or_else(|| error(part.span, "missing pattern variable"))
                    })
                    .transpose()?;
                let existing = if let Some(name) = name {
                    self.lookup_optional(name, part.span)?
                } else {
                    None
                };
                let symbol = if let Some(symbol) = existing {
                    if bounded || !symbol.info.kinds.contains(kinds) {
                        return Err(ParseError::new(
                            ErrorKind::Type,
                            part.span,
                            "pattern variable has incompatible type",
                        ));
                    }
                    if create
                        && (relationship
                            || !part.children().is_empty()
                            || pattern.children().len() == 1)
                    {
                        return Err(ParseError::new(
                            ErrorKind::DuplicateVariable,
                            part.span,
                            "CREATE redeclares a bound entity",
                        ));
                    }
                    symbol
                } else {
                    if optional {
                        kinds = kinds.union(ValueKinds::NULL);
                    }
                    let slot = self.new_slot(part.span)?;
                    let symbol = Symbol {
                        name: name.unwrap_or(""),
                        slot,
                        info: Info {
                            kinds,
                            row: true,
                            origin: Some(slot),
                            eligible: false,
                            constant: None,
                            invariant: None,
                        },
                    };
                    if name.is_some() {
                        if self.scope.len() >= self.limits.columns {
                            return Err(ParseError::new(
                                ErrorKind::Limit(LimitKind::Columns),
                                part.span,
                                "scope column limit",
                            ));
                        }
                        push(&mut self.scope, symbol, self.resources, part.span)?;
                    }
                    symbol
                };
                if relationship {
                    let origin = symbol.info.origin.unwrap_or(symbol.slot);
                    for prior in &relationships {
                        poll(self.resources, part.span)?;
                        if *prior == origin {
                            return Err(ParseError::new(
                                ErrorKind::RelationshipUniqueness,
                                part.span,
                                "relationship reused in one MATCH",
                            ));
                        }
                    }
                    push(&mut relationships, origin, self.resources, part.span)?;
                }
                *self
                    .facts
                    .get_mut(id.0)
                    .ok_or_else(|| error(part.span, "missing pattern fact"))? = Some(symbol.info);
                *self
                    .expressions
                    .get_mut(id.0)
                    .ok_or_else(|| error(part.span, "missing pattern slot"))? =
                    Expression::Slot(symbol.slot);
                for detail in part.children() {
                    let detail = node(self.ast, *detail)?;
                    if detail.kind == NodeKind::Properties {
                        for property in detail.children() {
                            let property = node(self.ast, *property)?;
                            let root = child(property, 0)?;
                            self.expression(root)?;
                            if create {
                                self.assignment(root)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
pub(super) fn property_kinds() -> ValueKinds {
    ValueKinds::NULL
        .union(ValueKinds::BOOL)
        .union(ValueKinds::I64)
        .union(ValueKinds::F64)
        .union(ValueKinds::STRING)
        .union(ValueKinds::LIST)
}
