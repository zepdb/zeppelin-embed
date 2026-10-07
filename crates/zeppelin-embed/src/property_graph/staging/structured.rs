use super::*;
use memory::Arena;
use std::io::{Cursor, Read};

pub(super) struct SourceReader<'a> {
    pub(super) source: &'a dyn CanonicalSource,
    pub(super) offset: u64,
}
impl Read for SourceReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = self.source.read_at(self.offset, output)?;
        if count > output.len() {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        self.offset = self
            .offset
            .checked_add(count as u64)
            .ok_or(std::io::ErrorKind::InvalidData)?;
        Ok(count)
    }
}
pub(super) fn canonical_poll(control: &mut WriteControl<'_>) -> Result<(), CanonicalError> {
    control(WritePhase::Canonical).map_err(|_| CanonicalError::Cancelled)
}
pub(super) fn checked_base(
    entity: &BaseEntity<'_>,
    view: BaseIdentity,
    high: HighWaters,
) -> Result<(), StageError> {
    if entity.view != view
        || entity.provenance.fields().original_generation > view.generation
        || entity.shape.kind() != entity.provenance.fields().incarnation.kind()
        || !covered(entity.provenance.fields().incarnation, high)
    {
        return Err(StageError::ViewMismatch);
    }
    Ok(())
}
fn covered(id: EntityId, high: HighWaters) -> bool {
    match id {
        EntityId::Node(id) => id.get() <= high.node,
        EntityId::Relationship(id) => id.get() <= high.relationship,
    }
}
pub(super) fn allocate(
    kind: EntityKind,
    high: &mut HighWaters,
    control: &mut WriteControl<'_>,
) -> Result<EntityId, StageError> {
    control(WritePhase::Identity)?;
    match kind {
        EntityKind::Node => {
            high.node = high
                .node
                .checked_add(1)
                .ok_or(StageError::IdentityOverflow)?;
            Ok(EntityId::Node(
                NodeId::new(high.node).map_err(|_| StageError::InvalidInput)?,
            ))
        }
        EntityKind::Relationship => {
            high.relationship = high
                .relationship
                .checked_add(1)
                .ok_or(StageError::IdentityOverflow)?;
            Ok(EntityId::Relationship(
                RelId::new(high.relationship).map_err(|_| StageError::InvalidInput)?,
            ))
        }
    }
}
pub(super) fn retain_provenance<'a>(
    provenance: OperationProvenance<'_>,
    key: Option<ApplicationKey<'a>>,
    control: &mut WriteControl<'_>,
) -> Result<OperationProvenance<'a>, StageError> {
    let p = provenance.fields();
    Ok(OperationProvenance::from_fields_with_control(
        Some(1),
        OperationFields {
            key,
            operation: p.operation,
            requested_revision: p.requested_revision,
            installed_revision: p.installed_revision,
            expected: p.expected,
            incarnation: p.incarnation,
            delete_mode: p.delete_mode,
            original_generation: p.original_generation,
        },
        &mut || canonical_poll(control),
    )?)
}
fn endpoint(reference: NodeRef<'_>, slots: &[Option<ItemReceipt>]) -> Result<NodeId, StageError> {
    match reference {
        NodeRef::Existing(id) => Ok(id),
        NodeRef::Local(local) => match slots
            .get(local.index() as usize)
            .and_then(|s| *s)
            .map(|r| r.entity)
        {
            Some(EntityId::Node(id)) => Ok(id),
            _ => Err(StageError::Endpoint),
        },
    }
}
struct Admission<'a> {
    current: Option<BaseEntity<'a>>,
    metadata: super::super::key_lifecycle::KeyMetadataState<'a>,
    prepared: Option<super::super::key_lifecycle::KeyPreparation<'a>>,
    decision: Option<KeyDecision<'a>>,
    encoded: Option<encode::Encoded<'a>>,
    image: Option<encode::PreparedImage<'a>>,
    input_bytes: usize,
}
fn metadata(
    operation: StructuredOperation,
    revision: GraphRevision,
) -> super::super::key_lifecycle::KeyRequestMetadata {
    use super::super::key_lifecycle::KeyRequestMetadata;
    let (operation, expected, delete_mode) = match operation {
        StructuredOperation::Create => (
            GraphOperation::StructuredCreate,
            ExpectedGraphState::Absent,
            None,
        ),
        StructuredOperation::Put(id) => (
            GraphOperation::StructuredPut,
            ExpectedGraphState::Entity(id),
            None,
        ),
        StructuredOperation::Delete(id, mode) => (
            GraphOperation::StructuredDelete,
            ExpectedGraphState::Entity(id),
            Some(mode),
        ),
        StructuredOperation::Recreate(revision) => (
            GraphOperation::StructuredRecreate,
            ExpectedGraphState::Deletion(revision),
            None,
        ),
    };
    KeyRequestMetadata {
        operation,
        revision,
        expected,
        delete_mode,
    }
}
fn provisional_endpoint(
    reference: NodeRef<'_>,
    slots: &[Option<ItemReceipt>],
) -> Result<Option<NodeId>, StageError> {
    match reference {
        NodeRef::Existing(id) => Ok(Some(id)),
        NodeRef::Local(local) => match slots
            .get(local.index() as usize)
            .ok_or(StageError::Endpoint)?
        {
            Some(receipt) => match receipt.entity {
                EntityId::Node(id) => Ok(Some(id)),
                _ => Err(StageError::Endpoint),
            },
            None => Ok(None),
        },
    }
}
/// Normalizes structured full-record requests against one admitted base. Every
/// precondition and possible replay is classified before private ID assignment.
/// Fresh relationships alone may defer endpoint-dependent canonical framing.
pub fn stage_structured<'a>(
    base: &'a dyn AdmittedBase,
    requests: &[StructuredWrite<'a, '_>],
    memory: &'a WriteMemory<'a>,
    control: &mut WriteControl<'_>,
) -> Result<StagedBatch<'a>, StageError> {
    stage_structured_with_preflight(
        base,
        base.identity()
            .generation
            .get()
            .checked_add(1)
            .map(GraphGeneration::new),
        requests,
        memory,
        control,
        &mut |_, _| Ok(()),
    )
}
/// Stages a batch at the exact generation assigned by its committer.
pub fn stage_structured_at_generation<'a>(
    base: &'a dyn AdmittedBase,
    target_generation: GraphGeneration,
    requests: &[StructuredWrite<'a, '_>],
    memory: &'a WriteMemory<'a>,
    control: &mut WriteControl<'_>,
) -> Result<StagedBatch<'a>, StageError> {
    stage_structured_with_preflight(
        base,
        Some(target_generation),
        requests,
        memory,
        control,
        &mut |_, _| Ok(()),
    )
}
pub(super) fn stage_structured_with_preflight<'a>(
    base: &'a dyn AdmittedBase,
    target_generation: Option<GraphGeneration>,
    requests: &[StructuredWrite<'a, '_>],
    memory: &'a WriteMemory<'a>,
    control: &mut WriteControl<'_>,
    preflight: &mut result::ResultPreflight<'_>,
) -> Result<StagedBatch<'a>, StageError> {
    let _work = memory.resources().begin_work();
    use super::super::key_lifecycle::{
        KeyMetadataState, KeyPreparation, complete_key, prepare_key,
    };
    control(WritePhase::Validate)?;
    let identity = base.identity();
    let mut high_waters = base.high_waters();
    if identity.generation.get() != 0
        && identity.roots.is_none()
        && identity.fold.manifest_generation == 0
    {
        return Err(StageError::ViewMismatch);
    }
    if requests.len() > memory.limits.changes
        || requests.len() > memory.limits.result_rows
        || requests
            .len()
            .checked_mul(std::mem::size_of::<ItemReceipt>())
            .is_none_or(|n| n > memory.limits.result_bytes)
    {
        return Err(StageError::Limit);
    }
    let mut targets = Arena::new(memory, requests.len(), control)?;
    let mut removed = Arena::new(memory, requests.len(), control)?;
    let mut admissions = Arena::new(memory, requests.len(), control)?;
    // No fresh ID or changed generation exists during metadata admission.
    for request in requests {
        control(WritePhase::Validate)?;
        if matches!(request.operation, StructuredOperation::Delete(..)) != request.image.is_none() {
            return Err(StageError::InvalidInput);
        }
        if let Some(image) = request.image {
            let kind = match image {
                WriteImage::Node(image) => {
                    if image.shape() != EntityShape::Node {
                        return Err(StageError::InvalidInput);
                    }
                    EntityKind::Node
                }
                WriteImage::Relationship { .. } => EntityKind::Relationship,
            };
            if kind != request.key.kind() {
                return Err(StageError::InvalidInput);
            }
        }
        let expected = match request.operation {
            StructuredOperation::Put(id) | StructuredOperation::Delete(id, _) => Some(id),
            _ => None,
        };
        if let StructuredOperation::Delete(EntityId::Relationship(id), _) = request.operation {
            removed.push(id)?;
        }
        let state = base.key(request.key, control)?;
        let (meta, current, resolved) = match state {
            BaseKeyState::NeverUsed => (KeyMetadataState::NeverUsed, None, None),
            BaseKeyState::Live(entity) => {
                checked_base(&entity, identity, high_waters)?;
                let id = entity.provenance.fields().incarnation;
                (
                    KeyMetadataState::Live(entity.provenance, entity.shape),
                    Some(entity),
                    Some(id),
                )
            }
            BaseKeyState::Deleted(view, p) => {
                if view != identity
                    || p.fields().original_generation > identity.generation
                    || !covered(p.fields().incarnation, high_waters)
                {
                    return Err(StageError::ViewMismatch);
                }
                (
                    KeyMetadataState::Deleted(p),
                    None,
                    Some(p.fields().incarnation),
                )
            }
        };
        targets.push(BatchTarget::new(Some(request.key), expected.or(resolved))?)?;
        admissions.push(Admission {
            current,
            metadata: meta,
            prepared: None,
            decision: None,
            encoded: None,
            image: None,
            input_bytes: 0,
        })?;
    }
    validate_distinct_targets(targets.as_mut_slice(), &mut || canonical_poll(control))?;
    drop(targets);
    for (request, admission) in requests.iter().zip(admissions.as_mut_slice()) {
        control(WritePhase::Validate)?;
        admission.prepared = Some(prepare_key(
            request.key,
            admission.metadata,
            metadata(request.operation, request.revision),
            &mut || canonical_poll(control),
        )?);
    }
    let mut admitted_input_bytes = 0usize;
    for (request, admission) in requests.iter().zip(admissions.as_mut_slice()) {
        admission.image = request
            .image
            .map(|image| encode::PreparedImage::new(image, base, memory, control))
            .transpose()?;
        admission.input_bytes = admission.image.as_ref().map_or(0, |image| image.len());
        let framing = super::super::provenance::measure_operation_framing(
            Some(request.key),
            metadata(request.operation, request.revision).expected,
            &mut || canonical_poll(control),
        )?;
        admitted_input_bytes = admitted_input_bytes
            .checked_add(admission.input_bytes)
            .and_then(|n| n.checked_add(framing as usize))
            .ok_or(StageError::Limit)?;
        if admitted_input_bytes > memory.limits.input_bytes {
            return Err(StageError::Limit);
        }
    }
    let mut slots = Arena::new(memory, requests.len(), control)?;
    for _ in requests {
        control(WritePhase::Validate)?;
        slots.push(None)?;
    }
    let mut scratch = [0u8; 4096];
    for kind in [EntityKind::Node, EntityKind::Relationship] {
        for (index, request) in requests.iter().enumerate() {
            control(WritePhase::Validate)?;
            if request.key.kind() != kind {
                continue;
            }
            let admission = admissions
                .as_mut_slice()
                .get_mut(index)
                .ok_or(StageError::InvalidInput)?;
            let endpoints = match request.image {
                Some(WriteImage::Relationship { source, target, .. }) => {
                    provisional_endpoint(source, &slots)?.zip(provisional_endpoint(target, &slots)?)
                }
                _ => None,
            };
            let deferred = matches!(request.image, Some(WriteImage::Relationship { .. }))
                && endpoints.is_none();
            let (encoded, decision) = if deferred {
                // Any genuinely fresh endpoint is above the base high-water, so
                // it cannot equal an existing incarnation's fixed endpoint.
                let change = match admission.prepared.ok_or(StageError::InvalidInput)? {
                    KeyPreparation::Change(change) if change.existing_incarnation().is_none() => {
                        change
                    }
                    KeyPreparation::Change(_) => {
                        return Err(KeyLifecycleError::RelationshipIdentityChange.into());
                    }
                    KeyPreparation::CompareReplay(_) => {
                        return Err(KeyLifecycleError::RevisionConflict.into());
                    }
                };
                (None, KeyDecision::Change(change))
            } else {
                let encoded = admission
                    .image
                    .take()
                    .map(|image| image.encode(endpoints, control))
                    .transpose()?;
                let mut after_source = Cursor::new(encoded.as_ref().map_or(&[][..], |e| &*e.bytes));
                let after = encoded.as_ref().map(|e| {
                    CanonicalRecord::from_validated(e.shape, e.fingerprint, &mut after_source)
                        .with_resources(memory.resources())
                });
                let empty = CanonicalSlice(&[]);
                let mut old_source = SourceReader {
                    source: &empty,
                    offset: 0,
                };
                let before = admission.current.map(|entity| {
                    old_source.source = entity.source;
                    CanonicalRecord::from_validated(
                        entity.shape,
                        entity.fingerprint,
                        &mut old_source,
                    )
                    .with_resources(memory.resources())
                });
                let decision = complete_key(
                    admission.prepared.ok_or(StageError::InvalidInput)?,
                    before,
                    after,
                    &mut scratch,
                    &mut || canonical_poll(control),
                )?;
                (encoded, decision)
            };
            if let KeyDecision::Replay(p) = decision {
                let p = p.fields();
                *slots
                    .as_mut_slice()
                    .get_mut(index)
                    .ok_or(StageError::InvalidInput)? = Some(ItemReceipt {
                    entity: p.incarnation,
                    revision: p.installed_revision,
                    generation: p.original_generation,
                    replayed: true,
                });
            }
            let admission = admissions
                .as_mut_slice()
                .get_mut(index)
                .ok_or(StageError::InvalidInput)?;
            admission.encoded = encoded;
            admission.decision = Some(decision);
        }
    }
    // Only authenticated exact replay may bypass endpoint admission. Every
    // real change is admitted before any allocation or publication.
    for (request, admission) in requests.iter().zip(&*admissions) {
        if matches!(admission.decision, Some(KeyDecision::Replay(_))) {
            continue;
        }
        if let Some(WriteImage::Relationship { source, target, .. }) = request.image {
            for endpoint in [source, target] {
                control(WritePhase::Validate)?;
                match endpoint {
                    NodeRef::Existing(id) => {
                        let entity = base
                            .entity(EntityId::Node(id), control)?
                            .ok_or(StageError::Endpoint)?;
                        checked_base(&entity, identity, high_waters)?;
                        if entity.provenance.fields().incarnation != EntityId::Node(id) {
                            return Err(StageError::ViewMismatch);
                        }
                        for other in requests {
                            control(WritePhase::Validate)?;
                            if matches!(other.operation,StructuredOperation::Delete(EntityId::Node(deleted),_) if deleted==id)
                            {
                                return Err(StageError::Endpoint);
                            }
                        }
                    }
                    NodeRef::Local(local) => {
                        let input = requests
                            .get(local.index() as usize)
                            .ok_or(StageError::Endpoint)?;
                        if input.key.kind() != EntityKind::Node
                            || !matches!(input.image, Some(WriteImage::Node(_)))
                            || !matches!(
                                input.operation,
                                StructuredOperation::Create | StructuredOperation::Recreate(_)
                            )
                        {
                            return Err(StageError::Endpoint);
                        }
                    }
                }
            }
        }
    }
    let mut decisions = Arena::new(memory, requests.len(), control)?;
    for admission in &*admissions {
        control(WritePhase::Validate)?;
        decisions.push(admission.decision.ok_or(StageError::InvalidInput)?)?;
    }
    for (request, decision) in requests.iter().zip(&*decisions) {
        control(WritePhase::Validate)?;
        if matches!(decision, KeyDecision::Change(_))
            && let StructuredOperation::Delete(EntityId::Node(id), GraphDeleteMode::Restrict) =
                request.operation
        {
            control(WritePhase::Incident)?;
            if base.has_live_incident(id, &removed, control)? {
                return Err(StageError::IncidentRelationship);
            }
        }
    }
    // Full base/revision/replay/integrity admission completed. Only now are
    // private checked IDs and operation provenance finalized.
    let mut receipts = Arena::new(memory, requests.len(), control)?;
    if receipts.allocated_bytes() > memory.limits.result_bytes {
        return Err(StageError::Limit);
    }
    preflight(requests.len(), control)?;
    let classification = super::super::key_lifecycle::summarize_key_batch_for_target(
        identity.generation,
        target_generation,
        &decisions,
        false,
        &mut || canonical_poll(control),
    )?;
    let mut provenance = Arena::new(memory, requests.len(), control)?;
    for (index, decision) in decisions.iter().enumerate() {
        control(WritePhase::Validate)?;
        let (p, replayed) = match *decision {
            KeyDecision::Replay(p) => (p, true),
            KeyDecision::Change(change) => {
                let request = requests.get(index).ok_or(StageError::InvalidInput)?;
                let id = match change.existing_incarnation() {
                    Some(id) => id,
                    None => allocate(request.key.kind(), &mut high_waters, control)?,
                };
                let generation = classification
                    .changed_generation
                    .ok_or(StageError::InvalidInput)?;
                (
                    change.install(id, generation, &mut || canonical_poll(control))?,
                    false,
                )
            }
            KeyDecision::NoOp => return Err(StageError::InvalidInput),
        };
        control(WritePhase::CoreResult)?;
        let fields = p.fields();
        let receipt = ItemReceipt {
            entity: fields.incarnation,
            revision: fields.installed_revision,
            generation: fields.original_generation,
            replayed,
        };
        *slots
            .as_mut_slice()
            .get_mut(index)
            .ok_or(StageError::InvalidInput)? = Some(receipt);
        receipts.push(receipt)?;
        provenance.push(p)?;
    }
    let mut deltas = Arena::new(memory, classification.changed_items, control)?;
    let mut input_bytes = 0usize;
    for (index, request) in requests.iter().enumerate() {
        control(WritePhase::Validate)?;
        let admission = admissions
            .as_mut_slice()
            .get_mut(index)
            .ok_or(StageError::InvalidInput)?;
        if admission.encoded.is_none()
            && let Some(WriteImage::Relationship { source, target, .. }) = request.image
        {
            let encoded = admission
                .image
                .take()
                .ok_or(StageError::InvalidInput)?
                .encode(
                    Some((endpoint(source, &slots)?, endpoint(target, &slots)?)),
                    control,
                )?;
            if encoded.bytes.len() != admission.input_bytes {
                return Err(StageError::InvalidInput);
            }
            admission.encoded = Some(encoded);
        }
        let p = *provenance.get(index).ok_or(StageError::InvalidInput)?;
        input_bytes = input_bytes
            .checked_add(p.encoded_len() as usize)
            .and_then(|n| n.checked_add(admission.input_bytes))
            .ok_or(StageError::Limit)?;
        if input_bytes > memory.limits.input_bytes {
            return Err(StageError::Limit);
        }
        if !receipts
            .get(index)
            .ok_or(StageError::InvalidInput)?
            .replayed
        {
            let (canonical, shape, after) = match admission.encoded.take() {
                Some(e) => (Some(e.bytes), Some(e.shape), e.membership),
                None => (None, None, Membership::default()),
            };
            let before = admission
                .current
                .map_or(Membership::default(), |entity| entity.membership);
            deltas.push(NormalizedDelta {
                provenance: p,
                canonical,
                shape,
                before,
                after,
            })?;
        }
    }
    if input_bytes != admitted_input_bytes {
        return Err(StageError::InvalidInput);
    }
    let symbols = symbols::prepare(
        base,
        requests
            .iter()
            .zip(&*receipts)
            .filter_map(|(request, receipt)| {
                (!receipt.replayed).then_some((Some(request.key), request.image))
            }),
        &mut high_waters,
        memory,
        control,
    )?;
    control(WritePhase::Finalize)?;
    if base.identity() != identity {
        return Err(StageError::ViewMismatch);
    }
    StagedBatch {
        base: identity,
        target_generation: target_generation.unwrap_or(identity.generation),
        high_waters,
        receipts,
        deltas,
        symbols,
        disposition: classification.disposition,
    }
    .enforce_relationship_rules(base, memory, control)
}
