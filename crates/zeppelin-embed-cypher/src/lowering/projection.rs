use super::*;
use zeppelin_embed::property_graph::query::QueryValue;
impl<'m, 'g, 'c> Builder<'m, 'g, 'c> {
    pub(super) fn projection(
        &mut self,
        bound: &BoundQuery<'_>,
        id: AstId,
        mut current: PlanNodeId,
    ) -> Result<PlanNodeId, ParseError> {
        let node = syntax(bound, id)?;
        let NodeKind::Projection { with, distinct } = node.kind else {
            return Err(invariant(node.span, "projection kind"));
        };
        let projection = bound
            .projections()
            .iter()
            .find(|p| p.syntax() == id)
            .ok_or_else(|| invariant(node.span, "bound projection"))?;
        let mut columns = Buffer::new(self.memory)?;
        let mut aggregate = false;
        for column in projection.columns() {
            let origin = if column.expression.0 as usize >= bound.syntax().nodes().len() {
                node.children()
                    .iter()
                    .find_map(|id| {
                        bound
                            .syntax()
                            .node(*id)
                            .filter(|n| n.kind == NodeKind::Star)
                            .map(|n| n.span)
                    })
                    .unwrap_or(node.span)
            } else {
                node.span
            };
            let expression = self.lower_expression(bound, column.expression, origin)?;
            aggregate |= matches!(
                self.expressions.slice().get(expression.0 as usize),
                Some(DraftExpr::Aggregate { .. })
            );
            columns.push(
                Projection {
                    slot: column.slot,
                    expression,
                },
                self.memory,
                self.control,
            )?;
        }
        let order_start = self.sort_keys.len();
        let mut offset = 0;
        let mut maximum = None;
        let mut has_bound = false;
        let mut predicate = None;
        for id in node.children() {
            let item = syntax(bound, *id)?;
            check(self.control, item.span)?;
            match item.kind {
                NodeKind::Order { descending } => {
                    let expression =
                        self.lower_expression(bound, ExprId(child(item, 0)?.0 as u32), item.span)?;
                    self.sort_keys.push(
                        SortKey {
                            expression,
                            descending,
                        },
                        self.memory,
                        self.control,
                    )?;
                }
                NodeKind::Skip => {
                    offset = self.bound(bound, item)?;
                    has_bound = true;
                }
                NodeKind::Limit => {
                    maximum = Some(self.bound(bound, item)?);
                    has_bound = true;
                }
                NodeKind::Predicate => {
                    predicate = Some(self.lower_expression(
                        bound,
                        ExprId(child(item, 0)?.0 as u32),
                        item.span,
                    )?)
                }
                _ => {}
            }
        }
        let order = Range {
            start: order_start,
            len: self.sort_keys.len() - order_start,
        };
        let mut hidden = Buffer::new(self.memory)?;
        if !aggregate && !distinct {
            for index in order.start..order.start + order.len {
                let root = self
                    .sort_keys
                    .slice()
                    .get(index)
                    .ok_or_else(|| invariant(node.span, "sort key"))?
                    .expression;
                self.order_inputs(root, columns.slice(), &mut hidden, node.span)?;
            }
        }
        if aggregate {
            let start = self.projections.len();
            for item in columns.slice() {
                if !matches!(
                    self.expressions.slice().get(item.expression.0 as usize),
                    Some(DraftExpr::Aggregate { .. })
                ) {
                    self.projections.push(*item, self.memory, self.control)?;
                }
            }
            let keys = Range {
                start,
                len: self.projections.len() - start,
            };
            let start = self.projections.len();
            for item in columns.slice() {
                if matches!(
                    self.expressions.slice().get(item.expression.0 as usize),
                    Some(DraftExpr::Aggregate { .. })
                ) {
                    self.projections.push(*item, self.memory, self.control)?;
                }
            }
            let aggregates = Range {
                start,
                len: self.projections.len() - start,
            };
            current = self.operator(
                DraftOp::Aggregate { keys, aggregates },
                &[current],
                node.span,
            )?;
            // Aggregate exposes keys then aggregates; restore textual column order.
            current = self.identity_projection(columns.slice(), current, false, node.span)?;
        } else {
            let start = self.projections.len();
            for item in columns.slice() {
                self.projections.push(*item, self.memory, self.control)?;
            }
            for slot in hidden.slice() {
                let expression = self.expression(DraftExpr::Slot(*slot), node.span)?;
                self.projections.push(
                    Projection {
                        slot: *slot,
                        expression,
                    },
                    self.memory,
                    self.control,
                )?;
            }
            let range = Range {
                start,
                len: self.projections.len() - start,
            };
            // With is final only when there is no temporary hidden-key scope.
            let kind = if with && hidden.len() == 0 {
                DraftOp::With(range)
            } else {
                DraftOp::Project(range)
            };
            current = self.operator(kind, &[current], node.span)?;
        }
        if distinct {
            current = self.operator(DraftOp::Distinct, &[current], node.span)?;
        }
        if order.len != 0 {
            current = self.operator(DraftOp::Sort(order), &[current], node.span)?;
        }
        if has_bound {
            current = self.operator(
                DraftOp::OffsetLimit {
                    offset,
                    limit: maximum,
                },
                &[current],
                node.span,
            )?;
        }
        if hidden.len() != 0 || aggregate && with {
            current = self.identity_projection(columns.slice(), current, with, node.span)?;
        }
        if let Some(predicate) = predicate {
            current = self.operator(DraftOp::Filter(predicate), &[current], node.span)?;
        }
        self.scope.arena.clear();
        for column in projection.columns() {
            self.scope.push(column.slot, self.memory, self.control)?;
        }
        Ok(current)
    }
    fn identity_projection(
        &mut self,
        columns: &[Projection],
        input: PlanNodeId,
        with: bool,
        span: Span,
    ) -> Result<PlanNodeId, ParseError> {
        let start = self.projections.len();
        for column in columns {
            let expression = self.expression(DraftExpr::Slot(column.slot), span)?;
            self.projections.push(
                Projection {
                    slot: column.slot,
                    expression,
                },
                self.memory,
                self.control,
            )?;
        }
        let range = Range {
            start,
            len: columns.len(),
        };
        self.operator(
            if with {
                DraftOp::With(range)
            } else {
                DraftOp::Project(range)
            },
            &[input],
            span,
        )
    }
    fn bound(&self, bound: &BoundQuery<'_>, item: &Node) -> Result<u64, ParseError> {
        let expression = syntax(bound, child(item, 0)?)?;
        let invalid = || {
            ParseError::new(
                ErrorKind::InvalidRange,
                item.span,
                "bound must be a nonnegative I64 literal or parameter",
            )
        };
        let value = match expression.kind {
            NodeKind::Integer(value) => value,
            NodeKind::Parameter(name) => {
                let name = bound.syntax().text(name).ok_or_else(invalid)?;
                let value = bound
                    .parameters()
                    .iter()
                    .find(|p| p.name == name)
                    .ok_or_else(invalid)?
                    .value;
                if let QueryValue::I64(value) = value {
                    value
                } else {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        };
        u64::try_from(value).map_err(|_| invalid())
    }
    fn order_inputs(
        &self,
        root: ExprId,
        columns: &[Projection],
        hidden: &mut Buffer<'m, 'g, SlotId>,
        span: Span,
    ) -> Result<(), ParseError> {
        let mut seen =
            QueryArena::new(self.memory, self.expressions.len()).map_err(memory_error)?;
        for _ in self.expressions.slice() {
            seen.push(false).map_err(memory_error)?;
        }
        *seen
            .as_mut_slice()
            .get_mut(root.0 as usize)
            .ok_or_else(|| invariant(span, "order root"))? = true;
        for index in (0..self.expressions.len()).rev() {
            check(self.control, span)?;
            if !seen.as_slice().get(index).copied().unwrap_or(false) {
                continue;
            }
            let expression = *self
                .expressions
                .slice()
                .get(index)
                .ok_or_else(|| invariant(span, "order expression"))?;
            let mut mark = |id: ExprId| -> Result<(), ParseError> {
                *seen
                    .as_mut_slice()
                    .get_mut(id.0 as usize)
                    .ok_or_else(|| invariant(span, "order dependency"))? = true;
                Ok(())
            };
            match expression {
                DraftExpr::Slot(slot) => {
                    if !columns.iter().any(|column| column.slot == slot)
                        && !hidden.slice().contains(&slot)
                    {
                        if !self.scope.slice().contains(&slot) {
                            return Err(invariant(span, "unbound hidden order key"));
                        }
                        hidden.push(slot, self.memory, self.control)?;
                    }
                }
                DraftExpr::Property { entity, .. } | DraftExpr::HasLabel { entity, .. } => {
                    mark(entity)?
                }
                DraftExpr::Unary { operand, .. } => mark(operand)?,
                DraftExpr::Binary { left, right, .. } => {
                    mark(left)?;
                    mark(right)?;
                }
                DraftExpr::List(range) => {
                    for id in range.get(self.children.slice())? {
                        mark(*id)?;
                    }
                }
                DraftExpr::Aggregate { .. } => {
                    return Err(invariant(span, "aggregate hidden order expression"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}
