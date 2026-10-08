use super::*;

/// Scoped complete mutation plan and its compiler metadata. The one-shot HRTB
/// consumer prevents the plan, facts, source maps, and owners from escaping.
pub struct LoweredMutation<'plan, 'facts> {
    common: LoweredRead<'plan, 'facts>,
    mutation_spans: &'plan [Span],
    requires_deleted_runtime_validation: bool,
}

impl<'p, 'f> LoweredMutation<'p, 'f> {
    pub fn plan(&self) -> &GraphPlan<'p, 'f> {
        self.common.plan()
    }

    pub fn columns(&self) -> &[ReadColumn<'p>] {
        self.common.columns()
    }

    pub fn source(&self) -> &'p str {
        self.common.source()
    }

    pub fn expression_spans(&self) -> &'p [Span] {
        self.common.expression_spans()
    }

    pub fn operator_spans(&self) -> &'p [Span] {
        self.common.operator_spans()
    }

    pub fn parameters(&self) -> &'p [ParameterBinding<'p>] {
        self.common.parameters()
    }

    pub fn owners(&self) -> &[RetainedAllocation<'p>] {
        self.common.owners()
    }

    /// Item spans parallel the flattened mutation descriptors in ascending
    /// Mutate operator order and then source item order.
    pub fn mutation_spans(&self) -> &'p [Span] {
        self.mutation_spans
    }

    /// Additional compiler-identified dynamic deleted-entity validation.
    /// False is not a live-entity certificate for a physical provider.
    pub fn requires_deleted_runtime_validation(&self) -> bool {
        self.requires_deleted_runtime_validation
    }
}

/// One-shot mutation lowerer. This constructs and validates plan data only;
/// it performs no writer admission, staging, identity allocation, or execution.
///
/// ```
/// use zeppelin_embed::property_graph::query::{ValueContext, resources::QueryMemory};
/// use zeppelin_embed_cypher::{CompileLimits, ParseError, compile_mutation_in};
/// fn compile_and_consume<'v>(memory: &QueryMemory<'_>, context: &mut ValueContext<'v>)
///     -> Result<(), ParseError>
/// {
///     compile_mutation_in("CREATE (n) RETURN n", &[], CompileLimits::default(), memory, context,
///         |mutation, _| {
///             let _description = mutation.plan().description();
///             Ok(())
///         })
/// }
/// ```
///
/// ```compile_fail
/// use zeppelin_embed::property_graph::query::{ValueContext, resources::QueryMemory};
/// use zeppelin_embed_cypher::{CompileLimits, LoweredMutation, ParseError, compile_mutation_in};
/// fn escape<'v>(memory: &QueryMemory<'_>, context: &mut ValueContext<'v>)
///     -> Result<LoweredMutation<'static, 'static>, ParseError>
/// {
///     compile_mutation_in("CREATE (n)", &[], CompileLimits::default(), memory, context,
///         |mutation, _| Ok(mutation))
/// }
/// ```
///
/// ```compile_fail
/// use zeppelin_embed::property_graph::query::{ValueContext, resources::QueryMemory};
/// use zeppelin_embed::property_graph::query::plan::PlanDescription;
/// use zeppelin_embed_cypher::{CompileLimits, ParseError, compile_mutation_in};
/// fn escape<'a, 'v>(memory: &QueryMemory<'_>, context: &mut ValueContext<'v>)
///     -> Result<PlanDescription<'a>, ParseError>
/// {
///     compile_mutation_in("CREATE (n)", &[], CompileLimits::default(), memory, context,
///         |mutation, _| Ok(mutation.plan().description()))
/// }
/// ```
///
/// ```compile_fail
/// use zeppelin_embed::property_graph::query::{ValueContext, resources::QueryMemory};
/// use zeppelin_embed_cypher::{CompileLimits, ParseError, Span, compile_mutation_in};
/// fn escape<'a, 'v>(memory: &QueryMemory<'_>, context: &mut ValueContext<'v>)
///     -> Result<&'a [Span], ParseError>
/// {
///     compile_mutation_in("CREATE (n)", &[], CompileLimits::default(), memory, context,
///         |mutation, _| Ok(mutation.mutation_spans()))
/// }
/// ```
///
/// ```compile_fail
/// use zeppelin_embed::property_graph::query::{ValueContext, resources::{QueryMemory, RetainedAllocation}};
/// use zeppelin_embed_cypher::{CompileLimits, ParseError, compile_mutation_in};
/// fn escape<'a, 'v>(memory: &QueryMemory<'_>, context: &mut ValueContext<'v>)
///     -> Result<&'a [RetainedAllocation<'a>], ParseError>
/// {
///     compile_mutation_in("CREATE (n)", &[], CompileLimits::default(), memory, context,
///         |mutation, _| Ok(mutation.owners()))
/// }
/// ```
pub fn compile_mutation_in<'v, T, C: ReadContext<'v>>(
    source: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
    memory: &QueryMemory<'_>,
    context: &mut C,
    consume: impl for<'plan, 'facts> FnOnce(
        LoweredMutation<'plan, 'facts>,
        &mut C,
    ) -> Result<T, ParseError>,
) -> Result<T, ParseError> {
    compile_route_in(
        source,
        parameters,
        limits,
        memory,
        context,
        Route::Mutation,
        Default::default(),
        |common, mutation_spans, requires_deleted_runtime_validation, context| {
            consume(
                LoweredMutation {
                    common,
                    mutation_spans,
                    requires_deleted_runtime_validation,
                },
                context,
            )
        },
    )
}

