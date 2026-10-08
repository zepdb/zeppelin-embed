use super::base::NativeAdmittedBase;
use super::maintenance::{ValidatedCompletedReclaim, spill::PreparedDurableSpill};
use super::persistence::{
    artifact_descriptor, catalog_payload, encode_framed, next_artifact, zeroed,
};
use super::{
    NativeGraphBundle, NativeGraphBundleInput, NativeGraphError, NativeGraphPublication,
    NativeReadLease,
};
use crate::property_graph::catalog::SymbolEntry;
use crate::property_graph::resources::{GraphReservation, GraphResources};
use crate::property_graph::staging::{
    ItemReceipt, ResultLayout, ResultMaterializer, ResultRegistration, StageError, StagedBatch,
    WriteLimits, WriteMemory, WritePhase, stage_structured_with_results_at_generation,
};
use crate::property_graph::storage::artifact::{ArtifactIdentity, Block, BlockKind, ContainerKind};
use crate::property_graph::storage::consolidation::ConsolidationOutcome;
use crate::property_graph::storage::inventory::{
    validate_inventory_changes, validate_inventory_retirement,
};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::participant::BatchCatalog;
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::prepared::{PackLimits, PreparedObjects};
use crate::property_graph::storage::records::{RecordCatalog, verify_record};
use crate::property_graph::storage::search::{
    PreparedSparseCheckpoint, SparseCheckpoint, SparseRoots,
    validate_persisted_maintenance_transition,
};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::TreeKind;
use crate::property_graph::storage::tree::directory::{
    BlockSource, GraphRoots, TreeError, lookup_entry,
};
use crate::property_graph::storage::{
    GraphPreparation, NativePreparationCatalog, NativePreparationSource, PreparedGraphArtifacts,
};
use crate::property_graph::wal::{
    BatchId, Change, CommitState, DescriptorList, Envelope, EnvelopeKind, HighWaters,
    InventoryChange, InventoryState, MAX_ENVELOPE_BYTES, Membership, Mutation, ReclaimComplete,
    ReclaimIntent, ReferenceList, RequiredRef, STACK_RESERVATION_BYTES, WalGraphRoots,
    WalResources, encode_envelope,
};
use crate::property_graph::{BatchDisposition, EntityId, GraphGeneration};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

/// Graph-only coordinator allocation. Unified commits supply this value to staging.
pub(super) fn assigned_generation(
    store: &crate::lifecycle::Store,
    base: GraphGeneration,
) -> Result<GraphGeneration, NativeGraphError> {
    #[cfg(test)]
    {
        let assigned = store
            .native_graph
            .assigned_generation
            .load(std::sync::atomic::Ordering::Acquire);
        if assigned != 0 {
            let assigned = GraphGeneration::new(assigned);
            if assigned <= base {
                return Err(NativeGraphError::Invalid(
                    "non-increasing assigned generation",
                ));
            }
            return Ok(assigned);
        }
    }
    let current = store.active.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "active segment",
        })
    })?;
    let current = current
        .as_ref()
        .ok_or(crate::lifecycle::StoreError::Closed)?
        .generation;
    if current < base.get() {
        return Err(NativeGraphError::Invalid(
            "graph generation exceeds store generation",
        ));
    }
    current
        .checked_add(1)
        .map(GraphGeneration::new)
        .ok_or(NativeGraphError::IdentityExhausted)
}

