//! Sparse retrieval preparation over the native candidate owned by one writer.

use super::codec::{
    MEMBERSHIP_BYTES, MembershipRow, Modality, ROOT_BYTES, ROW_BYTES, RootDescriptor,
    SOURCE_V1_BYTES, SOURCE_V2_BYTES, SOURCE_VALUE_BYTES, SourceFormat, SourceManifest,
    SourceValue, SparsePhysicalRoots, SparseRootState, SparseRoots, SparseRow,
    validate_catalog_interpretation, validate_row_correlation,
};
use super::vector_index::{NativeVectorRow, prepare_vector_index};
use crate::fts::graph_build::{GraphLexicalBuilder, GraphLexicalError};
use crate::fts::tokenizer::Analyzer;
use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::staging::{BaseIdentity, StagedBatch};
use crate::property_graph::storage::adjacency::NativeGraphCandidate;
use crate::property_graph::storage::artifact::{self, BlockKind, PhysicalRef};
use crate::property_graph::storage::consolidation::RecordRelocation;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::participant::BatchCatalog;
use crate::property_graph::storage::payload::{PayloadRef, prepare_payload, prepare_stream};
use crate::property_graph::storage::records::{NodeRecordState, RecordCatalog, verify_node_state};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::{TreeKind, directory::*};
use crate::property_graph::wal::{InventoryChange, Membership, RequiredRef};
use crate::property_graph::{EntityId, GraphGeneration, NodeId};

const VECTOR_SOURCE_MAX_ROWS: usize = 1_024;
const VECTOR_SOURCE_MAX_RESCORE_BYTES: usize = 4 * 1_024 * 1_024;
#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static MISS_NEXT_MAINTENANCE_PEER_RETARGET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn miss_next_maintenance_peer_retarget() {
    MISS_NEXT_MAINTENANCE_PEER_RETARGET.with(|scheduled| scheduled.set(true));
}

/// Exact analyzed transition emitted for the WAL mutation at one delta ordinal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreparedMembershipChange {
    pub(crate) ordinal: u32,
    pub(crate) node: Option<NodeId>,
    pub(crate) membership: Option<Membership>,
}

#[derive(Clone, Copy)]
struct PendingRow {
    node: NodeId,
    revision: u64,
    record: PayloadRef,
    analyzed_length: u32,
}

/// Pre-finish sparse roots and analyzed mutation membership. Finalization only
/// binds root blocks to the descriptors of the already-finished owned packs.
pub(crate) struct PreparedSparseCandidate<'m> {
    physical: SparsePhysicalRoots,
    changes: StorageBuffer<'m, PreparedMembershipChange>,
    generation: GraphGeneration,
    sequence: u64,
    base: SparseBase,
    native_roots: GraphRoots,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SparseBase {
    owner: u64,
    identity: BaseIdentity,
    sequence: u64,
    text: Option<RequiredRef>,
    vector: Option<RequiredRef>,
    catalog: RequiredRef,
}

impl SparseBase {
    fn from_lease(source: &NativeReadLease) -> Self {
        let admitted = source.bundle();
        Self {
            owner: source.token(),
            identity: admitted.base(),
            sequence: admitted.sequence(),
            text: admitted.text(),
            vector: admitted.vector(),
            catalog: admitted.catalog(),
        }
    }
}

