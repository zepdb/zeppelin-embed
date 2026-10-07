//! Sole coordinator for bounded native physical maintenance.

pub(super) mod orphans;
pub(super) mod spill;

use super::persistence::{artifact_descriptor, encode_framed, next_artifact, zeroed};
use super::write::{
    inventory_payload, io, prepare_maintenance_transition, prepare_reclaim_clear_transition,
    prepare_reclaim_completion_transition, protect_and_commit,
};
use super::{NativeGraphError, NativeMaintenanceAdmission, NativeReadLease};
use crate::property_graph::GraphGeneration;
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{StageError, WriteLimits, WriteMemory};
use crate::property_graph::storage::adjacency::RangeScratch;
use crate::property_graph::storage::artifact::{ArtifactIdentity, Block, BlockKind, ContainerKind};
use crate::property_graph::storage::consolidation::prepare_one_replacement;
use crate::property_graph::storage::inventory::{
    INVENTORY_FOLD_ADDITION_LIMIT, apply_inventory, inventory_resume_after, prepare_inventory_fold,
    retire_reclaimed_inventory, verify_inventory_entry,
};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::prepared::{PackLimits, PreparedObjects};
use crate::property_graph::storage::reclaim::{
    DurableRunReader, PendingIntentManifest, ProtectedClass, ProtectedRecord,
    ProtectedStreamBuilder, ProtectedValue, SpillBinding, SpillIo, SpillMark,
    decode_pending_intent_manifest, pending_intent_candidate_at, validate_protected_stream,
};
use crate::property_graph::storage::search::{
    SearchTraceCursor, SparseRoots, prepare_sparse_maintenance,
};
use crate::property_graph::storage::tree::TreeKind;
use crate::property_graph::storage::tree::directory::{BlockSink, DirectoryCursor, TreeScratch};
use crate::property_graph::storage::{NativePreparationCatalog, NativePreparationSource};
use crate::property_graph::wal::{
    BatchId, InventoryChange, InventoryState, MAX_ENVELOPE_BYTES, STACK_RESERVATION_BYTES,
    WalResources,
};
use crate::vfs::SyncKind;
use std::sync::Arc;

fn spill_error(
    writer: &spill::NativeSpillWriter<'_, '_>,
    error: crate::property_graph::storage::tree::directory::TreeError,
) -> NativeGraphError {
    writer.take_failure().unwrap_or_else(|| error.into())
}

fn emit_protected(
    record: ProtectedRecord,
    protected: &mut ProtectedStreamBuilder<'_>,
    mark: &mut SpillMark<'_>,
    writer: &mut spill::NativeSpillWriter<'_, '_>,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<(), NativeGraphError> {
    let artifact = record.artifact();
    if let Err(error) = protected.emit(record, writer, resources) {
        return Err(spill_error(writer, error));
    }
    if let Some(artifact) = artifact
        && let Err(error) = mark.emit(artifact, writer, resources)
    {
        return Err(spill_error(writer, error));
    }
    Ok(())
}

fn emit_required(
    class: ProtectedClass,
    required: crate::property_graph::wal::RequiredRef,
    protected: &mut ProtectedStreamBuilder<'_>,
    mark: &mut SpillMark<'_>,
    writer: &mut spill::NativeSpillWriter<'_, '_>,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<(), NativeGraphError> {
    emit_protected(
        ProtectedRecord::required(class, required),
        protected,
        mark,
        writer,
        resources,
    )
}

#[derive(Clone, Copy)]
struct ProofBaseRecords {
    captured: crate::property_graph::wal::RequiredRef,
    fold: ProtectedRecord,
    closure_count: usize,
}

fn capture_record_count(
    capture: &super::NativeProtectedRoots,
    admitted: &Arc<super::NativeGraphBundle>,
    base: ProofBaseRecords,
) -> Result<usize, NativeGraphError> {
    let mut count = 0_usize;
    while capture_record_at(capture, admitted, base, count).is_some() {
        count = count
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?;
    }
    count
        .checked_add(base.closure_count)
        .ok_or(NativeGraphError::IdentityExhausted)
}

fn capture_record_at(
    capture: &super::NativeProtectedRoots,
    admitted: &Arc<super::NativeGraphBundle>,
    base: ProofBaseRecords,
    mut index: usize,
) -> Option<ProtectedRecord> {
    let mut take = |record| {
        if index == 0 {
            Some(record)
        } else {
            index -= 1;
            None
        }
    };
    for bundle in capture.bundles() {
        let current = Arc::ptr_eq(bundle, admitted);
        for class in [
            Some(if current {
                ProtectedClass::Current
            } else {
                ProtectedClass::Reader
            }),
            current.then_some(ProtectedClass::PreparedBase),
        ]
        .into_iter()
        .flatten()
        {
            if current
                && let Some(record) = take(ProtectedRecord::captured_base(
                    class,
                    base.captured,
                    bundle.sequence(),
                ))
            {
                return Some(record);
            }
            for required in bundle
                .wal_roots()
                .slots
                .into_iter()
                .flatten()
                .chain(
                    [
                        // Removed by ZE-346 when the graph WAL and root selector are deleted.
                        Some(bundle.root_envelope()),
                        Some(bundle.catalog()),
                        bundle.text(),
                        bundle.vector(),
                        bundle.reclaim(),
                    ]
                    .into_iter()
                    .flatten(),
                )
                .chain(bundle.prepared_inventories().iter().copied())
            {
                if let Some(record) = take(ProtectedRecord::required(class, required)) {
                    return Some(record);
                }
            }
        }
    }
    for descriptor in capture.prepared().iter().copied() {
        if let Some(record) = take(ProtectedRecord::descriptor(
            ProtectedClass::PreparedAllocation,
            descriptor,
        )) {
            return Some(record);
        }
    }
    for spill in capture.spills() {
        if let Some(head) = spill.head
            && let Some(record) = take(ProtectedRecord::required(ProtectedClass::Proof, head))
        {
            return Some(record);
        }
        for descriptor in spill.pending.into_iter().flatten() {
            if let Some(record) = take(ProtectedRecord::descriptor(
                ProtectedClass::InFlight,
                descriptor,
            )) {
                return Some(record);
            }
        }
    }
    for proof in capture.proofs() {
        for required in [
            Some(proof.allocation_head),
            Some(proof.protected.head),
            Some(proof.mark.root),
            proof.intent,
        ]
        .into_iter()
        .flatten()
        {
            if let Some(record) = take(ProtectedRecord::required(ProtectedClass::Proof, required)) {
                return Some(record);
            }
        }
    }
    take(base.fold)
}

fn validate_authentic_protected_capture(
    capture: &super::NativeProtectedRoots,
    admitted: &Arc<super::NativeGraphBundle>,
    stream: crate::property_graph::storage::reclaim::DurableProtectedStream,
    base: ProofBaseRecords,
    writer: &spill::NativeSpillWriter<'_, '_>,
    storage: &StorageMemory<'_>,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<(), NativeGraphError> {
    let count = capture_record_count(capture, admitted, base)?;
    if stream.count != u64::try_from(count).map_err(|_| NativeGraphError::IdentityExhausted)? {
        return Err(NativeGraphError::Invalid(
            "durable protected capture record count",
        ));
    }
    let page = crate::property_graph::storage::reclaim::PROTECTED_CAPTURE_PAGE_RECORDS;
    let pages = count.div_ceil(page);
    let last_page_count = count
        .checked_sub(pages.saturating_sub(1).saturating_mul(page))
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut visited = 0_usize;
    validate_protected_stream(stream, writer, storage, resources, |actual, _| {
        let original = if visited < last_page_count {
            pages
                .saturating_sub(1)
                .checked_mul(page)
                .and_then(|start| start.checked_add(visited))
                .ok_or(crate::property_graph::storage::tree::directory::TreeError::Work)?
        } else {
            let rest = visited - last_page_count;
            let page_from_end = rest / page;
            let original_page = pages.checked_sub(2 + page_from_end).ok_or(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "durable protected capture page order",
                ),
            )?;
            original_page
                .checked_mul(page)
                .and_then(|start| start.checked_add(rest % page))
                .ok_or(crate::property_graph::storage::tree::directory::TreeError::Work)?
        };
        if original == count - 1 {
            if actual != base.fold {
                return Err(
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "protected fold authority order",
                    ),
                );
            }
            visited += 1;
            return Ok(());
        }
        if original >= count - base.closure_count - 1 {
            if actual.class != ProtectedClass::Reader
                || !matches!(actual.value, ProtectedValue::Required(_))
            {
                return Err(
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "reader closure protected record",
                    ),
                );
            }
            visited = visited
                .checked_add(1)
                .ok_or(crate::property_graph::storage::tree::directory::TreeError::Work)?;
            return Ok(());
        }
        let expected = capture_record_at(capture, admitted, base, original).ok_or(
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "durable protected capture expected record",
            ),
        )?;
        if actual != expected {
            return Err(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "durable protected capture mismatch",
                ),
            );
        }
        visited = visited
            .checked_add(1)
            .ok_or(crate::property_graph::storage::tree::directory::TreeError::Work)?;
        Ok(())
    })?;
    if visited != count {
        return Err(NativeGraphError::Invalid(
            "durable protected capture incomplete",
        ));
    }
    Ok(())
}