pub(super) struct NativeWriter {
    pub(super) last_graph_seq: u64,
    pub(super) envelope_bytes: usize,
    pub(super) complete_envelopes: u64,
    pub(super) stopped: bool,
    pub(super) checkpoint_failed: bool,
    pub(super) protected: Vec<crate::property_graph::wal::ArtifactDescriptor>,
    pub(super) durable_protected: Vec<NativeDurableProtection>,
    _protected_charge: GraphReservation,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_reclaim_completion_transition<'p, 'a, 'b, S, F, C>(
    store: &crate::lifecycle::Store,
    target_generation: GraphGeneration,
    shared: &GraphResources,
    lease: &'p NativeReadLease,
    objects: &'p PreparedObjects<'a, 'b, S, F>,
    sparse_checkpoint: &PreparedSparseCheckpoint,
    catalog: &C,
    roots: GraphRoots,
    binding: crate::property_graph::storage::reclaim::SpillBinding,
    intent: RequiredRef,
    completion: RequiredRef,
    candidates: &'p [crate::property_graph::wal::ArtifactDescriptor],
    partials: &'p [crate::property_graph::storage::reclaim::PartialTarget],
    reclaimed: &'p [InventoryChange],
    inventory: &'p [InventoryChange],
    inventory_identity: ArtifactIdentity,
    inventory_bytes: &'p [u8],
    inventory_ref: RequiredRef,
    batch: BatchId,
    output: &'p mut [u8],
    commit_artifacts: &'p mut StorageBuffer<'_, NativeCommitArtifact<'p>>,
    tree_resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
    wal_resources: &mut WalResources<'_>,
) -> Result<(NativeCommittedTransition<'p>, GraphGeneration), NativeGraphError>
where
    S: BlockSource,
    F: FnMut() -> Result<ArtifactIdentity, TreeError>,
    C: RecordCatalog<PreparedObjects<'a, 'b, S, F>>,
{
    let admitted = lease.bundle();
    let generation = target_generation;
    let sequence = admitted
        .sequence()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let intent_id = BatchId::new(binding.session.get())?;
    if !objects.is_finished()
        || objects.store() != admitted.base().store
        || objects.generation() != generation
        || roots.store() != admitted.base().store
        || roots.generation() != generation
        || !sparse_checkpoint.matches(
            SparseRoots {
                text: admitted.text(),
                vector: admitted.vector(),
            },
            admitted.roots(),
            roots,
            sequence,
            admitted.catalog(),
        )
        || binding.store != admitted.base().store
        // Foreground commits may carry a pending intent forward, so the
        // intent binds its own generation, at or before the admitted base.
        || binding.target_generation != intent.object.generation
        || binding.target_generation > admitted.base().generation
        || binding
            .sequence
            .checked_add(1)
            .is_none_or(|sequence| sequence > admitted.sequence())
        || admitted.reclaim() != Some(intent)
        || completion.object.store != admitted.base().store
        || completion.object.generation != generation
        || completion.block.artifact != completion.object.artifact
        || completion.block.kind != BlockKind::CommitParticipant
        || (candidates.is_empty() && partials.is_empty())
        || candidates.iter().filter(|v| v.family == 17).count() != reclaimed.len()
        || candidates.iter().filter(|v| v.family == 17).zip(reclaimed).any(|(candidate, change)| {
            change.object != *candidate || change.state != InventoryState::Reclaimed(intent_id)
        })
        || inventory.len() != objects.len()
        || commit_artifacts.capacity()
            != objects
                .len()
                .checked_add(1)
                .ok_or(NativeGraphError::Invalid("completion artifact count"))?
        || !commit_artifacts.as_slice().is_empty()
    {
        return Err(NativeGraphError::Invalid(
            "prepared reclaim completion facts",
        ));
    }
    for (base, target) in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::KeyFences,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
        TreeKind::OutRanges,
        TreeKind::InRanges,
    ]
    .into_iter()
    .map(|kind| (admitted.roots().directory(kind), roots.directory(kind)))
    {
        if base?.reference() != target?.reference() {
            return Err(NativeGraphError::Invalid(
                "reclaim completion changed logical graph root",
            ));
        }
    }
    validate_inventory_changes(
        objects,
        roots.directory(TreeKind::ObjectInventory)?,
        reclaimed,
        tree_resources,
    )?;
    let mut greatest_serial = admitted.high_waters().creation_serial;
    for (index, change) in inventory.iter().enumerate() {
        let artifact = objects.artifact(index)?;
        let descriptor =
            artifact_descriptor(artifact.identity(), ContainerKind::Object, artifact.bytes())?;
        if change.object != descriptor
            || change.state != InventoryState::Prepared
            || descriptor.generation != generation
            || descriptor.serial <= admitted.high_waters().creation_serial
        {
            return Err(NativeGraphError::Invalid(
                "reclaim completion prepared inventory",
            ));
        }
        greatest_serial = greatest_serial.max(descriptor.serial);
    }
    if inventory_identity.store != admitted.base().store
        || inventory_identity.generation != generation
        || inventory_identity.creation_serial <= greatest_serial
    {
        return Err(NativeGraphError::Invalid(
            "reclaim completion inventory identity",
        ));
    }
    let inventory_artifact = SupplementalArtifact {
        identity: inventory_identity,
        bytes: inventory_bytes,
        required: inventory_ref,
    };
    verify_prepared_inventory(inventory_artifact, inventory, None, tree_resources)?;
    let sparse = sparse_checkpoint.finalize(inventory)?;

    validate_persisted_maintenance_transition(
        objects,
        SparseCheckpoint {
            cutoff: admitted.sequence(),
            roots: SparseRoots {
                text: admitted.text(),
                vector: admitted.vector(),
            },
        },
        admitted.roots(),
        sparse,
        roots,
        admitted.catalog(),
        catalog,
        admitted.document(),
        admitted.lexical(),
        objects.memory(),
        tree_resources,
    )?;

    let graph = wal_roots(roots, inventory, admitted.wal_roots())?;
    let reference_count = admitted
        .prepared_inventories()
        .len()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut prepared_refs = StorageBuffer::new(objects.memory(), reference_count)?;
    prepared_refs.extend_from_slice(admitted.prepared_inventories())?;
    prepared_refs.push(inventory_ref)?;
    let state = CommitState {
        store: admitted.base().store,
        generation,
        sequence,
        graph,
        catalog: admitted.catalog(),
        vector: sparse.vector,
        text: sparse.text,
        reclaim: Some(completion),
        high_waters: HighWaters {
            creation_serial: inventory_identity.creation_serial,
            ..admitted.high_waters()
        },
        prepared_inventories: ReferenceList::Values(prepared_refs.as_slice()),
    };
    let change_count = inventory
        .len()
        .checked_add(reclaimed.len())
        .and_then(|count| count.checked_add(2))
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut changes = StorageBuffer::new(objects.memory(), change_count)?;
    for change in inventory {
        changes.push(Change::Inventory(*change))?;
    }
    for change in reclaimed {
        changes.push(Change::Inventory(*change))?;
    }
    changes.push(Change::Inventory(InventoryChange {
        object: inventory_ref.object,
        state: InventoryState::Prepared,
    }))?;
    changes.push(Change::ReclaimComplete(ReclaimComplete {
        id: intent_id,
        intent,
        completed: DescriptorList::Values(candidates),
        remaining: DescriptorList::Values(&[]),
    }))?;
    let encoded_length = encode_envelope(
        commit_state(admitted),
        Envelope {
            batch,
            kind: EnvelopeKind::Maintenance,
            changes: changes.as_slice(),
            state,
        },
        output,
        wal_resources,
    )?;
    let _input_charge = shared.reserve(
        reference_count
            .checked_mul(std::mem::size_of::<RequiredRef>())
            .ok_or(NativeGraphError::IdentityExhausted)?,
    )?;
    let next = NativeGraphBundle::assemble_committed(
        store,
        target_generation,
        shared,
        admitted,
        NativeGraphBundleInput {
            base: crate::property_graph::staging::BaseIdentity {
                store: admitted.base().store,
                generation,
                fold: admitted.base().fold,
                roots: admitted.base().roots,
            },
            root_envelope: admitted.root_envelope(),
            roots,
            wal_roots: graph,
            sequence,
            catalog: admitted.catalog(),
            vector: sparse.vector,
            text: sparse.text,
            reclaim: Some(completion),
            high_waters: state.high_waters,
            prepared_inventories: prepared_refs.as_slice().to_vec(),
            lexical: admitted.lexical(),
            document: admitted.document().cloned(),
        },
    )?;
    for index in 0..objects.len() {
        let artifact = objects.artifact(index)?;
        commit_artifacts.push(NativeCommitArtifact {
            identity: artifact.identity(),
            descriptor: inventory
                .get(index)
                .ok_or(NativeGraphError::Invalid("completion artifact descriptor"))?
                .object,
            bytes: artifact.bytes(),
        })?;
    }
    commit_artifacts.push(NativeCommitArtifact {
        identity: inventory_identity,
        descriptor: inventory_ref.object,
        bytes: inventory_bytes,
    })?;
    Ok((
        NativeCommittedTransition {
            admitted: Arc::clone(admitted),
            next,
            encoded: output
                .get(..encoded_length)
                .ok_or(NativeGraphError::Invalid("completion WAL extent"))?,
            artifacts: commit_artifacts.as_slice(),
            durable: None,
        },
        generation,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_reclaim_clear_transition<'p, 'a, 'b, S, F, C>(
    store: &crate::lifecycle::Store,
    target_generation: GraphGeneration,
    shared: &GraphResources,
    lease: &'p NativeReadLease,
    completed: &ValidatedCompletedReclaim<'_>,
    objects: &'p PreparedObjects<'a, 'b, S, F>,
    sparse_checkpoint: &PreparedSparseCheckpoint,
    catalog: &C,
    roots: GraphRoots,
    inventory: &'p [InventoryChange],
    inventory_identity: ArtifactIdentity,
    inventory_bytes: &'p [u8],
    inventory_ref: RequiredRef,
    batch: BatchId,
    output: &'p mut [u8],
    commit_artifacts: &'p mut StorageBuffer<'_, NativeCommitArtifact<'p>>,
    tree_resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
    wal_resources: &mut WalResources<'_>,
) -> Result<(NativeCommittedTransition<'p>, GraphGeneration), NativeGraphError>
where
    S: BlockSource,
    F: FnMut() -> Result<ArtifactIdentity, TreeError>,
    C: RecordCatalog<PreparedObjects<'a, 'b, S, F>>,
{
    let admitted = lease.bundle();
    let generation = target_generation;
    let sequence = admitted
        .sequence()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let manifest = completed.manifest();
    if !completed.matches(lease)
        || manifest.binding.store != admitted.base().store
        || manifest.remaining_count != 0
        || manifest
            .completed_count
            .checked_add(manifest.partial_count)
            .is_none_or(|count| count == 0)
        || !objects.is_finished()
        || objects.store() != admitted.base().store
        || objects.generation() != generation
        || roots.store() != admitted.base().store
        || roots.generation() != generation
        || !sparse_checkpoint.matches(
            SparseRoots {
                text: admitted.text(),
                vector: admitted.vector(),
            },
            admitted.roots(),
            roots,
            sequence,
            admitted.catalog(),
        )
        || inventory.len() != objects.len()
        || commit_artifacts.capacity()
            != objects
                .len()
                .checked_add(1)
                .ok_or(NativeGraphError::Invalid("clear artifact count"))?
        || !commit_artifacts.as_slice().is_empty()
    {
        return Err(NativeGraphError::Invalid("prepared reclaim clear facts"));
    }
    for kind in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::KeyFences,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
        TreeKind::OutRanges,
        TreeKind::InRanges,
    ] {
        if admitted.roots().directory(kind)?.reference() != roots.directory(kind)?.reference() {
            return Err(NativeGraphError::Invalid(
                "reclaim clear changed graph root",
            ));
        }
    }
    validate_inventory_retirement(
        objects,
        roots.directory(TreeKind::ObjectInventory)?,
        completed.reclaimed(),
        tree_resources,
    )?;
    let mut greatest_serial = admitted.high_waters().creation_serial;
    for (index, change) in inventory.iter().enumerate() {
        let artifact = objects.artifact(index)?;
        let descriptor =
            artifact_descriptor(artifact.identity(), ContainerKind::Object, artifact.bytes())?;
        if change.object != descriptor
            || change.state != InventoryState::Prepared
            || descriptor.generation != generation
            || descriptor.serial <= admitted.high_waters().creation_serial
        {
            return Err(NativeGraphError::Invalid(
                "reclaim clear prepared inventory",
            ));
        }
        greatest_serial = greatest_serial.max(descriptor.serial);
    }
    if inventory_identity.store != admitted.base().store
        || inventory_identity.generation != generation
        || inventory_identity.creation_serial <= greatest_serial
    {
        return Err(NativeGraphError::Invalid(
            "reclaim clear inventory identity",
        ));
    }
    let inventory_artifact = SupplementalArtifact {
        identity: inventory_identity,
        bytes: inventory_bytes,
        required: inventory_ref,
    };
    verify_prepared_inventory(inventory_artifact, inventory, None, tree_resources)?;
    let sparse = sparse_checkpoint.finalize(inventory)?;

    validate_persisted_maintenance_transition(
        objects,
        SparseCheckpoint {
            cutoff: admitted.sequence(),
            roots: SparseRoots {
                text: admitted.text(),
                vector: admitted.vector(),
            },
        },
        admitted.roots(),
        sparse,
        roots,
        admitted.catalog(),
        catalog,
        admitted.document(),
        admitted.lexical(),
        objects.memory(),
        tree_resources,
    )?;

    let graph = wal_roots(roots, inventory, admitted.wal_roots())?;
    let reference_count = admitted
        .prepared_inventories()
        .len()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut prepared_refs = StorageBuffer::new(objects.memory(), reference_count)?;
    prepared_refs.extend_from_slice(admitted.prepared_inventories())?;
    prepared_refs.push(inventory_ref)?;
    let state = CommitState {
        store: admitted.base().store,
        generation,
        sequence,
        graph,
        catalog: admitted.catalog(),
        vector: sparse.vector,
        text: sparse.text,
        reclaim: None,
        high_waters: HighWaters {
            creation_serial: inventory_identity.creation_serial,
            ..admitted.high_waters()
        },
        prepared_inventories: ReferenceList::Values(prepared_refs.as_slice()),
    };
    let change_count = inventory
        .len()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut changes = StorageBuffer::new(objects.memory(), change_count)?;
    for change in inventory {
        changes.push(Change::Inventory(*change))?;
    }
    changes.push(Change::Inventory(InventoryChange {
        object: inventory_ref.object,
        state: InventoryState::Prepared,
    }))?;
    let encoded_length = encode_envelope(
        commit_state(admitted),
        Envelope {
            batch,
            kind: EnvelopeKind::Maintenance,
            changes: changes.as_slice(),
            state,
        },
        output,
        wal_resources,
    )?;
    let _input_charge = shared.reserve(
        reference_count
            .checked_mul(std::mem::size_of::<RequiredRef>())
            .ok_or(NativeGraphError::IdentityExhausted)?,
    )?;
    let next = NativeGraphBundle::assemble_committed(
        store,
        target_generation,
        shared,
        admitted,
        NativeGraphBundleInput {
            base: crate::property_graph::staging::BaseIdentity {
                store: admitted.base().store,
                generation,
                fold: admitted.base().fold,
                roots: admitted.base().roots,
            },
            root_envelope: admitted.root_envelope(),
            roots,
            wal_roots: graph,
            sequence,
            catalog: admitted.catalog(),
            vector: sparse.vector,
            text: sparse.text,
            reclaim: None,
            high_waters: state.high_waters,
            prepared_inventories: prepared_refs.as_slice().to_vec(),
            lexical: admitted.lexical(),
            document: admitted.document().cloned(),
        },
    )?;
    for index in 0..objects.len() {
        let artifact = objects.artifact(index)?;
        commit_artifacts.push(NativeCommitArtifact {
            identity: artifact.identity(),
            descriptor: inventory
                .get(index)
                .ok_or(NativeGraphError::Invalid("clear artifact descriptor"))?
                .object,
            bytes: artifact.bytes(),
        })?;
    }
    commit_artifacts.push(NativeCommitArtifact {
        identity: inventory_identity,
        descriptor: inventory_ref.object,
        bytes: inventory_bytes,
    })?;
    Ok((
        NativeCommittedTransition {
            admitted: Arc::clone(admitted),
            next,
            encoded: output
                .get(..encoded_length)
                .ok_or(NativeGraphError::Invalid("reclaim clear WAL extent"))?,
            artifacts: commit_artifacts.as_slice(),
            durable: None,
        },
        generation,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NativeDurableProtection {
    pub(super) allocation_head: RequiredRef,
    pub(super) protected: crate::property_graph::storage::reclaim::DurableProtectedStream,
    pub(super) mark: crate::property_graph::storage::reclaim::DurableRun,
    pub(super) intent: Option<RequiredRef>,
}

impl NativeWriter {
    const MAX_PROTECTED_DESCRIPTORS: usize = 8_192;
    const MAX_DURABLE_PROTECTIONS: usize = 64;

    pub(super) fn new(
        last_graph_seq: u64,
        resources: &GraphResources,
    ) -> Result<Self, NativeGraphError> {
        let bytes = Self::MAX_PROTECTED_DESCRIPTORS
            .checked_mul(std::mem::size_of::<
                crate::property_graph::wal::ArtifactDescriptor,
            >())
            .and_then(|bytes| {
                Self::MAX_DURABLE_PROTECTIONS
                    .checked_mul(std::mem::size_of::<NativeDurableProtection>())
                    .and_then(|durable| bytes.checked_add(durable))
            })
            .ok_or(NativeGraphError::Invalid("protected descriptor capacity"))?;
        let mut charge = resources.reserve(bytes)?;
        let mut protected = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let reserved = crate::allocation_audit::attributed(|| {
            protected.try_reserve_exact(Self::MAX_PROTECTED_DESCRIPTORS)
        });
        #[cfg(not(feature = "allocation-audit"))]
        let reserved = protected.try_reserve_exact(Self::MAX_PROTECTED_DESCRIPTORS);
        reserved.map_err(|_| NativeGraphError::Invalid("protected descriptor allocation"))?;
        let mut durable_protected = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let reserved = crate::allocation_audit::attributed(|| {
            durable_protected.try_reserve_exact(Self::MAX_DURABLE_PROTECTIONS)
        });
        #[cfg(not(feature = "allocation-audit"))]
        let reserved = durable_protected.try_reserve_exact(Self::MAX_DURABLE_PROTECTIONS);
        reserved.map_err(|_| NativeGraphError::Invalid("durable protection allocation"))?;
        charge.resize(
            protected
                .capacity()
                .checked_mul(std::mem::size_of::<
                    crate::property_graph::wal::ArtifactDescriptor,
                >())
                .and_then(|bytes| {
                    durable_protected
                        .capacity()
                        .checked_mul(std::mem::size_of::<NativeDurableProtection>())
                        .and_then(|durable| bytes.checked_add(durable))
                })
                .ok_or(NativeGraphError::Invalid("protected descriptor capacity"))?,
        )?;
        Ok(Self {
            last_graph_seq,
            envelope_bytes: 0,
            complete_envelopes: 0,
            stopped: false,
            checkpoint_failed: false,
            protected,
            durable_protected,
            _protected_charge: charge,
        })
    }

    pub(super) fn resume(
        last_graph_seq: u64,
        resources: &GraphResources,
        complete_envelopes: u64,
        protected: &[crate::property_graph::wal::ArtifactDescriptor],
    ) -> Result<Self, NativeGraphError> {
        let mut writer = Self::new(last_graph_seq, resources)?;
        writer.can_protect(protected.len())?;
        writer.protected.extend_from_slice(protected);
        writer.complete_envelopes = complete_envelopes;
        Ok(writer)
    }

    pub(super) fn can_protect(&self, additional: usize) -> Result<(), NativeGraphError> {
        if self
            .protected
            .len()
            .checked_add(additional)
            .is_none_or(|needed| needed > self.protected.capacity())
        {
            return Err(NativeGraphError::Invalid(
                "protected descriptor capacity exhausted",
            ));
        }
        Ok(())
    }

    fn can_protect_durable(&self) -> Result<(), NativeGraphError> {
        if self.durable_protected.len() == self.durable_protected.capacity() {
            return Err(NativeGraphError::Invalid(
                "durable protection capacity exhausted",
            ));
        }
        Ok(())
    }
}

/// Prepared under the shared WAL lock; published only after the seal manifest.
pub(crate) struct NativeSealFold {
    admitted: Arc<NativeGraphBundle>,
    next: Arc<NativeGraphBundle>,
}

impl NativeSealFold {
    pub(crate) fn publish(self, store: &crate::lifecycle::Store) -> Result<(), NativeGraphError> {
        store
            .native_graph
            .publish_transition(&self.admitted, self.next)
    }
}

pub(super) fn commit_state(bundle: &NativeGraphBundle) -> CommitState<'_> {
    CommitState {
        store: bundle.base().store,
        generation: bundle.base().generation,
        sequence: bundle.sequence(),
        graph: bundle.wal_roots(),
        catalog: bundle.catalog(),
        vector: bundle.vector(),
        text: bundle.text(),
        reclaim: bundle.reclaim(),
        high_waters: bundle.high_waters(),
        prepared_inventories: ReferenceList::Values(bundle.prepared_inventories()),
    }
}

pub(super) fn checkpoint_current(
    store: &crate::lifecycle::Store,
    writer: &mut NativeWriter,
    admitted: &Arc<NativeGraphBundle>,
    resources: &GraphResources,
    control: &crate::lifecycle::QueryControl,
) -> Result<(), NativeGraphError> {
    let result = checkpoint_current_inner(store, writer, admitted, resources, control);
    if result.is_err() && !matches!(result, Err(NativeGraphError::StalePreparation)) {
        writer.checkpoint_failed = true;
    }
    result
}

fn checkpoint_current_inner(
    store: &crate::lifecycle::Store,
    writer: &mut NativeWriter,
    admitted: &Arc<NativeGraphBundle>,
    resources: &GraphResources,
    control: &crate::lifecycle::QueryControl,
) -> Result<(), NativeGraphError> {
    control
        .checkpoint()
        .map_err(|_| NativeGraphError::Stage(StageError::Cancelled))?;
    let checkpoint_memory = WriteMemory::new(resources, WriteLimits::default())?;
    let storage = StorageMemory::new(&checkpoint_memory, control, 32 * 1024 * 1024)?;
    let objects = super::recovery::manifest_inventory(store, admitted, &storage)?;
    #[cfg(test)]
    {
        let gate = store
            .native_graph
            .state
            .lock()
            .map_err(|_| {
                NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                    component: "native graph publication",
                })
            })?
            .checkpoint_inventory_hook
            .take();
        if let Some((entered, release)) = gate {
            entered.wait();
            release.wait();
        }
    }
    let mut wal_slot = store.wal_writer.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "WAL writer",
        })
    })?;
    let wal = wal_slot
        .as_mut()
        .ok_or(crate::lifecycle::StoreError::ReadOnly)?;
    let mut active_slot = store.active.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "active segment",
        })
    })?;
    let active = active_slot
        .as_mut()
        .ok_or(crate::lifecycle::StoreError::Closed)?;
    let (graph_mark, document_mark) = checkpoint_manifest_locked(
        store,
        admitted,
        writer.last_graph_seq,
        wal,
        active,
        objects,
        resources,
        store.vfs.as_ref(),
    )?;
    writer.complete_envelopes = 0;
    writer.envelope_bytes = 0;
    writer.checkpoint_failed = false;
    writer.protected.clear();
    if admitted.reclaim().is_some() {
        writer
            .durable_protected
            .retain(|proof| proof.intent.is_some());
    } else {
        writer.durable_protected.clear();
    }
    writer.last_graph_seq = graph_mark;
    if document_mark == graph_mark && document_mark == wal.durable_end() {
        wal.retire_visible_through(crate::wal::LogSeq::new(document_mark))?;
        wal.truncate_absorbed(
            store.vfs.as_ref(),
            &store.directory,
            store.durability_policy,
            crate::wal::LogSeq::new(document_mark),
        )?;
    }
    Ok(())
}

