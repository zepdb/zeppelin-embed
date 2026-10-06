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
    /// Pending label/relationship-type accesses actually requested.
    pub symbol_lookups: u64,
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
    /// Created by `create_fresh`: `target` names the identity this statement
    /// allocated, and nothing about it is read from the admitted base.
    fresh: bool,
}
impl Entry<'_, '_> {
    /// The admitted-base identity this entry edits; `None` for every creation.
    fn base(&self) -> Option<EntityId> {
        if self.fresh {
            None
        } else {
            self.target.existing()
        }
    }
}
/// Progressive private property/text overlay. Upstream bindings remain frozen;
/// this interface cannot scan, traverse adjacency or introduce new bindings.
pub struct GraphBatchReadView<'a, 'batch> {
    base: &'a dyn AdmittedBase,
    identity: BaseIdentity,
    memory: &'a WriteMemory<'a>,
    entries: Arena<'a, Entry<'a, 'batch>>,
    /// The base's allocation fences, advanced by every `create_fresh`.
    high_waters: HighWaters,
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
            high_waters: base.high_waters(),
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
            fresh: false,
        })
    }
    /// Creates one new unkeyed node or relationship and returns the identity
    /// it will publish under. The identity comes from the same allocator and
    /// the same admitted fences finalization uses for local creations, taken
    /// now rather than at finalization so a query can bind it: later items
    /// and later clauses of the statement address the entity as an ordinary
    /// `Existing` reference, and every read finds its staged image first.
    ///
    /// The identity is private to the statement until publication. Dropping
    /// the overlay, on any rejection, discards it with the advanced fences.
    pub fn create_fresh(
        &mut self,
        image: WriteImage<'a, 'batch>,
        control: &mut WriteControl<'_>,
    ) -> Result<EntityId, StageError> {
        control(WritePhase::Overlay)?;
        let kind = match image {
            WriteImage::Node(image) => image.shape().kind(),
            WriteImage::Relationship { .. } => EntityKind::Relationship,
        };
        let mut high_waters = self.high_waters;
        let id = structured::allocate(kind, &mut high_waters, control)?;
        let target = match id {
            EntityId::Node(id) => BatchEntityRef::Node(NodeRef::Existing(id)),
            EntityId::Relationship(id) => BatchEntityRef::Relationship(RelRef::Existing(id)),
        };
        self.entries.push(Entry {
            target,
            image: Some(image),
            deleted: None,
            fresh: true,
        })?;
        self.high_waters = high_waters;
        Ok(id)
    }
    /// Finalizes one Cypher statement: equal final images are NoOp, changed
    /// existing entities advance once, and consumed local IDs remain fenced.
    pub fn finish(self, control: &mut WriteControl<'_>) -> Result<StagedBatch<'a>, StageError> {
        let target = self
            .identity
            .generation
            .get()
            .checked_add(1)
            .map(GraphGeneration::new);
        self.finalize(target, control, &mut |_, _| Ok(()))
    }
    /// Finalizes at the exact generation assigned by the statement committer.
    pub fn finish_at_generation(
        self,
        target: GraphGeneration,
        control: &mut WriteControl<'_>,
    ) -> Result<StagedBatch<'a>, StageError> {
        self.finalize(Some(target), control, &mut |_, _| Ok(()))
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
        let target = self
            .identity
            .generation
            .get()
            .checked_add(1)
            .map(GraphGeneration::new);
        let batch = self.finalize(target, control, &mut |count, control| {
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
            fresh: false,
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
            fresh: false,
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
        let pending = self.staged(target, control)?;
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
        let pending = self.staged(BatchEntityRef::Node(target), control)?;
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
    /// Reads the labels a prior clause staged for this node. `None` means no
    /// clause has staged the node, so the caller must read its own admitted
    /// base: `AdmittedBase` exposes labels only inside a canonical image, so an
    /// in-overlay fallback would have to decode one. Deletion stays typed and a
    /// staged image of the wrong role is rejected, exactly as `property` does.
    pub fn pending_labels(
        &mut self,
        target: NodeRef<'batch>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<&'a [GraphName<'a>]>, StageError> {
        control(WritePhase::Overlay)?;
        self.counters.symbol_lookups = self
            .counters
            .symbol_lookups
            .checked_add(1)
            .ok_or(StageError::Limit)?;
        self.check_view()?;
        let Some(image) = self.staged(BatchEntityRef::Node(target), control)? else {
            self.check_view()?;
            return Ok(None);
        };
        let WriteImage::Node(image) = image else {
            return Err(StageError::InvalidInput);
        };
        let labels = image
            .staging_node_parts()
            .ok_or(StageError::InvalidInput)?
            .0;
        for label in labels {
            control(WritePhase::Overlay)?;
            self.counters.descriptors_examined = self
                .counters
                .descriptors_examined
                .checked_add(1)
                .ok_or(StageError::Limit)?;
            self.count_bytes(label.as_str().len(), 65536, control)?;
        }
        self.check_view()?;
        Ok(Some(labels))
    }
    /// Reads the type a prior clause staged for this relationship, on exactly
    /// the terms of `pending_labels`.
    pub fn pending_relationship_type(
        &mut self,
        target: RelRef<'batch>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<GraphName<'a>>, StageError> {
        control(WritePhase::Overlay)?;
        self.counters.symbol_lookups = self
            .counters
            .symbol_lookups
            .checked_add(1)
            .ok_or(StageError::Limit)?;
        self.check_view()?;
        let Some(image) = self.staged(BatchEntityRef::Relationship(target), control)? else {
            self.check_view()?;
            return Ok(None);
        };
        let WriteImage::Relationship {
            relationship_type, ..
        } = image
        else {
            return Err(StageError::InvalidInput);
        };
        self.count_bytes(relationship_type.as_str().len(), 65536, control)?;
        self.check_view()?;
        Ok(Some(relationship_type))
    }
    /// The whole image a prior clause of this statement staged for `target`.
    /// `None` means nothing is staged, so the caller must read its own
    /// admitted base; a target already deleted in this statement is typed.
    pub fn pending_image(
        &mut self,
        target: BatchEntityRef<'batch>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<WriteImage<'a, 'batch>>, StageError> {
        control(WritePhase::Overlay)?;
        self.check_view()?;
        let image = self.staged(target, control)?;
        self.check_view()?;
        Ok(image)
    }
    /// Scans the staged entries once, charging one examined descriptor per
    /// entry. `None` means no clause has staged `target`; a target already
    /// deleted in this statement is typed.
    fn staged(
        &mut self,
        target: BatchEntityRef<'batch>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<WriteImage<'a, 'batch>>, StageError> {
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
        Ok(pending)
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
        target_generation: Option<GraphGeneration>,
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
        // Fences already advanced by `create_fresh`, so local creations
        // allocate after every identity the statement has bound.
        let mut high_waters = self.high_waters;
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
            let existing = entry.base();
            let mut suppressed = existing.is_none() && entry.deleted.is_some();
            let endpoints = match entry.image {
                Some(WriteImage::Relationship { source, target, .. }) => {
                    for reference in [source, target] {
                        let mut declared = false;
                        let mut fresh = false;
                        for node in &*self.entries {
                            control(WritePhase::Validate)?;
                            if node.target == BatchEntityRef::Node(reference) {
                                declared = true;
                                fresh = node.fresh;
                                if entry.deleted.is_none()
                                    && node.deleted == Some(GraphDeleteMode::Restrict)
                                {
                                    return Err(StageError::IncidentRelationship);
                                }
                                if base.has_relationship_rules()
                                    && entry.deleted.is_none()
                                    && node.deleted.is_some()
                                {
                                    return Err(StageError::Endpoint);
                                }
                                suppressed |= existing.is_none() && node.deleted.is_some();
                                break;
                            }
                        }
                        match reference {
                            NodeRef::Existing(_) if fresh => {}
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
                structured::checked_base(current, self.identity, base.high_waters())?;
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
                )
                .with_resources(self.memory.resources());
                let mut after_source = Cursor::new(encoded.as_ref().map_or(&[][..], |e| &*e.bytes));
                let edit = match (entry.deleted, encoded.as_ref()) {
                    (Some(mode), _) => CypherEdit::Delete(mode),
                    (None, Some(e)) => CypherEdit::Put(
                        CanonicalRecord::from_validated(e.shape, e.fingerprint, &mut after_source)
                            .with_resources(self.memory.resources()),
                    ),
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
                (entry.base(), entry.deleted)
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
            let target = target_generation.ok_or(KeyLifecycleError::GenerationOverflow)?;
            if target <= self.identity.generation {
                return Err(KeyLifecycleError::InvalidGeneration.into());
            }
            Some(target)
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
                if entry.base().is_none() {
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
                        entry.base().ok_or(StageError::MissingEntity)?,
                        generation.ok_or(StageError::InvalidInput)?,
                        &mut || canonical_poll(control),
                    )?,
                    Some(KeyDecision::Replay(_)) => return Err(StageError::InvalidInput),
                    None => {
                        let id = match entry.target.existing() {
                            Some(id) if entry.fresh => id,
                            _ => allocate(kind, &mut high_waters, control)?,
                        };
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
        StagedBatch {
            base: self.identity,
            target_generation: generation.unwrap_or(self.identity.generation),
            high_waters,
            receipts,
            deltas,
            symbols,
            disposition,
        }
        .enforce_relationship_rules(base, self.memory, control)
    }
}
