//! Bind graph node records and adopt implicit document endpoints in the same batch.
use super::*;
use memory::Arena;

impl<'a> StagedBatch<'a> {
    pub(super) fn bind_document_nodes(
        mut self,
        base: &dyn AdmittedBase,
        memory: &'a WriteMemory<'a>,
        control: &mut WriteControl<'_>,
    ) -> Result<Self, StageError> {
        let capacity = self
            .deltas
            .iter()
            .filter(|delta| matches!(delta.shape(), Some(EntityShape::Relationship { .. })))
            .count()
            .checked_mul(2)
            .ok_or(StageError::Limit)?;
        let mut endpoints = Arena::new(memory, capacity, control)?;
        for delta in self.deltas.iter() {
            if let Some(EntityShape::Relationship { source, target, .. }) = delta.shape() {
                for node in [source, target] {
                    if !endpoints.contains(&node)
                        && !base.has_node_record(node, control)?
                        && !self.deltas.iter().any(|delta| {
                            delta.provenance().fields().incarnation == EntityId::Node(node)
                        })
                        && base.document_version(node)?.is_some()
                    {
                        endpoints.push(node)?;
                    }
                }
            }
        }
        for delta in self.deltas.as_mut_slice() {
            if let EntityId::Node(node) = delta.provenance().fields().incarnation {
                delta.document = base.document_version(node)?;
            }
        }
        if endpoints.is_empty() {
            return Ok(self);
        }
        let count = self
            .deltas
            .len()
            .checked_add(endpoints.len())
            .ok_or(StageError::Limit)?;
        if count > memory.limits.changes {
            return Err(StageError::Limit);
        }
        let mut deltas = Arena::new(memory, count, control)?;
        for node in endpoints.iter().copied() {
            let version = base.document_version(node)?.ok_or(StageError::Endpoint)?;
            let revision = GraphRevision::new(1).map_err(|_| StageError::InvalidInput)?;
            let contents = CanonicalContents::node(&mut [], &mut [], None, None)?;
            let mut canonical = Arena::new(memory, contents.encoded_len() as usize, control)?;
            contents.write_to(&mut canonical, &mut || structured::canonical_poll(control))?;
            let provenance = OperationProvenance::from_fields_with_control(
                Some(1),
                OperationFields {
                    operation: GraphOperation::CypherEdit,
                    key: None,
                    requested_revision: revision,
                    installed_revision: revision,
                    expected: ExpectedGraphState::Absent,
                    incarnation: EntityId::Node(node),
                    delete_mode: None,
                    original_generation: self.target_generation,
                },
                &mut || structured::canonical_poll(control),
            )?;
            deltas.push(NormalizedDelta {
                document: Some(version),
                provenance,
                canonical: Some(canonical),
                shape: Some(EntityShape::Node),
                before: Membership::default(),
                after: Membership::default(),
            })?;
        }
        for delta in self.deltas.drain() {
            deltas.push(delta)?;
        }
        self.deltas = deltas;
        Ok(self)
    }
}

impl<'a> StagedBatch<'a> {
    pub(super) fn bind_updated_documents(
        &mut self,
        base: &'a dyn AdmittedBase,
        versions: &[crate::ingest::DocumentVersion],
        memory: &'a WriteMemory<'a>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        if versions.is_empty() {
            return Ok(());
        }
        let count = self
            .deltas
            .len()
            .checked_add(versions.len())
            .ok_or(StageError::Limit)?;
        if count > memory.limits.changes {
            return Err(StageError::Limit);
        }
        let mut deltas = Arena::new(memory, count, control)?;
        for mut delta in self.deltas.drain() {
            if let EntityId::Node(node) = delta.provenance.fields().incarnation
                && let Some(version) = versions
                    .iter()
                    .find(|version| version.doc_id().get() == node.get())
            {
                if delta.shape.is_none() {
                    return Err(StageError::InvalidInput);
                }
                delta.document = Some(*version);
            }
            deltas.push(delta)?;
        }
        for version in versions {
            let node = NodeId::from(version.doc_id());
            if deltas
                .iter()
                .any(|delta| delta.provenance.fields().incarnation == EntityId::Node(node))
            {
                continue;
            }
            let Some(entity) = base.entity(EntityId::Node(node), control)? else {
                continue;
            };
            let old = entity.provenance.fields();
            let revision = old
                .installed_revision
                .checked_next()
                .map_err(|_| StageError::IdentityOverflow)?;
            let provenance = OperationProvenance::from_fields_with_control(
                Some(1),
                OperationFields {
                    operation: GraphOperation::CypherEdit,
                    key: old.key,
                    requested_revision: revision,
                    installed_revision: revision,
                    expected: ExpectedGraphState::Entity(EntityId::Node(node)),
                    incarnation: EntityId::Node(node),
                    delete_mode: None,
                    original_generation: self.target_generation,
                },
                &mut || structured::canonical_poll(control),
            )?;
            let length =
                usize::try_from(entity.fingerprint.bytes()).map_err(|_| StageError::Limit)?;
            if length > memory.limits.input_bytes {
                return Err(StageError::Limit);
            }
            let mut canonical = Arena::new(memory, length, control)?;
            let mut offset = 0usize;
            let mut chunk = [0u8; 4096];
            while offset < length {
                control(WritePhase::Canonical)?;
                let capacity = (length - offset).min(chunk.len());
                let count = entity
                    .source
                    .read_at(
                        offset as u64,
                        chunk.get_mut(..capacity).ok_or(StageError::InvalidInput)?,
                    )
                    .map_err(CanonicalError::Io)?;
                if count == 0 || count > capacity {
                    return Err(StageError::InvalidInput);
                }
                for byte in chunk.get(..count).ok_or(StageError::InvalidInput)? {
                    canonical.push(*byte)?;
                }
                offset = offset.checked_add(count).ok_or(StageError::Limit)?;
            }
            deltas.push(NormalizedDelta {
                document: Some(*version),
                provenance,
                canonical: Some(canonical),
                shape: Some(EntityShape::Node),
                before: entity.membership,
                after: entity.membership,
            })?;
            self.disposition = BatchDisposition::Changed;
        }
        self.deltas = deltas;
        Ok(())
    }
}