// The WAL and active locks serialize this fold with every document and graph
// publication. Purge must use these held locks rather than reacquiring them.
#[allow(clippy::too_many_arguments)]
fn checkpoint_manifest_locked(
    store: &crate::lifecycle::Store,
    admitted: &Arc<NativeGraphBundle>,
    graph_mark: u64,
    wal: &mut crate::ingest::StoreWal,
    active: &mut crate::ingest::ActiveState,
    objects: Vec<crate::manifest::GraphObject>,
    resources: &GraphResources,
    vfs: &dyn crate::vfs::Vfs,
) -> Result<(u64, u64), NativeGraphError> {
    // Purge can fold and replace the bundle without the native writer lock.
    // WAL and active now exclude every publisher; refuse stale inventory before
    // any durable mutation, so the caller can admit the replacement and retry.
    if !store.native_graph.is_current_bundle(admitted)? {
        return Err(NativeGraphError::StalePreparation);
    }
    let mut manifest = crate::ingest::load_current_manifest(
        vfs,
        &store.directory,
        wal.durable_end(),
        0,
        &store.schema,
    )?;
    let graph_mark = graph_mark.max(
        manifest
            .graph
            .as_ref()
            .ok_or(NativeGraphError::Invalid(
                "graph checkpoint without version barrier",
            ))?
            .graph_absorbed_through,
    );
    // Later document batches stay in the WAL above the unchanged document
    // watermark. The generation cutoff recorded at durable_end counts them
    // without absorbing their data, including a fold required by reclaim replay.
    let generation = active
        .generation
        .checked_add(1)
        .ok_or(crate::lifecycle::StoreError::GenerationOverflow)?;
    manifest.generation = generation;
    manifest.graph = Some(
        crate::manifest::GraphManifest::new(commit_state(admitted), graph_mark, objects)
            .map_err(crate::lifecycle::StoreError::Manifest)?,
    );
    if active.segment.is_empty() {
        manifest.log_seq = wal.durable_end();
    }
    manifest
        .record_generation_bump(wal.durable_end())
        .map_err(crate::lifecycle::StoreError::Manifest)?;
    let next = NativeGraphBundle::fold_transition(
        store,
        resources,
        admitted,
        crate::property_graph::staging::FoldMark {
            manifest_generation: generation,
            graph_absorbed_through: graph_mark,
            envelope_sequence: admitted.sequence(),
        },
        None,
    )?;
    let remapped = crate::lifecycle::PublishedSnapshot::from_manifest(
        vfs,
        &store.directory,
        &manifest,
        &store.accounting,
    )?;
    let mut publication = wal.manifest_publication()?;
    publication
        .commit_manifest(vfs, &store.directory, &manifest, store.durability_policy)
        .map_err(crate::lifecycle::StoreError::Manifest)?;
    let failure_path = store.directory.join(crate::manifest::io::MANIFEST_FILE);
    let mut snapshot =
        store
            .snapshot
            .write()
            .map_err(|_| NativeGraphError::CommitIndeterminate {
                stage: "checkpoint snapshot publication",
                path: failure_path.clone(),
                source: None,
            })?;
    *snapshot = Some(Arc::new(remapped));
    active.generation = generation;
    store
        .native_graph
        .publish_transition(admitted, next)
        .map_err(|_| NativeGraphError::CommitIndeterminate {
            stage: "checkpoint graph publication",
            path: failure_path,
            source: None,
        })?;
    publication.complete();
    Ok((graph_mark, manifest.log_seq))
}

impl NativeGraphPublication {
    pub(super) fn initialize_writer(
        &self,
        writer: NativeWriter,
        initial_serial: u64,
    ) -> Result<(), NativeGraphError> {
        let mut slot = self.writer.lock().map_err(|_| {
            NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                component: "native graph writer",
            })
        })?;
        if slot.is_some() {
            return Err(NativeGraphError::Invalid(
                "native graph writer already initialized",
            ));
        }
        let mut state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        if state.creation_serial_fence != 0 {
            return Err(NativeGraphError::Invalid(
                "native graph creation serial already initialized",
            ));
        }
        state.creation_serial_fence = initial_serial;
        *slot = Some(writer);
        Ok(())
    }
}

pub(crate) struct ReceiptRegistration(Box<[ItemReceipt]>);

impl ReceiptRegistration {
    pub(crate) fn into_receipts(self) -> Box<[ItemReceipt]> {
        self.0
    }
}
impl std::ops::Deref for ReceiptRegistration {
    type Target = [ItemReceipt];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Result backing and actual accounting move together, with no callback after
/// commit. A binding may borrow its prepared arenas for the owner's lifetime.
/// It also keeps the batch disposition, the admitted generation it was
/// classified against and, only when it committed, the changed generation.
pub(crate) struct NativePreparedResult<R> {
    registration: R,
    core: Vec<u8>,
    abi: Vec<u8>,
    _charges: [GraphReservation; 3],
    disposition: BatchDisposition,
    admitted: GraphGeneration,
    changed: Option<GraphGeneration>,
}
impl<R> NativePreparedResult<R> {
    /// `changed` is the generation the commit tail published, `None` when
    /// nothing committed.
    fn from_materialized(
        value: crate::property_graph::staging::MaterializedBatch<'_, R>,
        changed: Option<GraphGeneration>,
    ) -> Self {
        let (batch, registration, core, abi, charges) = value.into_prepared_parts();
        Self {
            registration,
            core,
            abi,
            _charges: charges,
            disposition: batch.disposition(),
            admitted: batch.base().generation,
            changed,
        }
    }
    /// The staged batch's logical disposition.
    pub(crate) const fn disposition(&self) -> BatchDisposition {
        self.disposition
    }
    /// The published generation the batch was classified against.
    pub(crate) const fn admitted_generation(&self) -> GraphGeneration {
        self.admitted
    }
    /// The generation the commit published; `None` unless it committed.
    pub(crate) const fn changed_generation(&self) -> Option<GraphGeneration> {
        self.changed
    }
    pub(crate) fn into_registration(self) -> R {
        self.registration
    }
    pub(super) fn core_bytes(&self) -> &[u8] {
        &self.core
    }
    pub(super) fn abi_bytes(&self) -> &[u8] {
        &self.abi
    }
}
impl<R> std::ops::Deref for NativePreparedResult<R> {
    type Target = R;
    fn deref(&self) -> &R {
        &self.registration
    }
}

impl ResultRegistration for ReceiptRegistration {
    fn capacity_bytes(&self) -> usize {
        self.0.len() * std::mem::size_of::<ItemReceipt>()
    }
}

struct ReceiptMaterializer;

impl ResultMaterializer for ReceiptMaterializer {
    type Registration = ReceiptRegistration;

    fn layout(
        &mut self,
        receipt_count: usize,
        control: &mut crate::property_graph::staging::WriteControl<'_>,
    ) -> Result<ResultLayout, StageError> {
        control(WritePhase::CoreResult)?;
        Ok(ResultLayout {
            rows: receipt_count,
            core_bytes: 0,
            abi_bytes: 0,
            registry_bytes: receipt_count
                .checked_mul(std::mem::size_of::<ItemReceipt>())
                .ok_or(StageError::Limit)?,
        })
    }

    fn materialize(
        &mut self,
        receipts: &[ItemReceipt],
        _: &mut [u8],
        _: &mut [u8],
        control: &mut crate::property_graph::staging::WriteControl<'_>,
    ) -> Result<Self::Registration, StageError> {
        control(WritePhase::AbiResult)?;
        let mut copied = Vec::new();
        copied.try_reserve_exact(receipts.len()).map_err(|_| {
            StageError::Memory(crate::lifecycle::StoreError::AllocationFailed {
                needed: std::mem::size_of_val(receipts) as u64,
                component: "native graph result registration",
            })
        })?;
        copied.extend_from_slice(receipts);
        Ok(ReceiptRegistration(copied.into_boxed_slice()))
    }
}

fn checkpoint(control: &crate::lifecycle::QueryControl, _: WritePhase) -> Result<(), StageError> {
    control.checkpoint().map_err(|_| StageError::Cancelled)
}

pub(super) fn io(path: &std::path::Path, source: std::io::Error) -> NativeGraphError {
    NativeGraphError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn encode_descriptor(
    output: &mut [u8],
    offset: usize,
    descriptor: crate::property_graph::wal::ArtifactDescriptor,
) -> Result<(), NativeGraphError> {
    let end = offset
        .checked_add(64)
        .ok_or(NativeGraphError::Invalid("prepared inventory descriptor"))?;
    let target = output
        .get_mut(offset..end)
        .ok_or(NativeGraphError::Invalid("prepared inventory descriptor"))?;
    target
        .get_mut(0..16)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.store.get().to_le_bytes());
    target
        .get_mut(16..32)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.artifact.get().to_le_bytes());
    target
        .get_mut(32..40)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.generation.get().to_le_bytes());
    target
        .get_mut(40..48)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.serial.to_le_bytes());
    target
        .get_mut(48..52)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.bytes.to_le_bytes());
    target
        .get_mut(52..54)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.family.to_le_bytes());
    target
        .get_mut(54..56)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.version.to_le_bytes());
    target
        .get_mut(56..64)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.checksum.to_le_bytes());
    Ok(())
}

pub(super) fn inventory_payload<'a>(
    memory: &'a StorageMemory<'a>,
    control: &crate::lifecycle::QueryControl,
    prepared: &[InventoryChange],
    catalog: Option<crate::property_graph::wal::ArtifactDescriptor>,
) -> Result<StorageBuffer<'a, u8>, NativeGraphError> {
    let count = prepared
        .len()
        .checked_add(usize::from(catalog.is_some()))
        .ok_or(NativeGraphError::Invalid("prepared inventory count"))?;
    let length = count
        .checked_mul(64)
        .and_then(|bytes| bytes.checked_add(16))
        .ok_or(NativeGraphError::Invalid("prepared inventory length"))?;
    let mut payload_owner = zeroed(memory, control, length)?;
    let payload = payload_owner.as_mut_slice();
    payload
        .get_mut(0..4)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(b"ZGCP");
    payload
        .get_mut(4..6)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&2_u16.to_le_bytes());
    payload
        .get_mut(6..8)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&1_u16.to_le_bytes());
    payload
        .get_mut(8..12)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(
            &u32::try_from(count)
                .map_err(|_| NativeGraphError::Invalid("prepared inventory count"))?
                .to_le_bytes(),
        );
    for (index, change) in prepared.iter().enumerate() {
        encode_descriptor(payload, 16 + index * 64, change.object)?;
    }
    if let Some(catalog) = catalog {
        encode_descriptor(payload, 16 + prepared.len() * 64, catalog)?;
    }
    Ok(payload_owner)
}

