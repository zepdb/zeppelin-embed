//! Complete bounded tracing for one admitted sparse retrieval participant.

use super::codec::{
    MEMBERSHIP_BYTES, MembershipRow, Modality, ROW_BYTES, SOURCE_VALUE_BYTES, SourceFormat,
    SourceManifest, SourceValue, SparseRootState, SparseRoots, SparseRow, validate_row_correlation,
};
use super::vector_index::{NativeVectorIndex, open_vector_index, validate_vector_index_rows};
use super::view::{SparseLexical, SparseOwner, SparseView, decode_lexical_prepare};
use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::storage::artifact::{self, BlockKind, PhysicalRef};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::records::{RecordCatalog, verify_record};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{
    BlockSource, DirectoryRoot, DirectoryTraceEvent, DirectoryTraceState, TreeError, TreeResources,
    TreeTraceReservation, lookup_fixed_scoped,
};
use crate::property_graph::storage::tree::{Key, TreeKind};
use crate::property_graph::storage::{NativePreparationCatalog, NativePreparationSource};
use crate::property_graph::wal::RequiredRef;
use crate::property_graph::{EntityId, GraphGeneration, GraphRevision, NodeId, StoreInstanceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SearchTraceResult {
    pub(crate) count: usize,
    pub(crate) complete: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct SparseTraceRecordFacts {
    revision: GraphRevision,
    has_text: bool,
    has_vector: bool,
    required_payloads: [PayloadRef; 2],
}

pub(crate) fn verify_sparse_trace_record<S: BlockSource>(
    payload: PayloadSlice<'_, S>,
    node: NodeId,
    catalog: &impl RecordCatalog<S>,
    document: Option<&crate::epoch::EmbeddingTower>,
    resources: &mut TreeResources<'_>,
) -> Result<SparseTraceRecordFacts, TreeError> {
    let record = verify_record(payload, EntityId::Node(node), catalog, document, resources)?;
    Ok(SparseTraceRecordFacts {
        revision: record.revision(),
        has_text: record.canonical().stored_text().is_some(),
        has_vector: record.canonical().stored_vector().is_some(),
        required_payloads: record.required_payloads(),
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DirectoryFamily {
    Members,
    Sources,
}

struct DirectoryWalk<'m> {
    root: DirectoryRoot,
    members: DirectoryRoot,
    sources: DirectoryRoot,
    family: DirectoryFamily,
    modality: Modality,
    descriptor_generation: GraphGeneration,
    descriptor_sequence: u64,
    trace: DirectoryTraceState<'m>,
}

#[allow(
    clippy::large_enum_variant,
    reason = "directory traversal events retain bounded inline state"
)]
enum DirectoryEvent {
    Reference(PhysicalRef),
    Member,
    Source(SourceState),
    Done,
}

impl<'m> DirectoryWalk<'m> {
    #[allow(
        clippy::too_many_arguments,
        reason = "one checked sparse directory context"
    )]
    fn new(
        root: DirectoryRoot,
        members: DirectoryRoot,
        sources: DirectoryRoot,
        family: DirectoryFamily,
        modality: Modality,
        generation: GraphGeneration,
        sequence: u64,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        let trace = DirectoryTraceState::new(root, resources)?;
        Ok(Self {
            root,
            members,
            sources,
            family,
            modality,
            descriptor_generation: generation,
            descriptor_sequence: sequence,
            trace,
        })
    }

    fn width(&self) -> usize {
        match self.family {
            DirectoryFamily::Members => 16,
            DirectoryFamily::Sources => 32,
        }
    }

    fn next<S: BlockSource>(
        &mut self,
        source: &S,
        resources: &mut TreeResources<'_>,
    ) -> Result<DirectoryEvent, TreeError> {
        match self.trace.next(source, resources)? {
            DirectoryTraceEvent::Reference(reference) => Ok(DirectoryEvent::Reference(reference)),
            DirectoryTraceEvent::Leaf(leaf) => {
                self.trace.with_leaf(leaf, resources, |entry, resources| {
                    let Key::Inline(key) = entry.key() else {
                        return Err(TreeError::Invalid("sparse trace overflow key"));
                    };
                    if key.len() != self.width() {
                        return Err(TreeError::Invalid("sparse trace key width"));
                    }
                    match self.family {
                        DirectoryFamily::Members => {
                            let value = entry.value();
                            if value.len() != MEMBERSHIP_BYTES {
                                return Err(TreeError::Invalid("sparse trace membership width"));
                            }
                            let member = MembershipRow::decode(value)?;
                            let mut source_key = [0_u8; 32];
                            artifact::encode_reference(member.source, &mut source_key)?;
                            let mut source_value = [0_u8; SOURCE_VALUE_BYTES];
                            if lookup_fixed_scoped(
                                source,
                                self.sources,
                                &source_key,
                                &mut source_value,
                                resources,
                            )? != Some(SOURCE_VALUE_BYTES)
                            {
                                return Err(TreeError::Invalid("sparse source value width"));
                            }
                            SourceValue::decode(&source_value)?;
                            Ok(DirectoryEvent::Member)
                        }
                        DirectoryFamily::Sources => {
                            let value = entry.value();
                            if value.len() != SOURCE_VALUE_BYTES {
                                return Err(TreeError::Invalid("sparse source value width"));
                            }
                            let reference = artifact::decode_reference(key)?;
                            let value = SourceValue::decode(value)?;
                            let (block_reference, block_store, block_generation, manifest) = source
                                .with_block(reference, resources, |block, _resources| {
                                    Ok((
                                        block.reference(),
                                        block.identity().store,
                                        block.identity().generation,
                                        SourceManifest::decode(block.payload())?,
                                    ))
                                })?;
                            if block_reference != reference
                                || reference.kind != BlockKind::CommitParticipant
                                || block_store != self.root.store()
                                || block_generation != manifest.generation
                                || manifest.modality != self.modality
                                || manifest.generation > self.descriptor_generation
                                || manifest.sequence > self.descriptor_sequence
                                || value.live_rows > u64::from(manifest.rows)
                            {
                                return Err(TreeError::Invalid(
                                    "sparse trace source cutoff or identity",
                                ));
                            }
                            let mask_len = usize::try_from(manifest.rows)
                                .map_err(|_| TreeError::Memory)?
                                .checked_add(7)
                                .ok_or(TreeError::Memory)?
                                / 8;
                            if value.mask.len() != mask_len as u64 {
                                return Err(TreeError::Invalid("sparse trace mask length"));
                            }
                            Ok(DirectoryEvent::Source(SourceState {
                                source: reference,
                                source_value_generation: entry.creation_generation(),
                                value,
                                manifest,
                                members: self.members,
                                store: self.root.store(),
                                phase: 0,
                                row: 0,
                                live_rows: 0,
                                live_length: 0,
                            }))
                        }
                    }
                })
            }
            DirectoryTraceEvent::Done => Ok(DirectoryEvent::Done),
        }
    }
}