fn select_rooted_candidates<'m>(
    lease: &NativeReadLease,
    storage: &'m StorageMemory<'m>,
    mark: crate::property_graph::storage::reclaim::DurableRun,
    writer: &spill::NativeSpillWriter<'_, 'm>,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<StorageBuffer<'m, crate::property_graph::wal::ArtifactDescriptor>, NativeGraphError> {
    const WINDOW_ROWS: usize = 8;
    let mut candidates = StorageBuffer::new(
        storage,
        crate::property_graph::storage::reclaim::MAX_CANDIDATES,
    )?;
    let root = lease
        .bundle()
        .roots()
        .directory(TreeKind::ObjectInventory)?;
    let mut reader = DurableRunReader::new(mark, storage)?;
    let mut live = reader.next(writer, resources)?;
    let mut lower = None;
    let mut complete = false;
    while !complete
        && candidates.as_slice().len() < crate::property_graph::storage::reclaim::MAX_CANDIDATES
    {
        let source = NativePreparationSource::new(lease, storage, 64)?;
        let mut cursor = DirectoryCursor::seek(
            &source,
            root,
            lower.as_ref().map(|key: &[u8; 16]| key.as_slice()),
            resources,
        )?;
        let mut rows = 0_usize;
        let mut last = None;
        while rows < WINDOW_ROWS
            && candidates.as_slice().len() < crate::property_graph::storage::reclaim::MAX_CANDIDATES
        {
            let Some(entry) = cursor.next_entry(resources)? else {
                complete = true;
                break;
            };
            let change = verify_inventory_entry(root, entry, resources)?;
            while live.is_some_and(|artifact| artifact < change.object.artifact) {
                live = reader.next(writer, resources)?;
            }
            if change.object.family == 17 && live != Some(change.object.artifact) {
                if !matches!(
                    change.state,
                    InventoryState::Prepared | InventoryState::Retained
                ) {
                    return Err(NativeGraphError::Invalid(
                        "active reclaim inventory requires resume",
                    ));
                }
                writer.validate_candidate(change.object, resources)?;
                candidates.push(change.object)?;
            }
            last = Some(change.object.artifact.get().to_le_bytes());
            rows += 1;
        }
        drop(cursor);
        drop(source);
        if complete
            || candidates.as_slice().len()
                == crate::property_graph::storage::reclaim::MAX_CANDIDATES
        {
            break;
        }
        let Some(next) = last.and_then(inventory_resume_after) else {
            break;
        };
        lower = Some(next);
    }
    Ok(candidates)
}

struct PreparedReclaimProof<'m> {
    pending_page_relocations:
        StorageBuffer<'m, crate::property_graph::storage::consolidation::PageRelocation>,
    drain: StorageBuffer<'m, crate::property_graph::storage::artifact::ArtifactId>,
    /// Complete unregistered objects this commit roots as bookkeeping.
    adoptions: StorageBuffer<'m, InventoryChange>,
    durable: spill::PreparedDurableSpill,
    candidates: StorageBuffer<'m, crate::property_graph::wal::ArtifactDescriptor>,
    /// Interrupted creations this commit's intent authorizes for unlink.
    partials: StorageBuffer<'m, crate::property_graph::storage::reclaim::PartialTarget>,
}

struct PendingReclaim<'m> {
    manifest: PendingIntentManifest,
    candidates: StorageBuffer<'m, crate::property_graph::wal::ArtifactDescriptor>,
    partials: StorageBuffer<'m, crate::property_graph::storage::reclaim::PartialTarget>,
    inventory: StorageBuffer<'m, InventoryChange>,
}

pub(super) struct ValidatedCompletedReclaim<'m> {
    admitted: NativeReadLease,
    reference: crate::property_graph::wal::RequiredRef,
    manifest: crate::property_graph::storage::reclaim::CompletedIntentManifest,
    reclaimed: StorageBuffer<'m, InventoryChange>,
}

impl ValidatedCompletedReclaim<'_> {
    pub(super) fn matches(&self, lease: &NativeReadLease) -> bool {
        self.admitted.token() == lease.token()
            && Arc::ptr_eq(self.admitted.bundle(), lease.bundle())
            && lease.bundle().reclaim() == Some(self.reference)
    }

    pub(super) const fn reference(&self) -> crate::property_graph::wal::RequiredRef {
        self.reference
    }

    pub(super) const fn manifest(
        &self,
    ) -> crate::property_graph::storage::reclaim::CompletedIntentManifest {
        self.manifest
    }

    pub(super) fn reclaimed(&self) -> &[InventoryChange] {
        self.reclaimed.as_slice()
    }
}

fn load_pending_reclaim<'m>(
    admission: &NativeMaintenanceAdmission,
    storage: &'m StorageMemory<'m>,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<PendingReclaim<'m>, NativeGraphError> {
    let admitted = admission.lease.bundle();
    let reclaim = admitted
        .reclaim()
        .ok_or(NativeGraphError::Invalid("pending reclaim root is absent"))?;
    let capacity = reclaim.block.length as usize;
    let mut bytes = StorageBuffer::new(storage, capacity)?;
    for _ in 0..capacity {
        bytes.push(0)?;
    }
    let source = NativePreparationSource::new(&admission.lease, storage, 1)?;
    // Foreground writes and checkpoints may land after the intent. They
    // cannot reach a candidate: a commit names only what its base reaches or
    // what it creates, and the candidates were unreachable from every
    // captured root. The proof pages keep the intent's own generation.
    let intent_generation = reclaim.object.generation;
    let length =
        source.copy_spill_page(reclaim, intent_generation, bytes.as_mut_slice(), resources)?;
    let payload = bytes
        .as_slice()
        .get(..length)
        .ok_or(NativeGraphError::Invalid("pending reclaim payload extent"))?;
    let manifest = decode_pending_intent_manifest(payload)?;
    if manifest.binding.store != admitted.base().store
        || manifest.binding.target_generation != intent_generation
        || intent_generation > admitted.base().generation
        || manifest
            .binding
            .sequence
            .checked_add(1)
            .is_none_or(|sequence| sequence > admitted.sequence())
        || manifest.binding.capture_generation >= manifest.binding.target_generation
        || manifest.binding.serial_fence > admitted.high_waters().creation_serial
    {
        return Err(NativeGraphError::Invalid(
            "pending reclaim bundle association",
        ));
    }
    let intent_id = BatchId::new(manifest.binding.session.get())?;
    let mut candidates = StorageBuffer::new(storage, manifest.candidate_count)?;
    let mut inventory = StorageBuffer::new(storage, manifest.candidate_count)?;
    for index in 0..manifest.candidate_count {
        let descriptor = pending_intent_candidate_at(payload, index)?;
        candidates.push(descriptor)?;
        if descriptor.family != 17 {
            continue;
        }
        inventory.push(InventoryChange {
            object: descriptor,
            state: InventoryState::ReclaimPending(intent_id),
        })?;
    }
    // Interrupted creations have no inventory row to move: they were never
    // registered, so there is nothing to mark pending and nothing to reclaim
    // from the tree. Only the file itself is removed.
    let mut partials = StorageBuffer::new(storage, manifest.partial_count)?;
    for index in 0..manifest.partial_count {
        partials.push(
            crate::property_graph::storage::reclaim::pending_intent_partial_at(payload, index)?,
        )?;
    }
    crate::property_graph::storage::inventory::validate_inventory_changes(
        &source,
        admitted.roots().directory(TreeKind::ObjectInventory)?,
        inventory.as_slice(),
        resources,
    )?;
    let reader = spill::NativeSpillReader::new(&admission.lease, storage, intent_generation)?;
    validate_protected_stream(
        manifest.protected,
        &reader,
        storage,
        resources,
        |record, resources| reader.validate_record(record, resources),
    )?;
    let mut mark = DurableRunReader::new(manifest.mark, storage)?;
    let mut candidate_index = 0_usize;
    let mut partial_index = 0_usize;
    while let Some(live) = mark.next(&reader, resources)? {
        while candidates
            .as_slice()
            .get(candidate_index)
            .is_some_and(|candidate| candidate.artifact < live)
        {
            candidate_index += 1;
        }
        if candidates
            .as_slice()
            .get(candidate_index)
            .is_some_and(|candidate| candidate.artifact == live)
        {
            return Err(NativeGraphError::Invalid(
                "pending reclaim candidate is marked live",
            ));
        }
        // Both partitions are sorted by artifact, so one walk answers both.
        while partials
            .as_slice()
            .get(partial_index)
            .is_some_and(|target| target.artifact < live)
        {
            partial_index += 1;
        }
        if partials
            .as_slice()
            .get(partial_index)
            .is_some_and(|target| target.artifact == live)
        {
            return Err(NativeGraphError::Invalid(
                "pending reclaim partial target is marked live",
            ));
        }
    }
    Ok(PendingReclaim {
        manifest,
        candidates,
        partials,
        inventory,
    })
}

