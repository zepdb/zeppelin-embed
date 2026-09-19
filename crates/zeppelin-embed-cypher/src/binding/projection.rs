use super::*;
impl<'a> Binder<'a, '_> {
    pub(super) fn projection(
        &mut self,
        clause_id: AstId,
        clause: &'a Node,
    ) -> Result<(), ParseError> {
        let NodeKind::Projection { distinct, with } = clause.kind else {
            return Err(error(clause.span, "invalid projection"));
        };
        let mut columns: Vec<BoundColumn<'a>> = Vec::new();
        let mut output = Vec::new();
        let mut aliases = Vec::new();
        let mut aggregate = false;
        let mut key_count = 0_usize;
        for item in clause.children() {
            let item = node(self.ast, *item)?;
            poll(self.resources, item.span)?;
            match item.kind {
                NodeKind::Star => {
                    for index in 0..self.scope.len() {
                        key_count += 1;
                        let symbol = *self
                            .scope
                            .get(index)
                            .ok_or_else(|| error(item.span, "missing wildcard symbol"))?;
                        if !with {
                            self.check_deleted(symbol.info, item.span)?;
                        }
                        let expression =
                            self.append_expression(Expression::Slot(symbol.slot), item.span)?;
                        self.project_column(
                            &mut columns,
                            &mut output,
                            symbol.name,
                            symbol.info,
                            expression,
                            item.span,
                        )?;
                    }
                }
                NodeKind::ProjectionItem { alias } => {
                    let root = child(item, 0)?;
                    let info = self.expression(root)?;
                    if !with {
                        self.check_deleted(info, item.span)?;
                    }
                    aggregate |= node(self.ast, root)?.aggregate;
                    if !node(self.ast, root)?.aggregate {
                        key_count += 1;
                    }
                    let expression = node(self.ast, root)?;
                    let ungrouped = node(self.ast, self.ungroup(root)?)?;
                    let name = if let Some(alias) = alias {
                        self.ast.text(alias)
                    } else if let NodeKind::Variable(name) = ungrouped.kind {
                        self.ast.text(name)
                    } else {
                        self.ast
                            .source()
                            .get(expression.span.start..expression.span.end)
                    }
                    .ok_or_else(|| error(item.span, "missing output name"))?;
                    let symbol = self.project_column(
                        &mut columns,
                        &mut output,
                        name,
                        info,
                        expr_id(root)?,
                        item.span,
                    )?;
                    if !expression.aggregate {
                        push(&mut aliases, (root, symbol), self.resources, item.span)?;
                    }
                }
                _ => {}
            }
        }
        if aggregate {
            self.singleton = key_count == 0;
        }
        for symbol in &mut output {
            symbol.info.eligible &= self.singleton;
        }
        let input = std::mem::take(&mut self.scope);
        for symbol in &output {
            push(&mut self.scope, *symbol, self.resources, clause.span)?;
        }
        if !distinct && !aggregate {
            for symbol in &input {
                poll(self.resources, clause.span)?;
                if self.lookup_optional(symbol.name, clause.span)?.is_none() {
                    push(&mut self.scope, *symbol, self.resources, clause.span)?;
                }
            }
        } else {
            self.order_aliases = aliases;
        }
        for item in clause.children() {
            let item = node(self.ast, *item)?;
            if matches!(item.kind, NodeKind::Order { .. }) {
                self.expression(child(item, 0)?)?;
            }
            if matches!(item.kind, NodeKind::Skip | NodeKind::Limit) {
                let root = child(item, 0)?;
                let info = self.expression(root)?;
                if info.kinds != ValueKinds::I64 || info.row {
                    return Err(ParseError::new(
                        ErrorKind::InvalidRange,
                        item.span,
                        "bound must be a nonnegative I64 constant or parameter",
                    ));
                }
                let value = match node(self.ast, root)?.kind {
                    NodeKind::Integer(value) => value,
                    NodeKind::Parameter(name) => {
                        let name = self
                            .ast
                            .text(name)
                            .ok_or_else(|| error(item.span, "missing bound parameter"))?;
                        match self.parameter(name, item.span)? {
                            QueryValue::I64(value) => value,
                            _ => {
                                return Err(ParseError::new(
                                    ErrorKind::InvalidRange,
                                    item.span,
                                    "invalid bound parameter",
                                ));
                            }
                        }
                    }
                    _ => {
                        return Err(ParseError::new(
                            ErrorKind::InvalidRange,
                            item.span,
                            "row-dependent bound",
                        ));
                    }
                };
                if value < 0 {
                    return Err(ParseError::new(
                        ErrorKind::InvalidRange,
                        item.span,
                        "negative bound",
                    ));
                }
            }
        }
        self.order_aliases.clear();
        self.scope = output;
        push(
            &mut self.projections,
            BoundProjection {
                syntax: clause_id,
                columns,
            },
            self.resources,
            clause.span,
        )?;
        for item in clause.children() {
            let item = node(self.ast, *item)?;
            if item.kind == NodeKind::Predicate {
                let info = self.expression(child(item, 0)?)?;
                require(info.kinds, ValueKinds::BOOL, item.span)?;
            }
        }
        Ok(())
    }
    fn project_column(
        &mut self,
        columns: &mut Vec<BoundColumn<'a>>,
        output: &mut Vec<Symbol<'a>>,
        name: &'a str,
        info: Info,
        expression: ExprId,
        span: Span,
    ) -> Result<Symbol<'a>, ParseError> {
        for prior in columns.iter() {
            poll(self.resources, span)?;
            if prior.name == name {
                return Err(ParseError::new(
                    ErrorKind::DuplicateVariable,
                    span,
                    "duplicate output name",
                ));
            }
        }
        if columns.len() >= self.limits.columns {
            return Err(ParseError::new(
                ErrorKind::Limit(LimitKind::Columns),
                span,
                "output column limit",
            ));
        }
        let slot = self.new_slot(span)?;
        let symbol = Symbol { name, slot, info };
        push(
            columns,
            BoundColumn {
                name,
                kinds: info.kinds,
                expression,
                slot,
            },
            self.resources,
            span,
        )?;
        push(output, symbol, self.resources, span)?;
        Ok(symbol)
    }
    pub(super) fn append_expression(
        &mut self,
        expression: Expression<'a>,
        span: Span,
    ) -> Result<ExprId, ParseError> {
        // Syntax identities include non-expression holes. Synthesized expressions
        // have their own bounded allowance; later lowering prunes holes before
        // the exact GraphPlan node-limit validation.
        if self
            .expressions
            .len()
            .saturating_sub(self.ast.nodes().len())
            >= self.limits.ast_nodes
        {
            return Err(ParseError::new(
                ErrorKind::Limit(LimitKind::AstNodes),
                span,
                "lowered expression limit",
            ));
        }
        let id = ExprId(
            u32::try_from(self.expressions.len())
                .map_err(|_| error(span, "expression identity overflow"))?,
        );
        push(&mut self.expressions, expression, self.resources, span)?;
        Ok(id)
    }
    pub(super) fn ungroup(&mut self, mut id: AstId) -> Result<AstId, ParseError> {
        loop {
            let node = node(self.ast, id)?;
            poll(self.resources, node.span)?;
            if node.kind != NodeKind::Group {
                return Ok(id);
            }
            id = child(node, 0)?;
        }
    }
    pub(super) fn projected_alias(&mut self, id: AstId) -> Result<Option<Symbol<'a>>, ParseError> {
        for index in 0..self.order_aliases.len() {
            let (candidate, symbol) = *self
                .order_aliases
                .get(index)
                .ok_or_else(|| error(Span::default(), "missing projected alias"))?;
            if self.equivalent(id, candidate)? {
                return Ok(Some(symbol));
            }
        }
        Ok(None)
    }
    fn equivalent(&mut self, a: AstId, b: AstId) -> Result<bool, ParseError> {
        let mut stack = Vec::new();
        push(&mut stack, (a, b), self.resources, Span::default())?;
        while let Some((a, b)) = stack.pop() {
            let a = self.ungroup(a)?;
            let b = self.ungroup(b)?;
            let a = node(self.ast, a)?;
            let b = node(self.ast, b)?;
            if let NodeKind::Variable(name) = a.kind {
                let name = self
                    .ast
                    .text(name)
                    .ok_or_else(|| error(a.span, "missing order variable"))?;
                // ORDER BY resolves output aliases first. A same-spelled input
                // expression is reusable only while its variables are absent
                // from the output scope; shadowing may change their values.
                if self.lookup_optional(name, a.span)?.is_some() {
                    return Ok(false);
                }
            }
            let names = match (a.kind, b.kind) {
                (NodeKind::Variable(a), NodeKind::Variable(b))
                | (NodeKind::Parameter(a), NodeKind::Parameter(b))
                | (NodeKind::String(a), NodeKind::String(b))
                | (NodeKind::PropertyAccess(a), NodeKind::PropertyAccess(b))
                | (NodeKind::Name(a), NodeKind::Name(b)) => Some((a, b)),
                _ => None,
            };
            if let Some((a, b)) = names {
                if self.ast.text(a) != self.ast.text(b) {
                    return Ok(false);
                }
            } else if a.kind != b.kind {
                return Ok(false);
            }
            if a.children().len() != b.children().len() {
                return Ok(false);
            }
            for (a, b) in a.children().iter().zip(b.children()) {
                push(&mut stack, (*a, *b), self.resources, Span::default())?;
            }
        }
        Ok(true)
    }
}