fn required_for_block(
    block: crate::property_graph::storage::artifact::PhysicalRef,
    prepared: &[InventoryChange],
    admitted: WalGraphRoots,
) -> Result<RequiredRef, NativeGraphError> {
    if let Some(object) = prepared
        .iter()
        .find(|change| change.object.artifact == block.artifact)
        .map(|change| change.object)
    {
        return Ok(RequiredRef { object, block });
    }
    admitted
        .slots
        .into_iter()
        .flatten()
        .find(|required| required.block == block)
        .ok_or(NativeGraphError::Invalid(
            "native root has no immutable descriptor",
        ))
}

pub(super) fn wal_roots(
    roots: GraphRoots,
    prepared: &[InventoryChange],
    admitted: WalGraphRoots,
) -> Result<WalGraphRoots, NativeGraphError> {
    let mut output = WalGraphRoots::default();
    for (slot, block) in roots.references().into_iter().enumerate() {
        if let Some(block) = block {
            *output
                .slots
                .get_mut(slot)
                .ok_or(NativeGraphError::Invalid("root slot"))? =
                Some(required_for_block(block, prepared, admitted)?);
        }
    }
    Ok(output)
}

fn canonical_required<S: BlockSource, C: RecordCatalog<S>>(
    source: &S,
    roots: GraphRoots,
    entity: EntityId,
    catalog: &C,
    document: Option<&crate::epoch::EmbeddingTower>,
    prepared: &[InventoryChange],
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<RequiredRef, NativeGraphError> {
    let (kind, key) = match entity {
        EntityId::Node(id) => (TreeKind::Nodes, id.get()),
        EntityId::Relationship(id) => (TreeKind::Relationships, id.get()),
    };
    let directory = roots.directory(kind)?;
    let entry = lookup_entry(source, directory, &key.to_le_bytes(), resources)?
        .ok_or(NativeGraphError::Invalid("prepared live record is absent"))?;
    let record_ref = PayloadRef::decode(entry.value())?;
    let record = verify_record(
        PayloadSlice::new(
            source,
            roots.store(),
            entry.creation_generation(),
            record_ref,
        ),
        entity,
        catalog,
        document,
        resources,
    )?;
    let canonical = record
        .required_payloads()
        .first()
        .ok_or(NativeGraphError::Invalid("canonical payload"))?
        .reference();
    let object = prepared
        .iter()
        .find(|change| change.object.artifact == canonical.artifact)
        .map(|change| change.object)
        .ok_or(NativeGraphError::Invalid(
            "prepared canonical has no immutable descriptor",
        ))?;
    Ok(RequiredRef {
        object,
        block: canonical,
    })
}

#[derive(Clone, Copy)]
struct SupplementalArtifact<'a> {
    identity: ArtifactIdentity,
    bytes: &'a [u8],
    required: RequiredRef,
}

#[derive(Clone, Copy)]
pub(super) struct NativeCommitArtifact<'a> {
    identity: ArtifactIdentity,
    descriptor: crate::property_graph::wal::ArtifactDescriptor,
    bytes: &'a [u8],
}

pub(super) struct NativeCommittedTransition<'a> {
    admitted: Arc<NativeGraphBundle>,
    next: Arc<NativeGraphBundle>,
    encoded: &'a [u8],
    artifacts: &'a [NativeCommitArtifact<'a>],
    durable: Option<PreparedDurableSpill>,
}

impl<'a> NativeCommittedTransition<'a> {
    pub(super) const fn wal_bytes(&self) -> &[u8] {
        self.encoded
    }

    pub(super) fn into_publication(self) -> (Arc<NativeGraphBundle>, Arc<NativeGraphBundle>) {
        (self.admitted, self.next)
    }
}

fn artifact_control_error(
    error: crate::property_graph::storage::artifact::ArtifactControlError<TreeError>,
) -> NativeGraphError {
    match error {
        crate::property_graph::storage::artifact::ArtifactControlError::Control(error) => {
            NativeGraphError::Read(error)
        }
        crate::property_graph::storage::artifact::ArtifactControlError::Format(error) => {
            NativeGraphError::Read(TreeError::Format(error))
        }
    }
}

fn verify_supplemental(
    artifact: SupplementalArtifact<'_>,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<(), NativeGraphError> {
    let descriptor = artifact_descriptor(artifact.identity, ContainerKind::Object, artifact.bytes)?;
    let frame = crate::property_graph::storage::artifact::decode_with_control(
        ContainerKind::Object,
        Some((artifact.identity.store, artifact.identity.artifact)),
        artifact.bytes,
        &mut |bytes| resources.step(bytes as u64),
    )
    .map_err(artifact_control_error)?;
    if descriptor != artifact.required.object
        || frame
            .reference(0)
            .map_err(|_| NativeGraphError::Invalid("supplemental artifact block"))?
            != artifact.required.block
    {
        return Err(NativeGraphError::Invalid(
            "supplemental artifact descriptor mismatch",
        ));
    }
    Ok(())
}