fn load_completed_reclaim<'m>(
    admission: &NativeMaintenanceAdmission,
    storage: &'m StorageMemory<'m>,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<ValidatedCompletedReclaim<'m>, NativeGraphError> {
    let admitted = admission.lease.bundle();
    let reference = admitted.reclaim().ok_or(NativeGraphError::Invalid(
        "completed reclaim root is absent",
    ))?;
    let capacity = reference.block.length as usize;
    let mut bytes = StorageBuffer::new(storage, capacity)?;
    for _ in 0..capacity {
        bytes.push(0)?;
    }
    let source = NativePreparationSource::new(&admission.lease, storage, 1)?;
    let length = source.copy_spill_page(
        reference,
        reference.object.generation,
        bytes.as_mut_slice(),
        resources,
    )?;
    let payload = bytes
        .as_slice()
        .get(..length)
        .ok_or(NativeGraphError::Invalid(
            "completed reclaim payload extent",
        ))?;
    let manifest =
        crate::property_graph::storage::reclaim::decode_completed_intent_manifest(payload)?;
    if manifest.binding.store != admitted.base().store
        || manifest.binding.target_generation >= reference.object.generation
        || reference.object.generation > admitted.base().generation
        || manifest.remaining_count != 0
        || manifest
            .completed_count
            .checked_add(manifest.partial_count)
            .is_none_or(|rows| rows == 0)
    {
        return Err(NativeGraphError::Invalid(
            "completed reclaim bundle association",
        ));
    }
    let intent_capacity = manifest.intent.block.length as usize;
    let mut intent_bytes = StorageBuffer::new(storage, intent_capacity)?;
    for _ in 0..intent_capacity {
        intent_bytes.push(0)?;
    }
    let intent_length = source.copy_spill_page(
        manifest.intent,
        manifest.binding.target_generation,
        intent_bytes.as_mut_slice(),
        resources,
    )?;
    let intent_payload = intent_bytes
        .as_slice()
        .get(..intent_length)
        .ok_or(NativeGraphError::Invalid("completed reclaim intent extent"))?;
    let intent = decode_pending_intent_manifest(intent_payload)?;
    if intent.binding != manifest.binding
        || intent.candidate_count != manifest.completed_count
        || intent.partial_count != manifest.partial_count
    {
        return Err(NativeGraphError::Invalid(
            "completed reclaim original intent association",
        ));
    }
    // The completion repeats the intent's partial partition verbatim. An
    // interrupted creation has no inventory row, so this is the only place
    // the retirement can check that the completion names what was authorized.
    for index in 0..manifest.partial_count {
        if crate::property_graph::storage::reclaim::completed_intent_partial_at(payload, index)?
            != crate::property_graph::storage::reclaim::pending_intent_partial_at(
                intent_payload,
                index,
            )?
        {
            return Err(NativeGraphError::Invalid(
                "completed reclaim partial partition",
            ));
        }
    }
    let intent_id = BatchId::new(manifest.binding.session.get())?;
    let mut reclaimed = StorageBuffer::new(storage, manifest.completed_count)?;
    for index in 0..manifest.completed_count {
        let completed =
            crate::property_graph::storage::reclaim::completed_intent_candidate_at(payload, index)?;
        let original = pending_intent_candidate_at(intent_payload, index)?;
        if completed != original {
            return Err(NativeGraphError::Invalid(
                "completed reclaim target partition",
            ));
        }
        if completed.family != 17 {
            continue;
        }
        reclaimed.push(InventoryChange {
            object: completed,
            state: InventoryState::Reclaimed(intent_id),
        })?;
    }
    crate::property_graph::storage::inventory::validate_inventory_changes(
        &source,
        admitted.roots().directory(TreeKind::ObjectInventory)?,
        reclaimed.as_slice(),
        resources,
    )?;
    Ok(ValidatedCompletedReclaim {
        admitted: admission.lease.clone(),
        reference,
        manifest,
        reclaimed,
    })
}

pub(super) fn active_reclaim_subtype(
    store: &crate::lifecycle::Store,
    admission: &NativeMaintenanceAdmission,
    control: &crate::lifecycle::QueryControl,
) -> Result<u16, NativeGraphError> {
    let shared = GraphResources::from_store(store)?;
    let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
    let storage = StorageMemory::new(&write_memory, control, 8 * 1024 * 1024)?;
    let mut resources =
        crate::property_graph::storage::tree::directory::TreeResources::for_prepare(
            &storage,
            32 * 1024 * 1024,
        )?;
    let reclaim = admission
        .lease
        .bundle()
        .reclaim()
        .ok_or(NativeGraphError::Invalid("reclaim root is absent"))?;
    let mut bytes = StorageBuffer::new(&storage, reclaim.block.length as usize)?;
    for _ in 0..reclaim.block.length as usize {
        bytes.push(0)?;
    }
    let source = NativePreparationSource::new(&admission.lease, &storage, 1)?;
    // The root keeps its own generation when later commits carry it forward.
    let length = source.copy_spill_page(
        reclaim,
        reclaim.object.generation,
        bytes.as_mut_slice(),
        &mut resources,
    )?;
    let subtype = bytes
        .as_slice()
        .get(..length)
        .and_then(|payload| payload.get(8..10))
        .and_then(|value| value.first_chunk::<2>())
        .copied()
        .ok_or(NativeGraphError::Invalid("reclaim state subtype"))?;
    Ok(u16::from_le_bytes(subtype))
}