impl PreparedSparseCandidate<'_> {
    pub(crate) fn changes(&self) -> &[PreparedMembershipChange] {
        self.changes.as_slice()
    }
    pub(crate) fn matches(
        &self,
        native: &NativeGraphCandidate<'_>,
        admitted: &crate::lifecycle::native_graph::NativeGraphBundle,
        owner: u64,
    ) -> bool {
        self.base
            == (SparseBase {
                owner,
                identity: admitted.base(),
                sequence: admitted.sequence(),
                text: admitted.text(),
                vector: admitted.vector(),
                catalog: admitted.catalog(),
            })
            && self.generation == native.target_generation()
            && self.sequence == native.sequence()
            && self.native_roots == native.roots()
    }
    pub(crate) fn finalize(&self, inventory: &[InventoryChange]) -> Result<SparseRoots, TreeError> {
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

fn required(block: PhysicalRef, inventory: &[InventoryChange]) -> Result<RequiredRef, TreeError> {
    let object = inventory
        .iter()
        .find(|entry| entry.object.artifact == block.artifact)
        .map(|entry| entry.object)
        .ok_or(TreeError::Invalid(
            "sparse root object is absent from inventory",
        ))?;
    Ok(RequiredRef { object, block })
}

fn lexical_error(error: GraphLexicalError) -> TreeError {
    match error {
        GraphLexicalError::Resource(error) => error,
        _ => TreeError::Invalid("sparse lexical preparation failed"),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn read_root<S: BlockSource>(
    source: &S,
    required: Option<RequiredRef>,
    modality: Modality,
    store: crate::property_graph::StoreInstanceId,
    generation: GraphGeneration,
    base_generation: GraphGeneration,
    base_sequence: u64,
    catalog: RequiredRef,
    lexical: crate::fts::tokenizer::TokenizerEpoch,
    document: Option<&crate::epoch::EmbeddingTower>,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<SparseRootState, TreeError> {
    let Some(required) = required else {
        if base_generation.get() != 0 {
            return Err(TreeError::Invalid("missing noninitial sparse root"));
        }
        return Ok(SparseRootState {
            members: DirectoryRoot::empty(store, TreeKind::SparseMembership, generation),
            sources: DirectoryRoot::empty(store, TreeKind::SparseSources, generation),
            live_rows: 0,
            live_length: 0,
            checkpoint: 0,
        });
    };
    let block = source.resolve(required.block, resources)?;
    if block.reference() != required.block
        || required.object.family != crate::format::FormatFamily::NativeGraphObject.id()
        || required.object.version != 1
        || block.identity().store != required.object.store
        || block.identity().artifact != required.object.artifact
        || block.identity().generation != required.object.generation
        || block.identity().creation_serial != required.object.serial
        || block.file_length() != required.object.bytes as usize
        || block.file_checksum() != required.object.checksum
        || required.block.kind != BlockKind::CommitParticipant
    {
        return Err(TreeError::Invalid(
            "sparse root required descriptor mismatch",
        ));
    }
    let descriptor = RootDescriptor::decode(block.payload())?;
    if descriptor.modality != modality
        || descriptor.store != store
        || descriptor.generation != base_generation
        || descriptor.sequence != base_sequence
        || descriptor.checkpoint > descriptor.sequence
        || descriptor.lexical != lexical
    {
        return Err(TreeError::Invalid(
            "sparse root base interpretation mismatch",
        ));
    }
    validate_catalog_interpretation(
        source,
        descriptor.catalog,
        store,
        lexical,
        document,
        super::view::SparseOwner::preparation(memory),
        resources,
    )?;
    let _ = catalog;
    Ok(SparseRootState {
        members: DirectoryRoot::from_reference(
            store,
            TreeKind::SparseMembership,
            generation,
            descriptor.members.reference(),
        )?,
        sources: DirectoryRoot::from_reference(
            store,
            TreeKind::SparseSources,
            generation,
            descriptor.sources.reference(),
        )?,
        live_rows: descriptor.live_rows,
        live_length: descriptor.live_length,
        checkpoint: descriptor.checkpoint,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn prepare_source<S: BlockSink>(
    sink: &mut S,
    store: crate::property_graph::StoreInstanceId,
    modality: Modality,
    rows: &[PendingRow],
    lexical: Option<&[u8]>,
    vector_index: Option<PayloadRef>,
    generation: GraphGeneration,
    sequence: u64,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<(PhysicalRef, PayloadRef, u64)>, TreeError> {
    if rows.is_empty() {
        return Ok(None);
    }
    let row_bytes_len = rows.len().checked_mul(ROW_BYTES).ok_or(TreeError::Memory)?;
    let mut row_bytes = StorageBuffer::new(memory, row_bytes_len)?;
    for row in rows {
        let mut encoded = [0_u8; ROW_BYTES];
        SparseRow {
            node: row.node,
            revision: row.revision,
            record: row.record,
            analyzed_length: row.analyzed_length,
        }
        .encode(&mut encoded)?;
        row_bytes.extend_from_slice(&encoded)?;
    }
    let row_table = prepare_payload(
        sink,
        store,
        generation,
        BlockKind::RetrievalRows,
        row_bytes.as_slice(),
        resources,
    )?;
    let lexical = lexical
        .map(|bytes| {
            prepare_payload(
                sink,
                store,
                generation,
                BlockKind::RetrievalLexical,
                bytes,
                resources,
            )
        })
        .transpose()?;
    let format = SourceFormat::V2;
    let manifest_length = match format {
        SourceFormat::V1 => SOURCE_V1_BYTES,
        SourceFormat::V2 => SOURCE_V2_BYTES,
    };
    let mut manifest = [0_u8; SOURCE_V2_BYTES];
    let encoded_manifest = manifest
        .get_mut(..manifest_length)
        .ok_or(TreeError::Invalid("sparse source manifest extent"))?;
    SourceManifest {
        format,
        modality,
        generation,
        sequence,
        rows: u32::try_from(rows.len()).map_err(|_| TreeError::Memory)?,
        row_table,
        lexical,
        vector_index,
    }
    .encode(encoded_manifest)?;
    let source = sink.append(
        BlockKind::CommitParticipant,
        generation,
        encoded_manifest,
        resources,
    )?;
    let mask_len = rows.len().checked_add(7).ok_or(TreeError::Memory)? / 8;
    let mut mask = StorageBuffer::new(memory, mask_len)?;
    for index in 0..mask_len {
        let remaining = rows.len().saturating_sub(index.saturating_mul(8));
        let value = if remaining >= 8 {
            u8::MAX
        } else {
            (1_u16
                .checked_shl(remaining as u32)
                .unwrap_or(0)
                .saturating_sub(1)) as u8
        };
        mask.push(value)?;
    }
    let mask = prepare_payload(
        sink,
        store,
        generation,
        BlockKind::RetrievalLiveRows,
        mask.as_slice(),
        resources,
    )?;
    let live_length = rows.iter().try_fold(0_u64, |sum, row| {
        sum.checked_add(u64::from(row.analyzed_length))
            .ok_or(TreeError::Memory)
    })?;
    Ok(Some((source, mask, live_length)))
}

struct MembershipValidator;
impl<S: BlockSource> LeafValidator<S> for MembershipValidator {
    fn verify(
        &mut self,
        _: &S,
        _: DirectoryRoot,
        entry: DirectoryEntry<'_>,
        _: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        MembershipRow::decode(entry.value()).map(|_| ())
    }
}

struct SourceValidator;
impl<S: BlockSource> LeafValidator<S> for SourceValidator {
    fn verify(
        &mut self,
        _: &S,
        _: DirectoryRoot,
        entry: DirectoryEntry<'_>,
        _: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        SourceValue::decode(entry.value()).map(|_| ())
    }
}

fn source_row<S: BlockSource>(
    source: &S,
    store: crate::property_graph::StoreInstanceId,
    member: MembershipRow,
    resources: &mut TreeResources<'_>,
) -> Result<(SourceManifest, SparseRow), TreeError> {
    let block = source.resolve(member.source, resources)?;
    if block.reference() != member.source || member.source.kind != BlockKind::CommitParticipant {
        return Err(TreeError::Invalid("sparse membership source mismatch"));
    }
    let manifest = SourceManifest::decode(block.payload())?;
    if member.row >= manifest.rows {
        return Err(TreeError::Invalid("sparse membership row is out of range"));
    }
    let table = PayloadSlice::new(source, store, manifest.generation, manifest.row_table);
    let mut encoded = [0_u8; ROW_BYTES];
    if table.read_at(
        u64::from(member.row) * ROW_BYTES as u64,
        &mut encoded,
        resources,
    )? != ROW_BYTES
    {
        return Err(TreeError::Invalid("short sparse source row"));
    }
    Ok((manifest, SparseRow::decode(&encoded)?))
}

#[allow(
    clippy::too_many_arguments,
    reason = "the pending edits share the existing preparation owners"
)]
fn remove_member_buffered<S: BlockSink>(
    sink: &mut S,
    mut state: SparseRootState,
    modality: Modality,
    node: NodeId,
    generation: GraphGeneration,
    memory: &StorageMemory<'_>,
    pending: &mut DirectoryBatch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(SparseRootState, bool), TreeError> {
    let node_key = node.get().to_le_bytes();
    let Some(entry) = lookup_entry(sink, state.members, &node_key, resources)? else {
        return Ok((state, false));
    };
    let member = MembershipRow::decode(entry.value())?;
    let (manifest, row) = source_row(sink, state.members.store(), member, resources)?;
    if manifest.modality != modality
        || validate_row_correlation(node, member, member.source, member.row, row).is_err()
    {
        return Err(TreeError::Invalid("sparse old membership correlation"));
    }
    let mut source_key = [0_u8; 32];
    artifact::encode_reference(member.source, &mut source_key)?;
    let source_entry = lookup_entry(sink, state.sources, &source_key, resources)?
        .ok_or(TreeError::Invalid("sparse old source is absent"))?;
    let old_value = SourceValue::decode(source_entry.value())?;
    let mask_len = usize::try_from(old_value.mask.len()).map_err(|_| TreeError::Memory)?;
    if mask_len != (manifest.rows as usize).saturating_add(7) / 8 {
        return Err(TreeError::Invalid("sparse live mask geometry"));
    }
    let mut mask = StorageBuffer::new(memory, mask_len)?;
    for _ in 0..mask_len {
        mask.push(0)?;
    }
    let mask_slice = PayloadSlice::new(
        sink,
        state.members.store(),
        source_entry.creation_generation(),
        old_value.mask,
    );
    if mask_slice.read_at(0, mask.as_mut_slice(), resources)? != mask_len {
        return Err(TreeError::Invalid("short sparse live mask"));
    }
    let byte = member.row as usize / 8;
    let bit = 1_u8 << (member.row % 8);
    let value = mask
        .as_mut_slice()
        .get_mut(byte)
        .ok_or(TreeError::Invalid("sparse mask row extent"))?;
    if *value & bit == 0 {
        return Err(TreeError::Invalid("sparse old row is not live"));
    }
    *value &= !bit;
    let mut scratch = TreeScratch::for_prepare(memory)?;
    pending.push(&node_key, None, resources)?;
    let row_length = if modality == Modality::Text {
        u64::from(row.analyzed_length)
    } else {
        0
    };
    let remaining_rows = old_value
        .live_rows
        .checked_sub(1)
        .ok_or(TreeError::Invalid("sparse source live-row underflow"))?;
    let remaining_length = old_value
        .live_length
        .checked_sub(row_length)
        .ok_or(TreeError::Invalid("sparse source length underflow"))?;
    if remaining_rows == 0 {
        if mask.as_slice().iter().any(|byte| *byte != 0) || remaining_length != 0 {
            return Err(TreeError::Invalid("sparse empty source retains live state"));
        }
        state.sources = remove_checked(
            sink,
            DirectoryMutation::new(state.sources, generation, SourceValidator),
            &source_key,
            &mut scratch,
            resources,
        )?;
    } else {
        let new_mask = prepare_payload(
            sink,
            state.members.store(),
            generation,
            BlockKind::RetrievalLiveRows,
            mask.as_slice(),
            resources,
        )?;
        let mut encoded = [0_u8; SOURCE_VALUE_BYTES];
        SourceValue {
            mask: new_mask,
            live_rows: remaining_rows,
            live_length: remaining_length,
        }
        .encode(&mut encoded)?;
        state.sources = insert_checked(
            sink,
            DirectoryMutation::new(state.sources, generation, SourceValidator),
            &source_key,
            &encoded,
            &mut scratch,
            resources,
        )?;
    }
    state.live_rows = state
        .live_rows
        .checked_sub(1)
        .ok_or(TreeError::Invalid("sparse total row underflow"))?;
    state.live_length = state
        .live_length
        .checked_sub(row_length)
        .ok_or(TreeError::Invalid("sparse total length underflow"))?;
    Ok((state, true))
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn install_source_buffered<S: BlockSink>(
    sink: &mut S,
    mut state: SparseRootState,
    source: PhysicalRef,
    mask: PayloadRef,
    rows: &[PendingRow],
    live_length: u64,
    generation: GraphGeneration,
    memory: &StorageMemory<'_>,
    pending: &mut DirectoryBatch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<SparseRootState, TreeError> {
    let mut scratch = TreeScratch::for_prepare(memory)?;
    let mut key = [0_u8; 32];
    artifact::encode_reference(source, &mut key)?;
    let mut value = [0_u8; SOURCE_VALUE_BYTES];
    SourceValue {
        mask,
        live_rows: rows.len() as u64,
        live_length,
    }
    .encode(&mut value)?;
    state.sources = insert_checked(
        sink,
        DirectoryMutation::new(state.sources, generation, SourceValidator),
        &key,
        &value,
        &mut scratch,
        resources,
    )?;
    for (row, pending_row) in rows.iter().enumerate() {
        let mut value = [0_u8; MEMBERSHIP_BYTES];
        MembershipRow {
            revision: pending_row.revision,
            source,
            row: u32::try_from(row).map_err(|_| TreeError::Memory)?,
        }
        .encode(&mut value)?;
        pending.push(
            &pending_row.node.get().to_le_bytes(),
            Some(&value),
            resources,
        )?;
    }
    state.live_rows = state
        .live_rows
        .checked_add(rows.len() as u64)
        .ok_or(TreeError::Memory)?;
    state.live_length = state
        .live_length
        .checked_add(live_length)
        .ok_or(TreeError::Memory)?;
    Ok(state)
}

/// Rebinds one indivisible sparse source cohort after a physical native-record
/// relocation. Immutable lexical/vector payloads and the live mask remain
/// shared; every live membership in the touched source follows its new manifest.
#[allow(clippy::too_many_arguments)]
pub(super) fn relocate_sparse_state<B: BlockSource, S: BlockSink>(
    source: &B,
    sink: &mut S,
    mut state: SparseRootState,
    modality: Modality,
    relocation: RecordRelocation,
    relocations: &[RecordRelocation],
    generation: GraphGeneration,
    sequence: u64,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<SparseRootState, TreeError> {
    #[cfg(any(test, feature = "test-support"))]
    let miss_peer = MISS_NEXT_MAINTENANCE_PEER_RETARGET.with(|scheduled| scheduled.replace(false));
    #[cfg(not(any(test, feature = "test-support")))]
    let miss_peer = false;
    let crate::property_graph::EntityId::Node(node) = relocation.entity else {
        return Ok(state);
    };
    let node_key = node.get().to_le_bytes();
    let Some(member_entry) = lookup_entry(&*sink, state.members, &node_key, resources)? else {
        return Ok(state);
    };
    let selected_member = MembershipRow::decode(member_entry.value())?;
    // A prior selected node in this cohort already patched all its relocations.
    let current = sink.resolve(selected_member.source, resources)?;
    let current_manifest = SourceManifest::decode(current.payload())?;
    if current_manifest.generation == generation && current_manifest.sequence == sequence {
        return Ok(state);
    }
    let source_block = source.resolve(selected_member.source, resources)?;
    if source_block.reference() != selected_member.source
        || selected_member.source.kind != BlockKind::CommitParticipant
    {
        return Err(TreeError::Invalid("maintenance sparse source reference"));
    }
    let manifest = SourceManifest::decode(source_block.payload())?;
    if manifest.modality != modality
        || manifest.generation > generation
        || manifest.sequence >= sequence
        || selected_member.row >= manifest.rows
    {
        return Err(TreeError::Invalid("maintenance sparse source cutoff"));
    }
    let mut old_source_key = [0_u8; 32];
    artifact::encode_reference(selected_member.source, &mut old_source_key)?;
    let source_entry = lookup_entry(&*sink, state.sources, &old_source_key, resources)?
        .ok_or(TreeError::Invalid("maintenance sparse source is absent"))?;
    let source_value = SourceValue::decode(source_entry.value())?;
    let mask = PayloadSlice::new(
        source,
        state.members.store(),
        source_entry.creation_generation(),
        source_value.mask,
    );
    let table = PayloadSlice::new(
        source,
        state.members.store(),
        manifest.generation,
        manifest.row_table,
    );
    let mut selected_bytes = [0_u8; ROW_BYTES];
    if table.read_at(
        u64::from(selected_member.row) * ROW_BYTES as u64,
        &mut selected_bytes,
        resources,
    )? != ROW_BYTES
    {
        return Err(TreeError::Invalid("short maintenance sparse selected row"));
    }
    let selected_row = SparseRow::decode(&selected_bytes)?;
    if selected_row.node != node
        || selected_row.revision != relocation.revision
        || selected_row.record != relocation.old_record
    {
        return Err(TreeError::Invalid(
            "maintenance sparse relocation correlation",
        ));
    }
    let mut patches = StorageBuffer::new(memory, relocations.len())?;
    for candidate in relocations {
        resources.step(1)?;
        let crate::property_graph::EntityId::Node(candidate_node) = candidate.entity else {
            continue;
        };
        let Some(entry) = lookup_entry(
            &*sink,
            state.members,
            &candidate_node.get().to_le_bytes(),
            resources,
        )?
        else {
            continue;
        };
        let member = MembershipRow::decode(entry.value())?;
        if member.source != selected_member.source {
            continue;
        }
        let mut bytes = [0_u8; ROW_BYTES];
        if table.read_at(
            u64::from(member.row) * ROW_BYTES as u64,
            &mut bytes,
            resources,
        )? != ROW_BYTES
        {
            return Err(TreeError::Invalid("short maintenance sparse patch row"));
        }
        let row = SparseRow::decode(&bytes)?;
        if row.node != candidate_node
            || row.revision != candidate.revision
            || row.record != candidate.old_record
        {
            return Err(TreeError::Invalid(
                "maintenance sparse relocation correlation",
            ));
        }
        let mut replacement = [0_u8; 48];
        candidate.new_record.encode_into(&mut replacement)?;
        patches.push((u64::from(member.row) * ROW_BYTES as u64 + 24, replacement))?;
    }
    let table_length = usize::try_from(manifest.row_table.len()).map_err(|_| TreeError::Memory)?;
    let row_table = prepare_stream(
        sink,
        state.members.store(),
        generation,
        BlockKind::RetrievalRows,
        table_length,
        &mut |offset, output, r| {
            if table.read_at(offset, output, r)? != output.len() {
                return Err(TreeError::Invalid("short maintenance sparse row table"));
            }
            let output_end = offset
                .checked_add(output.len() as u64)
                .ok_or(TreeError::Work)?;
            for (patch_start, replacement_record) in patches.as_slice() {
                r.step(1)?;
                let patch_start = *patch_start;
                let patch_end = patch_start + 48;
                let overlap_start = offset.max(patch_start);
                let overlap_end = output_end.min(patch_end);
                if overlap_start < overlap_end {
                    let target_start =
                        usize::try_from(overlap_start - offset).map_err(|_| TreeError::Memory)?;
                    let source_start = usize::try_from(overlap_start - patch_start)
                        .map_err(|_| TreeError::Memory)?;
                    let length = usize::try_from(overlap_end - overlap_start)
                        .map_err(|_| TreeError::Memory)?;
                    output
                        .get_mut(target_start..target_start + length)
                        .ok_or(TreeError::Invalid("maintenance sparse patch extent"))?
                        .copy_from_slice(
                            replacement_record
                                .get(source_start..source_start + length)
                                .ok_or(TreeError::Invalid("maintenance sparse patch source"))?,
                        );
                }
            }
            Ok(())
        },
        resources,
    )?;
    let manifest_length = match manifest.format {
        SourceFormat::V1 => SOURCE_V1_BYTES,
        SourceFormat::V2 => SOURCE_V2_BYTES,
    };
    let mut encoded_manifest = [0_u8; SOURCE_V2_BYTES];
    let encoded_manifest = encoded_manifest
        .get_mut(..manifest_length)
        .ok_or(TreeError::Invalid("maintenance sparse manifest extent"))?;
    SourceManifest {
        format: manifest.format,
        modality,
        generation,
        sequence,
        rows: manifest.rows,
        row_table,
        lexical: manifest.lexical,
        vector_index: manifest.vector_index,
    }
    .encode(encoded_manifest)?;
    let new_source = sink.append(
        BlockKind::CommitParticipant,
        generation,
        encoded_manifest,
        resources,
    )?;

    let mut scratch = TreeScratch::for_prepare(memory)?;
    state.sources = remove_checked(
        sink,
        DirectoryMutation::new(state.sources, generation, SourceValidator),
        &old_source_key,
        &mut scratch,
        resources,
    )?;
    let mut new_source_key = [0_u8; 32];
    artifact::encode_reference(new_source, &mut new_source_key)?;
    let mut encoded_source_value = [0_u8; SOURCE_VALUE_BYTES];
    source_value.encode(&mut encoded_source_value)?;
    state.sources = insert_checked(
        sink,
        DirectoryMutation::new(state.sources, generation, SourceValidator),
        &new_source_key,
        &encoded_source_value,
        &mut scratch,
        resources,
    )?;

    let mut pending = DirectoryBatch::new(memory, manifest.rows as usize)?;
    let mut observed_selected = false;
    let mut live_rows = 0_u64;
    let mut live_length = 0_u64;
    for ordinal in 0..manifest.rows {
        let mut bit = [0_u8; 1];
        if mask.read_at(u64::from(ordinal / 8), &mut bit, resources)? != 1 {
            return Err(TreeError::Invalid("short maintenance sparse live mask"));
        }
        let mut row_bytes = [0_u8; ROW_BYTES];
        if table.read_at(
            u64::from(ordinal) * ROW_BYTES as u64,
            &mut row_bytes,
            resources,
        )? != ROW_BYTES
        {
            return Err(TreeError::Invalid("short maintenance sparse cohort row"));
        }
        let row = SparseRow::decode(&row_bytes)?;
        if bit.first().copied().unwrap_or(0) & (1_u8 << (ordinal % 8)) == 0 {
            continue;
        }
        let prior = lookup_entry(
            &*sink,
            state.members,
            &row.node.get().to_le_bytes(),
            resources,
        )?
        .ok_or(TreeError::Invalid("maintenance sparse live peer is absent"))?;
        let prior = MembershipRow::decode(prior.value())?;
        if prior.source != selected_member.source
            || prior.row != ordinal
            || prior.revision != row.revision
        {
            return Err(TreeError::Invalid(
                "maintenance sparse live peer correlation",
            ));
        }
        let mut encoded = [0_u8; MEMBERSHIP_BYTES];
        MembershipRow {
            revision: row.revision,
            source: new_source,
            row: ordinal,
        }
        .encode(&mut encoded)?;
        if miss_peer && row.node != node {
            continue;
        }
        pending.push(&row.node.get().to_le_bytes(), Some(&encoded), resources)?;
        live_rows = live_rows.checked_add(1).ok_or(TreeError::Work)?;
        if modality == Modality::Text {
            live_length = live_length
                .checked_add(u64::from(row.analyzed_length))
                .ok_or(TreeError::Work)?;
        }
        observed_selected |= row.node == node && ordinal == selected_member.row;
    }
    if !observed_selected
        || live_rows != source_value.live_rows
        || live_length != source_value.live_length
    {
        return Err(TreeError::Invalid("maintenance sparse cohort aggregate"));
    }
    state.members = pending.flush(
        sink,
        DirectoryMutation::new(state.members, generation, MembershipValidator),
        &mut scratch,
        resources,
    )?;
    Ok(state)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_sparse_relocated_roots<B: BlockSource, S: BlockSink, C: RecordCatalog<B>>(
    source: &B,
    sink: &mut S,
    active: SparseRoots,
    base_native: GraphRoots,
    target_native: GraphRoots,
    catalog_required: RequiredRef,
    catalog: &C,
    document: Option<&crate::epoch::EmbeddingTower>,
    lexical: crate::fts::tokenizer::TokenizerEpoch,
    target_sequence: u64,
    relocations: &[RecordRelocation],
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<SparsePhysicalRoots, TreeError> {
    if target_native.store() != base_native.store()
        || target_native.generation().get()
            != base_native
                .generation()
                .get()
                .checked_add(1)
                .ok_or(TreeError::Work)?
        || target_sequence == 0
    {
        return Err(TreeError::Invalid("sparse maintenance relocation target"));
    }
    let view = super::view::SparseView::open(
        source,
        active,
        base_native,
        catalog_required,
        catalog,
        document,
        lexical,
        memory,
        resources,
    )?;
    if target_sequence != view.sequence().checked_add(1).ok_or(TreeError::Work)? {
        return Err(TreeError::Invalid("sparse maintenance target cutoff"));
    }
    view.validate_all(Modality::Text, resources)?;
    view.validate_all(Modality::Vector, resources)?;
    let interpretation_catalog = view.interpretation_catalog();

    let generation = target_native.generation();
    let mut text_state = read_root(
        source,
        active.text,
        Modality::Text,
        base_native.store(),
        generation,
        base_native.generation(),
        target_sequence - 1,
        interpretation_catalog,
        lexical,
        document,
        memory,
        resources,
    )?;
    let mut vector_state = if document.is_some() {
        read_root(
            source,
            active.vector,
            Modality::Vector,
            base_native.store(),
            generation,
            base_native.generation(),
            target_sequence - 1,
            interpretation_catalog,
            lexical,
            document,
            memory,
            resources,
        )?
    } else {
        if active.vector.is_some() {
            return Err(TreeError::Invalid(
                "vector sparse root without document space",
            ));
        }
        SparseRootState {
            members: DirectoryRoot::empty(
                base_native.store(),
                TreeKind::SparseMembership,
                generation,
            ),
            sources: DirectoryRoot::empty(base_native.store(), TreeKind::SparseSources, generation),
            live_rows: 0,
            live_length: 0,
            checkpoint: text_state.checkpoint,
        }
    };
    for relocation in relocations.iter().copied() {
        text_state = relocate_sparse_state(
            source,
            sink,
            text_state,
            Modality::Text,
            relocation,
            relocations,
            generation,
            target_sequence,
            memory,
            resources,
        )?;
        vector_state = relocate_sparse_state(
            source,
            sink,
            vector_state,
            Modality::Vector,
            relocation,
            relocations,
            generation,
            target_sequence,
            memory,
            resources,
        )?;
    }
    let text = append_root(
        sink,
        Modality::Text,
        base_native.store(),
        generation,
        target_sequence,
        interpretation_catalog,
        lexical,
        text_state,
        resources,
    )?;
    let vector = document
        .map(|_| {
            append_root(
                sink,
                Modality::Vector,
                base_native.store(),
                generation,
                target_sequence,
                interpretation_catalog,
                lexical,
                vector_state,
                resources,
            )
        })
        .transpose()?;
    Ok(SparsePhysicalRoots {
        text: Some(text),
        vector,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn append_root<S: BlockSink>(
    sink: &mut S,
    modality: Modality,
    store: crate::property_graph::StoreInstanceId,
    generation: GraphGeneration,
    sequence: u64,
    catalog: RequiredRef,
    lexical: crate::fts::tokenizer::TokenizerEpoch,
    state: SparseRootState,
    resources: &mut TreeResources<'_>,
) -> Result<PhysicalRef, TreeError> {
    let mut bytes = [0_u8; ROOT_BYTES];
    RootDescriptor {
        modality,
        store,
        generation,
        sequence,
        checkpoint: state.checkpoint,
        catalog,
        lexical,
        members: state.members,
        sources: state.sources,
        live_rows: state.live_rows,
        live_length: state.live_length,
    }
    .encode(&mut bytes)?;
    sink.append(BlockKind::CommitParticipant, generation, &bytes, resources)
}

/// Builds the text and optional vector sparse participant before owned packs finish.
#[allow(
    clippy::too_many_arguments,
    reason = "the coordinator passes one complete admitted state"
)]
pub(crate) fn prepare_sparse<'m, S: BlockSink, C: RecordCatalog<S>>(
    sink: &mut S,
    batch: &StagedBatch<'_>,
    native: &NativeGraphCandidate<'m>,
    catalog: &BatchCatalog<'_, C>,
    analyzer: &Analyzer,
    source: &NativeReadLease,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<PreparedSparseCandidate<'m>, TreeError> {
    resources.require_preparation(memory)?;
    memory.require_batch(batch)?;
    let base = SparseBase::from_lease(source);
    if base.identity != native.expected_base() || base.sequence != native.expected_sequence() {
        return Err(TreeError::Invalid(
            "sparse preparation source base mismatch",
        ));
    }
    let base_text = base.text;
    let base_vector = base.vector;
    let catalog_required = base.catalog;
    let document = source.bundle().document();
    let generation = native.target_generation();
    let store = native.roots().store();
    let mut text_state = read_root(
        sink,
        base_text,
        Modality::Text,
        store,
        generation,
        native.expected_base().generation,
        native.expected_sequence(),
        catalog_required,
        analyzer.epoch(),
        document,
        memory,
        resources,
    )?;
    let mut vector_state = if document.is_some() {
        read_root(
            sink,
            base_vector,
            Modality::Vector,
            store,
            generation,
            native.expected_base().generation,
            native.expected_sequence(),
            catalog_required,
            analyzer.epoch(),
            document,
            memory,
            resources,
        )?
    } else {
        if base_vector.is_some() {
            return Err(TreeError::Invalid(
                "vector sparse root without document space",
            ));
        }
        SparseRootState {
            members: DirectoryRoot::empty(store, TreeKind::SparseMembership, generation),
            sources: DirectoryRoot::empty(store, TreeKind::SparseSources, generation),
            live_rows: 0,
            live_length: 0,
            checkpoint: text_state.checkpoint,
        }
    };
    if document.is_some() && text_state.checkpoint != vector_state.checkpoint {
        return Err(TreeError::Invalid("sparse checkpoint marker mismatch"));
    }
    let mut text_edits = DirectoryBatch::new(memory, batch.deltas().len())?;
    let mut vector_edits = DirectoryBatch::new(memory, batch.deltas().len())?;
    let mut lexical_builder =
        GraphLexicalBuilder::new(analyzer, memory, resources).map_err(lexical_error)?;
    let mut text_rows = StorageBuffer::new(memory, batch.deltas().len())?;
    let mut vector_rows = StorageBuffer::new(memory, batch.deltas().len())?;
    let mut changes = StorageBuffer::new(memory, batch.deltas().len())?;
    let node_root = native.roots().directory(TreeKind::Nodes)?;
    for (ordinal, delta) in batch.deltas().iter().enumerate() {
        let fields = delta.provenance().fields();
        let EntityId::Node(node) = fields.incarnation else {
            changes.push(PreparedMembershipChange {
                ordinal: u32::try_from(ordinal).map_err(|_| TreeError::Memory)?,
                node: None,
                membership: None,
            })?;
            continue;
        };
        let key = node.get().to_le_bytes();
        let (next_text, old_text) = remove_member_buffered(
            sink,
            text_state,
            Modality::Text,
            node,
            generation,
            memory,
            &mut text_edits,
            resources,
        )?;
        text_state = next_text;
        let (next_vector, old_vector) = remove_member_buffered(
            sink,
            vector_state,
            Modality::Vector,
            node,
            generation,
            memory,
            &mut vector_edits,
            resources,
        )?;
        vector_state = next_vector;
        let Some(entry) = lookup_entry(sink, node_root, &key, resources)? else {
            return Err(TreeError::Invalid("sparse candidate node is absent"));
        };
        let record_ref = PayloadRef::decode(entry.value())?;
        let state = verify_node_state(
            PayloadSlice::new(sink, store, entry.creation_generation(), record_ref),
            node,
            catalog,
            document,
            resources,
        )?;
        let mut text_after = false;
        let mut vector_after = false;
        if let NodeRecordState::Live(record) = state {
            if let Some(text) = record.canonical().stored_text() {
                let length = usize::try_from(text.len()).map_err(|_| TreeError::Memory)?;
                let mut copy = StorageBuffer::new(memory, length)?;
                for _ in 0..length {
                    copy.push(0)?;
                }
                if text.read_at(0, copy.as_mut_slice(), resources)? != length {
                    return Err(TreeError::Invalid("short sparse text copy"));
                }
                let value = std::str::from_utf8(copy.as_slice())
                    .map_err(|_| TreeError::Invalid("invalid sparse text UTF-8"))?;
                if let Some(row) = lexical_builder
                    .push_text(value, resources)
                    .map_err(lexical_error)?
                {
                    text_after = true;
                    text_rows.push(PendingRow {
                        node,
                        revision: record.revision().get(),
                        record: record_ref,
                        analyzed_length: row,
                    })?;
                }
            }
            if record.canonical().stored_vector().is_some() {
                vector_after = true;
                vector_rows.push(PendingRow {
                    node,
                    revision: record.revision().get(),
                    record: record_ref,
                    analyzed_length: 0,
                })?;
            }
        }
        changes.push(PreparedMembershipChange {
            ordinal: u32::try_from(ordinal).map_err(|_| TreeError::Memory)?,
            node: Some(node),
            membership: Some(Membership {
                text_before: old_text,
                text_after,
                vector_before: old_vector,
                vector_after,
            }),
        })?;
    }
    let lexical = lexical_builder.finish(resources).map_err(lexical_error)?;
    if lexical.decoded().row_count() as usize != text_rows.as_slice().len() {
        return Err(TreeError::Invalid("sparse lexical row count mismatch"));
    }
    for (pending, length) in text_rows
        .as_mut_slice()
        .iter_mut()
        .zip(lexical.decoded().row_lengths())
    {
        pending.analyzed_length = *length;
    }
    let text_source = prepare_source(
        sink,
        store,
        Modality::Text,
        text_rows.as_slice(),
        Some(lexical.region()),
        None,
        generation,
        native.sequence(),
        memory,
        resources,
    )?;
    if let Some((source, mask, length)) = text_source {
        text_state = install_source_buffered(
            sink,
            text_state,
            source,
            mask,
            text_rows.as_slice(),
            length,
            generation,
            memory,
            &mut text_edits,
            resources,
        )?;
    }
    if !vector_rows.as_slice().is_empty() {
        let document = document.ok_or(TreeError::Invalid("vector source without document"))?;
        let dimensions = document.dims as usize;
        let row_bytes = dimensions.checked_mul(4).ok_or(TreeError::Memory)?;
        let byte_bound = if row_bytes > VECTOR_SOURCE_MAX_RESCORE_BYTES {
            1
        } else {
            (VECTOR_SOURCE_MAX_RESCORE_BYTES / row_bytes.max(1)).max(1)
        };
        let cohort_rows = VECTOR_SOURCE_MAX_ROWS.min(byte_bound);
        for cohort in vector_rows.as_slice().chunks(cohort_rows) {
            let coordinate_count = cohort
                .len()
                .checked_mul(dimensions)
                .ok_or(TreeError::Memory)?;
            let mut coordinates = StorageBuffer::new(memory, coordinate_count)?;
            let mut identities = StorageBuffer::new(memory, cohort.len())?;
            for row in cohort {
                let entry =
                    lookup_entry(sink, node_root, &row.node.get().to_le_bytes(), resources)?
                        .ok_or(TreeError::Invalid("vector cohort node is absent"))?;
                let record_ref = PayloadRef::decode(entry.value())?;
                if record_ref != row.record {
                    return Err(TreeError::Invalid("vector cohort record changed"));
                }
                let state = verify_node_state(
                    PayloadSlice::new(sink, store, entry.creation_generation(), record_ref),
                    row.node,
                    catalog,
                    Some(document),
                    resources,
                )?;
                let NodeRecordState::Live(record) = state else {
                    return Err(TreeError::Invalid("vector cohort record is not live"));
                };
                if record.revision().get() != row.revision {
                    return Err(TreeError::Invalid("vector cohort revision changed"));
                }
                let vector = record
                    .canonical()
                    .stored_vector()
                    .ok_or(TreeError::Invalid("vector cohort payload is absent"))?;
                if vector.dimensions() != document.dims {
                    return Err(TreeError::Invalid("vector cohort dimensions changed"));
                }
                identities.push(NativeVectorRow {
                    node: row.node,
                    revision: row.revision,
                })?;
                for dimension in 0..document.dims {
                    coordinates.push(vector.coordinate(dimension, resources)?)?;
                }
            }
            let vector_index = prepare_vector_index(
                sink,
                store,
                generation,
                identities.as_slice(),
                coordinates.as_slice(),
                dimensions,
                catalog_required,
                document.normalization,
                memory,
                resources,
            )?;
            let vector_source = prepare_source(
                sink,
                store,
                Modality::Vector,
                cohort,
                None,
                Some(vector_index),
                generation,
                native.sequence(),
                memory,
                resources,
            )?
            .ok_or(TreeError::Invalid("nonempty vector cohort was omitted"))?;
            vector_state = install_source_buffered(
                sink,
                vector_state,
                vector_source.0,
                vector_source.1,
                cohort,
                vector_source.2,
                generation,
                memory,
                &mut vector_edits,
                resources,
            )?;
        }
    }
    let mut scratch = TreeScratch::for_prepare(memory)?;
    text_state.members = text_edits.flush(
        sink,
        DirectoryMutation::new(text_state.members, generation, MembershipValidator),
        &mut scratch,
        resources,
    )?;
    vector_state.members = vector_edits.flush(
        sink,
        DirectoryMutation::new(vector_state.members, generation, MembershipValidator),
        &mut scratch,
        resources,
    )?;
    let text = append_root(
        sink,
        Modality::Text,
        store,
        generation,
        native.sequence(),
        catalog_required,
        analyzer.epoch(),
        text_state,
        resources,
    )?;
    let vector = if document.is_some() {
        Some(append_root(
            sink,
            Modality::Vector,
            store,
            generation,
            native.sequence(),
            catalog_required,
            analyzer.epoch(),
            vector_state,
            resources,
        )?)
    } else {
        None
    };
    Ok(PreparedSparseCandidate {
        physical: SparsePhysicalRoots {
            text: Some(text),
            vector,
        },
        changes,
        generation,
        sequence: native.sequence(),
        base,
        native_roots: native.roots(),
    })
}

#[cfg(test)]
fn remove_member<S: BlockSink>(
    sink: &mut S,
    state: SparseRootState,
    modality: Modality,
    node: NodeId,
    generation: GraphGeneration,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(SparseRootState, bool), TreeError> {
    let mut pending = DirectoryBatch::new(memory, 1)?;
    let (mut state, found) = remove_member_buffered(
        sink,
        state,
        modality,
        node,
        generation,
        memory,
        &mut pending,
        resources,
    )?;
    state.members = pending.flush(
        sink,
        DirectoryMutation::new(state.members, generation, MembershipValidator),
        &mut TreeScratch::for_prepare(memory)?,
        resources,
    )?;
    Ok((state, found))
}
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn install_source<S: BlockSink>(
    sink: &mut S,
    state: SparseRootState,
    source: PhysicalRef,
    mask: PayloadRef,
    rows: &[PendingRow],
    live_length: u64,
    generation: GraphGeneration,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<SparseRootState, TreeError> {
    let mut pending = DirectoryBatch::new(memory, rows.len())?;
    let mut state = install_source_buffered(
        sink,
        state,
        source,
        mask,
        rows,
        live_length,
        generation,
        memory,
        &mut pending,
        resources,
    )?;
    state.members = pending.flush(
        sink,
        DirectoryMutation::new(state.members, generation, MembershipValidator),
        &mut TreeScratch::for_prepare(memory)?,
        resources,
    )?;
    Ok(state)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "test assertions and fixed fixture indices"
)]
mod tests {
    use super::*;
    use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::staging::{WriteLimits, WriteMemory};
    use crate::property_graph::storage::artifact::{
        ArtifactId, ArtifactIdentity, Block, ContainerKind, FramedBlock,
    };
    use std::collections::BTreeMap;

    struct Objects {
        store: crate::property_graph::StoreInstanceId,
        next: u128,
        objects: BTreeMap<u128, Vec<u8>>,
    }

    impl Objects {
        fn new() -> Self {
            Self {
                store: crate::property_graph::StoreInstanceId::new(1_u128 << 100).unwrap(),
                next: 1,
                objects: BTreeMap::new(),
            }
        }
    }

    impl BlockSource for Objects {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            resources.step(1)?;
            let bytes = self
                .objects
                .get(&reference.artifact.get())
                .ok_or(TreeError::Missing)?;
            let frame = artifact::decode(
                ContainerKind::Object,
                Some((self.store, reference.artifact)),
                bytes,
            )?;
            Ok(frame.framed_block(reference)?)
        }
    }

    impl BlockSink for Objects {
        fn append(
            &mut self,
            kind: BlockKind,
            generation: GraphGeneration,
            bytes: &[u8],
            resources: &mut TreeResources<'_>,
        ) -> Result<PhysicalRef, TreeError> {
            resources.step(1)?;
            let artifact_id = ArtifactId::new(self.next)?;
            let identity = ArtifactIdentity {
                store: self.store,
                artifact: artifact_id,
                generation,
                creation_serial: self.next as u64,
            };
            let blocks = [Block {
                kind,
                payload: bytes,
            }];
            let mut output = vec![0; artifact::encoded_len(ContainerKind::Object, &blocks)?];
            artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut output)?;
            let reference = artifact::decode(
                ContainerKind::Object,
                Some((self.store, artifact_id)),
                &output,
            )?
            .reference(0)?;
            if self.objects.insert(self.next, output).is_some() {
                return Err(TreeError::Invalid("duplicate sparse fixture artifact"));
            }
            self.next += 1;
            Ok(reference)
        }
    }

    #[test]
    fn sparse_source_masks_are_cow_for_replace_and_delete() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let mut resources = TreeResources::for_prepare(&memory, u64::MAX).unwrap();
        let mut objects = Objects::new();
        let store_id = objects.store;
        let generation_one = GraphGeneration::new(1);
        let record_one = prepare_payload(
            &mut objects,
            store_id,
            generation_one,
            BlockKind::NodeRecord,
            b"record-one",
            &mut resources,
        )
        .unwrap();
        let node_a = NodeId::new(7).unwrap();
        let node_b = NodeId::new((1_u128 << 64) + 7).unwrap();
        let initial_rows = [
            PendingRow {
                node: node_a,
                revision: 1,
                record: record_one,
                analyzed_length: 2,
            },
            PendingRow {
                node: node_b,
                revision: 1,
                record: record_one,
                analyzed_length: 3,
            },
        ];
        let (source_one, mask_one, length_one) = prepare_source(
            &mut objects,
            store_id,
            Modality::Text,
            &initial_rows,
            Some(b"lexical-one"),
            None,
            generation_one,
            1,
            &memory,
            &mut resources,
        )
        .unwrap()
        .unwrap();
        let state = install_source(
            &mut objects,
            SparseRootState {
                members: DirectoryRoot::empty(store_id, TreeKind::SparseMembership, generation_one),
                sources: DirectoryRoot::empty(store_id, TreeKind::SparseSources, generation_one),
                live_rows: 0,
                live_length: 0,
                checkpoint: 0,
            },
            source_one,
            mask_one,
            &initial_rows,
            length_one,
            generation_one,
            &memory,
            &mut resources,
        )
        .unwrap();
        let retained_initial = state;

        let generation_two = GraphGeneration::new(2);
        let (state, before_replace) = remove_member(
            &mut objects,
            state,
            Modality::Text,
            node_a,
            generation_two,
            &memory,
            &mut resources,
        )
        .unwrap();
        assert!(before_replace);
        let record_two = prepare_payload(
            &mut objects,
            store_id,
            generation_two,
            BlockKind::NodeRecord,
            b"record-two",
            &mut resources,
        )
        .unwrap();
        let replacement_rows = [PendingRow {
            node: node_a,
            revision: 2,
            record: record_two,
            analyzed_length: 4,
        }];
        let (source_two, mask_two, length_two) = prepare_source(
            &mut objects,
            store_id,
            Modality::Text,
            &replacement_rows,
            Some(b"lexical-two"),
            None,
            generation_two,
            2,
            &memory,
            &mut resources,
        )
        .unwrap()
        .unwrap();
        let state = install_source(
            &mut objects,
            state,
            source_two,
            mask_two,
            &replacement_rows,
            length_two,
            generation_two,
            &memory,
            &mut resources,
        )
        .unwrap();
        assert_eq!((state.live_rows, state.live_length), (2, 7));
        let replaced = lookup_entry(
            &objects,
            state.members,
            &node_a.get().to_le_bytes(),
            &mut resources,
        )
        .unwrap()
        .unwrap();
        let replaced = MembershipRow::decode(replaced.value()).unwrap();
        assert_eq!(
            (replaced.revision, replaced.source, replaced.row),
            (2, source_two, 0)
        );
        let retained = lookup_entry(
            &objects,
            state.members,
            &node_b.get().to_le_bytes(),
            &mut resources,
        )
        .unwrap()
        .unwrap();
        assert_eq!(MembershipRow::decode(retained.value()).unwrap().revision, 1);
        assert!(
            lookup_entry(
                &objects,
                retained_initial.members,
                &node_a.get().to_le_bytes(),
                &mut resources,
            )
            .unwrap()
            .is_some()
        );

        let generation_three = GraphGeneration::new(3);
        let (state, before_delete) = remove_member(
            &mut objects,
            state,
            Modality::Text,
            node_b,
            generation_three,
            &memory,
            &mut resources,
        )
        .unwrap();
        assert!(before_delete);
        assert_eq!((state.live_rows, state.live_length), (1, 4));
        assert!(
            lookup_entry(
                &objects,
                state.members,
                &node_b.get().to_le_bytes(),
                &mut resources,
            )
            .unwrap()
            .is_none()
        );
        let mut source_key = [0_u8; 32];
        artifact::encode_reference(source_one, &mut source_key).unwrap();
        assert!(
            lookup_entry(&objects, state.sources, &source_key, &mut resources)
                .unwrap()
                .is_none()
        );
    }
}