#[derive(Clone, Copy)]
struct PayloadState {
    payload: PayloadRef,
    store: crate::property_graph::StoreInstanceId,
    generation: GraphGeneration,
    next: usize,
}

#[derive(Clone, Copy)]
struct SourceState {
    source: PhysicalRef,
    source_value_generation: GraphGeneration,
    value: SourceValue,
    manifest: SourceManifest,
    members: DirectoryRoot,
    store: crate::property_graph::StoreInstanceId,
    phase: u8,
    row: u32,
    live_rows: u64,
    live_length: u64,
}

/// Value-owned sparse trace state. It retains no mapped source or catalog, so a
/// coordinator may release and recreate bounded preparation windows per slice.
pub(crate) struct SearchTraceState<'m> {
    store: crate::property_graph::StoreInstanceId,
    native: crate::property_graph::storage::tree::directory::GraphRoots,
    binding: SearchTraceBinding,
    reservation: TreeTraceReservation<'m>,
    required: [Option<RequiredRef>; 2],
    states: [Option<SparseRootState>; 2],
    state_catalog: RequiredRef,
    historical_catalog: RequiredRef,
    lexical_epoch: crate::fts::tokenizer::TokenizerEpoch,
    generation: GraphGeneration,
    sequence: u64,
    root_slot: usize,
    root_phase: u8,
    directory: Option<DirectoryWalk<'m>>,
    source_state: Option<SourceState>,
    lexical: Option<SparseLexical<'m>>,
    payload: Option<PayloadState>,
    index_catalog: Option<RequiredRef>,
    record_payloads: [Option<PayloadRef>; 3],
    record_payload: usize,
    member_count: u64,
    source_rows: u64,
    source_length: u64,
    failed: bool,
    done: bool,
}

