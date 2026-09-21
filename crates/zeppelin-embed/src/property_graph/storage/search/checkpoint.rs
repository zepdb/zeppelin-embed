//! Checked read-only checkpoint and replay-transition adapters for sparse retrieval.

use super::codec::{ROOT_BYTES, RootDescriptor, SparsePhysicalRoots};
use super::{Modality, PreparedMembershipChange, SparseRoots, SparseView};
use crate::epoch::EmbeddingTower;
use crate::fts::tokenizer::TokenizerEpoch;
use crate::property_graph::staging::{NormalizedDelta, StagedBatch};
use crate::property_graph::storage::artifact::{BlockKind, PhysicalRef};
use crate::property_graph::storage::consolidation::RecordRelocation;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::records::{NodeRecordState, RecordCatalog, verify_node_state};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::TreeKind;
use crate::property_graph::storage::tree::directory::{
    BlockSink, BlockSource, GraphRoots, TreeError, TreeResources, lookup_entry,
};
use crate::property_graph::wal::{
    Change, ChangeReader, InventoryChange, RequiredRef, WalError, WalResources,
};
use crate::property_graph::{EntityId, NodeId};

/// Logical sparse cutoff with both complete root descriptors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SparseCheckpoint {
    pub(crate) cutoff: u64,
    pub(crate) roots: SparseRoots,
}

/// Private checkpoint descriptors before their owning packs finish.
pub(crate) struct PreparedSparseCheckpoint {
    physical: SparsePhysicalRoots,
    base: SparseRoots,
    base_native: GraphRoots,
    target_native: GraphRoots,
    target_sequence: u64,
    catalog: RequiredRef,
}

impl PreparedSparseCheckpoint {
    pub(crate) fn matches(
        &self,
        base: SparseRoots,
        base_native: GraphRoots,
        target_native: GraphRoots,
        target_sequence: u64,
        catalog: RequiredRef,
    ) -> bool {
        self.base == base
            && self.base_native == base_native
            && self.target_native == target_native
            && self.target_sequence == target_sequence
            && self.catalog == catalog
    }

    pub(crate) fn finalize(&self, inventory: &[InventoryChange]) -> Result<SparseRoots, TreeError> {
        fn required(
            block: PhysicalRef,
            inventory: &[InventoryChange],
        ) -> Result<RequiredRef, TreeError> {
            let object = inventory
                .iter()
                .find(|entry| entry.object.artifact == block.artifact)
                .map(|entry| entry.object)
                .ok_or(TreeError::Invalid(
                    "sparse checkpoint object is absent from inventory",
                ))?;
            Ok(RequiredRef { object, block })
        }
        Ok(SparseRoots {
            text: self
                .physical
                .text
                .map(|block| required(block, inventory))
                .transpose()?,
            vector: self
                .physical
                .vector
                .map(|block| required(block, inventory))
                .transpose()?,
        })
    }
}

