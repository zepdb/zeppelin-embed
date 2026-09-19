use super::*;
use memory::Arena;
/// Already-bound entity or branded statement-local declaration; never a scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchEntityRef<'batch> {
    /// Existing or local node binding.
    Node(NodeRef<'batch>),
    /// Existing or local relationship binding.
    Relationship(RelRef<'batch>),
}
impl BatchEntityRef<'_> {
    fn existing(self) -> Option<EntityId> {
        match self {
            Self::Node(NodeRef::Existing(id)) => Some(EntityId::Node(id)),
            Self::Relationship(RelRef::Existing(id)) => Some(EntityId::Relationship(id)),
            _ => None,
        }
    }
    fn kind(self) -> EntityKind {
        match self {
            Self::Node(_) => EntityKind::Node,
            Self::Relationship(_) => EntityKind::Relationship,
        }
    }
}
/// Deterministic work observations, not memory-bandwidth or global side effects.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OverlayCounters {
    /// Property accesses actually requested.
    pub property_lookups: u64,
    /// Stored-text accesses actually requested.
    pub text_lookups: u64,
    /// Pending entity/property descriptors examined by accessors.
    pub descriptors_examined: u64,
    /// Logical value/text bytes returned through bounded borrowed spans.
    pub value_bytes: u64,
}
#[derive(Clone, Copy)]
struct Entry<'a, 'batch> {
    target: BatchEntityRef<'batch>,
    image: Option<WriteImage<'a, 'batch>>,
    deleted: Option<GraphDeleteMode>,
}
/// Progressive private property/text overlay. Upstream bindings remain frozen;
/// this interface cannot scan, traverse adjacency or introduce new bindings.
pub struct GraphBatchReadView<'a, 'batch> {
    base: &'a dyn AdmittedBase,
    identity: BaseIdentity,
    memory: &'a WriteMemory<'a>,
    entries: Arena<'a, Entry<'a, 'batch>>,
    counters: OverlayCounters,
}
impl<'a, 'batch> GraphBatchReadView<'a, 'batch> {
    /// Reserves only bounded pending descriptors, borrowing the retained base.
    pub fn new(
        base: &'a dyn AdmittedBase,
        memory: &'a WriteMemory<'a>,
        capacity: usize,
        control: &mut WriteControl<'_>,
    ) -> Result<Self, StageError> {
        if capacity > memory.limits.changes {
            return Err(StageError::Limit);
        }
        control(WritePhase::Overlay)?;
        if base.identity().generation.get() != 0 && base.identity().roots.is_none() {
            return Err(StageError::ViewMismatch);
        }
        Ok(Self {
            base,
            identity: base.identity(),
            memory,
            entries: Arena::new(memory, capacity, control)?,
            counters: OverlayCounters::default(),
        })
    }
    /// Declares a new unkeyed node/relationship using an already frozen local
    /// binding. No global ID is assigned until final private normalization.
    pub fn create(
        &mut self,
        target: BatchEntityRef<'batch>,
        image: WriteImage<'a, 'batch>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        control(WritePhase::Overlay)?;
        if target.existing().is_some() {
            return Err(StageError::InvalidInput);
        }
        let kind = match image {
            WriteImage::Node(image) => image.shape().kind(),
            WriteImage::Relationship { .. } => EntityKind::Relationship,
        };
        if kind != target.kind() {
            return Err(StageError::InvalidInput);
        }
        for entry in &*self.entries {
            control(WritePhase::Overlay)?;
            if entry.target == target {
                return Err(StageError::Lifecycle(KeyLifecycleError::DuplicateTarget));
            }
        }
        self.entries.push(Entry {
            target,
            image: Some(image),
            deleted: None,
        })
    }
    /// Finalizes one Cypher statement: equal final images are NoOp, changed
    /// existing entities advance once, and consumed local IDs remain fenced.
    pub fn finish(self, control: &mut WriteControl<'_>) -> Result<StagedBatch<'a>, StageError> {
        self.finalize(control, &mut |_, _| Ok(()))
    }
    /// Normalizes and materializes complete bounded outputs before handoff,
    /// including NoOp statements. Failure releases all private capacity.
    pub fn finish_with_results<M: ResultMaterializer>(
        self,
        materializer: &mut M,
        control: &mut WriteControl<'_>,
    ) -> Result<MaterializedBatch<'a, M::Registration>, StageError> {
        let base = self.base;
        let memory = self.memory;
        let mut layout = None;
        let batch = self.finalize(control, &mut |count, control| {
            layout = Some(result::admit_layout(count, memory, materializer, control)?);
            Ok(())
        })?;
        result::materialize_batch(
            batch,
            base,
            memory,
            layout.ok_or(StageError::InvalidInput)?,
            materializer,
            control,
        )
    }
    /// Replaces a complete private image after one ordered update. Input backing
    /// remains immutable for this synchronous statement; final owned copies are
    /// charged before participant handoff. Repeated replacement consumes one slot.
    pub fn replace(
        &mut self,
        target: BatchEntityRef<'batch>,
        image: WriteImage<'a, 'batch>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        control(WritePhase::Overlay)?;
        let kind = match image {
            WriteImage::Node(image) => image.shape().kind(),
            WriteImage::Relationship { .. } => EntityKind::Relationship,
        };
        if kind != target.kind() {
            return Err(StageError::InvalidInput);
        }
        for entry in self.entries.as_mut_slice() {
            control(WritePhase::Overlay)?;
            if entry.target == target {
                if entry.deleted.is_some() {
                    return Err(StageError::DeletedEntity);
                }
                entry.image = Some(image);
                return Ok(());
            }
        }
        let id = target.existing().ok_or(StageError::MissingEntity)?;
        let entity = self
            .base
            .entity(id, control)?
            .ok_or(StageError::MissingEntity)?;
        structured::checked_base(&entity, self.identity, self.base.high_waters())?;
        self.entries.push(Entry {
            target,
            image: Some(image),
            deleted: None,
        })
    }
    /// Stages one tombstone; repeated deletion is one statement change.
    pub fn delete(
        &mut self,
        target: BatchEntityRef<'batch>,
        mode: GraphDeleteMode,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        control(WritePhase::Overlay)?;
        for entry in self.entries.as_mut_slice() {
            control(WritePhase::Overlay)?;
            if entry.target == target {
                entry.deleted = Some(mode);
                return Ok(());
            }
        }
        let id = target.existing().ok_or(StageError::MissingEntity)?;
        let entity = self
            .base
            .entity(id, control)?
            .ok_or(StageError::MissingEntity)?;
        structured::checked_base(&entity, self.identity, self.base.high_waters())?;
        self.entries.push(Entry {
            target,
            image: None,
            deleted: Some(mode),
        })
    }
    /// Reads the current pending property; absent is None and deletion is typed.
    pub fn property(
        &mut self,
        target: BatchEntityRef<'batch>,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'a>>, StageError> {
        control(WritePhase::Overlay)?;
        self.counters.property_lookups = self
            .counters
            .property_lookups
            .checked_add(1)
            .ok_or(StageError::Limit)?;
        self.check_view()?;
        let mut pending = None;
        for entry in &*self.entries {
            control(WritePhase::Overlay)?;
            self.counters.descriptors_examined = self
                .counters
                .descriptors_examined
                .checked_add(1)
                .ok_or(StageError::Limit)?;
            if entry.target == target {
                if entry.deleted.is_some() {
                    return Err(StageError::DeletedEntity);
                }
                pending = entry.image;
                break;
            }
        }
        let value = if let Some(image) = pending {
            let properties = match image {
                WriteImage::Node(image) => {
                    image
                        .staging_node_parts()
                        .ok_or(StageError::InvalidInput)?
                        .1
                }
                WriteImage::Relationship { properties, .. } => properties,
            };
            let mut found = None;
            for property in properties {
                control(WritePhase::Overlay)?;
                self.counters.descriptors_examined = self
                    .counters
                    .descriptors_examined
                    .checked_add(1)
                    .ok_or(StageError::Limit)?;
                if bounded::bytes(
                    property.name().as_str().as_bytes(),
                    name.as_str().as_bytes(),
                    control,
                )?
                .is_eq()
                {
                    found = Some(property.value());
                    break;
                }
            }
            found
        } else {
            let id = target.existing().ok_or(StageError::MissingEntity)?;
            self.check_existing(id, control)?;
            self.base.property(id, name, control)?
        };
        if let Some(value) = value {
            self.count_value(value, control)?;
        }
        self.check_view()?;
        Ok(value)
    }
    /// Reads original pending lexical text; present-empty is distinct from None.
    pub fn stored_text(
        &mut self,
        target: NodeRef<'batch>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<&'a str>, StageError> {
        control(WritePhase::Overlay)?;
        self.counters.text_lookups = self
            .counters
            .text_lookups
            .checked_add(1)
            .ok_or(StageError::Limit)?;
        self.check_view()?;
        let mut pending = None;
        for entry in &*self.entries {
            control(WritePhase::Overlay)?;
            self.counters.descriptors_examined = self
                .counters
                .descriptors_examined
                .checked_add(1)
                .ok_or(StageError::Limit)?;
            if entry.target == BatchEntityRef::Node(target) {
                if entry.deleted.is_some() {
                    return Err(StageError::DeletedEntity);
                }
                pending = entry.image;
                break;
            }
        }
        let text = if let Some(WriteImage::Node(image)) = pending {
            image
                .staging_node_parts()
                .ok_or(StageError::InvalidInput)?
                .2
        } else {
            let NodeRef::Existing(id) = target else {
                return Err(StageError::MissingEntity);
            };
            self.check_existing(EntityId::Node(id), control)?;
            self.base.stored_text(id, control)?
        };
        if let Some(text) = text {
            self.count_bytes(text.len(), 65536, control)?;
        }
        self.check_view()?;
        Ok(text)
    }
    fn check_view(&self) -> Result<(), StageError> {
        if self.base.identity() != self.identity {
            Err(StageError::ViewMismatch)
        } else {
            Ok(())
        }
    }
    fn check_existing(
        &self,
        id: EntityId,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        let entity = self
            .base
            .entity(id, control)?
            .ok_or(StageError::MissingEntity)?;
        structured::checked_base(&entity, self.identity, self.base.high_waters())?;
        if entity.provenance.fields().incarnation != id {
            return Err(StageError::ViewMismatch);
        }
        Ok(())
    }
    fn count_bytes(
        &mut self,
        bytes: usize,
        chunk: usize,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        let mut remaining = bytes;
        while remaining > 0 {
            control(WritePhase::Overlay)?;
            let count = remaining.min(chunk);
            self.counters.value_bytes = self
                .counters
                .value_bytes
                .checked_add(count as u64)
                .ok_or(StageError::Limit)?;
            remaining -= count;
        }
        Ok(())
    }
    fn count_value(
        &mut self,
        value: PropertyValue<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        match value.data() {
            PropertyData::Strings(values) => {
                for value in values {
                    control(WritePhase::Overlay)?;
                    self.count_bytes(value.len(), 65536, control)?;
                }
                Ok(())
            }
            PropertyData::Bools(_) => self.count_bytes(value.payload_bytes(), 256, control),
            PropertyData::Integers(_) | PropertyData::Floats(_) => {
                self.count_bytes(value.payload_bytes(), 256 * 8, control)
            }
            _ => self.count_bytes(value.payload_bytes(), 65536, control),
        }
    }
    /// Work accumulated by the actual accessors.
    pub const fn counters(&self) -> OverlayCounters {
        self.counters
    }
}

impl<'a, 'batch> GraphBatchReadView<'a, 'batch> {
    fn resolve_node(
        &self,
        reference: NodeRef<'batch>,
        slots: &[Option<ItemReceipt>],
        control: &mut WriteControl<'_>,
    ) -> Result<(NodeId, Option<GraphDeleteMode>), StageError> {
        for (index, entry) in self.entries.iter().enumerate() {
            control(WritePhase::Validate)?;
            if entry.target == BatchEntityRef::Node(reference) {
                let receipt = slots
                    .get(index)
                    .and_then(|slot| *slot)
                    .ok_or(StageError::Endpoint)?;
                let EntityId::Node(id) = receipt.entity else {
                    return Err(StageError::Endpoint);
                };
                return Ok((id, entry.deleted));
            }
        }
        let NodeRef::Existing(id) = reference else {
            return Err(StageError::Endpoint);
        };
        self.check_existing(EntityId::Node(id), control)?;
        Ok((id, None))
    }
    fn finalize(
        self,
        control: &mut WriteControl<'_>,
        preflight: &mut result::ResultPreflight<'_>,
    ) -> Result<StagedBatch<'a>, StageError> {
        use std::io::Cursor;
        use structured::{SourceReader, allocate, canonical_poll, retain_provenance};
        struct Prepared<'a> {
            encoded: Option<encode::Encoded<'a>>,
            image: Option<encode::PreparedImage<'a>>,
            decision: Option<KeyDecision<'a>>,
            previous: Option<OperationProvenance<'a>>,
            before: Membership,
            suppressed: bool,
        }
        control(WritePhase::Validate)?;
        self.check_view()?;
        let base: &'a dyn AdmittedBase = self.base;
        let mut high_waters = base.high_waters();
        let mut prepared = Arena::new(self.memory, self.entries.len(), control)?;
        let mut slots = Arena::new(self.memory, self.entries.len(), control)?;
        let mut keys = Arena::new(self.memory, self.entries.len(), control)?;
        let mut changed = Arena::new(self.memory, self.entries.len(), control)?;
        let mut removed = Arena::new(self.memory, self.entries.len(), control)?;
        let mut scratch = [0u8; 4096];
        let mut any_changed = false;
        let mut receipt_count = 0usize;
        let mut admitted_input_bytes = 0usize;
        // The full declaration graph and every existing lifecycle are validated
        // before computing a target generation or assigning a fresh identity.
        for entry in &*self.entries {
            control(WritePhase::Validate)?;
            let existing = entry.target.existing();
            let mut suppressed = existing.is_none() && entry.deleted.is_some();
            let endpoints = match entry.image {
                Some(WriteImage::Relationship { source, target, .. }) => {
                    for reference in [source, target] {
                        let mut declared = false;
                        for node in &*self.entries {
                            control(WritePhase::Validate)?;
                            if node.target == BatchEntityRef::Node(reference) {
                                declared = true;
                                if entry.deleted.is_none()
                                    && node.deleted == Some(GraphDeleteMode::Restrict)
                                {
                                    return Err(StageError::IncidentRelationship);
                                }
                                suppressed |= existing.is_none() && node.deleted.is_some();
                                break;
                            }
                        }
                        match reference {
                            NodeRef::Existing(id) => {
                                self.check_existing(EntityId::Node(id), control)?
                            }
                            NodeRef::Local(_) if !declared => return Err(StageError::Endpoint),
                            NodeRef::Local(_) => {}
                        }
                    }
                    match (source, target) {
                        (NodeRef::Existing(source), NodeRef::Existing(target)) => {
                            Some((source, target))
                        }
                        _ if existing.is_some() => {
                            return Err(KeyLifecycleError::RelationshipIdentityChange.into());
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            let current = existing
                .map(|id| base.entity(id, control)?.ok_or(StageError::MissingEntity))
                .transpose()?;
            if let Some(current) = &current {
                structured::checked_base(current, self.identity, high_waters)?;
                if Some(current.provenance.fields().incarnation) != existing {
                    return Err(StageError::ViewMismatch);
                }
            }
            let previous = current.as_ref().map(|e| e.provenance);
            let before = current
                .as_ref()
                .map_or(Membership::default(), |e| e.membership);
            let key = previous.and_then(|p| p.fields().key);
            let mut image = entry
                .image
                .map(|image| encode::PreparedImage::new(image, base, self.memory, control))
                .transpose()?;
            let image_bytes = image.as_ref().map_or(0, |image| image.len());
            let encoded = if existing.is_some() {
                image
                    .take()
                    .map(|image| image.encode(endpoints, control))
                    .transpose()?
            } else {
                None
            };
            let decision = if let Some(current) = current {
                let mut old_source = SourceReader {
                    source: current.source,
                    offset: 0,
                };
                let old_record = CanonicalRecord::from_validated(
                    current.shape,
                    current.fingerprint,
                    &mut old_source,
                );
                let mut after_source = Cursor::new(encoded.as_ref().map_or(&[][..], |e| &*e.bytes));
                let edit = match (entry.deleted, encoded.as_ref()) {
                    (Some(mode), _) => CypherEdit::Delete(mode),
                    (None, Some(e)) => CypherEdit::Put(CanonicalRecord::from_validated(
                        e.shape,
                        e.fingerprint,
                        &mut after_source,
                    )),
                    _ => return Err(StageError::InvalidInput),
                };
                Some(super::super::key_lifecycle::classify_cypher_record(
                    current.provenance,
                    old_record,
                    edit,
                    &mut scratch,
                    &mut || canonical_poll(control),
                )?)
            } else {
                None
            };
            let is_changed = match decision {
                Some(KeyDecision::NoOp) => false,
                Some(KeyDecision::Change(_)) | None => true,
                Some(KeyDecision::Replay(_)) => return Err(StageError::InvalidInput),
            };
            let expected = if is_changed {
                existing.map_or(ExpectedGraphState::Absent, ExpectedGraphState::Entity)
            } else {
                previous.ok_or(StageError::InvalidInput)?.fields().expected
            };
            let framing =
                super::super::provenance::measure_operation_framing(key, expected, &mut || {
                    canonical_poll(control)
                })?;
            admitted_input_bytes = admitted_input_bytes
                .checked_add(image_bytes)
                .and_then(|n| n.checked_add(framing as usize))
                .ok_or(StageError::Limit)?;
            if admitted_input_bytes > self.memory.limits.input_bytes {
                return Err(StageError::Limit);
            }
            any_changed |= is_changed;
            if !suppressed {
                receipt_count = receipt_count.checked_add(1).ok_or(StageError::Limit)?;
            }
            if let (Some(EntityId::Relationship(id)), Some(_)) = (existing, entry.deleted) {
                removed.push(id)?;
            }
            prepared.push(Prepared {
                encoded,
                image,
                decision,
                previous,
                before,
                suppressed,
            })?;
            slots.push(None)?;
            keys.push(key)?;
            changed.push(is_changed && !suppressed)?;
        }
        if receipt_count > self.memory.limits.result_rows {
            return Err(StageError::Limit);
        }
        for entry in &*self.entries {
            control(WritePhase::Validate)?;
            if let (Some(EntityId::Node(id)), Some(GraphDeleteMode::Restrict)) =
                (entry.target.existing(), entry.deleted)
            {
                control(WritePhase::Incident)?;
                if base.has_live_incident(id, &removed, control)? {
                    return Err(StageError::IncidentRelationship);
                }
            }
        }
        let mut receipts = Arena::new(self.memory, receipt_count, control)?;
        if receipts.allocated_bytes() > self.memory.limits.result_bytes {
            return Err(StageError::Limit);
        }
        preflight(receipt_count, control)?;
        let generation = if any_changed {
            Some(GraphGeneration::new(
                self.identity
                    .generation
                    .get()
                    .checked_add(1)
                    .ok_or(KeyLifecycleError::GenerationOverflow)?,
            ))
        } else {
            None
        };
        let mut deltas = Arena::new(self.memory, self.entries.len(), control)?;
        let mut input_bytes = 0usize;
        for kind in [EntityKind::Node, EntityKind::Relationship] {
            for (index, entry) in self.entries.iter().enumerate() {
                control(WritePhase::Validate)?;
                if entry.target.kind() != kind {
                    continue;
                }
                let item = prepared
                    .as_mut_slice()
                    .get_mut(index)
                    .ok_or(StageError::InvalidInput)?;
                if entry.target.existing().is_none() {
                    let endpoints = match entry.image {
                        Some(WriteImage::Relationship { source, target, .. }) => Some((
                            self.resolve_node(source, &slots, control)?.0,
                            self.resolve_node(target, &slots, control)?.0,
                        )),
                        _ => None,
                    };
                    item.encoded = item
                        .image
                        .take()
                        .map(|image| image.encode(endpoints, control))
                        .transpose()?;
                }
                let provenance = match item.decision {
                    Some(KeyDecision::NoOp) => retain_provenance(
                        item.previous.ok_or(StageError::InvalidInput)?,
                        *keys.get(index).ok_or(StageError::InvalidInput)?,
                        control,
                    )?,
                    Some(KeyDecision::Change(change)) => change.install(
                        entry.target.existing().ok_or(StageError::MissingEntity)?,
                        generation.ok_or(StageError::InvalidInput)?,
                        &mut || canonical_poll(control),
                    )?,
                    Some(KeyDecision::Replay(_)) => return Err(StageError::InvalidInput),
                    None => {
                        let id = allocate(kind, &mut high_waters, control)?;
                        let revision =
                            GraphRevision::new(1).map_err(|_| StageError::InvalidInput)?;
                        OperationProvenance::from_fields_with_control(
                            Some(1),
                            OperationFields {
                                operation: GraphOperation::CypherEdit,
                                key: None,
                                requested_revision: revision,
                                installed_revision: revision,
                                expected: ExpectedGraphState::Absent,
                                incarnation: id,
                                delete_mode: None,
                                original_generation: generation.ok_or(StageError::InvalidInput)?,
                            },
                            &mut || canonical_poll(control),
                        )?
                    }
                };
                input_bytes = input_bytes
                    .checked_add(provenance.encoded_len() as usize)
                    .and_then(|n| n.checked_add(item.encoded.as_ref().map_or(0, |e| e.bytes.len())))
                    .ok_or(StageError::Limit)?;
                if input_bytes > self.memory.limits.input_bytes {
                    return Err(StageError::Limit);
                }
                let p = provenance.fields();
                *slots
                    .as_mut_slice()
                    .get_mut(index)
                    .ok_or(StageError::InvalidInput)? = Some(ItemReceipt {
                    entity: p.incarnation,
                    revision: p.installed_revision,
                    generation: p.original_generation,
                    replayed: false,
                });
                if *changed.get(index).ok_or(StageError::InvalidInput)? {
                    let (canonical, shape, after) = if entry.deleted.is_some() {
                        (None, None, Membership::default())
                    } else {
                        let encoded = item.encoded.take().ok_or(StageError::InvalidInput)?;
                        (Some(encoded.bytes), Some(encoded.shape), encoded.membership)
                    };
                    deltas.push(NormalizedDelta {
                        provenance,
                        canonical,
                        shape,
                        before: item.before,
                        after,
                    })?;
                }
            }
        }
        if input_bytes != admitted_input_bytes {
            return Err(StageError::InvalidInput);
        }
        for (item, slot) in prepared.iter().zip(&*slots) {
            control(WritePhase::CoreResult)?;
            if !item.suppressed {
                receipts.push(slot.ok_or(StageError::InvalidInput)?)?;
            }
        }
        let inputs =
            self.entries
                .iter()
                .zip(&*keys)
                .zip(&*changed)
                .filter_map(|((entry, key), changed)| {
                    changed.then_some((
                        *key,
                        if entry.deleted.is_some() {
                            None
                        } else {
                            entry.image
                        },
                    ))
                });
        let symbols = symbols::prepare(base, inputs, &mut high_waters, self.memory, control)?;
        let disposition = if any_changed {
            BatchDisposition::Changed
        } else {
            BatchDisposition::NoOp
        };
        control(WritePhase::Finalize)?;
        self.check_view()?;
        Ok(StagedBatch {
            base: self.identity,
            high_waters,
            receipts,
            deltas,
            symbols,
            disposition,
        })
    }
}