fn verify_prepared_inventory(
    artifact: SupplementalArtifact<'_>,
    prepared: &[InventoryChange],
    catalog: Option<crate::property_graph::wal::ArtifactDescriptor>,
    resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
) -> Result<(), NativeGraphError> {
    verify_supplemental(artifact, resources)?;
    let frame = crate::property_graph::storage::artifact::decode_with_control(
        ContainerKind::Object,
        Some((artifact.identity.store, artifact.identity.artifact)),
        artifact.bytes,
        &mut |bytes| resources.step(bytes as u64),
    )
    .map_err(artifact_control_error)?;
    let payload = frame
        .resolve_framed_block(artifact.required.block)
        .map_err(|_| NativeGraphError::Invalid("prepared inventory block"))?;
    let count = prepared
        .len()
        .checked_add(usize::from(catalog.is_some()))
        .ok_or(NativeGraphError::Invalid("prepared inventory count"))?;
    let expected_length = count
        .checked_mul(64)
        .and_then(|bytes| bytes.checked_add(16))
        .ok_or(NativeGraphError::Invalid("prepared inventory length"))?;
    if payload.len() != expected_length
        || payload.get(..4) != Some(b"ZGCP".as_slice())
        || payload.get(4..6) != Some(2_u16.to_le_bytes().as_slice())
        || payload.get(6..8) != Some(1_u16.to_le_bytes().as_slice())
        || payload.get(8..12)
            != Some(
                u32::try_from(count)
                    .map_err(|_| NativeGraphError::Invalid("prepared inventory count"))?
                    .to_le_bytes()
                    .as_slice(),
            )
        || payload.get(12..16) != Some([0_u8; 4].as_slice())
    {
        return Err(NativeGraphError::Invalid("prepared inventory contents"));
    }
    for (index, descriptor) in prepared
        .iter()
        .map(|change| change.object)
        .chain(catalog)
        .enumerate()
    {
        resources.step(1)?;
        let mut encoded = [0_u8; 64];
        encode_descriptor(&mut encoded, 0, descriptor)?;
        let start = 16_usize
            .checked_add(
                index
                    .checked_mul(64)
                    .ok_or(NativeGraphError::Invalid("prepared inventory offset"))?,
            )
            .ok_or(NativeGraphError::Invalid("prepared inventory offset"))?;
        if payload.get(start..start + 64) != Some(encoded.as_slice()) {
            return Err(NativeGraphError::Invalid("prepared inventory descriptor"));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_committed_transition<'p, 'source, 'a, 'b, S, F, C>(
    store: &crate::lifecycle::Store,
    target_generation: GraphGeneration,
    shared: &GraphResources,
    control: &crate::lifecycle::QueryControl,
    lease: &'p NativeReadLease,
    batch: &'p StagedBatch<'_>,
    prepared: &'p PreparedGraphArtifacts<'source, 'a, 'b, S, F>,
    final_roots: GraphRoots,
    catalog: &C,
    catalog_artifact: Option<SupplementalArtifact<'p>>,
    catalog_ref: RequiredRef,
    inventory_artifact: SupplementalArtifact<'p>,
    batch_id: BatchId,
    output: &'p mut StorageBuffer<'a, u8>,
    commit_artifacts: &'p mut StorageBuffer<'_, NativeCommitArtifact<'p>>,
    tree_resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
    wal_resources: &mut WalResources<'_>,
) -> Result<NativeCommittedTransition<'p>, NativeGraphError>
where
    S: BlockSource,
    F: FnMut() -> Result<ArtifactIdentity, TreeError>,
    C: RecordCatalog<crate::property_graph::storage::prepared::PreparedObjects<'a, 'b, S, F>>,
{
    let admitted = lease.bundle();
    let candidate = prepared.candidate();
    let expected_generation = target_generation;
    let expected_sequence = admitted
        .sequence()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    if !prepared.matches_base(lease)
        || prepared.expected_fold() != admitted.base().fold
        || prepared.membership_changes().len() != batch.deltas().len()
        || batch.base() != admitted.base()
        || candidate.expected_base() != admitted.base()
        || candidate.expected_sequence() != admitted.sequence()
        || candidate.expected_roots() != admitted.wal_roots()
        || candidate.expected_catalog() != admitted.catalog()
        || candidate.expected_vector() != admitted.vector()
        || candidate.expected_text() != admitted.text()
        || candidate.expected_reclaim() != admitted.reclaim()
        || candidate.expected_high_waters() != admitted.high_waters()
        || candidate.expected_prepared_inventories() != admitted.prepared_inventories()
        || candidate.target_generation() != expected_generation
        || candidate.sequence() != expected_sequence
        || candidate.roots().generation() != expected_generation
        || final_roots != candidate.roots()
    {
        return Err(NativeGraphError::Invalid(
            "prepared committed transition facts",
        ));
    }
    if let Some(artifact) = catalog_artifact {
        verify_supplemental(artifact, tree_resources)?;
        if artifact.required != catalog_ref {
            return Err(NativeGraphError::Invalid("prepared catalog reference"));
        }
    } else if catalog_ref != admitted.catalog() {
        return Err(NativeGraphError::Invalid("borrowed catalog reference"));
    }
    verify_prepared_inventory(
        inventory_artifact,
        prepared.inventory(),
        catalog_artifact.map(|artifact| artifact.required.object),
        tree_resources,
    )?;

    let roots = final_roots;
    let graph = wal_roots(roots, prepared.inventory(), admitted.wal_roots())?;
    let sparse = prepared.sparse_roots();
    // Until ZE-46 folds allocation inventories, retain every previous inventory
    // in the checked WAL/checkpoint state. A checkpoint cannot forget them.
    let reference_count = admitted
        .prepared_inventories()
        .len()
        .checked_add(1)
        .filter(|count| *count <= NativeWriter::MAX_PROTECTED_DESCRIPTORS)
        .ok_or(NativeGraphError::Invalid(
            "prepared inventory capacity exhausted",
        ))?;
    let mut prepared_refs = StorageBuffer::new(prepared.objects().memory(), reference_count)?;
    prepared_refs.extend_from_slice(admitted.prepared_inventories())?;
    prepared_refs.push(inventory_artifact.required)?;
    let high = batch.high_waters();
    let state = CommitState {
        store: admitted.base().store,
        generation: expected_generation,
        sequence: expected_sequence,
        graph,
        catalog: catalog_ref,
        vector: sparse.vector,
        text: sparse.text,
        reclaim: admitted.reclaim(),
        high_waters: HighWaters {
            node: high.node,
            relationship: high.relationship,
            symbols: [
                high.symbols.label,
                high.symbols.relationship_type,
                high.symbols.property,
                high.symbols.namespace,
            ],
            creation_serial: inventory_artifact.identity.creation_serial,
        },
        prepared_inventories: ReferenceList::Values(prepared_refs.as_slice()),
    };
    let change_capacity = batch
        .deltas()
        .len()
        .checked_add(prepared.inventory().len())
        .and_then(|count| count.checked_add(2))
        .ok_or(NativeGraphError::Invalid("WAL change count"))?;
    let mut changes = StorageBuffer::new(prepared.objects().memory(), change_capacity)?;
    for (ordinal, delta) in batch.deltas().iter().enumerate() {
        let membership = prepared
            .membership_changes()
            .get(ordinal)
            .ok_or(NativeGraphError::Invalid("sparse membership ordinal"))?;
        if membership.ordinal as usize != ordinal {
            return Err(NativeGraphError::Invalid("sparse membership order"));
        }
        let fields = delta.provenance().fields();
        let membership_value = match fields.incarnation {
            EntityId::Node(node)
                if membership.node == Some(node) && membership.membership.is_some() =>
            {
                membership.membership.ok_or(NativeGraphError::Invalid(
                    "node sparse membership is absent",
                ))?
            }
            EntityId::Relationship(_)
                if membership.node.is_none() && membership.membership.is_none() =>
            {
                Membership::default()
            }
            _ => return Err(NativeGraphError::Invalid("sparse membership identity")),
        };
        let canonical = if delta.canonical().is_some() {
            Some(canonical_required(
                prepared.objects(),
                roots,
                fields.incarnation,
                catalog,
                admitted.document(),
                prepared.inventory(),
                tree_resources,
            )?)
        } else {
            None
        };
        changes.push(Change::Mutation(Mutation {
            provenance_version: 1,
            provenance: fields,
            live: canonical.is_some(),
            canonical,
            membership: membership_value,
        }))?;
    }
    for change in prepared.inventory() {
        changes.push(Change::Inventory(*change))?;
    }
    if let Some(catalog) = catalog_artifact {
        changes.push(Change::Inventory(InventoryChange {
            object: catalog.required.object,
            state: InventoryState::Prepared,
        }))?;
    }
    changes.push(Change::Inventory(InventoryChange {
        object: inventory_artifact.required.object,
        state: InventoryState::Prepared,
    }))?;
    for (index, change) in changes.as_slice().iter().enumerate() {
        let Change::Inventory(change) = change else {
            continue;
        };
        let descriptor = change.object;
        let duplicate = changes.as_slice().get(..index).ok_or(NativeGraphError::Invalid("inventory prefix"))?.iter().any(|previous| {
            matches!(previous, Change::Inventory(previous) if previous.object.artifact == descriptor.artifact || previous.object.serial == descriptor.serial)
        });
        if descriptor.store != admitted.base().store
            || descriptor.generation != expected_generation
            || descriptor.serial == 0
            || duplicate
        {
            return Err(NativeGraphError::Invalid(
                "committed transition descriptor union",
            ));
        }
    }
    let base_state = commit_state(admitted);
    let envelope = Envelope {
        batch: batch_id,
        kind: EnvelopeKind::Mutation,
        changes: changes.as_slice(),
        state,
    };
    let encoded_size =
        crate::property_graph::wal::envelope_size(base_state, envelope, wal_resources)?;
    *output = zeroed(prepared.objects().memory(), control, encoded_size)?;
    let encoded_length =
        encode_envelope(base_state, envelope, output.as_mut_slice(), wal_resources)?;
    let _input_charge = shared.reserve(
        reference_count
            .checked_mul(std::mem::size_of::<RequiredRef>())
            .ok_or(NativeGraphError::IdentityExhausted)?,
    )?;
    let next = NativeGraphBundle::assemble_committed(
        store,
        target_generation,
        shared,
        admitted,
        NativeGraphBundleInput {
            base: crate::property_graph::staging::BaseIdentity {
                store: admitted.base().store,
                generation: expected_generation,
                fold: admitted.base().fold,
                roots: admitted.base().roots,
            },
            root_envelope: admitted.root_envelope(),
            roots,
            wal_roots: graph,
            sequence: expected_sequence,
            catalog: catalog_ref,
            vector: sparse.vector,
            text: sparse.text,
            reclaim: admitted.reclaim(),
            high_waters: state.high_waters,
            prepared_inventories: prepared_refs.as_slice().to_vec(),
            lexical: admitted.lexical(),
            document: admitted.document().cloned(),
        },
    )?;
    for index in 0..prepared.objects().len() {
        let artifact = prepared.objects().artifact(index)?;
        let descriptor = prepared
            .inventory()
            .get(index)
            .ok_or(NativeGraphError::Invalid("prepared artifact descriptor"))?
            .object;
        commit_artifacts.push(NativeCommitArtifact {
            identity: artifact.identity(),
            descriptor,
            bytes: artifact.bytes(),
        })?;
    }
    if let Some(catalog) = catalog_artifact {
        commit_artifacts.push(NativeCommitArtifact {
            identity: catalog.identity,
            descriptor: catalog.required.object,
            bytes: catalog.bytes,
        })?;
    }
    commit_artifacts.push(NativeCommitArtifact {
        identity: inventory_artifact.identity,
        descriptor: inventory_artifact.required.object,
        bytes: inventory_artifact.bytes,
    })?;
    Ok(NativeCommittedTransition {
        admitted: Arc::clone(admitted),
        next,
        encoded: output
            .as_slice()
            .get(..encoded_length)
            .ok_or(NativeGraphError::Invalid("encoded WAL extent"))?,
        artifacts: commit_artifacts.as_slice(),
        durable: None,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_maintenance_transition<'p, 'a, 'b, S, F, C>(
    store: &crate::lifecycle::Store,
    target_generation: GraphGeneration,
    shared: &GraphResources,
    lease: &'p NativeReadLease,
    objects: &'p PreparedObjects<'a, 'b, S, F>,
    consolidated: &ConsolidationOutcome,
    sparse_checkpoint: &PreparedSparseCheckpoint,
    durable: Option<PreparedDurableSpill>,
    catalog: &C,
    inventory: &'p [InventoryChange],
    reclaim_pending: &'p [InventoryChange],
    reclaim_candidates: &'p [crate::property_graph::wal::ArtifactDescriptor],
    reclaim_partials: &'p [crate::property_graph::storage::reclaim::PartialTarget],
    inventory_identity: ArtifactIdentity,
    inventory_bytes: &'p [u8],
    inventory_ref: RequiredRef,
    batch: BatchId,
    output: &'p mut [u8],
    commit_artifacts: &'p mut StorageBuffer<'_, NativeCommitArtifact<'p>>,
    tree_resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
    wal_resources: &mut WalResources<'_>,
) -> Result<(NativeCommittedTransition<'p>, GraphGeneration), NativeGraphError>
where
    S: BlockSource,
    F: FnMut() -> Result<ArtifactIdentity, TreeError>,
    C: RecordCatalog<PreparedObjects<'a, 'b, S, F>>,
{
    let admitted = lease.bundle();
    let generation = target_generation;
    let sequence = admitted
        .sequence()
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let roots = consolidated.roots();
    if !objects.is_finished()
        || consolidated.expected_base() != admitted.roots()
        || consolidated.expected_sequence() != admitted.sequence()
        || consolidated.source_token() != lease.token()
        || objects.store() != admitted.base().store
        || objects.generation() != generation
        || roots.store() != admitted.base().store
        || roots.generation() != generation
        || inventory.len() != objects.len()
        || commit_artifacts.capacity()
            != objects
                .len()
                .checked_add(1)
                .ok_or(NativeGraphError::Invalid("maintenance artifact count"))?
        || !commit_artifacts.as_slice().is_empty()
        || inventory_identity.store != admitted.base().store
        || inventory_identity.generation != generation
        || inventory_identity.creation_serial <= admitted.high_waters().creation_serial
        || !consolidated.inventory_fold().matches_base(
            admitted.roots(),
            admitted.sequence(),
            lease.token(),
            admitted.prepared_inventories(),
        )
    {
        return Err(NativeGraphError::Invalid(
            "prepared maintenance transition facts",
        ));
    }
    if let Some(durable) = durable.as_ref() {
        let durable_binding = durable.binding();
        let durable_id = BatchId::new(durable_binding.session.get())?;
        if durable_binding.store != admitted.base().store
            || durable_binding.capture_generation != admitted.base().generation
            || durable_binding.target_generation != generation
            || durable_binding.sequence != admitted.sequence()
            || durable.protected().binding != durable_binding
            || durable.mark().binding != durable_binding
            || !durable.matches_admission(lease)
            || durable.candidate_count() != reclaim_candidates.len()
            || durable.partial_count() != reclaim_partials.len()
            || durable.intent().is_some()
                != (!reclaim_candidates.is_empty() || !reclaim_partials.is_empty())
            || reclaim_pending.len() != reclaim_candidates.iter().filter(|v| v.family == 17).count()
            || reclaim_pending
                .iter()
                .zip(reclaim_candidates.iter().filter(|v| v.family == 17))
                .any(|(pending, candidate)| {
                    pending.object != *candidate
                        || pending.state != InventoryState::ReclaimPending(durable_id)
                })
        {
            return Err(NativeGraphError::Invalid(
                "prepared maintenance durable proof association",
            ));
        }
        durable.validate_candidates(
            reclaim_candidates,
            reclaim_partials,
            objects.memory(),
            tree_resources,
        )?;
    }
    if durable.is_none()
        && (!reclaim_pending.is_empty()
            || !reclaim_candidates.is_empty()
            || !reclaim_partials.is_empty())
    {
        return Err(NativeGraphError::Invalid(
            "maintenance reclaim candidates lack durable proof",
        ));
    }
    if !sparse_checkpoint.matches(
        SparseRoots {
            text: admitted.text(),
            vector: admitted.vector(),
        },
        admitted.roots(),
        roots,
        sequence,
        admitted.catalog(),
    ) {
        return Err(NativeGraphError::Invalid(
            "prepared maintenance sparse association",
        ));
    }
    let mut greatest_serial = admitted.high_waters().creation_serial;
    for (index, change) in inventory.iter().enumerate() {
        let artifact = objects.artifact(index)?;
        let descriptor =
            artifact_descriptor(artifact.identity(), ContainerKind::Object, artifact.bytes())?;
        let duplicate = inventory
            .get(..index)
            .ok_or(NativeGraphError::Invalid("maintenance inventory prefix"))?
            .iter()
            .any(|previous| {
                previous.object.artifact == descriptor.artifact
                    || previous.object.serial == descriptor.serial
            });
        if change.state != InventoryState::Prepared
            || change.object != descriptor
            || descriptor.store != admitted.base().store
            || descriptor.generation != generation
            || descriptor.serial <= admitted.high_waters().creation_serial
            || duplicate
        {
            return Err(NativeGraphError::Invalid(
                "maintenance prepared inventory union",
            ));
        }
        greatest_serial = greatest_serial.max(descriptor.serial);
    }
    if inventory_identity.creation_serial <= greatest_serial {
        return Err(NativeGraphError::Invalid(
            "maintenance inventory creation serial order",
        ));
    }
    let inventory_artifact = SupplementalArtifact {
        identity: inventory_identity,
        bytes: inventory_bytes,
        required: inventory_ref,
    };
    verify_prepared_inventory(inventory_artifact, inventory, None, tree_resources)?;
    if inventory_ref.object.serial != inventory_identity.creation_serial
        || inventory
            .iter()
            .any(|change| change.object.artifact == inventory_ref.object.artifact)
    {
        return Err(NativeGraphError::Invalid(
            "maintenance inventory artifact identity",
        ));
    }
    let sparse = sparse_checkpoint.finalize(inventory)?;

    validate_persisted_maintenance_transition(
        objects,
        SparseCheckpoint {
            cutoff: admitted.sequence(),
            roots: SparseRoots {
                text: admitted.text(),
                vector: admitted.vector(),
            },
        },
        admitted.roots(),
        sparse,
        roots,
        admitted.catalog(),
        catalog,
        admitted.document(),
        admitted.lexical(),
        objects.memory(),
        tree_resources,
    )?;

    consolidated.inventory_fold().validate_candidate(
        objects,
        consolidated.inventory_fold_root(),
        target_generation,
        tree_resources,
    )?;
    validate_inventory_changes(
        objects,
        roots.directory(crate::property_graph::storage::tree::TreeKind::ObjectInventory)?,
        reclaim_pending,
        tree_resources,
    )?;
    validate_inventory_changes(
        objects,
        roots.directory(crate::property_graph::storage::tree::TreeKind::ObjectInventory)?,
        consolidated.adoptions(),
        tree_resources,
    )?;
    let graph = wal_roots(roots, inventory, admitted.wal_roots())?;
    // The fold retires a fully covered prefix of the admitted manifests.
    let retired = consolidated.inventory_fold().retired();
    let retained_count = admitted
        .prepared_inventories()
        .len()
        .checked_sub(retired)
        .ok_or(NativeGraphError::Invalid(
            "maintenance retained inventory count",
        ))?;
    let reference_count = retained_count
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let mut prepared_refs = StorageBuffer::new(objects.memory(), reference_count)?;
    prepared_refs.extend_from_slice(admitted.prepared_inventories().get(retired..).ok_or(
        NativeGraphError::Invalid("maintenance retained inventory extent"),
    )?)?;
    prepared_refs.push(inventory_ref)?;
    let previous_high = admitted.high_waters();
    let reclaim = durable
        .as_ref()
        .and_then(PreparedDurableSpill::intent)
        .or(admitted.reclaim());
    let state = CommitState {
        store: admitted.base().store,
        generation,
        sequence,
        graph,
        catalog: admitted.catalog(),
        vector: sparse.vector,
        text: sparse.text,
        reclaim,
        high_waters: HighWaters {
            creation_serial: inventory_identity.creation_serial,
            ..previous_high
        },
        prepared_inventories: ReferenceList::Values(prepared_refs.as_slice()),
    };
    let mut changes = StorageBuffer::new(
        objects.memory(),
        inventory
            .len()
            .checked_add(reclaim_pending.len())
            .and_then(|count| count.checked_add(1))
            .and_then(|count| {
                count.checked_add(usize::from(
                    durable
                        .as_ref()
                        .is_some_and(|value| value.intent().is_some()),
                ))
            })
            .ok_or(NativeGraphError::IdentityExhausted)?,
    )?;
    for change in inventory {
        changes.push(Change::Inventory(*change))?;
    }
    for change in reclaim_pending {
        changes.push(Change::Inventory(*change))?;
    }
    changes.push(Change::Inventory(InventoryChange {
        object: inventory_ref.object,
        state: InventoryState::Prepared,
    }))?;
    if let Some(durable) = durable.as_ref()
        && let Some(_intent) = durable.intent()
    {
        changes.push(Change::ReclaimIntent(ReclaimIntent {
            id: BatchId::new(durable.binding().session.get())?,
            capture_generation: durable.binding().capture_generation,
            capture_sequence: durable.binding().sequence,
            serial_fence: durable.binding().serial_fence,
            protected_roots: durable.protected().head,
            protected_digest: durable.protected().digest,
            completed_mark: durable.mark().root,
            mark_digest: durable.mark().digest,
            candidates: DescriptorList::Values(reclaim_candidates),
        }))?;
    }
    let encoded_length = encode_envelope(
        commit_state(admitted),
        Envelope {
            batch,
            kind: EnvelopeKind::Maintenance,
            changes: changes.as_slice(),
            state,
        },
        output,
        wal_resources,
    )?;
    let _input_charge = shared.reserve(
        reference_count
            .checked_mul(std::mem::size_of::<RequiredRef>())
            .ok_or(NativeGraphError::IdentityExhausted)?,
    )?;
    let next = NativeGraphBundle::assemble_committed(
        store,
        target_generation,
        shared,
        admitted,
        NativeGraphBundleInput {
            base: crate::property_graph::staging::BaseIdentity {
                store: admitted.base().store,
                generation,
                fold: admitted.base().fold,
                roots: admitted.base().roots,
            },
            root_envelope: admitted.root_envelope(),
            roots,
            wal_roots: graph,
            sequence,
            catalog: admitted.catalog(),
            vector: sparse.vector,
            text: sparse.text,
            reclaim,
            high_waters: state.high_waters,
            prepared_inventories: prepared_refs.as_slice().to_vec(),
            lexical: admitted.lexical(),
            document: admitted.document().cloned(),
        },
    )?;
    for index in 0..objects.len() {
        let artifact = objects.artifact(index)?;
        commit_artifacts.push(NativeCommitArtifact {
            identity: artifact.identity(),
            descriptor: inventory
                .get(index)
                .ok_or(NativeGraphError::Invalid("maintenance artifact descriptor"))?
                .object,
            bytes: artifact.bytes(),
        })?;
    }
    commit_artifacts.push(NativeCommitArtifact {
        identity: inventory_identity,
        descriptor: inventory_ref.object,
        bytes: inventory_bytes,
    })?;
    Ok((
        NativeCommittedTransition {
            admitted: Arc::clone(admitted),
            next,
            encoded: output
                .get(..encoded_length)
                .ok_or(NativeGraphError::Invalid("maintenance WAL extent"))?,
            artifacts: commit_artifacts.as_slice(),
            durable,
        },
        generation,
    ))
}

#[derive(Clone, Copy, Default)]
pub(super) struct NativeCommitAudit {
    allocations: u64,
    denials: u64,
}

pub(super) fn protect_and_commit(
    store: &crate::lifecycle::Store,
    writer: &mut NativeWriter,
    transition: NativeCommittedTransition<'_>,
    control: &crate::lifecycle::QueryControl,
    audit_publication: bool,
) -> Result<NativeCommitAudit, NativeGraphError> {
    use crate::lifecycle::stats::GraphWorkKind as W;
    let work = GraphResources::from_store(store)?;
    let _work_batch = work.begin_work();
    let record = |kind, units| work.record_work(kind, units);
    let committed_bytes = transition
        .artifacts
        .iter()
        .try_fold(0_u64, |sum, artifact| {
            sum.checked_add(artifact.bytes.len() as u64)
                .ok_or(NativeGraphError::IdentityExhausted)
        })?;
    writer.can_protect(transition.artifacts.len())?;
    if transition.durable.is_some() {
        writer.can_protect_durable()?;
    }
    for (index, artifact) in transition.artifacts.iter().enumerate() {
        let descriptor =
            artifact_descriptor(artifact.identity, ContainerKind::Object, artifact.bytes)?;
        let duplicate = transition
            .artifacts
            .get(..index)
            .ok_or(NativeGraphError::Invalid("commit artifact prefix"))?
            .iter()
            .any(|previous| {
                previous.identity.artifact == artifact.identity.artifact
                    || previous.identity.creation_serial == artifact.identity.creation_serial
            });
        if descriptor != artifact.descriptor || duplicate {
            return Err(NativeGraphError::Invalid("commit artifact binding"));
        }
    }
    for artifact in transition.artifacts {
        writer.protected.push(artifact.descriptor);
    }
    if let Some(durable) = transition.durable.as_ref() {
        writer.durable_protected.push(NativeDurableProtection {
            allocation_head: durable.allocation_head(),
            protected: durable.protected(),
            mark: durable.mark(),
            intent: durable.intent(),
        });
    }
    let directory = transition.admitted.directory();
    for artifact in transition.artifacts {
        let path = crate::property_graph::storage::allocation::artifact_path(
            directory,
            artifact.identity.artifact,
        );
        transition
            .admitted
            .vfs()
            .create_new(&path, artifact.bytes)
            .map_err(|source| io(&path, source))?;
        record(W::ArtifactWrites, 1);
        record(W::ArtifactBytesWritten, artifact.bytes.len() as u64);
        if let crate::lifecycle::durability::SyncRequirement::Sync(kind) =
            store.durability_policy.data_file_sync()
        {
            if kind == crate::vfs::SyncKind::Full {
                record(W::FullSyncAttempts, 1);
            }
            transition
                .admitted
                .vfs()
                .sync(&path, kind)
                .map_err(|source| io(&path, source))?;
            if kind == crate::vfs::SyncKind::Full {
                record(W::FullSyncSuccesses, 1);
            }
        }
    }
    if let crate::lifecycle::durability::SyncRequirement::Sync(kind) =
        store.durability_policy.directory_sync()
    {
        record(W::DirectorySyncAttempts, 1);
        transition
            .admitted
            .vfs()
            .sync(directory, kind)
            .map_err(|source| io(directory, source))?;
        record(W::DirectorySyncSuccesses, 1);
    }

    // Both counters are established before append. Once append begins, every
    // failure is indeterminate and stops admission rather than recomputing state.
    let next_wal_bytes = writer
        .envelope_bytes
        .checked_add(transition.wal_bytes().len())
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let next_complete_envelopes = writer
        .complete_envelopes
        .checked_add(1)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let failure_path = store.directory.join("wal.ze").clone();
    // Close wins over caller cancellation, only before the irreversible append.
    match store.state().map_err(NativeGraphError::Store)? {
        crate::lifecycle::StoreState::Open => {}
        crate::lifecycle::StoreState::Closing => {
            return Err(NativeGraphError::Store(
                crate::lifecycle::StoreError::Closing,
            ));
        }
        crate::lifecycle::StoreState::Closed => {
            return Err(NativeGraphError::Store(
                crate::lifecycle::StoreError::Closed,
            ));
        }
    }
    control
        .checkpoint()
        .map_err(|_| NativeGraphError::Stage(StageError::Cancelled))?;
    let payload = crate::ingest::wal_payload::encode_graph_commit(transition.wal_bytes())
        .map_err(|_| NativeGraphError::Invalid("graph WAL payload"))?;
    let mut wal_slot = store.wal_writer.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "WAL writer",
        })
    })?;
    let wal = wal_slot
        .as_mut()
        .ok_or(crate::lifecycle::StoreError::ReadOnly)?;
    let mut active_slot = store.active.lock().map_err(|_| {
        NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
            component: "active segment",
        })
    })?;
    let active = active_slot
        .as_mut()
        .ok_or(crate::lifecycle::StoreError::Closed)?;
    let generation = transition.next.base().generation.get();
    if active.generation.checked_add(1) != Some(generation) {
        return Err(NativeGraphError::StalePreparation);
    }
    let publication = wal.manifest_publication()?.arm();
    let before_io = wal.io_work();
    let result = wal.commit_many(&[(crate::ingest::wal_payload::GRAPH_COMMIT_V1, &payload)]);
    let after_io = wal.io_work();
    for (index, kind) in [
        W::WalAppends,
        W::WalBytesAppended,
        W::FullSyncAttempts,
        W::FullSyncSuccesses,
    ]
    .into_iter()
    .enumerate()
    {
        record(
            kind,
            after_io
                .get(index)
                .copied()
                .unwrap_or(0)
                .saturating_sub(before_io.get(index).copied().unwrap_or(0)),
        );
    }
    record(
        W::EncodedWalBytes,
        after_io
            .get(1)
            .copied()
            .unwrap_or(0)
            .saturating_sub(before_io.get(1).copied().unwrap_or(0)),
    );
    let range = match result {
        Ok(range) => range,
        Err(source) if source.is_definite_wal_refusal() => {
            publication.complete();
            return Err(NativeGraphError::Store(source));
        }
        Err(source) => {
            writer.stopped = true;
            let _ = store.native_graph.stop_admissions();
            return Err(NativeGraphError::CommitIndeterminate {
                stage: "WAL commit",
                path: failure_path,
                source: Some(std::io::Error::other(source.to_string())),
            });
        }
    };
    writer.last_graph_seq = range.start.get();
    writer.envelope_bytes = next_wal_bytes;
    let expose = || {
        active.generation = generation;
        if store
            .native_graph
            .publish_committed_transition(transition)
            .is_err()
        {
            writer.stopped = true;
            let _ = store.native_graph.stop_admissions();
            return Err(NativeGraphError::CommitIndeterminate {
                stage: "bundle publication",
                path: failure_path,
                source: None,
            });
        }
        let _ = store.native_graph.pack_bytes_since_reclaim.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |bytes| Some(bytes.saturating_add(committed_bytes)),
        );
        let _ = store.native_graph.commits_since_reclaim.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |commits| Some(commits.saturating_add(1)),
        );
        writer.complete_envelopes = next_complete_envelopes;
        Ok(())
    };
    #[cfg(all(feature = "allocation-audit", any(test, feature = "test-seams")))]
    {
        if audit_publication {
            let ((result, denials), audit) = crate::allocation_audit::audit_engine_path(|| {
                crate::allocation_audit::fail_attributed_allocation(1, expose)
            });
            result?;
            publication.complete();
            return Ok(NativeCommitAudit {
                allocations: audit.allocations,
                denials,
            });
        }
    }
    #[cfg(not(all(feature = "allocation-audit", any(test, feature = "test-seams"))))]
    let _ = audit_publication;
    expose()?;
    publication.complete();
    Ok(NativeCommitAudit::default())
}

