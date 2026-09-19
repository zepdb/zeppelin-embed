use super::*;
use zeppelin_embed::property_graph::query::{Arithmetic, Comparison, StringPredicate};
impl<'a> Binder<'a, '_> {
    pub(super) fn expression(&mut self, root: AstId) -> Result<Info, ParseError> {
        let mut pending = Vec::new();
        push(
            &mut pending,
            (root, false, 1_usize),
            self.resources,
            node(self.ast, root)?.span,
        )?;
        while let Some((id, finish, depth)) = pending.pop() {
            let node = node(self.ast, id)?;
            poll(self.resources, node.span)?;
            if depth > self.limits.depth {
                return Err(ParseError::new(
                    ErrorKind::Limit(LimitKind::Depth),
                    node.span,
                    "bound expression depth",
                ));
            }
            if self.facts.get(id.0).copied().flatten().is_some() {
                continue;
            }
            if !finish {
                if let Some(symbol) = self.projected_alias(id)? {
                    *self
                        .expressions
                        .get_mut(id.0)
                        .ok_or_else(|| error(node.span, "missing projected expression"))? =
                        Expression::Slot(symbol.slot);
                    *self
                        .facts
                        .get_mut(id.0)
                        .ok_or_else(|| error(node.span, "missing projected fact"))? =
                        Some(symbol.info);
                    continue;
                }
                push(&mut pending, (id, true, depth), self.resources, node.span)?;
                for child in node.children().iter().rev() {
                    if !matches!(super::node(self.ast, *child)?.kind, NodeKind::Name(_)) {
                        push(
                            &mut pending,
                            (*child, false, depth + 1),
                            self.resources,
                            node.span,
                        )?;
                    }
                }
                continue;
            }
            let mut row = false;
            let mut origin = None;
            let mut eligible = false;
            let mut invariant = None;
            let mut constant = if matches!(
                node.kind,
                NodeKind::Integer(_)
                    | NodeKind::Float(_)
                    | NodeKind::Boolean(_)
                    | NodeKind::Null
                    | NodeKind::String(_)
                    | NodeKind::Parameter(_)
                    | NodeKind::List
            ) {
                Some(id)
            } else {
                None
            };
            for child in node.children() {
                if let Some(info) = self.facts.get(child.0).copied().flatten() {
                    row |= info.row;
                }
            }
            let (expression, kinds) = match node.kind {
                NodeKind::Integer(v) => (Expression::Literal(Literal::I64(v)), ValueKinds::I64),
                NodeKind::Float(v) => (Expression::Literal(Literal::F64(v)), ValueKinds::F64),
                NodeKind::Boolean(v) => (Expression::Literal(Literal::Bool(v)), ValueKinds::BOOL),
                NodeKind::Null => (Expression::Literal(Literal::Null), ValueKinds::NULL),
                NodeKind::String(id) => (
                    Expression::Literal(Literal::String(
                        self.ast
                            .text(id)
                            .ok_or_else(|| error(node.span, "missing string"))?,
                    )),
                    ValueKinds::STRING,
                ),
                NodeKind::Variable(name) => {
                    let symbol = self.lookup(
                        self.ast
                            .text(name)
                            .ok_or_else(|| error(node.span, "missing variable name"))?,
                        node.span,
                    )?;
                    row = symbol.info.row;
                    origin = symbol.info.origin;
                    eligible = symbol.info.eligible;
                    constant = symbol.info.constant;
                    invariant = symbol.info.invariant;
                    (Expression::Slot(symbol.slot), symbol.info.kinds)
                }
                NodeKind::Parameter(name) => {
                    let name = self
                        .ast
                        .text(name)
                        .ok_or_else(|| error(node.span, "missing parameter name"))?;
                    let mut found = None;
                    for (index, parameter) in self.parameters.iter().enumerate() {
                        poll(self.resources, node.span)?;
                        if parameter.name == name {
                            found = Some((index, parameter.value));
                        }
                    }
                    let (index, value) = found.ok_or_else(|| {
                        ParseError::new(ErrorKind::Parameter, node.span, "missing parameter")
                    })?;
                    let kind = value_kind(value);
                    (
                        Expression::Parameter(ParameterId(
                            u32::try_from(index)
                                .map_err(|_| error(node.span, "parameter identity overflow"))?,
                        )),
                        kind,
                    )
                }
                NodeKind::Group => {
                    let child = child(node, 0)?;
                    origin = self.info(child)?.origin;
                    eligible = self.info(child)?.eligible;
                    constant = self.info(child)?.constant;
                    invariant = self.info(child)?.invariant;
                    (
                        *self
                            .expressions
                            .get(child.0)
                            .ok_or_else(|| error(node.span, "missing grouped expression"))?,
                        self.info(child)?.kinds,
                    )
                }
                NodeKind::Unary(op) => {
                    let operand = child(node, 0)?;
                    let kinds = self.info(operand)?.kinds;
                    let (operation, result) = match op {
                        UnaryOp::Not => {
                            require(kinds, ValueKinds::BOOL, node.span)?;
                            (UnaryExpression::Not, ValueKinds::BOOL)
                        }
                        UnaryOp::Plus | UnaryOp::Minus => {
                            require(kinds, ValueKinds::I64.union(ValueKinds::F64), node.span)?;
                            (
                                if op == UnaryOp::Plus {
                                    UnaryExpression::Positive
                                } else {
                                    UnaryExpression::Negate
                                },
                                {
                                    let mut numeric = ValueKinds::default();
                                    for kind in [ValueKinds::I64, ValueKinds::F64] {
                                        if kinds.contains(kind) {
                                            numeric = numeric.union(kind);
                                        }
                                    }
                                    numeric
                                },
                            )
                        }
                        UnaryOp::IsNull => (UnaryExpression::IsNull, ValueKinds::BOOL),
                        UnaryOp::IsNotNull => (UnaryExpression::IsNotNull, ValueKinds::BOOL),
                    };
                    (
                        Expression::Unary {
                            operation,
                            operand: expr_id(operand)?,
                        },
                        if !matches!(op, UnaryOp::IsNull | UnaryOp::IsNotNull)
                            && kinds.contains(ValueKinds::NULL)
                        {
                            result.union(ValueKinds::NULL)
                        } else {
                            result
                        },
                    )
                }
                NodeKind::Binary(op) => {
                    let left = child(node, 0)?;
                    let right = child(node, 1)?;
                    let a = self.info(left)?.kinds;
                    let b = self.info(right)?.kinds;
                    let (operation, result) = match op {
                        BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                            require(a, ValueKinds::BOOL, node.span)?;
                            require(b, ValueKinds::BOOL, node.span)?;
                            (
                                match op {
                                    BinaryOp::And => BinaryExpression::And,
                                    BinaryOp::Or => BinaryExpression::Or,
                                    _ => BinaryExpression::Xor,
                                },
                                ValueKinds::BOOL,
                            )
                        }
                        BinaryOp::Add
                        | BinaryOp::Subtract
                        | BinaryOp::Multiply
                        | BinaryOp::Divide
                        | BinaryOp::Remainder => {
                            require(a, ValueKinds::I64.union(ValueKinds::F64), node.span)?;
                            require(b, ValueKinds::I64.union(ValueKinds::F64), node.span)?;
                            let operation = match op {
                                BinaryOp::Add => Arithmetic::Add,
                                BinaryOp::Subtract => Arithmetic::Subtract,
                                BinaryOp::Multiply => Arithmetic::Multiply,
                                BinaryOp::Divide => Arithmetic::Divide,
                                _ => Arithmetic::Remainder,
                            };
                            let mut result = ValueKinds::default();
                            if a.contains(ValueKinds::I64) && b.contains(ValueKinds::I64) {
                                result = result.union(ValueKinds::I64);
                            }
                            if a.contains(ValueKinds::F64) || b.contains(ValueKinds::F64) {
                                result = result.union(ValueKinds::F64);
                            }
                            (BinaryExpression::Arithmetic(operation), result)
                        }
                        BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => {
                            require(a, ValueKinds::STRING, node.span)?;
                            require(b, ValueKinds::STRING, node.span)?;
                            (
                                BinaryExpression::String(match op {
                                    BinaryOp::StartsWith => StringPredicate::StartsWith,
                                    BinaryOp::EndsWith => StringPredicate::EndsWith,
                                    _ => StringPredicate::Contains,
                                }),
                                ValueKinds::BOOL,
                            )
                        }
                        BinaryOp::In => {
                            require(b, ValueKinds::LIST, node.span)?;
                            (
                                BinaryExpression::In,
                                ValueKinds::BOOL.union(ValueKinds::NULL),
                            )
                        }
                        _ => (
                            BinaryExpression::Comparison(match op {
                                BinaryOp::Eq => Comparison::Equal,
                                BinaryOp::Ne => Comparison::NotEqual,
                                BinaryOp::Lt => Comparison::Less,
                                BinaryOp::Le => Comparison::LessEqual,
                                BinaryOp::Gt => Comparison::Greater,
                                _ => Comparison::GreaterEqual,
                            }),
                            // Lists may contain nulls, and incomparable order
                            // operands also produce null in the shared core.
                            ValueKinds::BOOL.union(ValueKinds::NULL),
                        ),
                    };
                    let result = if a.contains(ValueKinds::NULL) || b.contains(ValueKinds::NULL) {
                        result.union(ValueKinds::NULL)
                    } else {
                        result
                    };
                    (
                        Expression::Binary {
                            operation,
                            left: expr_id(left)?,
                            right: expr_id(right)?,
                        },
                        result,
                    )
                }
                NodeKind::PropertyAccess(name) => {
                    let entity = child(node, 0)?;
                    self.check_deleted(self.info(entity)?, node.span)?;
                    require(
                        self.info(entity)?.kinds,
                        ValueKinds::NODE.union(ValueKinds::REL),
                        node.span,
                    )?;
                    (
                        Expression::Property {
                            entity: expr_id(entity)?,
                            name: self.graph_name(name, node.span)?,
                        },
                        super::patterns::property_kinds(),
                    )
                }
                NodeKind::Index => {
                    let list = child(node, 0)?;
                    let index = child(node, 1)?;
                    require(self.info(list)?.kinds, ValueKinds::LIST, node.span)?;
                    require(self.info(index)?.kinds, ValueKinds::I64, node.span)?;
                    (
                        Expression::Binary {
                            operation: BinaryExpression::Index,
                            left: expr_id(list)?,
                            right: expr_id(index)?,
                        },
                        ValueKinds::ANY,
                    )
                }
                NodeKind::LabelPredicate => {
                    let entity = child(node, 0)?;
                    self.check_deleted(self.info(entity)?, node.span)?;
                    let kinds = self.info(entity)?.kinds;
                    require(kinds, ValueKinds::NODE, node.span)?;
                    let mut prior = None;
                    for label_id in node.children().iter().skip(1) {
                        poll(self.resources, node.span)?;
                        let label = super::node(self.ast, *label_id)?;
                        let NodeKind::Name(name) = label.kind else {
                            return Err(error(label.span, "missing predicate label"));
                        };
                        let expression = Expression::HasLabel {
                            entity: expr_id(entity)?,
                            label: self.graph_name(name, label.span)?,
                        };
                        *self
                            .expressions
                            .get_mut(label_id.0)
                            .ok_or_else(|| error(label.span, "missing label expression"))? =
                            expression;
                        let current = expr_id(*label_id)?;
                        prior = Some(if let Some(left) = prior {
                            self.append_expression(
                                Expression::Binary {
                                    operation: BinaryExpression::And,
                                    left,
                                    right: current,
                                },
                                node.span,
                            )?
                        } else {
                            current
                        });
                    }
                    let root = prior.ok_or_else(|| error(node.span, "empty label predicate"))?;
                    (
                        *self
                            .expressions
                            .get(root.0 as usize)
                            .ok_or_else(|| error(node.span, "missing label predicate"))?,
                        if kinds.contains(ValueKinds::NULL) {
                            ValueKinds::BOOL.union(ValueKinds::NULL)
                        } else {
                            ValueKinds::BOOL
                        },
                    )
                }
                NodeKind::Function { function, distinct } => {
                    let operand = node.children().first().copied();
                    let kinds = operand
                        .map(|id| self.info(id).map(|info| info.kinds))
                        .transpose()?
                        .unwrap_or_default();
                    match function {
                        Function::Count | Function::Collect => {
                            if function == Function::Collect {
                                constant = Some(id);
                            }
                            eligible = function == Function::Collect
                                && distinct
                                && kinds.contains(ValueKinds::NODE);
                            let operation = if function == Function::Count {
                                AggregateExpression::Count { distinct }
                            } else {
                                AggregateExpression::Collect { distinct }
                            };
                            (
                                Expression::Aggregate {
                                    operation,
                                    operand: operand.map(expr_id).transpose()?,
                                },
                                if function == Function::Count {
                                    ValueKinds::I64
                                } else {
                                    ValueKinds::LIST
                                },
                            )
                        }
                        _ => {
                            let operand = operand
                                .ok_or_else(|| error(node.span, "missing function operand"))?;
                            if matches!(
                                function,
                                Function::Labels | Function::Type | Function::StoredText
                            ) {
                                self.check_deleted(self.info(operand)?, node.span)?;
                            } else if self.has_deletions {
                                self.runtime_deleted_checks = true;
                            }
                            let (operation, allowed, result) = match function {
                                Function::Labels => {
                                    (UnaryExpression::Labels, ValueKinds::NODE, ValueKinds::LIST)
                                }
                                Function::Type => (
                                    UnaryExpression::RelType,
                                    ValueKinds::REL,
                                    ValueKinds::STRING,
                                ),
                                Function::Size => (
                                    UnaryExpression::Size,
                                    ValueKinds::LIST.union(ValueKinds::STRING),
                                    ValueKinds::I64,
                                ),
                                Function::NodeId => (
                                    UnaryExpression::NodeIdText,
                                    ValueKinds::NODE,
                                    ValueKinds::STRING,
                                ),
                                Function::RelationshipId => (
                                    UnaryExpression::RelIdText,
                                    ValueKinds::REL,
                                    ValueKinds::STRING,
                                ),
                                Function::StoredText => (
                                    UnaryExpression::StoredText,
                                    ValueKinds::NODE,
                                    ValueKinds::STRING.union(ValueKinds::NULL),
                                ),
                                _ => return Err(error(node.span, "invalid scalar function")),
                            };
                            require(kinds, allowed, node.span)?;
                            (
                                Expression::Unary {
                                    operation,
                                    operand: expr_id(operand)?,
                                },
                                if kinds.contains(ValueKinds::NULL) {
                                    result.union(ValueKinds::NULL)
                                } else {
                                    result
                                },
                            )
                        }
                    }
                }
                NodeKind::List => (
                    Expression::List(
                        self.links
                            .get(id.0)
                            .ok_or_else(|| error(node.span, "missing list operands"))?,
                    ),
                    ValueKinds::LIST,
                ),
                _ => return Err(error(node.span, "binding expression not implemented")),
            };
            if invariant.is_none() && !row {
                invariant = match expression {
                    Expression::Literal(_) | Expression::Parameter(_) => Some(id),
                    Expression::Unary { operand, .. } => self
                        .info(AstId(usize::try_from(operand.0).map_err(|_| {
                            error(node.span, "invariant operand identity overflow")
                        })?))?
                        .invariant
                        .map(|_| id),
                    Expression::Binary { left, right, .. } => {
                        let left = self.info(AstId(usize::try_from(left.0).map_err(|_| {
                            error(node.span, "invariant left identity overflow")
                        })?))?;
                        let right =
                            self.info(AstId(usize::try_from(right.0).map_err(|_| {
                                error(node.span, "invariant right identity overflow")
                            })?))?;
                        (left.invariant.is_some() && right.invariant.is_some()).then_some(id)
                    }
                    Expression::List(items) => items
                        .iter()
                        .try_fold(true, |all, child| {
                            let child = AstId(usize::try_from(child.0).map_err(|_| {
                                error(node.span, "invariant list identity overflow")
                            })?);
                            Ok::<_, ParseError>(all && self.info(child)?.invariant.is_some())
                        })?
                        .then_some(id),
                    Expression::Aggregate { .. }
                    | Expression::Slot(_)
                    | Expression::Property { .. }
                    | Expression::HasLabel { .. } => None,
                };
            }
            *self
                .expressions
                .get_mut(id.0)
                .ok_or_else(|| error(node.span, "missing expression storage"))? = expression;
            *self
                .facts
                .get_mut(id.0)
                .ok_or_else(|| error(node.span, "missing fact storage"))? = Some(Info {
                kinds,
                row,
                origin,
                eligible,
                constant,
                invariant,
            });
        }
        self.info(root)
    }
    pub(super) fn info(&self, id: AstId) -> Result<Info, ParseError> {
        self.facts
            .get(id.0)
            .copied()
            .flatten()
            .ok_or_else(|| error(Span::default(), "missing expression fact"))
    }
}
fn value_kind(value: QueryValue<'_>) -> ValueKinds {
    match value {
        QueryValue::Null => ValueKinds::NULL,
        QueryValue::Bool(_) => ValueKinds::BOOL,
        QueryValue::I64(_) => ValueKinds::I64,
        QueryValue::F64(_) => ValueKinds::F64,
        QueryValue::String(_) => ValueKinds::STRING,
        QueryValue::List(_) => ValueKinds::LIST,
        QueryValue::NodeRef(_) => ValueKinds::NODE,
        QueryValue::RelRef(_) => ValueKinds::REL,
    }
}