#[derive(Clone, Copy)]
enum SearchTraceBinding {
    Lease(u64),
    Captured {
        checkpoint: RequiredRef,
        sequence: u64,
    },
}

/// Existing borrowed cursor facade retained for ordinary callers and focused
/// corruption tests. Maintenance consumes its value state between windows.
pub(crate) struct SearchTraceCursor<'s, 'm, S, C> {
    state: SearchTraceState<'m>,
    #[cfg(any(test, feature = "test-seams"))]
    source: &'s S,
    #[cfg(any(test, feature = "test-seams"))]
    catalog: &'s C,
    #[cfg(any(test, feature = "test-seams"))]
    lease: &'s NativeReadLease,
    #[cfg(any(test, feature = "test-seams"))]
    document: Option<&'s crate::epoch::EmbeddingTower>,
    #[cfg(any(test, feature = "test-seams"))]
    memory: &'m crate::property_graph::storage::memory::StorageMemory<'m>,
    borrowed: core::marker::PhantomData<(&'s S, &'s C)>,
}

impl<'s, 'lease, 'm>
    SearchTraceCursor<
        's,
        'm,
        NativePreparationSource<'lease, 'm>,
        NativePreparationCatalog<'s, 'lease, 'm>,
    >
{
    pub(crate) fn for_preparation(
        source: &'s NativePreparationSource<'lease, 'm>,
        catalog: &'s NativePreparationCatalog<'s, 'lease, 'm>,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        if !catalog.owns(source) {
            return Err(TreeError::Invalid("foreign sparse trace catalog"));
        }
        let bundle = source.lease().bundle();
        let roots = SparseRoots {
            text: bundle.text(),
            vector: bundle.vector(),
        };
        let state = SearchTraceState::open_checked(
            source,
            catalog,
            roots,
            bundle.roots(),
            bundle.catalog(),
            bundle.document(),
            bundle.lexical(),
            SearchTraceBinding::Lease(source.lease().token()),
            source.memory(),
            resources,
        )?;
        Ok(Self {
            state,
            #[cfg(any(test, feature = "test-seams"))]
            source,
            #[cfg(any(test, feature = "test-seams"))]
            catalog,
            #[cfg(any(test, feature = "test-seams"))]
            lease: source.lease(),
            #[cfg(any(test, feature = "test-seams"))]
            document: bundle.document(),
            #[cfg(any(test, feature = "test-seams"))]
            memory: source.memory(),
            borrowed: core::marker::PhantomData,
        })
    }

    pub(crate) fn into_state(self) -> SearchTraceState<'m> {
        self.state
    }
}