/// One outcome of the irreversible commit tail. `Checkpointed` means no batch
/// was committed: the caller must rebuild its whole attempt against the new
/// generation and try again.
pub(super) enum CommitStep {
    /// The staged batch changed no durable participant.
    NoOp,
    /// The batch is durable and published at `generation`.
    Committed {
        generation: GraphGeneration,
        audit: NativeCommitAudit,
    },
    /// A checkpoint ran instead of a commit; the caller re-loops.
    Checkpointed,
}

/// The shared commit tail: classify the staged batch, prepare its artifacts,
/// encode one envelope and publish it. Both the structured-write path and the
/// query-mutation path reach durability only through this function, so one
/// checkpoint, ordering and protection protocol exists.
#[allow(
    clippy::too_many_arguments,
    reason = "one commit carries its store, writer, lease, base, batch and controls"
)]
pub(super) fn commit_staged_batch<'m>(
    store: &crate::lifecycle::Store,
    writer: &mut NativeWriter,
    lease: &NativeReadLease,
    admitted: &Arc<NativeGraphBundle>,
    shared: &GraphResources,
    storage: &'m StorageMemory<'m>,
    control: &crate::lifecycle::QueryControl,
    base: &NativeAdmittedBase<'_, '_, '_, 'm>,
    staged_batch: &StagedBatch<'_>,
    allow_pending_checkpoint: &mut bool,
) -> Result<CommitStep, NativeGraphError> {
    // No-op acknowledgements share the same WAL failure state as changes.
    // Keep this check at the tail used by structured and query mutations.
    store
        .wal_writer
        .lock()
        .map_err(|_| crate::lifecycle::StoreError::Synchronization {
            component: "WAL writer",
        })?
        .as_ref()
        .ok_or(crate::lifecycle::StoreError::ReadOnly)?
        .manifest_publication()?
        .complete();
    if staged_batch.disposition() != BatchDisposition::Changed {
        return Ok(CommitStep::NoOp);
    }
    if writer.checkpoint_failed {
        return Err(NativeGraphError::CheckpointRequired);
    }
    let committed_tail = writer.envelope_bytes;
    if writer.complete_envelopes >= 64 || committed_tail >= MAX_ENVELOPE_BYTES {
        return match checkpoint_current(store, writer, admitted, shared, control) {
            Ok(()) | Err(NativeGraphError::StalePreparation) => Ok(CommitStep::Checkpointed),
            Err(error) => Err(error),
        };
    }

    let target_generation = staged_batch.target_generation();
    let source = base.source();
    let mut resources_guard = base.resources()?;
    let resources = &mut **resources_guard;
    let store_identity = admitted.base().store;
    let identity_source = || {
        let creation_serial = store
            .native_graph
            .burn_creation_serial()
            .map_err(|_| TreeError::Invalid("native creation serial unavailable"))?;
        let mut entropy = crate::property_graph::storage::allocation::OsEntropy;
        let value = crate::property_graph::storage::allocation::fresh_store_identity(&mut entropy)
            .map_err(TreeError::Io)?;
        Ok(ArtifactIdentity {
            store: store_identity,
            artifact: crate::property_graph::storage::artifact::ArtifactId::new(value.get())?,
            generation: target_generation,
            creation_serial,
        })
    };
    let pack_limits = PackLimits::default();
    #[cfg(all(feature = "graph-cypher", feature = "test-seams"))]
    let pack_limits =
        if crate::property_graph::query::native_relational_test_support::capacity_fixture_active() {
            PackLimits {
                blocks: 16_384,
                ..pack_limits
            }
        } else {
            pack_limits
        };
    let prepared_result = GraphPreparation::new(
        source,
        target_generation,
        identity_source,
        pack_limits,
        &store.tokenizer,
        resources,
    )?
    .prepare(staged_batch, resources);
    let prepared = prepared_result.map_err(|failure| {
        let (error, _, _) = failure.into_parts();
        NativeGraphError::Read(error)
    })?;
    if !prepared.matches_base(lease) {
        return Err(NativeGraphError::PreparedBaseChanged);
    }

    let catalog = NativePreparationCatalog::open(source, resources)?;
    let batch_catalog = BatchCatalog {
        base: &catalog,
        additions: staged_batch.symbols(),
    };
    let total_symbols = catalog
        .symbol_entries()
        .len()
        .checked_add(staged_batch.symbols().len())
        .ok_or(NativeGraphError::Invalid("catalog symbol count"))?;
    let mut merged = StorageBuffer::<SymbolEntry<'_>>::new(storage, total_symbols)?;
    for entry in catalog
        .symbol_entries()
        .iter()
        .chain(staged_batch.symbols())
    {
        merged.push(*entry)?;
    }

    let prepared_max_serial = prepared
        .inventory()
        .iter()
        .map(|change| change.object.serial)
        .max()
        .unwrap_or(store.native_graph.serial_fence()?);
    let high = staged_batch.high_waters();
    let high_symbols = [
        high.symbols.label,
        high.symbols.relationship_type,
        high.symbols.property,
        high.symbols.namespace,
    ];
    let catalog_changed = !staged_batch.symbols().is_empty()
        || high.node != admitted.high_waters().node
        || high.relationship != admitted.high_waters().relationship
        || high_symbols != admitted.high_waters().symbols;
    let (catalog_artifact, catalog_ref, catalog_floor) = if catalog_changed {
        let catalog_serial = store.native_graph.burn_creation_serial()?;
        if catalog_serial <= prepared_max_serial {
            return Err(NativeGraphError::Invalid("catalog creation serial order"));
        }
        let catalog_identity = ArtifactIdentity {
            store: store_identity,
            artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)?,
            generation: target_generation,
            creation_serial: catalog_serial,
        };
        let payload = catalog_payload(
            storage,
            control,
            store_identity,
            admitted.lexical(),
            admitted.document(),
            high.node,
            high.relationship,
            merged.as_slice(),
            high.symbols,
            catalog.relationship_rules(),
        )?;
        let (bytes, required) = encode_framed(
            storage,
            control,
            ContainerKind::Object,
            catalog_identity,
            &[Block {
                kind: BlockKind::CommitParticipant,
                payload: payload.as_slice(),
            }],
        )?;
        (Some((catalog_identity, bytes)), required, catalog_serial)
    } else {
        (None, admitted.catalog(), prepared_max_serial)
    };

    let inventory_serial = store.native_graph.burn_creation_serial()?;
    if inventory_serial <= catalog_floor {
        return Err(NativeGraphError::Invalid("inventory creation serial order"));
    }
    let inventory_identity = ArtifactIdentity {
        store: store_identity,
        artifact: next_artifact(&mut crate::property_graph::storage::allocation::OsEntropy)?,
        generation: target_generation,
        creation_serial: inventory_serial,
    };
    let inventory_payload = inventory_payload(
        storage,
        control,
        prepared.inventory(),
        catalog_artifact.as_ref().map(|_| catalog_ref.object),
    )?;
    let (inventory_bytes, inventory_ref) = encode_framed(
        storage,
        control,
        ContainerKind::Object,
        inventory_identity,
        &[Block {
            kind: BlockKind::CommitParticipant,
            payload: inventory_payload.as_slice(),
        }],
    )?;

    let protected_start = prepared
        .inventory()
        .len()
        .checked_add(usize::from(catalog_artifact.is_some()))
        .and_then(|count| count.checked_add(1))
        .ok_or(NativeGraphError::Invalid("protected descriptor count"))?;
    writer.can_protect(protected_start)?;
    let batch = BatchId::new(
        crate::property_graph::storage::allocation::fresh_store_identity(
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .map_err(|source| io(&store.directory.join("wal.ze"), source))?
        .get(),
    )?;
    // The format maximum is a refusal bound, not a reservation for every batch.
    // The transition measures its exact envelope before allocating this buffer.
    let mut envelope_bytes = StorageBuffer::new(storage, 0)?;
    let mut cancelled = || control.checkpoint().is_err();
    let mut wal_resources = WalResources::new(
        (MAX_ENVELOPE_BYTES as u64) * 4,
        STACK_RESERVATION_BYTES,
        &mut cancelled,
    )?
    .with_accounting(shared);
    let catalog_supplement =
        catalog_artifact
            .as_ref()
            .map(|(identity, bytes)| SupplementalArtifact {
                identity: *identity,
                bytes: bytes.as_slice(),
                required: catalog_ref,
            });
    let inventory_supplement = SupplementalArtifact {
        identity: inventory_identity,
        bytes: inventory_bytes.as_slice(),
        required: inventory_ref,
    };
    let mut supplemental = StorageBuffer::new(storage, 2)?;
    if let Some(catalog) = catalog_supplement {
        supplemental.push(InventoryChange {
            object: catalog.required.object,
            state: InventoryState::Prepared,
        })?;
    }
    supplemental.push(InventoryChange {
        object: inventory_ref.object,
        state: InventoryState::Prepared,
    })?;
    let _supplemental_registration = lease.register_prepared(supplemental.as_slice())?;
    #[allow(unused_mut)]
    let mut final_roots = prepared.candidate().roots();
    #[cfg(any(test, feature = "test-seams"))]
    if store
        .native_graph
        .substitute_old_out
        .swap(false, std::sync::atomic::Ordering::AcqRel)
    {
        let old = admitted.roots().for_generation(target_generation)?;
        final_roots.replace(old.directory(TreeKind::OutRanges)?)?;
    }
    let mut commit_artifacts = StorageBuffer::new(storage, protected_start)?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "transition-start",
        resources.work(),
    );
    let transition = prepare_committed_transition(
        store,
        target_generation,
        shared,
        control,
        lease,
        staged_batch,
        &prepared,
        final_roots,
        &batch_catalog,
        catalog_supplement,
        catalog_ref,
        inventory_supplement,
        batch,
        &mut envelope_bytes,
        &mut commit_artifacts,
        resources,
        &mut wal_resources,
    )?;

    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "transition-end",
        resources.work(),
    );
    let tail_bytes = writer.envelope_bytes;
    let pending_tail_bytes = tail_bytes
        .checked_add(transition.wal_bytes().len())
        .ok_or(NativeGraphError::IdentityExhausted)?;
    if pending_tail_bytes > MAX_ENVELOPE_BYTES {
        if !*allow_pending_checkpoint {
            return Err(NativeGraphError::WalTailBoundExceeded);
        }
        match checkpoint_current(store, writer, admitted, shared, control) {
            Ok(()) => {}
            Err(NativeGraphError::StalePreparation) => return Ok(CommitStep::Checkpointed),
            Err(error) => return Err(error),
        }
        *allow_pending_checkpoint = false;
        return Ok(CommitStep::Checkpointed);
    }

    let audit = protect_and_commit(store, writer, transition, control, true)?;
    Ok(CommitStep::Committed {
        generation: target_generation,
        audit,
    })
}

