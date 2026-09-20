//! Complete bounded tracing for one admitted sparse retrieval participant.

use super::codec::{
    MEMBERSHIP_BYTES, MembershipRow, Modality, ROW_BYTES, SOURCE_VALUE_BYTES, SourceManifest,
    SourceValue, SparseRootState, SparseRoots, SparseRow, validate_row_correlation,
};
use super::view::{SparseLexical, SparseView, decode_lexical_prepare};
use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::storage::artifact::{self, BlockKind, PhysicalRef};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::records::{RecordCatalog, verify_record};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{
    BlockSource, DirectoryRoot, MAX_DEPTH, TreeError, TreeResources, TreeTraceReservation,
    lookup_entry, trace_page,
};
use crate::property_graph::storage::tree::{Cell, Key, TreeKind};
use crate::property_graph::storage::{NativePreparationCatalog, NativePreparationSource};
use crate::property_graph::wal::RequiredRef;
use crate::property_graph::{EntityId, GraphGeneration};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SearchTraceResult {
    pub(crate) count: usize,
    pub(crate) complete: bool,
}

#[derive(Clone, Copy)]
struct Bound {
    bytes: [u8; 32],
    len: u8,
}

impl Bound {
    fn from_key(key: Key<'_>, width: usize) -> Result<Self, TreeError> {
        let Key::Inline(bytes) = key else {
            return Err(TreeError::Invalid("sparse trace overflow key"));
        };
        if bytes.len() != width {
            return Err(TreeError::Invalid("sparse trace key width"));
        }
        let mut output = [0_u8; 32];
        output
            .get_mut(..width)
            .ok_or(TreeError::Invalid("sparse trace key extent"))?
            .copy_from_slice(bytes);
        Ok(Self {
            bytes: output,
            len: u8::try_from(width).map_err(|_| TreeError::Memory)?,
        })
    }

    fn key(&self) -> Result<Key<'_>, TreeError> {
        Ok(Key::Inline(
            self.bytes
                .get(..usize::from(self.len))
                .ok_or(TreeError::Invalid("sparse trace bound extent"))?,
        ))
    }
}

