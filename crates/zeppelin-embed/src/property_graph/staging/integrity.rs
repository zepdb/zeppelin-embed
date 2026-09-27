//! Shared pre-publication referential integrity for structured and Cypher writes.
use super::*;
use catalog::OnDelete;
use memory::Arena;

impl<'a> StagedBatch<'a> {
    pub(super) fn enforce_relationship_rules(
        mut self,
        base: &'a dyn AdmittedBase,
        memory: &'a WriteMemory<'a>,
        control: &mut WriteControl<'_>,
    ) -> Result<Self, StageError> {
        if !base.has_relationship_rules()
            || !self.deltas.iter().any(|d| {
                matches!(d.provenance.fields().incarnation, EntityId::Node(_)) && d.shape.is_none()
            })
        {
            return Ok(self);
        }
        let mut deleted = Arena::new(memory, memory.limits.changes, control)?;
        for delta in &*self.deltas {
            control(WritePhase::Incident)?;
            if delta.shape.is_none()
                && let EntityId::Node(id) = delta.provenance.fields().incarnation
            {
                deleted.push(id)?;
            }
        }
        let explicit = deleted.len();
        let removed = |rel: RelId| {
            self.deltas.iter().any(|d| {
                d.provenance.fields().incarnation == EntityId::Relationship(rel)
                    && d.shape.is_none()
            })
        };
        let mut cursor = 0usize;
        while let Some(node) = deleted.get(cursor).copied() {
            control(WritePhase::Incident)?;
            base.visit_incoming_rules(
                node,
                &mut |rel, child, policy| {
                    if policy == OnDelete::Cascade && !removed(rel) && !deleted.contains(&child) {
                        deleted.push(child)?;
                    }
                    Ok(())
                },
                control,
            )?;
            cursor += 1;
        }
        // Restrict is checked against the complete closure, independent of
        // edge traversal order. Explicit edge removal releases the dependency.
        for node in &*deleted {
            base.visit_incoming_rules(
                *node,
                &mut |rel, child, policy| {
                    if policy == OnDelete::Restrict && !removed(rel) && !deleted.contains(&child) {
                        return Err(StageError::IncidentRelationship);
                    }
                    Ok(())
                },
                control,
            )?;
        }
        for delta in &*self.deltas {
            control(WritePhase::Incident)?;
            match delta.shape {
                Some(EntityShape::Relationship { source, target, .. })
                    if deleted.contains(&source) || deleted.contains(&target) =>
                {
                    return Err(StageError::Endpoint);
                }
                Some(EntityShape::Node) if matches!(delta.provenance.fields().incarnation, EntityId::Node(id) if deleted.contains(&id)) =>
                {
                    return Err(StageError::DeletedEntity);
                }
                _ => {}
            }
        }
        let additional = deleted
            .len()
            .checked_sub(explicit)
            .ok_or(StageError::InvalidInput)?;
        let count = self
            .deltas
            .len()
            .checked_add(additional)
            .ok_or(StageError::Limit)?;
        if count > memory.limits.changes {
            return Err(StageError::Limit);
        }
        if additional == 0 {
            return Ok(self);
        }
        let generation = GraphGeneration::new(
            self.base
                .generation
                .get()
                .checked_add(1)
                .ok_or(KeyLifecycleError::GenerationOverflow)?,
        );
        let mut deltas = Arena::new(memory, count, control)?;
        for delta in self.deltas.drain() {
            deltas.push(delta)?;
        }
        for id in deleted.iter().skip(explicit) {
            control(WritePhase::Incident)?;
            let entity = base
                .entity(EntityId::Node(*id), control)?
                .ok_or(StageError::Endpoint)?;
            structured::checked_base(&entity, self.base, self.high_waters)?;
            let fields = entity.provenance.fields();
            let revision = fields
                .installed_revision
                .checked_next()
                .map_err(|_| KeyLifecycleError::RevisionOverflow)?;
            // Implicit deletes have no caller retry token. They use the same
            // normalized edit provenance and revision rule as statement edits.
            let provenance = OperationProvenance::from_fields_with_control(
                Some(1),
                OperationFields {
                    operation: GraphOperation::CypherEdit,
                    key: fields.key,
                    requested_revision: revision,
                    installed_revision: revision,
                    expected: ExpectedGraphState::Entity(fields.incarnation),
                    incarnation: fields.incarnation,
                    delete_mode: Some(GraphDeleteMode::Detach),
                    original_generation: generation,
                },
                &mut || structured::canonical_poll(control),
            )?;
            deltas.push(NormalizedDelta {
                provenance,
                canonical: None,
                shape: None,
                before: entity.membership,
                after: Membership::default(),
            })?;
        }
        self.deltas = deltas;
        Ok(self)
    }
}