impl crate::lifecycle::Store {
    pub(crate) fn apply_native_graph(
        &self,
        requests: &[crate::property_graph::staging::StructuredWrite<'_, '_>],
        control: &crate::lifecycle::QueryControl,
    ) -> Result<NativePreparedResult<ReceiptRegistration>, NativeGraphError> {
        let mut materializer = ReceiptMaterializer;
        self.apply_native_graph_with_materializer(requests, control, &mut materializer)
    }

    pub(crate) fn apply_native_graph_with_materializer<M: ResultMaterializer>(
        &self,
        requests: &[crate::property_graph::staging::StructuredWrite<'_, '_>],
        control: &crate::lifecycle::QueryControl,
        materializer: &mut M,
    ) -> Result<NativePreparedResult<M::Registration>, NativeGraphError> {
        self.apply_native_graph_with_materializer_inner(requests, control, materializer, true)
    }

    /// Test-only durable monotone jump; ordinary writes still allocate IDs.
    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn jump_native_graph_allocators_for_test(
        &self,
        next_node: crate::property_graph::NodeId,
        next_relationship: crate::property_graph::RelId,
        control: &crate::lifecycle::QueryControl,
    ) -> Result<GraphGeneration, NativeGraphError> {
        self.native_graph.require_writable()?;
        let requests = &[];
        let mut allow_pending_checkpoint = true;
        loop {
            let mut writer_slot = self.native_graph.writer.lock().map_err(|_| {
                NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                    component: "native graph writer",
                })
            })?;
            let writer = writer_slot
                .as_mut()
                .ok_or_else(|| self.absent_native_graph_writer())?;
            if writer.stopped {
                return Err(NativeGraphError::WritesStopped);
            }