#[derive(Clone, Copy)]
struct PageFrame {
    reference: PhysicalRef,
    expected_level: Option<u16>,
    generation_bound: GraphGeneration,
    next_cell: usize,
    emitted: bool,
    lower: Option<Bound>,
    upper: Option<Bound>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DirectoryFamily {
    Members,
    Sources,
}

struct DirectoryWalk {
    root: DirectoryRoot,
    members: DirectoryRoot,
    sources: DirectoryRoot,
    family: DirectoryFamily,
    modality: Modality,
    descriptor_generation: GraphGeneration,
    descriptor_sequence: u64,
    frames: [Option<PageFrame>; MAX_DEPTH],
    depth: usize,
}

enum DirectoryEvent {
    Reference(PhysicalRef),
    Member,
    Source(SourceState),
    Done,
}

impl DirectoryWalk {
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
    ) -> Result<Self, TreeError> {
        let mut frames = [None; MAX_DEPTH];
        let mut depth = 0;
        if let Some(reference) = root.reference() {
            frames
                .get_mut(0)
                .ok_or(TreeError::Invalid("sparse trace directory depth"))?
                .replace(PageFrame {
                    reference,
                    expected_level: None,
                    generation_bound: root.generation(),
                    next_cell: 0,
                    emitted: false,
                    lower: None,
                    upper: None,
                });
            depth = 1;
        }
        Ok(Self {
            root,
            members,
            sources,
            family,
            modality,
            descriptor_generation: generation,
            descriptor_sequence: sequence,
            frames,
            depth,
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
        loop {
            if self.depth == 0 {
                return Ok(DirectoryEvent::Done);
            }
            let frame_index = self.depth - 1;
            let frame = self
                .frames
                .get(frame_index)
                .copied()
                .flatten()
                .ok_or(TreeError::Invalid("sparse trace page frame"))?;
            let lower = frame.lower.as_ref().map(Bound::key).transpose()?;
            let upper = frame.upper.as_ref().map(Bound::key).transpose()?;
            let (page, count) =
                trace_page(source, self.root, frame.reference, lower, upper, resources)?;
            if frame
                .expected_level
                .is_some_and(|level| level != page.header().level)
                || page.header().generation > frame.generation_bound
            {
                return Err(TreeError::Invalid("sparse trace child level or generation"));
            }
            if !frame.emitted {
                self.frames
                    .get_mut(frame_index)
                    .and_then(Option::as_mut)
                    .ok_or(TreeError::Invalid("sparse trace emitted frame"))?
                    .emitted = true;
                return Ok(DirectoryEvent::Reference(frame.reference));
            }
            if frame.next_cell == count {
                *self
                    .frames
                    .get_mut(frame_index)
                    .ok_or(TreeError::Invalid("sparse trace pop extent"))? = None;
                self.depth -= 1;
                continue;
            }
            if frame.next_cell > count {
                return Err(TreeError::Invalid("sparse trace page cell extent"));
            }
            let cell = page.cell(frame.next_cell)?;
            self.frames
                .get_mut(frame_index)
                .and_then(Option::as_mut)
                .ok_or(TreeError::Invalid("sparse trace live frame"))?
                .next_cell += 1;
            if page.header().level == 0 {
                let Cell::Leaf { key, value } = cell else {
                    return Err(TreeError::Invalid("sparse trace leaf cell"));
                };
                let key = Bound::from_key(key, self.width())?;
                return match self.family {
                    DirectoryFamily::Members => {
                        if value.len() != MEMBERSHIP_BYTES {
                            return Err(TreeError::Invalid("sparse trace membership width"));
                        }
                        let member = MembershipRow::decode(value)?;
                        let mut source_key = [0_u8; 32];
                        artifact::encode_reference(member.source, &mut source_key)?;
                        let source_entry =
                            lookup_entry(source, self.sources, &source_key, resources)?
                                .ok_or(TreeError::Invalid("sparse member source is absent"))?;
                        if source_entry.value().len() != SOURCE_VALUE_BYTES {
                            return Err(TreeError::Invalid("sparse source value width"));
                        }
                        SourceValue::decode(source_entry.value())?;
                        let _ = key;
                        Ok(DirectoryEvent::Member)
                    }
                    DirectoryFamily::Sources => {
                        if value.len() != SOURCE_VALUE_BYTES {
                            return Err(TreeError::Invalid("sparse source value width"));
                        }
                        let reference = artifact::decode_reference(
                            key.bytes
                                .get(..32)
                                .ok_or(TreeError::Invalid("sparse source key extent"))?,
                        )?;
                        let value = SourceValue::decode(value)?;
                        let block = source.resolve(reference, resources)?;
                        let manifest = SourceManifest::decode(block.payload())?;
                        if block.reference() != reference
                            || reference.kind != BlockKind::CommitParticipant
                            || block.identity().store != self.root.store()
                            || block.identity().generation != manifest.generation
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
                            source_value_generation: page.header().generation,
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
                };
            }
            let Cell::Branch { upper, child } = cell else {
                return Err(TreeError::Invalid("sparse trace branch cell"));
            };
            let child_lower = if frame.next_cell == 0 {
                frame.lower
            } else {
                let Cell::Branch {
                    upper: previous, ..
                } = page.cell(frame.next_cell - 1)?
                else {
                    return Err(TreeError::Invalid("sparse trace prior branch"));
                };
                previous
                    .map(|key| Bound::from_key(key, self.width()))
                    .transpose()?
            };
            let child_upper = upper
                .map(|key| Bound::from_key(key, self.width()))
                .transpose()?
                .or(frame.upper);
            let expected_level = page
                .header()
                .level
                .checked_sub(1)
                .ok_or(TreeError::Invalid("sparse trace branch level"))?;
            *self
                .frames
                .get_mut(self.depth)
                .ok_or(TreeError::Invalid("sparse trace directory depth"))? = Some(PageFrame {
                reference: child,
                expected_level: Some(expected_level),
                generation_bound: page.header().generation,
                next_cell: 0,
                emitted: false,
                lower: child_lower,
                upper: child_upper,
            });
            self.depth += 1;
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

/// One fixed-state cursor bound to an actual admitted preparation source.
pub(crate) struct SearchTraceCursor<'s, 'm, S, C> {
    source: &'s S,
    catalog: &'s C,
    lease: &'s NativeReadLease,
    document: Option<&'s crate::epoch::EmbeddingTower>,
    store: crate::property_graph::StoreInstanceId,
    native: crate::property_graph::storage::tree::directory::GraphRoots,
    memory: &'m crate::property_graph::storage::memory::StorageMemory<'m>,
    reservation: TreeTraceReservation<'m>,
    required: [Option<RequiredRef>; 2],
    states: [Option<SparseRootState>; 2],
    historical_catalog: RequiredRef,
    generation: GraphGeneration,
    sequence: u64,
    root_slot: usize,
    root_phase: u8,
    directory: Option<DirectoryWalk>,
    source_state: Option<SourceState>,
    lexical: Option<SparseLexical<'m>>,
    payload: Option<PayloadState>,
    record_payloads: [Option<PayloadRef>; 3],
    record_payload: usize,
    member_count: u64,
    source_rows: u64,
    source_length: u64,
    failed: bool,
    done: bool,
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
        Self::open_checked(
            source,
            catalog,
            roots,
            bundle.roots(),
            bundle.catalog(),
            bundle.document(),
            bundle.lexical(),
            source.lease(),
            source.memory(),
            resources,
        )
    }
}

impl<'s, 'm, S: BlockSource, C: RecordCatalog<S>> SearchTraceCursor<'s, 'm, S, C> {
    #[allow(
        clippy::too_many_arguments,
        reason = "one fully bound sparse trace view"
    )]
    fn open_checked(
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
        drop(view);
        let reservation = resources.reserve_trace(std::mem::size_of::<Self>())?;
        Ok(Self {
            source,
            catalog,
            lease,
            document,
            store,
            native,
            memory,
            reservation,
            required,
            states,
            historical_catalog,
            generation,
            sequence,
            root_slot: 0,
            root_phase: 0,
            directory: None,
            source_state: None,
            lexical: None,
            payload: None,
            record_payloads: [None; 3],
            record_payload: 0,
            member_count: 0,
            source_rows: 0,
            source_length: 0,
            failed: false,
            done: false,
        })
    }

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
        Self::open_checked(
            source,
            catalog,
            roots,
            native,
            catalog_required,
            document,
            lexical,
            lease,
            memory,
            resources,
        )
    }

    fn modality(&self) -> Result<Modality, TreeError> {
        match self.root_slot {
            0 => Ok(Modality::Text),
            1 => Ok(Modality::Vector),
            _ => Err(TreeError::Invalid("sparse trace modality slot")),
        }
    }

    fn schedule_payload(&mut self, payload: PayloadRef, generation: GraphGeneration) {
        self.payload = Some(PayloadState {
            payload,
            store: self.store,
            generation,
            next: 0,
        });
    }

    fn next_reference(
        &mut self,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<PhysicalRef>, TreeError> {
        loop {
            if let Some(mut payload) = self.payload {
                match payload.payload.physical_reference_at(
                    self.source,
                    payload.store,
                    payload.generation,
                    payload.next,
                    resources,
                )? {
                    Some(reference) => {
                        payload.next = payload.next.checked_add(1).ok_or(TreeError::Work)?;
                        self.payload = Some(payload);
                        return Ok(Some(reference));
                    }
                    None => self.payload = None,
                }
                continue;
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
                                self.source,
                                state.store,
                                state.manifest.generation,
                                lexical,
                                self.lease.bundle().lexical(),
                                self.memory,
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
                    4 if state.row < state.manifest.rows => {
                        let row_index = state.row;
                        let mut row_bytes = [0_u8; ROW_BYTES];
                        let rows = PayloadSlice::new(
                            self.source,
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
                        let record = verify_record(
                            PayloadSlice::new(
                                self.source,
                                state.store,
                                state.manifest.generation,
                                row.record,
                            ),
                            EntityId::Node(row.node),
                            self.catalog,
                            self.document,
                            resources,
                        )?;
                        if record.revision().get() != row.revision {
                            return Err(TreeError::Invalid("sparse trace row revision"));
                        }
                        match state.manifest.modality {
                            Modality::Text
                                if row.analyzed_length == 0
                                    || record.canonical().stored_text().is_none()
                                    || self.lexical.as_ref().and_then(|decoded| {
                                        decoded.row_lengths().get(row_index as usize).copied()
                                    }) != Some(row.analyzed_length) =>
                            {
                                return Err(TreeError::Invalid("sparse trace text row payload"));
                            }
                            Modality::Vector
                                if row.analyzed_length != 0
                                    || record.canonical().stored_vector().is_none() =>
                            {
                                return Err(TreeError::Invalid("sparse trace vector row payload"));
                            }
                            _ => {}
                        }
                        let mut mask = [0_u8; 1];
                        if PayloadSlice::new(
                            self.source,
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
                            let entry = lookup_entry(
                                self.source,
                                state.members,
                                &row.node.get().to_le_bytes(),
                                resources,
                            )?
                            .ok_or(TreeError::Invalid("live sparse trace row lacks membership"))?;
                            if entry.value().len() != MEMBERSHIP_BYTES {
                                return Err(TreeError::Invalid("sparse trace membership width"));
                            }
                            let member = MembershipRow::decode(entry.value())?;
                            validate_row_correlation(
                                row.node,
                                member,
                                state.source,
                                row_index,
                                row,
                            )?;
                            let native = self.native.directory(TreeKind::Nodes)?;
                            let native_entry = lookup_entry(
                                self.source,
                                native,
                                &row.node.get().to_le_bytes(),
                                resources,
                            )?
                            .ok_or(TreeError::Invalid("sparse trace native node is absent"))?;
                            if PayloadRef::decode(native_entry.value())? != row.record {
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
                        let [canonical, provenance] = record.required_payloads();
                        self.record_payloads =
                            [Some(row.record), Some(canonical), Some(provenance)];
                        self.record_payload = 0;
                        self.source_state = Some(state);
                    }
                    4 => {
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
                match directory.next(self.source, resources)? {
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

    pub(crate) fn trace(
        &mut self,
        output: &mut [Option<PhysicalRef>],
        resources: &mut TreeResources<'_>,
    ) -> Result<SearchTraceResult, TreeError> {
        if self.failed {
            return Err(TreeError::Invalid("sparse trace cursor previously failed"));
        }
        let result = (|| {
            self.lease
                .check_active()
                .map_err(crate::property_graph::query::runtime::RuntimeError::Value)
                .map_err(TreeError::Runtime)?;
            self.reservation.require(resources)?;
            resources.step(0)?;
            if output.is_empty() || output.len() > 256 {
                return Err(TreeError::Memory);
            }
            output.fill(None);
            let mut count = 0_usize;
            while count < output.len() {
                let Some(reference) = self.next_reference(resources)? else {
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
}
