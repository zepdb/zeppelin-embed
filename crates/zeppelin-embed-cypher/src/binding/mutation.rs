use super::*;
impl<'a> Binder<'a, '_> {
    pub(super) fn mutation(&mut self, clause: &'a Node) -> Result<(), ParseError> {
        for id in clause.children() {
            let item = node(self.ast, *id)?;
            poll(self.resources, item.span)?;
            let (variable, allowed) = match item.kind {
                NodeKind::SetProperty { variable, .. }
                | NodeKind::RemoveProperty { variable, .. } => {
                    (variable, ValueKinds::NODE.union(ValueKinds::REL))
                }
                NodeKind::SetLabels { variable } | NodeKind::RemoveLabels { variable } => {
                    (variable, ValueKinds::NODE)
                }
                NodeKind::Variable(variable) if matches!(clause.kind, NodeKind::Delete { .. }) => {
                    (variable, ValueKinds::NODE.union(ValueKinds::REL))
                }
                _ => return Err(error(item.span, "invalid mutation item")),
            };
            let name = self
                .ast
                .text(variable)
                .ok_or_else(|| error(item.span, "missing mutation variable"))?;
            let symbol = self.lookup(name, item.span)?;
            require(symbol.info.kinds, allowed, item.span)?;
            if !matches!(clause.kind, NodeKind::Delete { .. }) {
                self.check_deleted(symbol.info, item.span)?;
            }
            *self
                .expressions
                .get_mut(id.0)
                .ok_or_else(|| error(item.span, "missing mutation symbol"))? =
                Expression::Slot(symbol.slot);
            *self
                .facts
                .get_mut(id.0)
                .ok_or_else(|| error(item.span, "missing mutation fact"))? = Some(symbol.info);
            if matches!(item.kind, NodeKind::SetProperty { .. }) {
                let root = child(item, 0)?;
                self.expression(root)?;
                self.assignment(root)?;
            }
            if matches!(clause.kind, NodeKind::Delete { .. }) {
                // Projected/indexed variables may have no static origin.
                self.has_deletions = true;
            }
            if matches!(clause.kind, NodeKind::Delete { .. })
                && let Some(origin) = symbol.info.origin
                && !self.deleted.contains(&origin)
            {
                push(&mut self.deleted, origin, self.resources, item.span)?;
            }
        }
        Ok(())
    }
    pub(super) fn check_deleted(&mut self, info: Info, span: Span) -> Result<(), ParseError> {
        if !self.has_deletions {
            return Ok(());
        }
        let mut pending = Vec::new();
        push(&mut pending, info, self.resources, span)?;
        while let Some(info) = pending.pop() {
            poll(self.resources, span)?;
            for deleted in &self.deleted {
                poll(self.resources, span)?;
                if info.origin == Some(*deleted) {
                    if info.kinds.contains(ValueKinds::NULL) {
                        // Empty OPTIONAL input may return null. A nonnull value
                        // must still trigger the later atomic deleted check.
                        self.runtime_deleted_checks = true;
                        continue;
                    }
                    return Err(ParseError::new(
                        ErrorKind::DeletedEntity,
                        span,
                        "provably deleted entity access or result",
                    ));
                }
            }
            // Distinct binding origins may identify the same entity at runtime.
            // Indexing loses static origin; bounded paths carry relationship
            // lists. Preserve dynamic checks for each of those possible aliases.
            if info.kinds.contains(ValueKinds::NODE)
                || info.kinds.contains(ValueKinds::REL)
                || (info.kinds.contains(ValueKinds::LIST) && info.origin.is_some())
            {
                self.runtime_deleted_checks = true;
            }
            if info.kinds.contains(ValueKinds::LIST)
                && let Some(source) = info.constant
            {
                let source = node(self.ast, source)?;
                if source.kind == NodeKind::List
                    || matches!(
                        source.kind,
                        NodeKind::Function {
                            function: Function::Collect,
                            ..
                        }
                    )
                {
                    for child in source.children() {
                        push(&mut pending, self.info(*child)?, self.resources, span)?;
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn assignment(&mut self, root: AstId) -> Result<(), ParseError> {
        let root = self.info(root)?.constant.unwrap_or(root);
        let root = self.ungroup(root)?;
        let node = node(self.ast, root)?;
        let info = self.info(root)?;
        require(info.kinds, super::patterns::property_kinds(), node.span)?;
        match node.kind {
            NodeKind::List => {
                let scalars = [
                    ValueKinds::BOOL,
                    ValueKinds::I64,
                    ValueKinds::F64,
                    ValueKinds::STRING,
                ];
                let mut possible = ValueKinds::BOOL
                    .union(ValueKinds::I64)
                    .union(ValueKinds::F64)
                    .union(ValueKinds::STRING);
                for id in node.children() {
                    poll(self.resources, node.span)?;
                    let current = self.info(*id)?.kinds;
                    let mut common = ValueKinds::default();
                    for scalar in scalars {
                        if possible.contains(scalar) && current.contains(scalar) {
                            common = common.union(scalar);
                        }
                    }
                    if common == ValueKinds::default() {
                        return Err(ParseError::new(
                            ErrorKind::Type,
                            node.span,
                            "property list requires homogeneous nonnull scalars",
                        ));
                    }
                    // Facts describe possible values. Nullable/computed scalars
                    // remain valid candidates; execution checks their actual
                    // nonnull homogeneous values before any durable mutation.
                    possible = common;
                }
            }
            NodeKind::Parameter(name) => {
                let name = self
                    .ast
                    .text(name)
                    .ok_or_else(|| error(node.span, "missing property parameter"))?;
                let value = self.parameter(name, node.span)?;
                if let QueryValue::List(list) = value {
                    let mut kind = None;
                    for index in 0..list.len() {
                        poll(self.resources, node.span)?;
                        let value = list
                            .get(index)
                            .ok_or_else(|| error(node.span, "missing list element"))?;
                        let current = match value {
                            QueryValue::Bool(_) => ValueKinds::BOOL,
                            QueryValue::I64(_) => ValueKinds::I64,
                            QueryValue::F64(_) => ValueKinds::F64,
                            QueryValue::String(_) => ValueKinds::STRING,
                            _ => {
                                return Err(ParseError::new(
                                    ErrorKind::Type,
                                    node.span,
                                    "property list requires homogeneous nonnull scalars",
                                ));
                            }
                        };
                        if kind.is_some_and(|prior| prior != current) {
                            return Err(ParseError::new(
                                ErrorKind::Type,
                                node.span,
                                "mixed property parameter list",
                            ));
                        }
                        kind = Some(current);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