impl<'m> SearchTraceState<'m> {
    #[allow(
        clippy::too_many_arguments,
        reason = "one fully bound sparse trace view"
    )]
    fn open_checked<'s, S: BlockSource, C: RecordCatalog<S>>(
        source: &'s S,
        catalog: &'s C,
        roots: SparseRoots,
        native: crate::property_graph::storage::tree::directory::GraphRoots,
        catalog_required: RequiredRef,
        document: Option<&'s crate::epoch::EmbeddingTower>,
        lexical: crate::fts::tokenizer::TokenizerEpoch,
        binding: SearchTraceBinding,
        memory: &'m crate::property_graph::storage::memory::StorageMemory<'m>,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        let view = SparseView::open(
            source,
            roots,
            native,
            catalog_required,
            catalog,
            document,
            lexical,
            memory,
            resources,
        )?;
        let required = [roots.text, roots.vector];
        let states = [
            view.root_state(Modality::Text),
            view.root_state(Modality::Vector),
        ];
        let historical_catalog = view.interpretation_catalog();
        let generation = view.generation();
        let sequence = view.sequence();
        let store = native.store();
        let reservation = resources.reserve_trace(std::mem::size_of::<Self>())?;
        Ok(Self {
            store,
            native,
            binding,
            reservation,
            required,
            states,
            state_catalog: catalog_required,
            historical_catalog,
            lexical_epoch: lexical,
            generation,
            sequence,
            root_slot: 0,
            root_phase: 0,
            directory: None,
            source_state: None,
            lexical: None,
            payload: None,
            index_catalog: None,
            record_payloads: [None; 3],
            record_payload: 0,
            member_count: 0,
            source_rows: 0,
            source_length: 0,
            failed: false,
            done: false,
        })
    }

    fn modality(&self) -> Result<Modality, TreeError> {
        match self.root_slot {
            0 => Ok(Modality::Text),
            1 => Ok(Modality::Vector),
            _ => Err(TreeError::Invalid("sparse trace modality slot")),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one coordinator-authenticated captured sparse state"
    )]
    pub(crate) fn for_captured<'s, S: BlockSource, C: RecordCatalog<S>>(
        source: &'s S,
        catalog: &'s C,
        checkpoint: RequiredRef,
        state: crate::property_graph::wal::CommitState<'_>,
        document: Option<&'s crate::epoch::EmbeddingTower>,
        lexical: crate::fts::tokenizer::TokenizerEpoch,
        memory: &'m crate::property_graph::storage::memory::StorageMemory<'m>,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        let native = crate::property_graph::storage::tree::directory::GraphRoots::from_references(
            state.store,
            state.generation,
            state
                .graph
                .slots
                .map(|root| root.map(|required| required.block)),
        )?;
        Self::open_checked(
            source,
            catalog,
            SparseRoots {
                text: state.text,
                vector: state.vector,
            },
            native,
            state.catalog,
            document,
            lexical,
            SearchTraceBinding::Captured {
                checkpoint,
                sequence: state.sequence,
            },
            memory,
            resources,
        )
    }

    fn schedule_payload(&mut self, payload: PayloadRef, generation: GraphGeneration) {
        self.payload = Some(PayloadState {
            payload,
            store: self.store,
            generation,
            next: 0,
        });
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one authenticated sparse trace slice"
    )]
    fn next_reference<S: BlockSource, C: RecordCatalog<S>, F, V>(
        &mut self,
        source: &S,
        _catalog: &C,
        document: Option<&crate::epoch::EmbeddingTower>,
        memory: &'m crate::property_graph::storage::memory::StorageMemory<'m>,
        verify_row: &mut F,
        validate_vectors: &mut V,
        resources: &mut TreeResources<'m>,
    ) -> Result<Option<PhysicalRef>, TreeError>
    where
        F: FnMut(
            StoreInstanceId,
            GraphGeneration,
            PayloadRef,
            NodeId,
            &mut TreeResources<'m>,
        ) -> Result<SparseTraceRecordFacts, TreeError>,
        V: FnMut(
            StoreInstanceId,
            GraphGeneration,
            PayloadRef,
            u32,
            &NativeVectorIndex<'m>,
            &mut TreeResources<'m>,
        ) -> Result<(), TreeError>,
    {
        loop {
            if let Some(mut payload) = self.payload {
                let reference = if source.scoped_blocks() {
                    payload.payload.physical_reference_at_scoped(
                        source,
                        payload.store,
                        payload.generation,
                        payload.next,
                        resources,
                    )?
                } else {
                    payload.payload.physical_reference_at(
                        source,
                        payload.store,
                        payload.generation,
                        payload.next,
                        resources,
                    )?
                };
                match reference {
                    Some(reference) => {
                        payload.next = payload.next.checked_add(1).ok_or(TreeError::Work)?;
                        self.payload = Some(payload);
                        return Ok(Some(reference));
                    }
                    None => self.payload = None,
                }
                continue;
            }
            if let Some(required) = self.index_catalog.take() {
                return Ok(Some(required.block));
            }
            if self.record_payload < self.record_payloads.len() {
                let index = self.record_payload;
                self.record_payload += 1;
                if let Some(payload) = self
                    .record_payloads
                    .get_mut(index)
                    .ok_or(TreeError::Invalid("sparse trace record payload extent"))?
                    .take()
                {
                    let generation = self
                        .source_state
                        .ok_or(TreeError::Invalid("sparse trace record source"))?
                        .manifest
                        .generation;
                    self.schedule_payload(payload, generation);
                    continue;
                }
            }
            if let Some(mut state) = self.source_state {
                match state.phase {
                    0 => {
                        state.phase = 1;
                        self.source_state = Some(state);
                        self.schedule_payload(state.value.mask, state.source_value_generation);
                    }
                    1 => {
                        state.phase = 2;
                        self.source_state = Some(state);
                        return Ok(Some(state.source));
                    }
                    2 => {
                        state.phase = 3;
                        self.source_state = Some(state);
                        self.schedule_payload(state.manifest.row_table, state.manifest.generation);
                    }
                    3 => {
                        state.phase = 4;
                        self.source_state = Some(state);
                        if let Some(lexical) = state.manifest.lexical {
                            let decoded = decode_lexical_prepare(
                                source,
                                state.store,
                                state.manifest.generation,
                                lexical,
                                self.lexical_epoch,
                                memory,
                                resources,
                            )?;
                            if decoded.row_count() != state.manifest.rows {
                                return Err(TreeError::Invalid("sparse trace lexical row count"));
                            }
                            self.lexical = Some(decoded);
                            self.schedule_payload(lexical, state.manifest.generation);
                        } else if state.manifest.modality == Modality::Text {
                            return Err(TreeError::Invalid("missing sparse trace lexical region"));
                        }
                    }
                    4 => {
                        state.phase = 5;
                        self.source_state = Some(state);
                        if state.manifest.modality == Modality::Vector
                            && state.manifest.format == SourceFormat::V2
                        {
                            let payload = state
                                .manifest
                                .vector_index
                                .ok_or(TreeError::Invalid("missing sparse trace vector index"))?;
                            let index = open_vector_index(
                                source,
                                state.store,
                                state.manifest.generation,
                                payload,
                                self.lexical_epoch,
                                document,
                                SparseOwner::preparation(memory),
                                resources,
                            )?;
                            validate_vectors(
                                state.store,
                                state.manifest.generation,
                                state.manifest.row_table,
                                state.manifest.rows,
                                &index,
                                resources,
                            )?;
                            self.index_catalog = Some(index.interpretation_catalog());
                            self.schedule_payload(payload, state.manifest.generation);
                        }
                    }
                    5 if state.row < state.manifest.rows => {
                        let row_index = state.row;
                        let mut row_bytes = [0_u8; ROW_BYTES];
                        let rows = PayloadSlice::new(
                            source,
                            state.store,
                            state.manifest.generation,
                            state.manifest.row_table,
                        );
                        if rows.read_at(
                            u64::from(row_index) * ROW_BYTES as u64,
                            &mut row_bytes,
                            resources,
                        )? != ROW_BYTES
                        {
                            return Err(TreeError::Invalid("short sparse trace row"));
                        }
                        let row = SparseRow::decode(&row_bytes)?;
                        let record = verify_row(
                            state.store,
                            state.manifest.generation,
                            row.record,
                            row.node,
                            resources,
                        )?;
                        if record.revision.get() != row.revision {
                            return Err(TreeError::Invalid("sparse trace row revision"));
                        }
                        match state.manifest.modality {
                            Modality::Text
                                if row.analyzed_length == 0
                                    || !record.has_text
                                    || self.lexical.as_ref().and_then(|decoded| {
                                        decoded.row_lengths().get(row_index as usize).copied()
                                    }) != Some(row.analyzed_length) =>
                            {
                                return Err(TreeError::Invalid("sparse trace text row payload"));
                            }
                            Modality::Vector if row.analyzed_length != 0 || !record.has_vector => {
                                return Err(TreeError::Invalid("sparse trace vector row payload"));
                            }
                            _ => {}
                        }
                        let mut mask = [0_u8; 1];
                        if PayloadSlice::new(
                            source,
                            state.store,
                            state.source_value_generation,
                            state.value.mask,
                        )
                        .read_at(
                            u64::from(row_index / 8),
                            &mut mask,
                            resources,
                        )? != 1
                        {
                            return Err(TreeError::Invalid("short sparse trace mask"));
                        }
                        let byte = mask
                            .first()
                            .copied()
                            .ok_or(TreeError::Invalid("sparse trace mask byte"))?;
                        if row_index + 1 == state.manifest.rows && state.manifest.rows % 8 != 0 {
                            let valid = (1_u16 << (state.manifest.rows % 8)) as u8 - 1;
                            if byte & !valid != 0 {
                                return Err(TreeError::Invalid("sparse trace mask tail bits"));
                            }
                        }
                        if byte & (1_u8 << (row_index % 8)) != 0 {
                            let mut membership = [0_u8; MEMBERSHIP_BYTES];
                            if lookup_fixed_scoped(
                                source,
                                state.members,
                                &row.node.get().to_le_bytes(),
                                &mut membership,
                                resources,
                            )? != Some(MEMBERSHIP_BYTES)
                            {
                                return Err(TreeError::Invalid(
                                    "live sparse trace row lacks membership",
                                ));
                            }
                            let member = MembershipRow::decode(&membership)?;
                            validate_row_correlation(
                                row.node,
                                member,
                                state.source,
                                row_index,
                                row,
                            )?;
                            let native = self.native.directory(TreeKind::Nodes)?;
                            let mut native_value = [0_u8; 48];
                            if lookup_fixed_scoped(
                                source,
                                native,
                                &row.node.get().to_le_bytes(),
                                &mut native_value,
                                resources,
                            )? != Some(native_value.len())
                            {
                                return Err(TreeError::Invalid(
                                    "sparse trace native node is absent",
                                ));
                            }
                            if PayloadRef::decode(&native_value)? != row.record {
                                return Err(TreeError::Invalid(
                                    "sparse trace native record mismatch",
                                ));
                            }
                            state.live_rows =
                                state.live_rows.checked_add(1).ok_or(TreeError::Work)?;
                            state.live_length = state
                                .live_length
                                .checked_add(u64::from(row.analyzed_length))
                                .ok_or(TreeError::Work)?;
                        }
                        state.row += 1;
                        let [canonical, provenance] = record.required_payloads;
                        self.record_payloads =
                            [Some(row.record), Some(canonical), Some(provenance)];
                        self.record_payload = 0;
                        self.source_state = Some(state);
                    }
                    5 => {
                        if state.live_rows != state.value.live_rows
                            || (state.manifest.modality == Modality::Text
                                && state.live_length != state.value.live_length)
                            || (state.manifest.modality == Modality::Vector
                                && (state.live_length != 0 || state.value.live_length != 0))
                        {
                            return Err(TreeError::Invalid("sparse trace source aggregate"));
                        }
                        self.source_rows = self
                            .source_rows
                            .checked_add(state.live_rows)
                            .ok_or(TreeError::Work)?;
                        self.source_length = self
                            .source_length
                            .checked_add(state.live_length)
                            .ok_or(TreeError::Work)?;
                        self.source_state = None;
                        self.lexical = None;
                    }
                    _ => return Err(TreeError::Invalid("sparse trace source phase")),
                }
                continue;
            }
            if let Some(directory) = self.directory.as_mut() {
                match directory.next(source, resources)? {
                    DirectoryEvent::Reference(reference) => return Ok(Some(reference)),
                    DirectoryEvent::Member => {
                        self.member_count =
                            self.member_count.checked_add(1).ok_or(TreeError::Work)?;
                    }
                    DirectoryEvent::Source(state) => self.source_state = Some(state),
                    DirectoryEvent::Done => {
                        self.directory = None;
                    }
                }
                continue;
            }
            if self.root_slot >= self.required.len() {
                self.done = true;
                return Ok(None);
            }
            let Some(required) = self.required.get(self.root_slot).copied().flatten() else {
                self.root_slot += 1;
                self.root_phase = 0;
                continue;
            };
            let state = self
                .states
                .get(self.root_slot)
                .copied()
                .flatten()
                .ok_or(TreeError::Invalid("sparse trace missing root state"))?;
            match self.root_phase {
                0 => {
                    self.root_phase = 1;
                    return Ok(Some(required.block));
                }
                1 => {
                    self.root_phase = 2;
                    return Ok(Some(self.historical_catalog.block));
                }
                2 => {
                    self.root_phase = 3;
                    self.directory = Some(DirectoryWalk::new(
                        state.members,
                        state.members,
                        state.sources,
                        DirectoryFamily::Members,
                        self.modality()?,
                        self.generation,
                        self.sequence,
                        resources,
                    )?);
                }
                3 => {
                    self.root_phase = 4;
                    self.directory = Some(DirectoryWalk::new(
                        state.sources,
                        state.members,
                        state.sources,
                        DirectoryFamily::Sources,
                        self.modality()?,
                        self.generation,
                        self.sequence,
                        resources,
                    )?);
                }
                4 => {
                    if self.member_count != state.live_rows
                        || self.source_rows != state.live_rows
                        || self.source_length != state.live_length
                    {
                        return Err(TreeError::Invalid("sparse trace root aggregate"));
                    }
                    self.root_slot += 1;
                    self.root_phase = 0;
                    self.member_count = 0;
                    self.source_rows = 0;
                    self.source_length = 0;
                }
                _ => return Err(TreeError::Invalid("sparse trace root phase")),
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one authenticated sparse trace slice"
    )]
    fn trace_with<S: BlockSource, C: RecordCatalog<S>, F, V>(
        &mut self,
        source: &S,
        catalog: &C,
        lease: Option<&NativeReadLease>,
        document: Option<&crate::epoch::EmbeddingTower>,
        memory: &'m crate::property_graph::storage::memory::StorageMemory<'m>,
        output: &mut [Option<PhysicalRef>],
        verify_row: &mut F,
        validate_vectors: &mut V,
        resources: &mut TreeResources<'m>,
    ) -> Result<SearchTraceResult, TreeError>
    where
        F: FnMut(
            StoreInstanceId,
            GraphGeneration,
            PayloadRef,
            NodeId,
            &mut TreeResources<'m>,
        ) -> Result<SparseTraceRecordFacts, TreeError>,
        V: FnMut(
            StoreInstanceId,
            GraphGeneration,
            PayloadRef,
            u32,
            &NativeVectorIndex<'m>,
            &mut TreeResources<'m>,
        ) -> Result<(), TreeError>,
    {
        if self.failed {
            return Err(TreeError::Invalid("sparse trace cursor previously failed"));
        }
        let result = (|| {
            if let Some(lease) = lease {
                lease
                    .check_active()
                    .map_err(crate::property_graph::query::runtime::RuntimeError::Value)
                    .map_err(TreeError::Runtime)?;
            }
            self.reservation.require(resources)?;
            resources.step(0)?;
            if output.is_empty() || output.len() > 256 {
                return Err(TreeError::Memory);
            }
            output.fill(None);
            let mut count = 0_usize;
            while count < output.len() {
                let Some(reference) = self.next_reference(
                    source,
                    catalog,
                    document,
                    memory,
                    verify_row,
                    validate_vectors,
                    resources,
                )?
                else {
                    return Ok(SearchTraceResult {
                        count,
                        complete: true,
                    });
                };
                *output
                    .get_mut(count)
                    .ok_or(TreeError::Invalid("sparse trace output extent"))? = Some(reference);
                count += 1;
            }
            Ok(SearchTraceResult {
                count,
                complete: self.done,
            })
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn trace_preparation<'s, 'lease>(
        &mut self,
        source: &'s NativePreparationSource<'lease, 'm>,
        catalog: &NativePreparationCatalog<'s, 'lease, 'm>,
        output: &mut [Option<PhysicalRef>],
        resources: &mut TreeResources<'m>,
    ) -> Result<SearchTraceResult, TreeError> {
        let bundle = source.lease().bundle();
        if !catalog.owns(source)
            || !matches!(self.binding, SearchTraceBinding::Lease(token) if token == source.lease().token())
            || bundle.base().store != self.store
            || bundle.roots() != self.native
            || bundle.sequence() != self.sequence
        {
            return Err(TreeError::Invalid("foreign sparse trace window"));
        }
        let mut verify_row =
            |store, generation, record, node, resources: &mut TreeResources<'m>| {
                verify_sparse_trace_record(
                    PayloadSlice::new(source, store, generation, record),
                    node,
                    catalog,
                    bundle.document(),
                    resources,
                )
            };
        let mut validate_vectors =
            |store,
             generation,
             row_table,
             rows,
             index: &NativeVectorIndex<'m>,
             resources: &mut TreeResources<'m>| {
                validate_vector_index_rows(
                    source,
                    store,
                    generation,
                    row_table,
                    rows,
                    catalog,
                    bundle.document(),
                    index,
                    resources,
                )
            };
        self.trace_with(
            source,
            catalog,
            Some(source.lease()),
            bundle.document(),
            source.memory(),
            output,
            &mut verify_row,
            &mut validate_vectors,
            resources,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one coordinator-authenticated captured sparse trace window"
    )]
    pub(crate) fn trace_captured<S: BlockSource, C: RecordCatalog<S>, F, V>(
        &mut self,
        source: &S,
        catalog: &C,
        checkpoint: RequiredRef,
        state: crate::property_graph::wal::CommitState<'_>,
        document: Option<&crate::epoch::EmbeddingTower>,
        lexical: crate::fts::tokenizer::TokenizerEpoch,
        memory: &'m crate::property_graph::storage::memory::StorageMemory<'m>,
        output: &mut [Option<PhysicalRef>],
        verify_row: &mut F,
        validate_vectors: &mut V,
        resources: &mut TreeResources<'m>,
    ) -> Result<SearchTraceResult, TreeError>
    where
        F: FnMut(
            StoreInstanceId,
            GraphGeneration,
            PayloadRef,
            NodeId,
            &mut TreeResources<'m>,
        ) -> Result<SparseTraceRecordFacts, TreeError>,
        V: FnMut(
            StoreInstanceId,
            GraphGeneration,
            PayloadRef,
            u32,
            &NativeVectorIndex<'m>,
            &mut TreeResources<'m>,
        ) -> Result<(), TreeError>,
    {
        let native = crate::property_graph::storage::tree::directory::GraphRoots::from_references(
            state.store,
            state.generation,
            state
                .graph
                .slots
                .map(|root| root.map(|required| required.block)),
        )?;
        if !matches!(
            self.binding,
            SearchTraceBinding::Captured {
                checkpoint: bound_checkpoint,
                sequence: bound_sequence,
            } if bound_checkpoint == checkpoint && bound_sequence == state.sequence
        ) || state.store != self.store
            || state.generation != self.generation
            || native != self.native
            || state.catalog != self.state_catalog
            || [state.text, state.vector] != self.required
            || lexical != self.lexical_epoch
        {
            return Err(TreeError::Invalid("foreign captured sparse trace window"));
        }
        self.trace_with(
            source,
            catalog,
            None,
            document,
            memory,
            output,
            verify_row,
            validate_vectors,
            resources,
        )
    }
}

impl<'s, 'm, S: BlockSource, C: RecordCatalog<S>> SearchTraceCursor<'s, 'm, S, C> {
    #[cfg(test)]
    #[allow(
        clippy::too_many_arguments,
        reason = "corruption fixture supplies one coherent view"
    )]
    pub(crate) fn for_test(
        source: &'s S,
        catalog: &'s C,
        roots: SparseRoots,
        native: crate::property_graph::storage::tree::directory::GraphRoots,
        catalog_required: RequiredRef,
        document: Option<&'s crate::epoch::EmbeddingTower>,
        lexical: crate::fts::tokenizer::TokenizerEpoch,
        lease: &'s NativeReadLease,
        memory: &'m crate::property_graph::storage::memory::StorageMemory<'m>,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        let state = SearchTraceState::open_checked(
            source,
            catalog,
            roots,
            native,
            catalog_required,
            document,
            lexical,
            SearchTraceBinding::Lease(lease.token()),
            memory,
            resources,
        )?;
        Ok(Self {
            state,
            source,
            catalog,
            lease,
            document,
            memory,
            borrowed: core::marker::PhantomData,
        })
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn trace(
        &mut self,
        output: &mut [Option<PhysicalRef>],
        resources: &mut TreeResources<'m>,
    ) -> Result<SearchTraceResult, TreeError> {
        let mut verify_row =
            |store, generation, record, node, resources: &mut TreeResources<'m>| {
                verify_sparse_trace_record(
                    PayloadSlice::new(self.source, store, generation, record),
                    node,
                    self.catalog,
                    self.document,
                    resources,
                )
            };
        let mut validate_vectors =
            |store,
             generation,
             row_table,
             rows,
             index: &NativeVectorIndex<'m>,
             resources: &mut TreeResources<'m>| {
                validate_vector_index_rows(
                    self.source,
                    store,
                    generation,
                    row_table,
                    rows,
                    self.catalog,
                    self.document,
                    index,
                    resources,
                )
            };
        self.state.trace_with(
            self.source,
            self.catalog,
            Some(self.lease),
            self.document,
            self.memory,
            output,
            &mut verify_row,
            &mut validate_vectors,
            resources,
        )
    }
}