fn prepare_durable_proof<'m>(
    relocation_bytes: u64,
    target_generation: GraphGeneration,
    store: &crate::lifecycle::Store,
    admission: &NativeMaintenanceAdmission,
    storage: &'m StorageMemory<'m>,
    control: &crate::lifecycle::QueryControl,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'m>,
) -> Result<PreparedReclaimProof<'m>, NativeGraphError> {
    let capture = store.capture_native_read_roots()?;
    let admitted = admission.lease.bundle();
    // A newer published bundle has no retained lease to trace through, and
    // this preparation cannot publish over it. Refuse before any spill file
    // is created; the recheck under the writer lock stays authoritative.
    if !capture.is_current(admitted) {
        return Err(NativeGraphError::StalePreparation);
    }
    if target_generation <= admitted.base().generation {
        return Err(NativeGraphError::Invalid(
            "non-increasing maintenance generation",
        ));
    }
    let binding = SpillBinding {
        store: admitted.base().store,
        session: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)?,
        capture_generation: admitted.base().generation,
        target_generation,
        sequence: admitted.sequence(),
        serial_fence: capture.serial_fence(),
    };
    let mut writer = spill::NativeSpillWriter::new(
        store,
        &admission.lease,
        storage,
        control,
        binding,
        64 * 1024 * 1024,
    )?;
    let mut protected = ProtectedStreamBuilder::new(storage, binding)?;
    #[cfg(any(test, feature = "test-seams"))]
    let chunk = spill::qualification::chunk();
    #[cfg(not(any(test, feature = "test-seams")))]
    let chunk = crate::property_graph::storage::reclaim::SPILL_CHUNK_LIMIT;
    let mut mark = SpillMark::new(storage, binding, chunk)?;

    let mut census = {
        let source = NativePreparationSource::new(
            &admission.lease,
            storage,
            crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
        )?;
        crate::property_graph::storage::consolidation::pack_census(
            &source,
            admitted.roots(),
            admitted.prepared_inventories(),
            resources,
        )?
    };

    if admitted.base().fold.envelope_sequence != admitted.sequence()
        || admitted.base().fold.manifest_generation != admitted.base().generation.get()
    {
        return Err(NativeGraphError::Invalid(
            "reclaim capture must follow a fold",
        ));
    }
    let mut cancelled = || control.checkpoint().is_err();
    let mut wal_resources = WalResources::new(u64::MAX, STACK_RESERVATION_BYTES, &mut cancelled)?;
    let capacity = crate::property_graph::wal::commit_state_size(
        super::write::commit_state(admitted),
        &mut wal_resources,
    )?
    .checked_add(112)
    .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut payload = zeroed(storage, control, capacity)?;
    let (length, state_digest) = crate::property_graph::storage::reclaim::encode_captured_base(
        binding,
        admitted.base().fold,
        super::write::commit_state(admitted),
        payload.as_mut_slice(),
        &mut wal_resources,
    )?;
    let captured = writer
        .append_page(
            payload
                .as_slice()
                .get(..length)
                .ok_or(NativeGraphError::Invalid("captured base extent"))?,
            resources,
        )
        .map_err(|error| spill_error(&writer, error))?;
    let mut base = ProofBaseRecords {
        captured,
        closure_count: 0,
        fold: ProtectedRecord {
            class: ProtectedClass::Wal,
            value: ProtectedValue::FoldAuthority {
                manifest_generation: admitted.base().fold.manifest_generation,
                graph_absorbed_through: admitted.base().fold.graph_absorbed_through,
                envelope_sequence: admitted.base().fold.envelope_sequence,
                state_digest,
            },
        },
    };
    for index in 0..capture_record_count(&capture, admitted, base)? - 1 {
        let record = capture_record_at(&capture, admitted, base, index)
            .ok_or(NativeGraphError::Invalid("capture record index"))?;
        emit_protected(record, &mut protected, &mut mark, &mut writer, resources)?;
    }
    for bundle in capture.bundles() {
        let lease = capture.lease_for(bundle).ok_or(NativeGraphError::Invalid(
            "captured graph bundle has no retained lease",
        ))?;
        // S6c retains the reader's checkpoint/WAL identities, not its WAL history.
        // Removed by ZE-346 when the graph WAL and root selector are deleted.
        let wal_identity = super::recovery::checkpoint_wal_identity(
            store,
            bundle.directory(),
            bundle.root_envelope(),
            control,
        )?;
        mark.emit(
            crate::property_graph::storage::artifact::ArtifactId::new(wal_identity)
                .map_err(crate::property_graph::storage::tree::directory::TreeError::Format)?,
            &mut writer,
            resources,
        )?;
        // A fold changes only the publication token and legacy control locator.
        // Its retained admission has the same immutable CommitState as Current,
        // whose complete trace already proves this identical closure live.
        if !Arc::ptr_eq(bundle, admitted)
            && bundle.base().generation == admitted.base().generation
            && bundle.sequence() == admitted.sequence()
        {
            continue;
        }
        {
            let source = NativePreparationSource::new(
                lease,
                storage,
                crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            )?;
            let catalog = NativePreparationCatalog::open(&source, resources)?;
            let mut range_scratch = RangeScratch::for_prepare(storage, resources)?;
            if !Arc::ptr_eq(bundle, admitted) {
                let mut visitor = |reference, resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>| {
                    let required = reader_required(&source, reference, resources)?;
                    protected.emit(ProtectedRecord::required(ProtectedClass::Reader, required), &mut writer, resources)?;
                    mark.emit(reference.artifact, &mut writer, resources)?;
                    base.closure_count = base.closure_count.checked_add(1).ok_or(crate::property_graph::storage::tree::directory::TreeError::Work)?;
                    Ok(())
                };
                if let Err(error) = crate::property_graph::storage::reclaim::trace_graph_state(
                    &source,
                    &catalog,
                    bundle.roots(),
                    bundle.sequence(),
                    bundle.document(),
                    &mut range_scratch,
                    &mut visitor,
                    resources,
                ) {
                    return Err(spill_error(&writer, error));
                }
            } else if let Err(error) = crate::property_graph::storage::reclaim::trace_graph_bundle(
                &source,
                &catalog,
                bundle.roots(),
                bundle.sequence(),
                bundle.document(),
                &mut range_scratch,
                &mut mark,
                &mut writer,
                Arc::ptr_eq(bundle, admitted).then_some(census.as_mut_slice()),
                resources,
            ) {
                return Err(spill_error(&writer, error));
            }
        }
        {
            let mut state = {
                let source = NativePreparationSource::new(lease, storage, 64)?;
                let catalog = NativePreparationCatalog::open(&source, resources)?;
                SearchTraceCursor::for_preparation(&source, &catalog, resources)?.into_state()
            };
            let mut output = [None; crate::property_graph::storage::reclaim::TRACE_OUTPUT_LIMIT];
            loop {
                let source = NativePreparationSource::new(lease, storage, 64)?;
                let catalog = NativePreparationCatalog::open(&source, resources)?;
                let result = state.trace_preparation(&source, &catalog, &mut output, resources);
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        return Err(error.into());
                    }
                };
                for reference in output.iter().take(result.count).flatten().copied() {
                    if !Arc::ptr_eq(bundle, admitted) {
                        let required = reader_required(&source, reference, resources)?;
                        protected
                            .emit(
                                ProtectedRecord::required(ProtectedClass::Reader, required),
                                &mut writer,
                                resources,
                            )
                            .map_err(|error| spill_error(&writer, error))?;
                        base.closure_count = base
                            .closure_count
                            .checked_add(1)
                            .ok_or(NativeGraphError::IdentityExhausted)?;
                    }
                    if let Err(error) = mark.emit(reference.artifact, &mut writer, resources) {
                        return Err(spill_error(&writer, error));
                    }
                }
                if result.complete {
                    break;
                }
            }
        }
    }
    // Open proofs retain their full captured closure, including their role-7
    // base; a later fold must not make those proof objects reclaimable.
    for proof in capture.proofs() {
        let reader = spill::NativeSpillReader::new(
            &admission.lease,
            storage,
            proof.mark.binding.target_generation,
        )?;
        let mut cursor = DurableRunReader::new(proof.mark, storage)?;
        while let Some(artifact) = cursor.next(&reader, resources)? {
            mark.emit(artifact, &mut writer, resources)
                .map_err(|error| spill_error(&writer, error))?;
        }
    }
    emit_protected(base.fold, &mut protected, &mut mark, &mut writer, resources)?;
    let protected = match protected.finish(&mut writer, resources) {
        Ok(value) => value,
        Err(error) => return Err(spill_error(&writer, error)),
    };
    validate_authentic_protected_capture(
        &capture, admitted, protected, base, &writer, storage, resources,
    )?;
    let mark = match mark.finish(&mut writer, resources) {
        Ok(Some(value)) => value,
        Ok(None) => return Err(NativeGraphError::Invalid("empty completed native mark")),
        Err(error) => return Err(spill_error(&writer, error)),
    };
    let mut protected_count = 0_u64;
    if let Err(error) = validate_protected_stream(protected, &writer, storage, resources, |_, _| {
        protected_count = protected_count
            .checked_add(1)
            .ok_or(crate::property_graph::storage::tree::directory::TreeError::Work)?;
        Ok(())
    }) {
        return Err(spill_error(&writer, error));
    }
    if protected_count != protected.count {
        return Err(NativeGraphError::Invalid("protected stream record count"));
    }
    let mut reader = DurableRunReader::new(mark, storage)?;
    while let Some(_artifact) = match reader.next(&writer, resources) {
        Ok(value) => value,
        Err(error) => return Err(spill_error(&writer, error)),
    } {}
    let drain = crate::property_graph::storage::consolidation::select_drain(
        census.as_mut_slice(),
        relocation_bytes,
        storage,
    )?;
    let source = NativePreparationSource::new(
        &admission.lease,
        storage,
        crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
    )?;
    let pending_page_relocations =
        crate::property_graph::storage::consolidation::collect_drain_pages(
            &source,
            admitted.roots(),
            drain.as_slice(),
            storage,
            resources,
        )?;
    let mut candidates =
        select_rooted_candidates(&admission.lease, storage, mark, &writer, resources)?;
    // Both classes of unreachable file are selected before the intent is
    // written, because the intent is the one durable record that authorizes
    // either of them to be removed.
    let orphans::OrphanSelection {
        adoptions,
        partials,
        history,
    } = orphans::select_adoptions(
        admission,
        &capture,
        mark,
        &writer,
        storage,
        resources,
        crate::property_graph::storage::reclaim::MAX_CANDIDATES - candidates.as_slice().len(),
    )?;
    // Removed by ZE-346 when the graph WAL and root selector are deleted.
    for descriptor in history.as_slice().iter().copied() {
        candidates.push(descriptor)?;
    }
    candidates
        .as_mut_slice()
        .sort_unstable_by_key(|v| v.artifact);
    let (intent, intent_digest) =
        if candidates.as_slice().is_empty() && partials.as_slice().is_empty() {
            (None, 0)
        } else {
            let length = crate::property_graph::storage::reclaim::pending_intent_bytes(
                candidates.as_slice().len(),
                partials.as_slice().len(),
            )?;
            let mut payload = zeroed(storage, control, length)?;
            let (encoded, digest) = crate::property_graph::storage::reclaim::encode_pending_intent(
                binding,
                protected,
                mark,
                candidates.as_slice(),
                partials.as_slice(),
                payload.as_mut_slice(),
            )?;
            let bytes = payload
                .as_slice()
                .get(..encoded)
                .ok_or(NativeGraphError::Invalid("pending reclaim intent extent"))?;
            let reference = match writer.append_page(bytes, resources) {
                Ok(reference) => reference,
                Err(error) => return Err(spill_error(&writer, error)),
            };
            (Some(reference), digest)
        };
    let candidate_count = candidates.as_slice().len();
    let partial_count = partials.as_slice().len();
    let durable = writer.finish(
        capture,
        protected,
        mark,
        intent,
        intent_digest,
        candidate_count,
        partial_count,
    )?;
    Ok(PreparedReclaimProof {
        pending_page_relocations,
        drain,
        adoptions,
        durable,
        candidates,
        partials,
    })
}

#[cfg(any(test, feature = "test-seams"))]
#[derive(Debug)]
pub(super) struct SpillProbeReport {
    pub(super) ordered: Vec<u128>,
    pub(super) run: crate::property_graph::storage::reclaim::DurableRun,
    pub(super) spill_runs: usize,
    pub(super) merges: usize,
    pub(super) max_batch: usize,
    pub(super) created_objects: u64,
    pub(super) disk_bytes: u64,
    pub(super) maximum_encoded_backing: usize,
    pub(super) charged_peak_bytes: usize,
    pub(super) read_windows: u64,
    pub(super) maximum_mapped_window: u64,
    pub(super) released_each_read_window: bool,
    pub(super) mapped_bytes_released: bool,
    pub(super) allocation_head: Option<crate::property_graph::wal::RequiredRef>,
    pub(super) captured_head: Option<crate::property_graph::wal::RequiredRef>,
    pub(super) captured_pending: [Option<crate::property_graph::wal::ArtifactDescriptor>; 2],
    /// Mark pages the one authenticating `next` walk read.
    pub(super) walk_page_reads: u64,
    /// Membership questions asked through `DurableRunReader::contains`.
    pub(super) membership_queries: u64,
    /// Mark pages those membership questions read in total.
    pub(super) membership_page_reads: u64,
}

