use super::*;
use zeppelin_embed::property_graph::query::{Comparison, plan::Direction as NativeDirection};

impl Builder<'_, '_, '_> {
    fn fresh(&mut self, span: Span) -> Result<SlotId, ParseError> {
        let slot = SlotId(self.next_slot);
        self.next_slot = self.next_slot.checked_add(1).ok_or_else(|| limit(span))?;
        Ok(slot)
    }
    fn scoped(&self, slot: SlotId) -> bool {
        self.scope.slice().contains(&slot)
    }
    fn add_scope(&mut self, slot: SlotId, span: Span) -> Result<(), ParseError> {
        if !self.scoped(slot) {
            if self.scope.len() >= MAX_COLUMNS {
                return Err(limit(span));
            }
            self.scope.push(slot, self.memory, self.control)?;
        }
        Ok(())
    }
    fn equality(&mut self, a: SlotId, b: SlotId, span: Span) -> Result<ExprId, ParseError> {
        let left = self.expression(DraftExpr::Slot(a), span)?;
        let right = self.expression(DraftExpr::Slot(b), span)?;
        self.expression(
            DraftExpr::Binary {
                operation: BinaryExpression::Comparison(Comparison::Equal),
                left,
                right,
            },
            span,
        )
    }
    fn conjunct(
        &mut self,
        prior: Option<ExprId>,
        next: ExprId,
        span: Span,
    ) -> Result<ExprId, ParseError> {
        match prior {
            None => Ok(next),
            Some(left) => self.expression(
                DraftExpr::Binary {
                    operation: BinaryExpression::And,
                    left,
                    right: next,
                },
                span,
            ),
        }
    }
    fn constraints(
        &mut self,
        bound: &BoundQuery<'_>,
        node: &Node,
        entity: SlotId,
    ) -> Result<Option<ExprId>, ParseError> {
        let mut predicate = None;
        for id in node.children() {
            let detail = syntax(bound, *id)?;
            match detail.kind {
                NodeKind::Name(name) if matches!(node.kind, NodeKind::NodePattern { .. }) => {
                    let label = self.copy_text(
                        bound
                            .syntax()
                            .text(name)
                            .ok_or_else(|| invariant(detail.span, "node label"))?,
                    )?;
                    let entity = self.expression(DraftExpr::Slot(entity), node.span)?;
                    let next =
                        self.expression(DraftExpr::HasLabel { entity, label }, detail.span)?;
                    predicate = Some(self.conjunct(predicate, next, detail.span)?);
                }
                NodeKind::Properties => {
                    for id in detail.children() {
                        let property = syntax(bound, *id)?;
                        let NodeKind::Property(name) = property.kind else {
                            return Err(invariant(property.span, "inline property"));
                        };
                        let right = self.lower_expression(
                            bound,
                            ExprId(child(property, 0)?.0 as u32),
                            property.span,
                        )?;
                        let name = self.copy_text(
                            bound
                                .syntax()
                                .text(name)
                                .ok_or_else(|| invariant(property.span, "property name"))?,
                        )?;
                        let entity = self.expression(DraftExpr::Slot(entity), node.span)?;
                        let left =
                            self.expression(DraftExpr::Property { entity, name }, property.span)?;
                        let next = self.expression(
                            DraftExpr::Binary {
                                operation: BinaryExpression::Comparison(Comparison::Equal),
                                left,
                                right,
                            },
                            property.span,
                        )?;
                        predicate = Some(self.conjunct(predicate, next, property.span)?);
                    }
                }
                _ => {}
            }
        }
        Ok(predicate)
    }
    fn filter_constraints(
        &mut self,
        bound: &BoundQuery<'_>,
        node: &Node,
        entity: SlotId,
        input: PlanNodeId,
    ) -> Result<PlanNodeId, ParseError> {
        if let Some(predicate) = self.constraints(bound, node, entity)? {
            self.operator(DraftOp::Filter(predicate), &[input], node.span)
        } else {
            Ok(input)
        }
    }
    /// Only the complete, infallible ID equality of an initial fixed-length
    /// read may replace enumeration. Constraints and WHERE remain residuals.
    fn id_equality_lookup(
        &self,
        bound: &BoundQuery<'_>,
        clause: &Node,
    ) -> Result<Option<zeppelin_embed::property_graph::NodeId>, ParseError> {
        let [part_id, predicate_id] = clause.children() else {
            return Ok(None);
        };
        let part = syntax(bound, *part_id)?;
        let predicate = syntax(bound, *predicate_id)?;
        let Some(node_id) = part.children().first() else {
            return Ok(None);
        };
        if part.children().iter().any(|id| {
            bound.syntax().node(*id).is_some_and(|node| {
                matches!(
                    node.kind,
                    NodeKind::RelationshipPattern {
                        bounds: Some(_),
                        ..
                    }
                )
            })
        }) {
            return Ok(None);
        }
        let node = syntax(bound, *node_id)?;
        if predicate.kind != NodeKind::Predicate
            || !matches!(node.kind, NodeKind::NodePattern { .. })
            || node.children().iter().any(|id| {
                bound
                    .syntax()
                    .node(*id)
                    .is_some_and(|detail| detail.kind == NodeKind::Properties)
            })
        {
            return Ok(None);
        }
        let anchor = slot(bound, *node_id)?;
        let expression = |id: ExprId| bound.expressions().get(id.0 as usize);
        let Some(Expression::Binary {
            operation: BinaryExpression::Comparison(Comparison::Equal),
            left,
            right,
        }) = expression(ExprId(child(predicate, 0)?.0 as u32))
        else {
            return Ok(None);
        };
        let is_id = |id| {
            matches!(expression(id), Some(Expression::Unary {
            operation: UnaryExpression::NodeIdText, operand,
        }) if matches!(expression(*operand), Some(Expression::Slot(slot)) if *slot == anchor))
        };
        let value = if is_id(*left) {
            *right
        } else if is_id(*right) {
            *left
        } else {
            return Ok(None);
        };
        let text = match expression(value) {
            Some(Expression::Literal(Literal::String(text))) => *text,
            Some(Expression::Parameter(id)) => {
                match bound.parameters().get(id.0 as usize).map(|p| p.value) {
                    Some(zeppelin_embed::property_graph::query::QueryValue::String(text)) => text,
                    _ => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
        if text.len() != 32
            || !text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Ok(None);
        }
        let id = u128::from_str_radix(text, 16)
            .map_err(|_| invariant(predicate.span, "canonical node identity"))?;
        Ok(Some(zeppelin_embed::ingest::DocId::new(id).into()))
    }
    pub(super) fn pattern(
        &mut self,
        bound: &BoundQuery<'_>,
        clause: &Node,
        left: PlanNodeId,
        optional: bool,
        initial_read: bool,
    ) -> Result<PlanNodeId, ParseError> {
        let pattern = PatternId(self.pattern_id);
        self.pattern_id = self
            .pattern_id
            .checked_add(1)
            .ok_or_else(|| limit(clause.span))?;
        let lookup = if initial_read && !optional {
            self.id_equality_lookup(bound, clause)?
        } else {
            None
        };
        let mut current = left;
        let mut attached = None;
        for id in clause.children() {
            let part = syntax(bound, *id)?;
            if part.kind == NodeKind::Predicate {
                attached = Some(self.lower_expression(
                    bound,
                    ExprId(child(part, 0)?.0 as u32),
                    part.span,
                )?);
                continue;
            }
            let first = child(part, 0)?;
            let mut anchor = slot(bound, first)?;
            let first_node = syntax(bound, first)?;
            // A simple label on a fresh fixed-length relationship anchor is
            // represented by the structured scan's existing label field.
            let scan_label = if initial_read
                && !optional
                && lookup.is_none()
                && part.children().len() > 1
                && !part.children().iter().any(|id| {
                    bound.syntax().node(*id).is_some_and(|node| {
                        matches!(
                            node.kind,
                            NodeKind::RelationshipPattern {
                                bounds: Some(_),
                                ..
                            }
                        )
                    })
                })
                && !self.scoped(anchor)
            {
                if let [label_id] = first_node.children()
                    && let NodeKind::Name(name) = syntax(bound, *label_id)?.kind
                {
                    Some(
                        self.copy_text(
                            bound
                                .syntax()
                                .text(name)
                                .ok_or_else(|| invariant(first_node.span, "anchor label"))?,
                        )?,
                    )
                } else {
                    None
                }
            } else {
                None
            };
            if self.scoped(anchor) {
                let operand = self.expression(DraftExpr::Slot(anchor), first_node.span)?;
                let predicate = self.expression(
                    DraftExpr::Unary {
                        operation: UnaryExpression::IsNotNull,
                        operand,
                    },
                    first_node.span,
                )?;
                current = self.operator(DraftOp::Filter(predicate), &[current], first_node.span)?;
            } else {
                let op = match lookup {
                    Some(id) => DraftOp::LookupNode { output: anchor, id },
                    None => DraftOp::Scan {
                        output: anchor,
                        label: scan_label,
                    },
                };
                current = self.operator(op, &[current], first_node.span)?;
                self.add_scope(anchor, first_node.span)?;
            }
            if scan_label.is_none() {
                current = self.filter_constraints(bound, first_node, anchor, current)?;
            }
            let mut index = 1;
            while index < part.children().len() {
                let relationship_id = child(part, index)?;
                let target_id = child(part, index + 1)?;
                let relationship_node = syntax(bound, relationship_id)?;
                let target_node = syntax(bound, target_id)?;
                let wanted_rel = slot(bound, relationship_id)?;
                let wanted_node = slot(bound, target_id)?;
                let reuse_rel = self.scoped(wanted_rel);
                let reuse_node = self.scoped(wanted_node);
                let relationship = if reuse_rel {
                    self.fresh(relationship_node.span)?
                } else {
                    wanted_rel
                };
                let node = if reuse_node {
                    self.fresh(target_node.span)?
                } else {
                    wanted_node
                };
                let NodeKind::RelationshipPattern {
                    direction, bounds, ..
                } = relationship_node.kind
                else {
                    return Err(invariant(relationship_node.span, "relationship pattern"));
                };
                let direction = match direction {
                    crate::Direction::Incoming => NativeDirection::Incoming,
                    crate::Direction::Outgoing => NativeDirection::Outgoing,
                    crate::Direction::Both => NativeDirection::Either,
                };
                let start = self.names.len();
                for id in relationship_node.children() {
                    let detail = syntax(bound, *id)?;
                    if let NodeKind::Name(name) = detail.kind {
                        let name = self.copy_text(
                            bound
                                .syntax()
                                .text(name)
                                .ok_or_else(|| invariant(detail.span, "relationship type"))?,
                        )?;
                        self.names.push(name, self.memory, self.control)?;
                    }
                }
                let types = Range {
                    start,
                    len: self.names.len() - start,
                };
                let op = if let Some(bounds) = bounds {
                    let has_properties = relationship_node.children().iter().any(|id| {
                        bound.syntax().node(*id).is_some_and(|n| {
                            n.kind == NodeKind::Properties && !n.children().is_empty()
                        })
                    });
                    let edge_predicate = if has_properties {
                        let current_edge = self.fresh(relationship_node.span)?;
                        self.constraints(bound, relationship_node, current_edge)?
                            .map(|expression| EdgePredicate {
                                current_edge,
                                expression,
                            })
                    } else {
                        None
                    };
                    // A RHS that reads this newly produced LIST cannot run
                    // before the path exists. Keep the entire bag at the
                    // completed stage, preserving conjunction/error semantics.
                    let (edge_predicate, completed_edge_predicate) = match edge_predicate {
                        Some(predicate)
                            if self.references_slot(
                                predicate.expression,
                                wanted_rel,
                                relationship_node.span,
                            )? =>
                        {
                            (
                                None,
                                Some(CompletedEdgePredicate {
                                    current_edge: predicate.current_edge,
                                    expression: predicate.expression,
                                }),
                            )
                        }
                        other => (other, None),
                    };
                    DraftOp::Bounded {
                        source: anchor,
                        node,
                        relationships: relationship,
                        direction,
                        types,
                        pattern,
                        min: u8::try_from(bounds.lower)
                            .map_err(|_| limit(relationship_node.span))?,
                        max: u8::try_from(bounds.upper)
                            .map_err(|_| limit(relationship_node.span))?,
                        edge_predicate,
                        completed_edge_predicate,
                    }
                } else {
                    DraftOp::Expand {
                        source: anchor,
                        node,
                        relationship,
                        direction,
                        types,
                        pattern,
                    }
                };
                current = self.operator(op, &[current], relationship_node.span)?;
                if bounds.is_none() {
                    current =
                        self.filter_constraints(bound, relationship_node, relationship, current)?;
                }
                for (reuse, candidate, original) in [
                    (reuse_rel, relationship, wanted_rel),
                    (reuse_node, node, wanted_node),
                ] {
                    if reuse {
                        let predicate =
                            self.equality(candidate, original, relationship_node.span)?;
                        current = self.operator(
                            DraftOp::Filter(predicate),
                            &[current],
                            relationship_node.span,
                        )?;
                    }
                }
                self.add_scope(wanted_rel, relationship_node.span)?;
                self.add_scope(wanted_node, target_node.span)?;
                if reuse_rel || reuse_node {
                    let start = self.projections.len();
                    for index in 0..self.scope.len() {
                        let slot = *self
                            .scope
                            .slice()
                            .get(index)
                            .ok_or_else(|| invariant(clause.span, "reused scope"))?;
                        let expression =
                            self.expression(DraftExpr::Slot(slot), relationship_node.span)?;
                        self.projections.push(
                            Projection { slot, expression },
                            self.memory,
                            self.control,
                        )?;
                    }
                    current = self.operator(
                        DraftOp::Project(Range {
                            start,
                            len: self.scope.len(),
                        }),
                        &[current],
                        relationship_node.span,
                    )?;
                }
                current = self.filter_constraints(bound, target_node, wanted_node, current)?;
                anchor = wanted_node;
                index += 2;
            }
        }
        if optional {
            self.operator(DraftOp::Optional(attached), &[left, current], clause.span)
        } else if let Some(predicate) = attached {
            self.operator(DraftOp::Filter(predicate), &[current], clause.span)
        } else {
            Ok(current)
        }
    }
}
