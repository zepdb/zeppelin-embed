use super::*;

#[derive(Clone, Copy)]
struct Frame {
    id: ExprId,
    finish: bool,
    depth: usize,
    span: Span,
}
impl<'m, 'g, 'c> Builder<'m, 'g, 'c> {
    /// Copy only reachable scalar IR. Synthetic binder IDs share this remap;
    /// syntax holes and unused intermediate label/group nodes never enter core.
    pub(super) fn lower_expression(
        &mut self,
        bound: &BoundQuery<'_>,
        root: ExprId,
        origin: Span,
    ) -> Result<ExprId, ParseError> {
        let mut pending = Buffer::new(self.memory)?;
        let mut length = 0;
        let memory = self.memory;
        let control = self.control;
        let push = |pending: &mut Buffer<'m, 'g, Frame>,
                    length: &mut usize,
                    frame: Frame|
         -> Result<(), ParseError> {
            if *length >= MAX_PLAN_NODES * 2 {
                return Err(limit(frame.span));
            }
            if let Some(cell) = pending.arena.as_mut_slice().get_mut(*length) {
                *cell = frame;
            } else {
                pending.push(frame, memory, control)?;
            }
            *length += 1;
            Ok(())
        };
        push(
            &mut pending,
            &mut length,
            Frame {
                id: root,
                finish: false,
                depth: 1,
                span: origin,
            },
        )?;
        while length != 0 {
            length -= 1;
            let frame = *pending
                .slice()
                .get(length)
                .ok_or_else(|| invariant(origin, "expression frame"))?;
            check(self.control, frame.span)?;
            if frame.depth > MAX_PLAN_DEPTH {
                return Err(limit(frame.span));
            }
            if self
                .remap
                .slice()
                .get(frame.id.0 as usize)
                .copied()
                .flatten()
                .is_some()
            {
                continue;
            }
            let expression = *bound
                .expressions()
                .get(frame.id.0 as usize)
                .ok_or_else(|| invariant(frame.span, "bound expression reference"))?;
            let span = bound
                .syntax()
                .nodes()
                .get(frame.id.0 as usize)
                .map_or(frame.span, |node| node.span);
            if !frame.finish {
                push(
                    &mut pending,
                    &mut length,
                    Frame {
                        finish: true,
                        span,
                        ..frame
                    },
                )?;
                let mut child = |id| {
                    push(
                        &mut pending,
                        &mut length,
                        Frame {
                            id,
                            finish: false,
                            depth: frame.depth + 1,
                            span,
                        },
                    )
                };
                match expression {
                    Expression::Aggregate {
                        operand: Some(id), ..
                    }
                    | Expression::Property { entity: id, .. }
                    | Expression::HasLabel { entity: id, .. }
                    | Expression::Unary { operand: id, .. } => child(id)?,
                    Expression::Binary { left, right, .. } => {
                        child(right)?;
                        child(left)?;
                    }
                    Expression::List(items) => {
                        for id in items.iter().rev() {
                            child(*id)?;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let draft = match expression {
                Expression::Literal(Literal::String(value)) => {
                    DraftExpr::String(self.copy_text(value)?)
                }
                Expression::Literal(Literal::Null) => DraftExpr::Literal(Literal::Null),
                Expression::Literal(Literal::Bool(v)) => DraftExpr::Literal(Literal::Bool(v)),
                Expression::Literal(Literal::I64(v)) => DraftExpr::Literal(Literal::I64(v)),
                Expression::Literal(Literal::F64(v)) => DraftExpr::Literal(Literal::F64(v)),
                Expression::Slot(slot) => DraftExpr::Slot(slot),
                Expression::Parameter(id) => DraftExpr::Parameter(id),
                Expression::Aggregate { operation, operand } => DraftExpr::Aggregate {
                    operation,
                    operand: operand.map(|id| self.mapped(id, span)).transpose()?,
                },
                Expression::Unary { operation, operand } => DraftExpr::Unary {
                    operation,
                    operand: self.mapped(operand, span)?,
                },
                Expression::Binary {
                    operation,
                    left,
                    right,
                } => DraftExpr::Binary {
                    operation,
                    left: self.mapped(left, span)?,
                    right: self.mapped(right, span)?,
                },
                Expression::Property { entity, name } => DraftExpr::Property {
                    entity: self.mapped(entity, span)?,
                    name: self.copy_text(name.as_str())?,
                },
                Expression::HasLabel { entity, label } => DraftExpr::HasLabel {
                    entity: self.mapped(entity, span)?,
                    label: self.copy_text(label.as_str())?,
                },
                Expression::List(items) => {
                    let range = Range {
                        start: self.children.len(),
                        len: items.len(),
                    };
                    for item in items {
                        self.children
                            .push(self.mapped(*item, span)?, self.memory, self.control)?;
                    }
                    DraftExpr::List(range)
                }
            };
            let id = self.expression(draft, span)?;
            *self
                .remap
                .arena
                .as_mut_slice()
                .get_mut(frame.id.0 as usize)
                .ok_or_else(|| invariant(span, "expression remap"))? = Some(id);
        }
        self.mapped(root, origin)
    }
    /// Dependency walk over the copied postorder DAG, not syntax spelling: a
    /// list read nested below size/index/arithmetic still needs the complete path.
    pub(super) fn references_slot(
        &self,
        root: ExprId,
        wanted: SlotId,
        span: Span,
    ) -> Result<bool, ParseError> {
        let mut seen =
            QueryArena::new(self.memory, self.expressions.len()).map_err(memory_error)?;
        for _ in self.expressions.slice() {
            seen.push(false).map_err(memory_error)?;
        }
        *seen
            .as_mut_slice()
            .get_mut(root.0 as usize)
            .ok_or_else(|| invariant(span, "predicate root"))? = true;
        for index in (0..=root.0 as usize).rev() {
            check(self.control, span)?;
            if !seen.as_slice().get(index).copied().unwrap_or(false) {
                continue;
            }
            let expression = *self
                .expressions
                .slice()
                .get(index)
                .ok_or_else(|| invariant(span, "predicate dependency"))?;
            let mut mark = |id: ExprId| -> Result<(), ParseError> {
                *seen
                    .as_mut_slice()
                    .get_mut(id.0 as usize)
                    .ok_or_else(|| invariant(span, "predicate child"))? = true;
                Ok(())
            };
            match expression {
                DraftExpr::Slot(slot) if slot == wanted => return Ok(true),
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
                DraftExpr::Aggregate {
                    operand: Some(id), ..
                } => mark(id)?,
                _ => {}
            }
        }
        Ok(false)
    }
    fn mapped(&self, id: ExprId, span: Span) -> Result<ExprId, ParseError> {
        self.remap
            .slice()
            .get(id.0 as usize)
            .copied()
            .flatten()
            .ok_or_else(|| invariant(span, "unmapped scalar dependency"))
    }
}