#[cfg(any(test, feature = "test-seams"))]
pub(super) fn run_spill_probe(
    store: &crate::lifecycle::Store,
    admission: &NativeMaintenanceAdmission,
    input: &[u128],
    chunk: usize,
) -> Result<SpillProbeReport, NativeGraphError> {
    use crate::property_graph::storage::artifact::ArtifactId;
    use crate::property_graph::storage::reclaim::{DurableRunReader, SpillBinding, SpillMark};

    let control = crate::lifecycle::QueryControl::Cancel(crate::lifecycle::CancelToken::new());
    let mapped_before = store.accounting.audit()?.mapped_bytes;
    let shared = GraphResources::from_store(store)?;
    let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
    let storage = StorageMemory::new(&write_memory, &control, 4 * 1024 * 1024)?;
    let mut resources =
        crate::property_graph::storage::tree::directory::TreeResources::for_prepare(
            &storage,
            64 * 1024 * 1024,
        )?;
    let admitted = admission.lease.bundle();
    let target_generation = super::write::assigned_generation(store, admitted.base().generation)?;
    let binding = SpillBinding {
        store: admitted.base().store,
        session: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)?,
        capture_generation: admitted.base().generation,
        target_generation,
        sequence: admitted.sequence(),
        serial_fence: admission.serial_fence,
    };
    let mut writer = spill::NativeSpillWriter::new(
        store,
        &admission.lease,
        &storage,
        &control,
        binding,
        64 * 1024 * 1024,
    )?;
    let mut mark = SpillMark::new(&storage, writer.binding(), chunk)?;
    for value in input {
        let id = ArtifactId::new(*value)
            .map_err(|_| NativeGraphError::Invalid("spill probe artifact identity"))?;
        if let Err(error) = mark.emit(id, &mut writer, &mut resources) {
            return Err(writer.take_failure().unwrap_or_else(|| error.into()));
        }
    }
    let run = match mark.finish(&mut writer, &mut resources) {
        Ok(Some(run)) => run,
        Ok(None) => return Err(NativeGraphError::Invalid("spill probe produced no run")),
        Err(error) => return Err(writer.take_failure().unwrap_or_else(|| error.into())),
    };
    let mut reader = DurableRunReader::new(run, &storage)?;
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(input.len()).map_err(|_| {
        NativeGraphError::Read(crate::property_graph::storage::tree::directory::TreeError::Memory)
    })?;
    let _ = crate::property_graph::storage::reclaim::take_mark_page_reads_for_test();
    loop {
        match reader.next(&writer, &mut resources) {
            Ok(Some(id)) => ordered.push(id.get()),
            Ok(None) => break,
            Err(error) => return Err(writer.take_failure().unwrap_or_else(|| error.into())),
        }
    }
    let walk_page_reads = crate::property_graph::storage::reclaim::take_mark_page_reads_for_test();
    // ZE-163: every id the walk emitted, asked again as an exact membership
    // question over the same authenticated run. Each answer must cost one
    // bounded root-to-leaf descent, not another walk.
    let mut membership_queries = 0_u64;
    let stride = ordered.len().div_ceil(256).max(1);
    for value in ordered.iter().step_by(stride) {
        let id = ArtifactId::new(*value)
            .map_err(|_| NativeGraphError::Invalid("spill probe artifact identity"))?;
        match reader.contains(id, &writer, &mut resources) {
            Ok(true) => membership_queries += 1,
            Ok(false) => {
                return Err(NativeGraphError::Invalid(
                    "spill probe membership is absent",
                ));
            }
            Err(error) => return Err(writer.take_failure().unwrap_or_else(|| error.into())),
        }
    }
    let membership_page_reads =
        crate::property_graph::storage::reclaim::take_mark_page_reads_for_test();
    let capture = store.capture_native_read_roots()?;
    let captured = capture
        .spills()
        .iter()
        .find(|spill| spill.admission_token == admission.lease.token())
        .copied()
        .ok_or(NativeGraphError::Invalid("spill probe capture is absent"))?;
    let stats = writer.stats();
    drop(writer);
    let mapped_bytes_released = store.accounting.audit()?.mapped_bytes == mapped_before;
    Ok(SpillProbeReport {
        ordered,
        run,
        spill_runs: mark.spill_runs(),
        merges: mark.merges(),
        max_batch: mark.max_batch(),
        created_objects: stats.created_objects,
        disk_bytes: stats.disk_bytes,
        maximum_encoded_backing: stats.maximum_encoded_backing,
        charged_peak_bytes: stats.charged_peak_bytes,
        read_windows: stats.read_windows,
        maximum_mapped_window: stats.maximum_mapped_window,
        released_each_read_window: stats.released_each_read_window,
        mapped_bytes_released,
        allocation_head: stats.allocation_head,
        captured_head: captured.head,
        captured_pending: captured.pending,
        walk_page_reads,
        membership_queries,
        membership_page_reads,
    })
}