impl Builder<'_, '_, '_> {
    pub(super) fn mutation(
        &mut self,
        bound: &BoundQuery<'_>,
        clause: &Node,
        current: PlanNodeId,
    ) -> Result<PlanNodeId, ParseError> {
        let eager = self.operator(DraftOp::Eager, &[current], clause.span)?;
        let start = self.mutations.len();
        match clause.kind {
            NodeKind::Create => self.create(bound, clause)?,
            NodeKind::Set | NodeKind::Remove | NodeKind::Delete { .. } => {
                self.update(bound, clause)?
            }
            _ => return Err(invariant(clause.span, "mutation clause")),
        }
        let len = self
            .mutations
            .len()
            .checked_sub(start)
            .ok_or_else(|| invariant(clause.span, "mutation range"))?;
        if len == 0 {
            return Err(invariant(clause.span, "empty mutation clause"));
        }
        self.operator(DraftOp::Mutate(Range { start, len }), &[eager], clause.span)
    }

    fn create(&mut self, bound: &BoundQuery<'_>, clause: &Node) -> Result<(), ParseError> {
        for pattern_id in clause.children() {
            let pattern = syntax(bound, *pattern_id)?;
            if pattern.kind != NodeKind::Pattern {
                return Err(invariant(pattern.span, "CREATE pattern"));
            }
            let first_id = child(pattern, 0)?;
            let first = syntax(bound, first_id)?;
            self.ensure_created_node(bound, first_id, first)?;
            self.inline_properties(bound, first_id, first)?;

            let mut left_id = first_id;
            let mut index = 1usize;
            while index < pattern.children().len() {
                let relationship_id = child(pattern, index)?;
                let right_id = child(
                    pattern,
                    index.checked_add(1).ok_or_else(|| limit(pattern.span))?,
                )?;
                let relationship = syntax(bound, relationship_id)?;
                let right = syntax(bound, right_id)?;
                self.ensure_created_node(bound, right_id, right)?;
                self.create_relationship(bound, left_id, relationship_id, right_id, relationship)?;
                self.inline_properties(bound, relationship_id, relationship)?;
                self.inline_properties(bound, right_id, right)?;
                left_id = right_id;
                index = index.checked_add(2).ok_or_else(|| limit(pattern.span))?;
            }
        }
        Ok(())
    }

    fn update(&mut self, bound: &BoundQuery<'_>, clause: &Node) -> Result<(), ParseError> {
        for item_id in clause.children() {
            let item = syntax(bound, *item_id)?;
            let entity = self.source_expression(bound, *item_id, item.span)?;
            match item.kind {
                NodeKind::SetProperty { property, .. } => {
                    let name = self.copy_symbol(bound, property, item.span, "SET property")?;
                    let value_id = child(item, 0)?;
                    let value = self.source_expression(bound, value_id, item.span)?;
                    self.push_mutation(
                        DraftMutation::SetProperty {
                            entity,
                            name,
                            value,
                        },
                        item.span,
                    )?;
                }
                NodeKind::RemoveProperty { property, .. } => {
                    let name = self.copy_symbol(bound, property, item.span, "REMOVE property")?;
                    self.push_mutation(DraftMutation::RemoveProperty { entity, name }, item.span)?;
                }
                NodeKind::SetLabels { .. } | NodeKind::RemoveLabels { .. } => {
                    let present = matches!(item.kind, NodeKind::SetLabels { .. });
                    for label_id in item.children() {
                        let label = syntax(bound, *label_id)?;
                        let NodeKind::Name(name) = label.kind else {
                            return Err(invariant(label.span, "updated label"));
                        };
                        let label = self.copy_symbol(bound, name, label.span, "updated label")?;
                        self.push_mutation(
                            DraftMutation::SetLabel {
                                entity,
                                label,
                                present,
                            },
                            item.span,
                        )?;
                    }
                }
                NodeKind::Variable(_) => {
                    let NodeKind::Delete { detach } = clause.kind else {
                        return Err(invariant(item.span, "DELETE item"));
                    };
                    self.push_mutation(DraftMutation::Delete { entity, detach }, item.span)?;
                }
                _ => return Err(invariant(item.span, "mutation item")),
            }
        }
        Ok(())
    }

    fn ensure_created_node(
        &mut self,
        bound: &BoundQuery<'_>,
        id: AstId,
        node: &Node,
    ) -> Result<(), ParseError> {
        let NodeKind::NodePattern { .. } = node.kind else {
            return Err(invariant(node.span, "CREATE node"));
        };
        let output = slot(bound, id)?;
        if self.mutation_scoped(output) {
            return Ok(());
        }
        let start = self.names.len();
        for detail_id in node.children() {
            let detail = syntax(bound, *detail_id)?;
            if let NodeKind::Name(name) = detail.kind {
                let name = self.copy_symbol(bound, name, detail.span, "CREATE label")?;
                self.names.push(name, self.memory, self.control)?;
            }
        }
        let len = self
            .names
            .len()
            .checked_sub(start)
            .ok_or_else(|| invariant(node.span, "CREATE labels"))?;
        self.push_mutation(
            DraftMutation::CreateNode {
                output,
                labels: Range { start, len },
            },
            node.span,
        )?;
        self.add_mutation_scope(output, node.span)
    }

    fn create_relationship(
        &mut self,
        bound: &BoundQuery<'_>,
        left_id: AstId,
        relationship_id: AstId,
        right_id: AstId,
        relationship: &Node,
    ) -> Result<(), ParseError> {
        let NodeKind::RelationshipPattern {
            direction,
            bounds: None,
            ..
        } = relationship.kind
        else {
            return Err(invariant(relationship.span, "CREATE relationship"));
        };
        let output = slot(bound, relationship_id)?;
        if self.mutation_scoped(output) {
            return Err(invariant(relationship.span, "reused CREATE relationship"));
        }
        let mut relationship_type = None;
        for detail_id in relationship.children() {
            let detail = syntax(bound, *detail_id)?;
            if let NodeKind::Name(name) = detail.kind {
                if relationship_type.is_some() {
                    return Err(invariant(detail.span, "multiple CREATE relationship types"));
                }
                relationship_type =
                    Some(self.copy_symbol(bound, name, detail.span, "CREATE relationship type")?);
            }
        }
        let relationship_type = relationship_type
            .ok_or_else(|| invariant(relationship.span, "missing CREATE relationship type"))?;
        let left = self.source_expression(bound, left_id, relationship.span)?;
        let right = self.source_expression(bound, right_id, relationship.span)?;
        let (source, target) = match direction {
            crate::Direction::Outgoing => (left, right),
            crate::Direction::Incoming => (right, left),
            crate::Direction::Both => {
                return Err(invariant(
                    relationship.span,
                    "undirected CREATE relationship",
                ));
            }
        };
        self.push_mutation(
            DraftMutation::CreateRelationship {
                output,
                source,
                target,
                relationship_type,
            },
            relationship.span,
        )?;
        self.add_mutation_scope(output, relationship.span)
    }

    fn inline_properties(
        &mut self,
        bound: &BoundQuery<'_>,
        entity_id: AstId,
        entity_node: &Node,
    ) -> Result<(), ParseError> {
        for detail_id in entity_node.children() {
            let detail = syntax(bound, *detail_id)?;
            if detail.kind != NodeKind::Properties {
                continue;
            }
            for property_id in detail.children() {
                let property = syntax(bound, *property_id)?;
                let NodeKind::Property(name) = property.kind else {
                    return Err(invariant(property.span, "CREATE property"));
                };
                let entity = self.source_expression(bound, entity_id, entity_node.span)?;
                let name = self.copy_symbol(bound, name, property.span, "CREATE property name")?;
                let value_id = child(property, 0)?;
                let value = self.source_expression(bound, value_id, property.span)?;
                self.push_mutation(
                    DraftMutation::SetProperty {
                        entity,
                        name,
                        value,
                    },
                    property.span,
                )?;
            }
        }
        Ok(())
    }

    fn source_expression(
        &mut self,
        bound: &BoundQuery<'_>,
        id: AstId,
        span: Span,
    ) -> Result<ExprId, ParseError> {
        let id = u32::try_from(id.0)
            .map(ExprId)
            .map_err(|_| invariant(span, "source expression identity"))?;
        self.lower_expression(bound, id, span)
    }

    fn copy_symbol(
        &mut self,
        bound: &BoundQuery<'_>,
        id: TextId,
        span: Span,
        message: &'static str,
    ) -> Result<Range, ParseError> {
        let value = bound
            .syntax()
            .text(id)
            .ok_or_else(|| invariant(span, message))?;
        self.copy_text(value)
    }

    fn mutation_scoped(&self, slot: SlotId) -> bool {
        self.scope.slice().contains(&slot)
    }

    fn add_mutation_scope(&mut self, slot: SlotId, span: Span) -> Result<(), ParseError> {
        if !self.mutation_scoped(slot) {
            if self.scope.len() >= MAX_COLUMNS {
                return Err(limit(span));
            }
            self.scope.push(slot, self.memory, self.control)?;
        }
        Ok(())
    }

    fn push_mutation(&mut self, item: DraftMutation, span: Span) -> Result<(), ParseError> {
        if self.mutations.len() >= MAX_PLAN_NODES {
            return Err(limit(span));
        }
        self.mutations.push(item, self.memory, self.control)?;
        self.mutation_spans.push(span, self.memory, self.control)?;
        Ok(())
    }
}