            let lease = self.admit_native_read()?;
            let admitted = Arc::clone(lease.bundle());
            let shared = GraphResources::from_store(self)?;
            let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
            let storage = StorageMemory::new(&write_memory, control, 32 * 1024 * 1024)?;
            let preparation_checkpoint = || match self.state() {
                Ok(crate::lifecycle::StoreState::Open) => Ok(()),
                Ok(
                    crate::lifecycle::StoreState::Closing | crate::lifecycle::StoreState::Closed,
                ) => Err(TreeError::Control(
                    crate::lifecycle::QueryError::ReadCancelled { partial: false },
                )),
                Err(error) => Err(TreeError::Control(crate::lifecycle::QueryError::Store(
                    error,
                ))),
            };
            let source = NativePreparationSource::new(
                &lease,
                &storage,
                crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            )?;
            let mut base_resources = source
                .resources(64 * 1024 * 1024)?
                .with_preparation_checkpoint(&preparation_checkpoint)?;
            let resources_cell = RefCell::new(&mut base_resources);
            let first_storage_error = Cell::new(None);
            let base = NativeAdmittedBase::new(
                &lease,
                &source,
                &storage,
                requests,
                &resources_cell,
                &first_storage_error,
            )?;
            let mut write_control = |phase| checkpoint(control, phase);
            let mut staged = crate::property_graph::staging::stage_structured_at_generation(
                &base,
                assigned_generation(self, admitted.base().generation)?,
                requests,
                &write_memory,
                &mut write_control,
            )?;
            staged.jump_allocators_for_test(next_node, next_relationship)?;
            match commit_staged_batch(
                self,
                writer,
                &lease,
                &admitted,
                &shared,
                &storage,
                control,
                &base,
                &staged,
                &mut allow_pending_checkpoint,
            )? {
                CommitStep::Checkpointed => continue,
                CommitStep::Committed { generation, .. } => return Ok(generation),
                CommitStep::NoOp => {
                    return Err(NativeGraphError::Invalid("allocator jump was not changed"));
                }
            }
        }
    }

    fn apply_native_graph_with_materializer_inner<M: ResultMaterializer>(
        &self,
        requests: &[crate::property_graph::staging::StructuredWrite<'_, '_>],
        control: &crate::lifecycle::QueryControl,
        materializer: &mut M,
        mut allow_pending_checkpoint: bool,
    ) -> Result<NativePreparedResult<M::Registration>, NativeGraphError> {
        let request_resources = GraphResources::from_store(self)?;
        let _request_work = request_resources.begin_work();
        self.native_graph.require_writable()?;
        let mut maintenance_checked = false;
        let mut run_maintenance = false;
        loop {
            if run_maintenance {
                self.auto_maintain_native_graph(control)?;
                run_maintenance = false;
            }
            let mut writer_slot = self.native_graph.writer.lock().map_err(|_| {
                NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                    component: "native graph writer",
                })
            })?;
            let writer = writer_slot
                .as_mut()
                .ok_or_else(|| self.absent_native_graph_writer())?;
            if writer.stopped {
                return Err(NativeGraphError::WritesStopped);
            }

            let lease = self.admit_native_read()?;
            let admitted = Arc::clone(lease.bundle());
            let shared = GraphResources::from_store(self)?;
            let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
            // Each structured mutation performs its own bounded directory edits.
            // Sharing one single-mutation allowance across a batch rejects valid
            // batches before either the 8 MiB input or 32 MiB storage cap is met.
            let mutation_count = u64::try_from(requests.len().max(1))
                .map_err(|_| NativeGraphError::Read(TreeError::Work))?;
            let default_preparation_work = (64_u64 * 1024 * 1024)
                .checked_mul(mutation_count)
                .ok_or(NativeGraphError::Read(TreeError::Work))?;
            #[cfg(any(test, feature = "test-seams"))]
            let (storage_limit, preparation_work) =
                crate::property_graph::storage::search::native_vector_index_test_limits(
                    32 * 1024 * 1024,
                    default_preparation_work,
                );
            #[cfg(not(any(test, feature = "test-seams")))]
            let (storage_limit, preparation_work) = (32 * 1024 * 1024, default_preparation_work);
            #[cfg(all(feature = "graph-cypher", feature = "test-seams"))]
            let preparation_work =
                crate::property_graph::query::native_relational_test_support::capacity_fixture_work(
                    preparation_work,
                );
            let storage = StorageMemory::new(&write_memory, control, storage_limit)?;
            let preparation_checkpoint = || match self.state() {
                Ok(crate::lifecycle::StoreState::Open) => Ok(()),
                Ok(
                    crate::lifecycle::StoreState::Closing | crate::lifecycle::StoreState::Closed,
                ) => Err(TreeError::Control(
                    crate::lifecycle::QueryError::ReadCancelled { partial: false },
                )),
                Err(error) => Err(TreeError::Control(crate::lifecycle::QueryError::Store(
                    error,
                ))),
            };
            let source = NativePreparationSource::new(
                &lease,
                &storage,
                crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            )?;
            let mut base_resources = source
                .resources(preparation_work)?
                .with_preparation_checkpoint(&preparation_checkpoint)?;
            let resources_cell = RefCell::new(&mut base_resources);
            let first_storage_error = Cell::new(None);
            let base = NativeAdmittedBase::new(
                &lease,
                &source,
                &storage,
                requests,
                &resources_cell,
                &first_storage_error,
            )?;
            let mut write_control = |phase| checkpoint(control, phase);
            let staged = stage_structured_with_results_at_generation(
                &base,
                assigned_generation(self, admitted.base().generation)?,
                requests,
                &write_memory,
                materializer,
                &mut write_control,
            );
            if let Some(error) = base.take_error() {
                return Err(NativeGraphError::Stage(StageError::NativeStorage(error)));
            }
            #[cfg(all(test, feature = "graph-cypher"))]
            crate::property_graph::storage::preparation_work_capture::phase(
                "staging-end",
                resources_cell.borrow().work(),
            );
            let materialized = staged?;
            let staged_batch = materialized.batch();
            // Refusals, replays and no-ops cannot authorize maintenance.
            // Drop this attempt before maintenance takes the writer lock,
            // then rebuild against the generation it publishes.
            if !maintenance_checked && staged_batch.disposition() == BatchDisposition::Changed {
                maintenance_checked = true;
                if self.native_graph_maintenance_due()? {
                    run_maintenance = true;
                    continue;
                }
            }
            let step = commit_staged_batch(
                self,
                writer,
                &lease,
                &admitted,
                &shared,
                &storage,
                control,
                &base,
                staged_batch,
                &mut allow_pending_checkpoint,
            )?;
            let (commit_audit, changed) = match step {
                CommitStep::NoOp => {
                    return Ok(NativePreparedResult::from_materialized(materialized, None));
                }
                CommitStep::Checkpointed => continue,
                CommitStep::Committed { audit, generation } => (audit, generation),
            };
            #[cfg(all(feature = "allocation-audit", any(test, feature = "test-seams")))]
            {
                let ((result, handoff_denied), handoff) =
                    crate::allocation_audit::audit_engine_path(|| {
                        crate::allocation_audit::fail_attributed_allocation(1, || {
                            NativePreparedResult::from_materialized(materialized, Some(changed))
                        })
                    });
                self.native_graph.commit_allocations.store(
                    commit_audit.allocations + handoff.allocations,
                    std::sync::atomic::Ordering::Release,
                );
                self.native_graph.commit_allocation_denials.store(
                    commit_audit.denials + handoff_denied,
                    std::sync::atomic::Ordering::Release,
                );
                return Ok(result);
            }
            #[cfg(not(all(feature = "allocation-audit", any(test, feature = "test-seams"))))]
            {
                let _ = commit_audit;
                return Ok(NativePreparedResult::from_materialized(
                    materialized,
                    Some(changed),
                ));
            }
        }
    }

    /// Seal holds WAL and active, so the current bundle cannot change while
    /// its inventory and fold transition are prepared. No durable write here.
    pub(crate) fn prepare_native_graph_seal(
        &self,
        manifest: &mut crate::manifest::Manifest,
        generation: u64,
        absorbed_through: u64,
    ) -> Result<NativeSealFold, NativeGraphError> {
        let admitted = self
            .native_graph
            .state
            .lock()
            .map_err(|_| {
                NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                    component: "native graph publication",
                })
            })?
            .current
            .clone()
            .ok_or(NativeGraphError::Invalid("seal fold has no current roots"))?;
        let graph = manifest.graph.as_ref().ok_or(NativeGraphError::Invalid(
            "seal fold without version barrier",
        ))?;
        if graph.graph_absorbed_through < admitted.base().fold.graph_absorbed_through {
            return Err(NativeGraphError::Store(
                crate::lifecycle::StoreError::Manifest(crate::manifest::ManifestError::Decode(
                    format!(
                        "cannot rotate WAL through {absorbed_through}: graph absorbed only through {}",
                        graph.graph_absorbed_through,
                    ),
                )),
            ));
        }
        let resources = GraphResources::from_store(self)?;
        let control = crate::lifecycle::QueryControl::Cancel(crate::lifecycle::CancelToken::new());
        let next = NativeGraphBundle::fold_transition(
            self,
            &resources,
            &admitted,
            crate::property_graph::staging::FoldMark {
                manifest_generation: generation,
                graph_absorbed_through: absorbed_through,
                envelope_sequence: admitted.sequence(),
            },
            None,
        )?;
        if admitted.sequence() == admitted.base().fold.envelope_sequence {
            // No graph state changed: preserve the already durable inventory
            // and generation history, just absorb the document-only interval.
            manifest
                .graph
                .as_mut()
                .ok_or(NativeGraphError::Invalid(
                    "seal fold without version barrier",
                ))?
                .graph_absorbed_through = absorbed_through;
        } else {
            let memory = WriteMemory::new(&resources, WriteLimits::default())?;
            let storage = StorageMemory::new(&memory, &control, 32 * 1024 * 1024)?;
            let objects = super::recovery::manifest_inventory(self, &admitted, &storage)?;
            manifest.graph = Some(
                crate::manifest::GraphManifest::new(
                    commit_state(&admitted),
                    absorbed_through,
                    objects,
                )
                .map_err(crate::lifecycle::StoreError::Manifest)?,
            );
            manifest
                .record_generation_bump(absorbed_through)
                .map_err(crate::lifecycle::StoreError::Manifest)?;
        }
        Ok(NativeSealFold { admitted, next })
    }

    /// Called only by the GraphStore close owner after Open -> Closing. No new
    /// writer can be admitted; an already admitted writer is drained by this
    /// lock. Stopped writers retain their WAL for recovery, never guessed state.
    pub(crate) fn checkpoint_native_graph_for_close(&self) -> Result<(), NativeGraphError> {
        let mut writer_slot = self.native_graph.writer.lock().map_err(|_| {
            NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                component: "native graph writer",
            })
        })?;
        let Some(writer) = writer_slot.as_mut() else {
            return Ok(());
        };
        if writer.stopped || writer.complete_envelopes == 0 {
            return Ok(());
        }
        loop {
            let admitted = self
                .native_graph
                .state
                .lock()
                .map_err(|_| {
                    NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                        component: "native graph publication",
                    })
                })?
                .current
                .clone()
                .ok_or(NativeGraphError::Invalid(
                    "native graph close has no current roots",
                ))?;
            let resources = GraphResources::from_store(self)?;
            let control =
                crate::lifecycle::QueryControl::Cancel(crate::lifecycle::CancelToken::new());
            match checkpoint_current(self, writer, &admitted, &resources, &control) {
                Err(NativeGraphError::StalePreparation) => continue,
                result => return result,
            }
        }
    }

    /// Purge and snapshot export already own state, WAL and active. Taking the native writer here
    /// would reverse the graph write lock order. Its counters/protection remain
    /// conservative until its next checkpoint; the manifest owns durability.
    pub(crate) fn checkpoint_native_graph_locked(
        &self,
        wal: &mut crate::ingest::StoreWal,
        active: &mut crate::ingest::ActiveState,
        graph_mark: u64,
        vfs: &dyn crate::vfs::Vfs,
    ) -> Result<(), NativeGraphError> {
        let admitted = self
            .native_graph
            .state
            .lock()
            .map_err(|_| {
                NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                    component: "native graph publication",
                })
            })?
            .current
            .clone()
            .ok_or(NativeGraphError::Invalid(
                "purge checkpoint has no current roots",
            ))?;
        let resources = GraphResources::from_store(self)?;
        let control = crate::lifecycle::QueryControl::Cancel(crate::lifecycle::CancelToken::new());
        let checkpoint_memory = WriteMemory::new(&resources, WriteLimits::default())?;
        let storage = StorageMemory::new(&checkpoint_memory, &control, 32 * 1024 * 1024)?;
        let objects = super::recovery::manifest_inventory(self, &admitted, &storage)?;
        checkpoint_manifest_locked(
            self, &admitted, graph_mark, wal, active, objects, &resources, vfs,
        )?;
        Ok(())
    }

    pub(crate) fn checkpoint_native_graph(
        &self,
        control: &crate::lifecycle::QueryControl,
    ) -> Result<(), NativeGraphError> {
        self.native_graph.require_writable()?;
        let mut writer_slot = self.native_graph.writer.lock().map_err(|_| {
            NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                component: "native graph writer",
            })
        })?;
        let writer = writer_slot
            .as_mut()
            .ok_or_else(|| self.absent_native_graph_writer())?;
        if writer.stopped {
            return Err(NativeGraphError::WritesStopped);
        }
        loop {
            let lease = self.admit_native_read()?;
            let admitted = Arc::clone(lease.bundle());
            let resources = GraphResources::from_store(self)?;
            match checkpoint_current(self, writer, &admitted, &resources, control) {
                Err(NativeGraphError::StalePreparation) => continue,
                result => return result,
            }
        }
    }

    pub(crate) fn admit_native_graph_maintenance(
        &self,
    ) -> Result<super::NativeMaintenanceAdmission, NativeGraphError> {
        self.native_graph.require_writable()?;
        let writer_slot = self.native_graph.writer.lock().map_err(|_| {
            NativeGraphError::Store(crate::lifecycle::StoreError::Synchronization {
                component: "native graph writer",
            })
        })?;
        let writer = writer_slot
            .as_ref()
            .ok_or_else(|| self.absent_native_graph_writer())?;
        if writer.stopped {
            return Err(NativeGraphError::WritesStopped);
        }
        let lease = self.admit_native_read()?;
        Ok(super::NativeMaintenanceAdmission {
            lease,
            serial_fence: self.native_graph.serial_fence()?,
        })
    }

    pub(crate) fn commit_native_graph_maintenance(
        &self,
        admission: &super::NativeMaintenanceAdmission,
        control: &crate::lifecycle::QueryControl,
    ) -> Result<super::maintenance::NativeMaintenanceReport, NativeGraphError> {
        super::maintenance::commit(self, admission, control)
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn commit_native_graph_maintenance_with_limits(
        &self,
        admission: &super::NativeMaintenanceAdmission,
        control: &crate::lifecycle::QueryControl,
        limits: super::maintenance::MaintenanceLimits,
    ) -> Result<super::maintenance::NativeMaintenanceReport, NativeGraphError> {
        super::maintenance::commit_with_limits(self, admission, control, limits)
    }
}