fn resume_pending_reclaim(
    store: &crate::lifecycle::Store,
    admission: &NativeMaintenanceAdmission,
    control: &crate::lifecycle::QueryControl,
) -> Result<NativeMaintenanceReport, NativeGraphError> {
    let admitted = Arc::clone(admission.lease.bundle());
    let shared = GraphResources::from_store(store)?;
    let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
    let limits = MaintenanceLimits::default();
    let storage = StorageMemory::new(&write_memory, control, limits.storage_bytes)?;
    let mut resources =
        crate::property_graph::storage::tree::directory::TreeResources::for_prepare(
            &storage,
            limits.work,
        )?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-resume-start",
        resources.work(),
    );
    let pending = load_pending_reclaim(admission, &storage, &mut resources)?;
    let generation = super::write::assigned_generation(store, admitted.base().generation)?;
    let store_identity = admitted.base().store;
    let source = NativePreparationSource::new(&admission.lease, &storage, 64)?;
    let identity_source = || {
        let creation_serial = store.native_graph.burn_creation_serial().map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "native creation serial unavailable",
            )
        })?;
        Ok(ArtifactIdentity {
            store: store_identity,
            artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)
                .map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "native artifact identity unavailable",
                    )
                })?,
            generation,
            creation_serial,
        })
    };
    let mut objects = PreparedObjects::new(
        &source,
        identity_source,
        store_identity,
        generation,
        PackLimits::default(),
        &storage,
        &mut resources,
    )?;
    let intent_id = BatchId::new(pending.manifest.binding.session.get())?;
    let mut reclaimed = StorageBuffer::new(&storage, pending.candidates.as_slice().len())?;
    for descriptor in pending
        .candidates
        .as_slice()
        .iter()
        .copied()
        .filter(|v| v.family == 17)
    {
        reclaimed.push(InventoryChange {
            object: descriptor,
            state: InventoryState::Reclaimed(intent_id),
        })?;
    }
    let mut roots = admitted.roots().for_generation(generation)?;
    let mut tree = TreeScratch::for_prepare(&storage)?;
    let inventory_root = apply_inventory(
        &mut objects,
        roots.directory(TreeKind::ObjectInventory)?,
        reclaimed.as_slice(),
        generation,
        &mut tree,
        &mut resources,
    )?;
    roots.replace(inventory_root)?;
    let catalog = NativePreparationCatalog::open(&source, &mut resources)?;
    let sparse = prepare_sparse_maintenance(
        &source,
        &mut objects,
        SparseRoots {
            text: admitted.text(),
            vector: admitted.vector(),
        },
        admitted.roots(),
        roots,
        admitted.catalog(),
        &catalog,
        admitted.document(),
        admitted.lexical(),
        admitted
            .sequence()
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?,
        &[],
        &storage,
        &mut resources,
    )?;
    let completion_capacity = crate::property_graph::storage::reclaim::reclaim_completion_bytes(
        pending.candidates.as_slice().len(),
        pending.partials.as_slice().len(),
    )?;
    let mut completion_payload = zeroed(&storage, control, completion_capacity)?;
    let intent = admitted
        .reclaim()
        .ok_or(NativeGraphError::Invalid("pending reclaim root"))?;
    let (completion_length, _) =
        crate::property_graph::storage::reclaim::encode_reclaim_completion(
            pending.manifest.binding,
            intent,
            pending.candidates.as_slice(),
            &[],
            pending.partials.as_slice(),
            completion_payload.as_mut_slice(),
        )?;
    let completion_block = objects.append(
        BlockKind::CommitParticipant,
        generation,
        completion_payload
            .as_slice()
            .get(..completion_length)
            .ok_or(NativeGraphError::Invalid("reclaim completion extent"))?,
        &mut resources,
    )?;
    objects.finish(&mut resources)?;
    let mut inventory = StorageBuffer::new(&storage, objects.len())?;
    let mut completion_descriptor = None;
    let mut new_pack_bytes = 0_u64;
    for index in 0..objects.len() {
        let artifact = objects.artifact(index)?;
        let descriptor =
            artifact_descriptor(artifact.identity(), ContainerKind::Object, artifact.bytes())?;
        if descriptor.artifact == completion_block.artifact {
            completion_descriptor = Some(descriptor);
        }
        new_pack_bytes = new_pack_bytes
            .checked_add(u64::from(descriptor.bytes))
            .ok_or(NativeGraphError::IdentityExhausted)?;
        inventory.push(InventoryChange {
            object: descriptor,
            state: InventoryState::Prepared,
        })?;
    }
    let completion = crate::property_graph::wal::RequiredRef {
        object: completion_descriptor.ok_or(NativeGraphError::Invalid(
            "reclaim completion descriptor is absent",
        ))?,
        block: completion_block,
    };
    let _prepared_registration = admission.lease.register_prepared(inventory.as_slice())?;
    let inventory_identity = ArtifactIdentity {
        store: store_identity,
        artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)?,
        generation,
        creation_serial: store.native_graph.burn_creation_serial()?,
    };
    let inventory_payload = inventory_payload(&storage, control, inventory.as_slice(), None)?;
    let (inventory_bytes, inventory_ref) = encode_framed(
        &storage,
        control,
        ContainerKind::Object,
        inventory_identity,
        &[Block {
            kind: BlockKind::CommitParticipant,
            payload: inventory_payload.as_slice(),
        }],
    )?;
    let supplemental = [InventoryChange {
        object: inventory_ref.object,
        state: InventoryState::Prepared,
    }];
    let _supplemental_registration = admission.lease.register_prepared(&supplemental)?;
    let batch = BatchId::new(
        crate::property_graph::storage::allocation::fresh_store_identity(
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .map_err(|source| io(admitted.directory(), source))?
        .get(),
    )?;
    let mut envelope_bytes = zeroed(&storage, control, MAX_ENVELOPE_BYTES)?;
    let mut cancelled = || control.checkpoint().is_err();
    let mut wal_resources = WalResources::new(
        (MAX_ENVELOPE_BYTES as u64) * 4,
        STACK_RESERVATION_BYTES,
        &mut cancelled,
    )?;
    let artifact_count = objects
        .len()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut commit_artifacts = StorageBuffer::new(&storage, artifact_count)?;
    let (transition, generation) = prepare_reclaim_completion_transition(
        store,
        generation,
        &shared,
        &admission.lease,
        &objects,
        &sparse,
        &catalog,
        roots,
        pending.manifest.binding,
        intent,
        completion,
        pending.candidates.as_slice(),
        pending.partials.as_slice(),
        reclaimed.as_slice(),
        inventory.as_slice(),
        inventory_identity,
        inventory_bytes.as_slice(),
        inventory_ref,
        batch,
        envelope_bytes.as_mut_slice(),
        &mut commit_artifacts,
        &mut resources,
        &mut wal_resources,
    )?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-resume-prepared",
        resources.work(),
    );
    let mut writer_slot = store.native_graph.writer.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "native graph writer",
        })
    })?;
    let writer = writer_slot
        .as_mut()
        .ok_or_else(|| store.absent_native_graph_writer())?;
    if writer.stopped {
        return Err(NativeGraphError::WritesStopped);
    }
    if !store.native_graph.is_current_bundle(&admitted)? {
        return Err(NativeGraphError::StalePreparation);
    }
    let committed_tail = writer
        .wal
        .bytes
        .checked_sub(crate::property_graph::wal::HEADER_BYTES)
        .ok_or(NativeGraphError::Invalid("native WAL byte accounting"))?;
    let pending_tail = committed_tail
        .checked_add(transition.wal_bytes().len())
        .ok_or(NativeGraphError::IdentityExhausted)?;
    if writer.complete_envelopes >= 64
        || committed_tail >= MAX_ENVELOPE_BYTES
        || pending_tail > MAX_ENVELOPE_BYTES
    {
        super::write::checkpoint_current(store, writer, &admitted, &shared, control)?;
        return Err(NativeGraphError::StalePreparation);
    }
    let mut removed_bytes = 0_u64;
    let mut already_missing = 0_u64;
    for candidate in pending.candidates.as_slice().iter().copied() {
        let validator = NativePreparationSource::new(&admission.lease, &storage, 1)?;
        match validator.validate_object_descriptor(candidate, &mut resources) {
            Ok(()) => {
                let (path, path_charge) = NativePreparationSource::charged_candidate_path(
                    &storage,
                    admitted.directory(),
                    candidate,
                )?;
                match admitted.vfs().delete(&path) {
                    Ok(()) => {
                        removed_bytes = removed_bytes
                            .checked_add(u64::from(candidate.bytes))
                            .ok_or(NativeGraphError::IdentityExhausted)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        already_missing = already_missing
                            .checked_add(1)
                            .ok_or(NativeGraphError::IdentityExhausted)?;
                    }
                    Err(source) => return Err(io(&path, source)),
                }
                drop(path);
                drop(path_charge);
            }
            Err(crate::property_graph::storage::tree::directory::TreeError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                already_missing = already_missing
                    .checked_add(1)
                    .ok_or(NativeGraphError::IdentityExhausted)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    // Interrupted creations. There is no descriptor to validate, so the file's
    // own bytes are the proof: re-observe the length and the digest the intent
    // recorded and unlink only on an exact match. Anything else means the path
    // now holds something this intent never named, and it is retained.
    for target in pending.partials.as_slice().iter().copied() {
        let (path, path_charge) =
            NativePreparationSource::charged_path(&storage, admitted.directory(), target.artifact)?;
        let observed = match admitted.vfs().open(&path) {
            Ok(length) => Some(length),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(io(&path, source)),
        };
        match observed {
            None => {
                already_missing = already_missing
                    .checked_add(1)
                    .ok_or(NativeGraphError::IdentityExhausted)?;
            }
            Some(length)
                if length == target.observed
                    && orphans::observe_digest(admitted.vfs(), &path, length, &mut resources)?
                        == target.digest =>
            {
                match admitted.vfs().delete(&path) {
                    Ok(()) => {
                        removed_bytes = removed_bytes
                            .checked_add(target.observed)
                            .ok_or(NativeGraphError::IdentityExhausted)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        already_missing = already_missing
                            .checked_add(1)
                            .ok_or(NativeGraphError::IdentityExhausted)?;
                    }
                    Err(source) => return Err(io(&path, source)),
                }
            }
            Some(_) => {}
        }
        drop(path);
        drop(path_charge);
    }
    admitted
        .vfs()
        .sync(admitted.directory(), SyncKind::Full)
        .map_err(|source| io(admitted.directory(), source))?;
    let _ = protect_and_commit(store, writer, transition, control, false)?;
    let reclaimed_bytes = pending
        .candidates
        .as_slice()
        .iter()
        .try_fold(0_u64, |total, candidate| {
            total.checked_add(u64::from(candidate.bytes))
        })
        .and_then(|total| {
            pending
                .partials
                .as_slice()
                .iter()
                .try_fold(total, |total, target| total.checked_add(target.observed))
        })
        .ok_or(NativeGraphError::IdentityExhausted)?;
    Ok(NativeMaintenanceReport {
        relocated_bytes: 0,
        drained_packs: 0,
        replaced_physical_refs: 0,
        new_pack_bytes,
        generation,
        reclaimed_bytes,
        removed_bytes,
        already_missing,
    })
}

fn retire_completed_reclaim(
    store: &crate::lifecycle::Store,
    admission: &NativeMaintenanceAdmission,
    control: &crate::lifecycle::QueryControl,
) -> Result<NativeMaintenanceReport, NativeGraphError> {
    let admitted = Arc::clone(admission.lease.bundle());
    let shared = GraphResources::from_store(store)?;
    if admitted.needs_fold_before_retirement() {
        let mut writer_slot = store.native_graph.writer.lock().map_err(|_| {
            NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                component: "native graph writer",
            })
        })?;
        let writer = writer_slot
            .as_mut()
            .ok_or_else(|| store.absent_native_graph_writer())?;
        if writer.stopped {
            return Err(NativeGraphError::WritesStopped);
        }
        if !store.native_graph.is_current_bundle(&admitted)? {
            return Err(NativeGraphError::StalePreparation);
        }
        super::write::checkpoint_current(store, writer, &admitted, &shared, control)?;
        return Err(NativeGraphError::StalePreparation);
    }

    let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
    let limits = MaintenanceLimits::default();
    let storage = StorageMemory::new(&write_memory, control, limits.storage_bytes)?;
    let mut resources =
        crate::property_graph::storage::tree::directory::TreeResources::for_prepare(
            &storage,
            limits.work,
        )?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-retirement-start",
        resources.work(),
    );
    let completed = load_completed_reclaim(admission, &storage, &mut resources)?;
    let generation = super::write::assigned_generation(store, admitted.base().generation)?;
    let sequence = admitted
        .sequence()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let store_identity = admitted.base().store;
    let source = NativePreparationSource::new(&admission.lease, &storage, 64)?;
    let identity_source = || {
        let creation_serial = store.native_graph.burn_creation_serial().map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "native creation serial unavailable",
            )
        })?;
        Ok(ArtifactIdentity {
            store: store_identity,
            artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)
                .map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "native artifact identity unavailable",
                    )
                })?,
            generation,
            creation_serial,
        })
    };
    let mut objects = PreparedObjects::new(
        &source,
        identity_source,
        store_identity,
        generation,
        PackLimits {
            streams: crate::property_graph::storage::prepared::PackStreams::ByLifetime,
            ..PackLimits::default()
        },
        &storage,
        &mut resources,
    )?;
    let mut roots = admitted.roots().for_generation(generation)?;
    let mut tree = TreeScratch::for_prepare(&storage)?;
    let inventory_root = retire_reclaimed_inventory(
        &mut objects,
        roots.directory(TreeKind::ObjectInventory)?,
        completed.reclaimed(),
        generation,
        &mut tree,
        &mut resources,
    )?;
    roots.replace(inventory_root)?;
    let catalog = NativePreparationCatalog::open(&source, &mut resources)?;
    let sparse = prepare_sparse_maintenance(
        &source,
        &mut objects,
        SparseRoots {
            text: admitted.text(),
            vector: admitted.vector(),
        },
        admitted.roots(),
        roots,
        admitted.catalog(),
        &catalog,
        admitted.document(),
        admitted.lexical(),
        sequence,
        &[],
        &storage,
        &mut resources,
    )?;
    objects.finish(&mut resources)?;
    let mut inventory = StorageBuffer::new(&storage, objects.len())?;
    let mut new_pack_bytes = 0_u64;
    for index in 0..objects.len() {
        let artifact = objects.artifact(index)?;
        let descriptor =
            artifact_descriptor(artifact.identity(), ContainerKind::Object, artifact.bytes())?;
        new_pack_bytes = new_pack_bytes
            .checked_add(u64::from(descriptor.bytes))
            .ok_or(NativeGraphError::IdentityExhausted)?;
        inventory.push(InventoryChange {
            object: descriptor,
            state: InventoryState::Prepared,
        })?;
    }
    let _prepared_registration = admission.lease.register_prepared(inventory.as_slice())?;
    let inventory_identity = ArtifactIdentity {
        store: store_identity,
        artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)?,
        generation,
        creation_serial: store.native_graph.burn_creation_serial()?,
    };
    let inventory_payload = inventory_payload(&storage, control, inventory.as_slice(), None)?;
    let (inventory_bytes, inventory_ref) = encode_framed(
        &storage,
        control,
        ContainerKind::Object,
        inventory_identity,
        &[Block {
            kind: BlockKind::CommitParticipant,
            payload: inventory_payload.as_slice(),
        }],
    )?;
    let supplemental = [InventoryChange {
        object: inventory_ref.object,
        state: InventoryState::Prepared,
    }];
    let _supplemental_registration = admission.lease.register_prepared(&supplemental)?;
    let batch = BatchId::new(
        crate::property_graph::storage::allocation::fresh_store_identity(
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .map_err(|source| io(admitted.directory(), source))?
        .get(),
    )?;
    let mut envelope_bytes = zeroed(&storage, control, MAX_ENVELOPE_BYTES)?;
    let mut cancelled = || control.checkpoint().is_err();
    let mut wal_resources = WalResources::new(
        (MAX_ENVELOPE_BYTES as u64) * 4,
        STACK_RESERVATION_BYTES,
        &mut cancelled,
    )?;
    let artifact_count = objects
        .len()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut commit_artifacts = StorageBuffer::new(&storage, artifact_count)?;
    let (transition, generation) = prepare_reclaim_clear_transition(
        store,
        generation,
        &shared,
        &admission.lease,
        &completed,
        &objects,
        &sparse,
        &catalog,
        roots,
        inventory.as_slice(),
        inventory_identity,
        inventory_bytes.as_slice(),
        inventory_ref,
        batch,
        envelope_bytes.as_mut_slice(),
        &mut commit_artifacts,
        &mut resources,
        &mut wal_resources,
    )?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-retirement-prepared",
        resources.work(),
    );
    let mut writer_slot = store.native_graph.writer.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "native graph writer",
        })
    })?;
    let writer = writer_slot
        .as_mut()
        .ok_or_else(|| store.absent_native_graph_writer())?;
    if writer.stopped {
        return Err(NativeGraphError::WritesStopped);
    }
    if !store.native_graph.is_current_bundle(&admitted)? {
        return Err(NativeGraphError::StalePreparation);
    }
    let committed_tail = writer
        .wal
        .bytes
        .checked_sub(crate::property_graph::wal::HEADER_BYTES)
        .ok_or(NativeGraphError::Invalid("native WAL byte accounting"))?;
    let pending_tail = committed_tail
        .checked_add(transition.wal_bytes().len())
        .ok_or(NativeGraphError::IdentityExhausted)?;
    if writer.complete_envelopes >= 64
        || committed_tail >= MAX_ENVELOPE_BYTES
        || pending_tail > MAX_ENVELOPE_BYTES
    {
        super::write::checkpoint_current(store, writer, &admitted, &shared, control)?;
        return Err(NativeGraphError::StalePreparation);
    }
    let _ = protect_and_commit(store, writer, transition, control, false)?;
    let cleared = store.admit_native_read()?;
    let cleared_bundle = Arc::clone(cleared.bundle());
    super::write::checkpoint_current(store, writer, &cleared_bundle, &shared, control)?;
    store
        .native_graph
        .pack_bytes_since_reclaim
        .store(0, std::sync::atomic::Ordering::Relaxed);
    store
        .native_graph
        .commits_since_reclaim
        .store(0, std::sync::atomic::Ordering::Relaxed);
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-retired",
        resources.work(),
    );
    Ok(NativeMaintenanceReport {
        relocated_bytes: 0,
        drained_packs: 0,
        replaced_physical_refs: 0,
        new_pack_bytes,
        generation,
        reclaimed_bytes: 0,
        removed_bytes: 0,
        already_missing: 0,
    })
}