/// Re-emits only the two root descriptors at a validated complete cutoff. All
/// membership/source COW roots and immutable source bytes remain shared.
#[allow(
    clippy::too_many_arguments,
    reason = "checkpoint preparation binds every interpretation owner"
)]
pub(crate) fn prepare_sparse_checkpoint<'m, S: BlockSink, C: RecordCatalog<S>>(
    sink: &mut S,
    active: SparseRoots,
    native: GraphRoots,
    catalog_required: RequiredRef,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    lexical: TokenizerEpoch,
    cutoff: u64,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<PreparedSparseCheckpoint, TreeError> {
    let view = SparseView::open(
        &*sink,
        active,
        native,
        catalog_required,
        catalog,
        document,
        lexical,
        memory,
        resources,
    )?;
    if cutoff == 0 || cutoff != view.sequence() {
        return Err(TreeError::Invalid("sparse checkpoint preparation cutoff"));
    }
    view.validate_all(Modality::Text, resources)?;
    view.validate_all(Modality::Vector, resources)?;
    let generation = view.generation();
    let sequence = view.sequence();
    let interpretation_catalog = view.interpretation_catalog();
    let text_state = view
        .root_state(Modality::Text)
        .ok_or(TreeError::Invalid("missing sparse text checkpoint state"))?;
    let vector_state = view.root_state(Modality::Vector);
    drop(view);
    let append = |sink: &mut S,
                  modality: Modality,
                  state: super::codec::SparseRootState,
                  resources: &mut TreeResources<'_>|
     -> Result<PhysicalRef, TreeError> {
        let mut bytes = [0_u8; ROOT_BYTES];
        RootDescriptor {
            modality,
            store: native.store(),
            generation,
            sequence,
            checkpoint: cutoff,
            catalog: interpretation_catalog,
            lexical,
            members: state.members,
            sources: state.sources,
            live_rows: state.live_rows,
            live_length: state.live_length,
        }
        .encode(&mut bytes)?;
        sink.append(BlockKind::CommitParticipant, generation, &bytes, resources)
    };
    let text = append(sink, Modality::Text, text_state, resources)?;
    let vector = vector_state
        .map(|state| append(sink, Modality::Vector, state, resources))
        .transpose()?;
    Ok(PreparedSparseCheckpoint {
        physical: SparsePhysicalRoots {
            text: Some(text),
            vector,
        },
        base: active,
        base_native: native,
        target_native: native,
        target_sequence: sequence,
        catalog: catalog_required,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "maintenance binds old population roots to one new graph cutoff"
)]
pub(crate) fn prepare_sparse_maintenance<'m, B: BlockSource, S: BlockSink, C: RecordCatalog<B>>(
    source: &B,
    sink: &mut S,
    active: SparseRoots,
    base_native: GraphRoots,
    target_native: GraphRoots,
    catalog_required: RequiredRef,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    lexical: TokenizerEpoch,
    target_sequence: u64,
    relocations: &[RecordRelocation],
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<PreparedSparseCheckpoint, TreeError> {
    let physical = super::prepare::prepare_sparse_relocated_roots(
        source,
        sink,
        active,
        base_native,
        target_native,
        catalog_required,
        catalog,
        document,
        lexical,
        target_sequence,
        relocations,
        memory,
        resources,
    )?;
    Ok(PreparedSparseCheckpoint {
        physical,
        base: active,
        base_native,
        target_native,
        target_sequence,
        catalog: catalog_required,
    })
}

/// Reopens and streams the complete active populations at the supplied
/// complete-envelope sequence.
#[allow(
    clippy::too_many_arguments,
    reason = "checkpoint validation binds every interpretation owner"
)]
pub(crate) fn validate_checkpoint<'a, 'm, S: BlockSource, C: RecordCatalog<S>>(
    source: &'a S,
    checkpoint: SparseCheckpoint,
    native: GraphRoots,
    catalog_required: RequiredRef,
    catalog: &'a C,
    document: Option<&'a EmbeddingTower>,
    lexical: TokenizerEpoch,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<(u64, u64), TreeError> {
    let view = SparseView::open(
        source,
        checkpoint.roots,
        native,
        catalog_required,
        catalog,
        document,
        lexical,
        memory,
        resources,
    )?;
    if checkpoint.cutoff == 0
        || checkpoint.cutoff != view.sequence()
        || view.checkpoint() > checkpoint.cutoff
        || view.generation() != native.generation()
    {
        return Err(TreeError::Invalid("sparse checkpoint cutoff"));
    }
    let text = view.validate_all(Modality::Text, resources)?;
    let vector = view.validate_all(Modality::Vector, resources)?;
    Ok((text, vector))
}

fn transition_count(count: u64, before: bool, after: bool) -> Result<u64, TreeError> {
    match (before, after) {
        (false, true) => count.checked_add(1).ok_or(TreeError::Work),
        (true, false) => count.checked_sub(1).ok_or(TreeError::Invalid(
            "sparse replay transition count underflow",
        )),
        _ => Ok(count),
    }
}

fn validate_target_origin<S: BlockSource, C: RecordCatalog<S>>(
    source: &S,
    native: GraphRoots,
    node: NodeId,
    delta: &NormalizedDelta<'_>,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let root = native.directory(TreeKind::Nodes)?;
    let entry = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
        .ok_or(TreeError::Invalid("sparse replay target node is absent"))?;
    let record = PayloadRef::decode(entry.value())?;
    let state = verify_node_state(
        PayloadSlice::new(source, native.store(), entry.creation_generation(), record),
        node,
        catalog,
        document,
        resources,
    )?;
    let expected = delta.provenance().fields();
    let observed = match (state, delta.canonical()) {
        (NodeRecordState::Live(record), Some(canonical)) => {
            if record
                .canonical_bytes()
                .compare_bytes(canonical, resources)?
                != std::cmp::Ordering::Equal
            {
                return Err(TreeError::Invalid("sparse replay canonical origin"));
            }
            record
                .provenance()
                .fields_with_key(expected.key, resources)?
        }
        (NodeRecordState::Tombstone(tombstone), None) => tombstone
            .provenance()
            .fields_with_key(expected.key, resources)?,
        _ => return Err(TreeError::Invalid("sparse replay live state origin")),
    };
    if observed != expected {
        return Err(TreeError::Invalid("sparse replay operation provenance"));
    }
    Ok(())
}

/// Validates one complete read-only active transition against its normalized
/// canonical changes and exact analyzed membership output.
#[allow(
    clippy::too_many_arguments,
    reason = "replay validation binds both complete states and owners"
)]
pub(crate) fn validate_replay_transition<'a, 'm, S: BlockSource, C: RecordCatalog<S>>(
    source: &'a S,
    base: SparseCheckpoint,
    base_native: GraphRoots,
    target_roots: SparseRoots,
    target_native: GraphRoots,
    catalog_required: RequiredRef,
    catalog: &'a C,
    document: Option<&'a EmbeddingTower>,
    lexical: TokenizerEpoch,
    batch: &StagedBatch<'_>,
    changes: &[PreparedMembershipChange],
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<(u64, u64), TreeError> {
    if changes.len() != batch.deltas().len() || changes.is_empty() {
        return Err(TreeError::Invalid("sparse replay change cardinality"));
    }
    let base_view = SparseView::open(
        source,
        base.roots,
        base_native,
        catalog_required,
        catalog,
        document,
        lexical,
        memory,
        resources,
    )?;
    let target_view = SparseView::open(
        source,
        target_roots,
        target_native,
        catalog_required,
        catalog,
        document,
        lexical,
        memory,
        resources,
    )?;
    if base.cutoff != base_view.sequence()
        || target_view.checkpoint() != base_view.checkpoint()
        || target_view.sequence() != base.cutoff.checked_add(1).ok_or(TreeError::Work)?
        || target_view.generation() != target_native.generation()
    {
        return Err(TreeError::Invalid("sparse replay cutoff order"));
    }
    let mut expected_text = base_view.validate_all(Modality::Text, resources)?;
    let mut expected_vector = base_view.validate_all(Modality::Vector, resources)?;
    for (ordinal, (delta, change)) in batch.deltas().iter().zip(changes).enumerate() {
        if change.ordinal != u32::try_from(ordinal).map_err(|_| TreeError::Memory)? {
            return Err(TreeError::Invalid("sparse replay delta order"));
        }
        let fields = delta.provenance().fields();
        match fields.incarnation {
            EntityId::Relationship(_) => {
                if change.node.is_some() || change.membership.is_some() {
                    return Err(TreeError::Invalid("sparse replay relationship membership"));
                }
            }
            EntityId::Node(node) => {
                if change.node != Some(node) {
                    return Err(TreeError::Invalid("sparse replay node identity"));
                }
                let membership = change.membership.ok_or(TreeError::Invalid(
                    "sparse replay missing analyzed membership",
                ))?;
                let before_text = base_view.lookup(Modality::Text, node, resources)?.is_some();
                let before_vector = base_view
                    .lookup(Modality::Vector, node, resources)?
                    .is_some();
                let after_text = target_view
                    .lookup(Modality::Text, node, resources)?
                    .is_some();
                let after_vector = target_view
                    .lookup(Modality::Vector, node, resources)?
                    .is_some();
                if (membership.text_before, membership.vector_before)
                    != (before_text, before_vector)
                    || (membership.text_after, membership.vector_after)
                        != (after_text, after_vector)
                {
                    return Err(TreeError::Invalid("sparse replay analyzed membership"));
                }
                validate_target_origin(
                    source,
                    target_native,
                    node,
                    delta,
                    catalog,
                    document,
                    resources,
                )?;
                expected_text = transition_count(expected_text, before_text, after_text)?;
                expected_vector = transition_count(expected_vector, before_vector, after_vector)?;
            }
        }
    }
    base_view.validate_unchanged_membership(&target_view, Modality::Text, changes, resources)?;
    base_view.validate_unchanged_membership(&target_view, Modality::Vector, changes, resources)?;
    let target_text = target_view.validate_all(Modality::Text, resources)?;
    let target_vector = target_view.validate_all(Modality::Vector, resources)?;
    if (expected_text, expected_vector) != (target_text, target_vector) {
        return Err(TreeError::Invalid(
            "sparse replay unexplained active population",
        ));
    }
    Ok((target_text, target_vector))
}

/// Validates one complete persisted WAL transition directly from its borrowed
/// mutation frames. The caller retains the immutable WAL bytes for this scope.
#[allow(
    clippy::too_many_arguments,
    reason = "persisted replay validation binds both complete states and owners"
)]
pub(crate) fn validate_persisted_replay_transition<'a, 'm, S: BlockSource, C: RecordCatalog<S>>(
    source: &'a S,
    base: SparseCheckpoint,
    base_native: GraphRoots,
    target_roots: SparseRoots,
    target_native: GraphRoots,
    catalog_required: RequiredRef,
    catalog: &'a C,
    document: Option<&'a EmbeddingTower>,
    lexical: TokenizerEpoch,
    mut changes: ChangeReader<'_>,
    wal: &mut WalResources<'_>,
    wal_error: &mut Option<WalError>,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<(u64, u64), TreeError> {
    let base_empty = base.cutoff == 0 && base.roots == SparseRoots::default();
    if base.cutoff == 0 && !base_empty {
        return Err(TreeError::Invalid("sparse replay zero base cutoff"));
    }
    let base_view = if base_empty {
        None
    } else {
        Some(SparseView::open(
            source,
            base.roots,
            base_native,
            catalog_required,
            catalog,
            document,
            lexical,
            memory,
            resources,
        )?)
    };
    let target_view = SparseView::open(
        source,
        target_roots,
        target_native,
        catalog_required,
        catalog,
        document,
        lexical,
        memory,
        resources,
    )?;
    if target_view.sequence() != base.cutoff.checked_add(1).ok_or(TreeError::Work)?
        || target_view.generation() != target_native.generation()
        || base_view.as_ref().is_some_and(|view| {
            view.sequence() != base.cutoff || target_view.checkpoint() != view.checkpoint()
        })
    {
        return Err(TreeError::Invalid("persisted sparse replay cutoff order"));
    }
    let mut expected_text = match &base_view {
        Some(view) => view.validate_all(Modality::Text, resources)?,
        None => 0,
    };
    let mut expected_vector = match &base_view {
        Some(view) => view.validate_all(Modality::Vector, resources)?,
        None => 0,
    };
    let mut persisted = StorageBuffer::new(memory, crate::property_graph::MAX_GRAPH_CHANGES)?;
    let mut ordinal = 0_u32;
    while let Some(change) = changes.next_change(wal).map_err(|error| {
        if wal_error.is_none() {
            *wal_error = Some(error);
        }
        TreeError::Invalid("persisted sparse replay change")
    })? {
        let Change::Mutation(mutation) = change else {
            if matches!(
                change,
                Change::ReclaimIntent(_) | Change::ReclaimComplete(_)
            ) {
                return Err(TreeError::Invalid("persisted sparse reclaim change"));
            }
            continue;
        };
        let node = match mutation.provenance.incarnation {
            EntityId::Relationship(_) => {
                if mutation.membership != crate::property_graph::wal::Membership::default() {
                    return Err(TreeError::Invalid("persisted relationship membership"));
                }
                None
            }
            EntityId::Node(node) => {
                let before_text = match &base_view {
                    Some(view) => view.lookup(Modality::Text, node, resources)?.is_some(),
                    None => false,
                };
                let before_vector = match &base_view {
                    Some(view) => view.lookup(Modality::Vector, node, resources)?.is_some(),
                    None => false,
                };
                let after_text = target_view
                    .lookup(Modality::Text, node, resources)?
                    .is_some();
                let after_vector = target_view
                    .lookup(Modality::Vector, node, resources)?
                    .is_some();
                if (
                    mutation.membership.text_before,
                    mutation.membership.vector_before,
                ) != (before_text, before_vector)
                    || (
                        mutation.membership.text_after,
                        mutation.membership.vector_after,
                    ) != (after_text, after_vector)
                {
                    return Err(TreeError::Invalid("persisted sparse analyzed membership"));
                }
                expected_text = transition_count(expected_text, before_text, after_text)?;
                expected_vector = transition_count(expected_vector, before_vector, after_vector)?;
                Some(node)
            }
        };
        persisted.push(PreparedMembershipChange {
            ordinal,
            node,
            membership: node.map(|_| mutation.membership),
        })?;
        ordinal = ordinal.checked_add(1).ok_or(TreeError::Work)?;
    }
    if let Some(base_view) = &base_view {
        base_view.validate_unchanged_membership(
            &target_view,
            Modality::Text,
            persisted.as_slice(),
            resources,
        )?;
        base_view.validate_unchanged_membership(
            &target_view,
            Modality::Vector,
            persisted.as_slice(),
            resources,
        )?;
    }
    let target_text = target_view.validate_all(Modality::Text, resources)?;
    let target_vector = target_view.validate_all(Modality::Vector, resources)?;
    if (expected_text, expected_vector) != (target_text, target_vector) {
        return Err(TreeError::Invalid(
            "persisted sparse unexplained active population",
        ));
    }
    Ok((target_text, target_vector))
}

/// Validates a physical-only maintenance transition. Both strict views must
/// already resolve every native/sparse correlation; this additionally proves
/// that changed source metadata is an exact record-reference rebind.
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_persisted_maintenance_transition<
    'a,
    'm,
    S: BlockSource,
    C: RecordCatalog<S>,
>(
    source: &'a S,
    base: SparseCheckpoint,
    base_native: GraphRoots,
    target_roots: SparseRoots,
    target_native: GraphRoots,
    catalog_required: RequiredRef,
    catalog: &'a C,
    document: Option<&'a EmbeddingTower>,
    lexical: TokenizerEpoch,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<(u64, u64), TreeError> {
    if base.cutoff == 0 || base.roots == SparseRoots::default() {
        return Err(TreeError::Invalid("sparse maintenance base cutoff"));
    }
    let base_view = SparseView::open(
        source,
        base.roots,
        base_native,
        catalog_required,
        catalog,
        document,
        lexical,
        memory,
        resources,
    )?;
    let target_view = SparseView::open(
        source,
        target_roots,
        target_native,
        catalog_required,
        catalog,
        document,
        lexical,
        memory,
        resources,
    )?;
    if base_view.sequence() != base.cutoff
        || target_view.sequence() != base.cutoff.checked_add(1).ok_or(TreeError::Work)?
        || target_view.generation() != target_native.generation()
        || target_view.checkpoint() != base_view.checkpoint()
    {
        return Err(TreeError::Invalid("sparse maintenance cutoff order"));
    }
    let text = base_view.validate_maintenance_rebind(&target_view, Modality::Text, resources)?;
    let vector =
        base_view.validate_maintenance_rebind(&target_view, Modality::Vector, resources)?;
    if text != target_view.validate_all(Modality::Text, resources)?
        || vector != target_view.validate_all(Modality::Vector, resources)?
    {
        return Err(TreeError::Invalid("sparse maintenance population changed"));
    }
    Ok((text, vector))
}