/// Observable result of one bounded native physical-maintenance commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeMaintenanceReport {
    pub(crate) relocated_bytes: u64,
    pub(crate) drained_packs: u32,
    pub(crate) replaced_physical_refs: u64,
    pub(crate) new_pack_bytes: u64,
    pub(crate) generation: GraphGeneration,
    pub(crate) reclaimed_bytes: u64,
    pub(crate) removed_bytes: u64,
    pub(crate) already_missing: u64,
}

/// Budgets for one maintenance preparation. Production always uses
/// `default()`; a focused test shrinks one field to fire that refusal on the
/// real path.
#[derive(Clone, Copy)]
pub(crate) struct MaintenanceLimits {
    pub(crate) relocation_bytes: u64,
    pub(crate) sweep_limit: usize,
    pub(crate) inventory_additions: usize,
    pub(crate) storage_bytes: usize,
    pub(crate) work: u64,
}

impl Default for MaintenanceLimits {
    fn default() -> Self {
        Self {
            sweep_limit: 128,
            relocation_bytes: crate::property_graph::storage::consolidation::RELOCATION_BYTES,
            inventory_additions: INVENTORY_FOLD_ADDITION_LIMIT,
            storage_bytes: 32 * 1024 * 1024,
            work: 1024 * 1024 * 1024,
        }
    }
}

pub(super) fn commit(
    store: &crate::lifecycle::Store,
    admission: &NativeMaintenanceAdmission,
    control: &crate::lifecycle::QueryControl,
) -> Result<NativeMaintenanceReport, NativeGraphError> {
    commit_with_limits(store, admission, control, MaintenanceLimits::default())
}

pub(super) fn commit_with_limits(
    store: &crate::lifecycle::Store,
    admission: &NativeMaintenanceAdmission,
    control: &crate::lifecycle::QueryControl,
    limits: MaintenanceLimits,
) -> Result<NativeMaintenanceReport, NativeGraphError> {
    if admission.lease.bundle().reclaim().is_some() {
        return match active_reclaim_subtype(store, admission, control)? {
            2 => resume_pending_reclaim(store, admission, control),
            3 => retire_completed_reclaim(store, admission, control),
            _ => Err(NativeGraphError::Invalid(
                "unsupported reclaim state subtype",
            )),
        };
    }
    store.native_graph.require_writable()?;
    control
        .checkpoint()
        .map_err(|_| NativeGraphError::Stage(StageError::Cancelled))?;
    let admitted = Arc::clone(admission.lease.bundle());
    if admitted.sequence() == 0 {
        // Nothing was ever committed: no record, sparse state or inventory
        // exists to move, fold or reclaim. Publishing a Maintenance envelope
        // over the empty base would only burn a generation.
        return Ok(NativeMaintenanceReport {
            relocated_bytes: 0,
            drained_packs: 0,
            replaced_physical_refs: 0,
            new_pack_bytes: 0,
            generation: admitted.base().generation,
            reclaimed_bytes: 0,
            removed_bytes: 0,
            already_missing: 0,
        });
    }
    let shared = GraphResources::from_store(store)?;
    let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
    let storage = StorageMemory::new(&write_memory, control, limits.storage_bytes)?;
    let mut resources =
        crate::property_graph::storage::tree::directory::TreeResources::for_prepare(
            &storage,
            limits.work,
        )?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-start",
        resources.work(),
    );
    let folded_inventory = {
        let fold_source = NativePreparationSource::new(
            &admission.lease,
            &storage,
            crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
        )?;
        prepare_inventory_fold(
            &fold_source,
            admitted.prepared_inventories(),
            admitted.roots(),
            admitted.sequence(),
            admission.lease.token(),
            limits.inventory_additions,
            &storage,
            &mut resources,
        )?
    };
    #[cfg(any(test, feature = "test-seams"))]
    let work_before_proof = resources.work();
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-fold-end",
        resources.work(),
    );
    if admitted.base().fold.envelope_sequence != admitted.sequence() {
        let mut slot = store
            .native_graph
            .writer
            .lock()
            .map_err(|_| NativeGraphError::Invalid("native graph writer lock"))?;
        let writer = slot
            .as_mut()
            .ok_or_else(|| store.absent_native_graph_writer())?;
        if !store.native_graph.is_current_bundle(&admitted)? {
            return Err(NativeGraphError::StalePreparation);
        }
        super::write::checkpoint_current(store, writer, &admitted, &shared, control)?;
        drop(slot);
        drop(folded_inventory);
        drop(resources);
        drop(storage);
        let folded = store.admit_native_graph_maintenance()?;
        return commit_with_limits(store, &folded, control, limits);
    }
    let generation = super::write::assigned_generation(store, admitted.base().generation)?;
    let proof = prepare_durable_proof(
        limits.relocation_bytes,
        generation,
        store,
        admission,
        &storage,
        control,
        &mut resources,
    )?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-proof-end",
        resources.work(),
    );
    #[cfg(any(test, feature = "test-seams"))]
    store.native_graph.proof_work.store(
        resources.work().saturating_sub(work_before_proof),
        std::sync::atomic::Ordering::Release,
    );
    let reclaim_id = BatchId::new(proof.durable.binding().session.get())?;
    let mut reclaim_pending = StorageBuffer::new(&storage, proof.candidates.as_slice().len())?;
    for descriptor in proof
        .candidates
        .as_slice()
        .iter()
        .copied()
        .filter(|v| v.family == 17)
    {
        reclaim_pending.push(InventoryChange {
            object: descriptor,
            state: InventoryState::ReclaimPending(reclaim_id),
        })?;
    }
    // Replacement's sweep and directory edits share one attempt-wide source,
    // just like structured writes. Retain their authentication proofs under
    // the existing native artifact capacity rather than a 64-object window.
    let source = NativePreparationSource::new(
        &admission.lease,
        &storage,
        crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
    )?;
    let catalog = NativePreparationCatalog::open(&source, &mut resources)?;
    let sequence = admitted
        .sequence()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let store_identity = admitted.base().store;
    let identity_source = || {
        let creation_serial = store.native_graph.burn_creation_serial().map_err(|_| {
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "native creation serial unavailable",
            )
        })?;
        Ok(ArtifactIdentity {
            store: store_identity,
            artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)
                .map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "native artifact identity unavailable",
                    )
                })?,
            generation,
            creation_serial,
        })
    };
    let mut objects = PreparedObjects::new(
        &source,
        identity_source,
        store_identity,
        generation,
        PackLimits {
            // The subsequent WAL envelope reserves 16 MiB from the same
            // 32 MiB storage allowance, so bound maintenance's output packs.
            artifact_bytes: 2 * 1024 * 1024,
            streams: crate::property_graph::storage::prepared::PackStreams::ByLifetime,
            ..PackLimits::default()
        },
        &storage,
        &mut resources,
    )?;
    let drained_packs = u32::try_from(proof.drain.as_slice().len())
        .map_err(|_| NativeGraphError::IdentityExhausted)?;
    let mut sweep_progress = store.native_graph.sweep_resume.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "native graph sweep progress",
        })
    })?;
    let mut next_sweep = *sweep_progress;
    let consolidated = prepare_one_replacement(
        &source,
        &mut objects,
        admitted.roots(),
        admitted.sequence(),
        generation,
        &catalog,
        admitted.document(),
        proof.drain.as_slice(),
        limits.relocation_bytes,
        limits.sweep_limit,
        &mut next_sweep,
        folded_inventory,
        proof.pending_page_relocations.as_slice(),
        reclaim_pending.as_slice(),
        proof.adoptions.as_slice(),
        &storage,
        &mut resources,
    )?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-directories-end",
        resources.work(),
    );
    let sparse = prepare_sparse_maintenance(
        &source,
        &mut objects,
        SparseRoots {
            text: admitted.text(),
            vector: admitted.vector(),
        },
        admitted.roots(),
        consolidated.roots(),
        admitted.catalog(),
        &catalog,
        admitted.document(),
        admitted.lexical(),
        sequence,
        consolidated.relocations(),
        &storage,
        &mut resources,
    )?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-sparse-end",
        resources.work(),
    );
    objects.finish(&mut resources)?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-sealing-end",
        resources.work(),
    );

    let mut inventory = StorageBuffer::new(&storage, objects.len())?;
    let mut new_pack_bytes = 0_u64;
    for index in 0..objects.len() {
        let artifact = objects.artifact(index)?;
        let descriptor =
            artifact_descriptor(artifact.identity(), ContainerKind::Object, artifact.bytes())?;
        new_pack_bytes = new_pack_bytes
            .checked_add(u64::from(descriptor.bytes))
            .ok_or(NativeGraphError::IdentityExhausted)?;
        inventory.push(InventoryChange {
            object: descriptor,
            state: InventoryState::Prepared,
        })?;
    }
    let _prepared_registration = admission.lease.register_prepared(inventory.as_slice())?;
    let inventory_serial = store.native_graph.burn_creation_serial()?;
    let inventory_identity = ArtifactIdentity {
        store: store_identity,
        artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)?,
        generation,
        creation_serial: inventory_serial,
    };
    let inventory_payload = inventory_payload(&storage, control, inventory.as_slice(), None)?;
    let (inventory_bytes, inventory_ref) = encode_framed(
        &storage,
        control,
        ContainerKind::Object,
        inventory_identity,
        &[Block {
            kind: BlockKind::CommitParticipant,
            payload: inventory_payload.as_slice(),
        }],
    )?;
    let supplemental = [InventoryChange {
        object: inventory_ref.object,
        state: InventoryState::Prepared,
    }];
    let _supplemental_registration = admission.lease.register_prepared(&supplemental)?;

    let batch = BatchId::new(
        crate::property_graph::storage::allocation::fresh_store_identity(
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .map_err(|source| io(admitted.directory(), source))?
        .get(),
    )?;
    let mut envelope_bytes = zeroed(&storage, control, MAX_ENVELOPE_BYTES)?;
    let mut cancelled = || control.checkpoint().is_err();
    let mut wal_resources = WalResources::new(
        (MAX_ENVELOPE_BYTES as u64) * 4,
        STACK_RESERVATION_BYTES,
        &mut cancelled,
    )?;
    let artifact_count = objects
        .len()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut commit_artifacts = StorageBuffer::new(&storage, artifact_count)?;
    let PreparedReclaimProof {
        pending_page_relocations: _,
        drain: _,
        adoptions: _adoptions,
        durable,
        candidates,
        partials,
    } = proof;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-transition-start",
        resources.work(),
    );
    let (transition, generation) = prepare_maintenance_transition(
        store,
        generation,
        &shared,
        &admission.lease,
        &objects,
        &consolidated,
        &sparse,
        Some(durable),
        &catalog,
        inventory.as_slice(),
        reclaim_pending.as_slice(),
        candidates.as_slice(),
        partials.as_slice(),
        inventory_identity,
        inventory_bytes.as_slice(),
        inventory_ref,
        batch,
        envelope_bytes.as_mut_slice(),
        &mut commit_artifacts,
        &mut resources,
        &mut wal_resources,
    )?;

    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "maintenance-transition-end",
        resources.work(),
    );
    #[cfg(any(test, feature = "test-seams"))]
    {
        let hook = store
            .native_graph
            .state
            .lock()
            .map_err(|_| {
                NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                    component: "native graph publication",
                })
            })?
            .maintenance_writer_hook
            .take();
        if let Some((entered, release)) = hook {
            entered.wait();
            release.wait();
        }
    }
    let mut writer_slot = store.native_graph.writer.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "native graph writer",
        })
    })?;
    let writer = writer_slot
        .as_mut()
        .ok_or_else(|| store.absent_native_graph_writer())?;
    if writer.stopped {
        return Err(NativeGraphError::WritesStopped);
    }
    if !store.native_graph.is_current_bundle(&admitted)? {
        return Err(NativeGraphError::StalePreparation);
    }
    let committed_tail = writer
        .wal
        .bytes
        .checked_sub(crate::property_graph::wal::HEADER_BYTES)
        .ok_or(NativeGraphError::Invalid("native WAL byte accounting"))?;
    if writer.complete_envelopes >= 64 || committed_tail >= MAX_ENVELOPE_BYTES {
        super::write::checkpoint_current(store, writer, &admitted, &shared, control)?;
        return Err(NativeGraphError::StalePreparation);
    }
    let pending_tail = committed_tail
        .checked_add(transition.wal_bytes().len())
        .ok_or(NativeGraphError::IdentityExhausted)?;
    if pending_tail > MAX_ENVELOPE_BYTES {
        super::write::checkpoint_current(store, writer, &admitted, &shared, control)?;
        return Err(NativeGraphError::StalePreparation);
    }
    let _ = protect_and_commit(store, writer, transition, control, false)?;
    *sweep_progress = next_sweep;
    Ok(NativeMaintenanceReport {
        relocated_bytes: consolidated.relocated_bytes(),
        drained_packs,
        replaced_physical_refs: consolidated.replaced_physical_refs(),
        new_pack_bytes,
        generation,
        reclaimed_bytes: 0,
        removed_bytes: 0,
        already_missing: 0,
    })
}

fn reader_required(
    source: &NativePreparationSource<'_, '_>,
    reference: crate::property_graph::storage::artifact::PhysicalRef,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<
    crate::property_graph::wal::RequiredRef,
    crate::property_graph::storage::tree::directory::TreeError,
> {
    use crate::property_graph::storage::tree::directory::BlockSource;
    let block = source.resolve(reference, resources)?;
    let identity = block.identity();
    Ok(crate::property_graph::wal::RequiredRef {
        object: crate::property_graph::wal::ArtifactDescriptor {
            store: identity.store,
            artifact: identity.artifact,
            generation: identity.generation,
            serial: identity.creation_serial,
            bytes: u32::try_from(block.file_length())
                .map_err(|_| crate::property_graph::storage::tree::directory::TreeError::Memory)?,
            family: 17,
            version: 1,
            checksum: block.file_checksum(),
        },
        block: reference,
    })
}
