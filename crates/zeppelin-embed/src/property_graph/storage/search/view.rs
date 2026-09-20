//! Checked sparse retrieval views over one immutable source and native graph.

use super::codec::{
    MEMBERSHIP_BYTES, MembershipRow, Modality, ROW_BYTES, RootDescriptor, SOURCE_VALUE_BYTES,
    SourceManifest, SourceValue, SparseRootState, SparseRoots, SparseRow,
    validate_catalog_interpretation, validate_row_correlation,
};
use crate::epoch::EmbeddingTower;
use crate::fts::graph_build::{DecodedGraphLexical, GraphLexicalError};
use crate::fts::sealed::SealedSegment;
use crate::fts::tokenizer::TokenizerEpoch;
use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::query::resources::{
    MemoryError, QueryArena, QueryMemory, QueryReservation,
};
use crate::property_graph::query::runtime::RuntimeError;
use crate::property_graph::storage::artifact::{self, BlockKind};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory, StorageReservation};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::records::{
    NodeRecordState, RecordCatalog, StoredVector, verify_node_state,
};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{
    BlockSource, DirectoryCursor, GraphRoots, QueryOwner, TreeError, TreeResources, lookup_entry,
};
use crate::property_graph::storage::tree::{Key, TreeKind};
use crate::property_graph::wal::RequiredRef;
use crate::property_graph::{NodeId, StoreInstanceId};

fn lexical_error(error: GraphLexicalError) -> TreeError {
    match error {
        GraphLexicalError::Resource(error) => error,
        GraphLexicalError::Tokenizer(_) => TreeError::Invalid("invalid sparse lexical tokenizer"),
        GraphLexicalError::Postings(_) => TreeError::Invalid("invalid sparse lexical postings"),
        GraphLexicalError::Region(_) => TreeError::Invalid("invalid sparse lexical region"),
        GraphLexicalError::Failed => TreeError::Invalid("failed sparse lexical decoder"),
    }
}

#[derive(Clone, Copy)]
pub(super) enum SparseOwner<'m> {
    Preparation(&'m StorageMemory<'m>),
    Query {
        memory: &'m QueryMemory<'m>,
        owner: QueryOwner<'m, 'm>,
        lease: &'m NativeReadLease,
    },
}

impl<'m> SparseOwner<'m> {
    pub(super) const fn preparation(memory: &'m StorageMemory<'m>) -> Self {
        Self::Preparation(memory)
    }

    pub(super) fn query(
        memory: &'m QueryMemory<'m>,
        lease: &'m NativeReadLease,
        resources: &TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        Ok(Self::Query {
            memory,
            owner: resources.query_owner(memory)?,
            lease,
        })
    }

    pub(super) fn check(self, resources: &mut TreeResources<'_>) -> Result<(), TreeError> {
        match self {
            Self::Preparation(memory) => resources.require_preparation(memory),
            Self::Query {
                memory,
                owner,
                lease,
            } => {
                lease
                    .check_active()
                    .map_err(RuntimeError::Value)
                    .map_err(TreeError::Runtime)?;
                resources.step(0)?;
                resources.require_query_owner(owner)?;
                resources.require_query(memory)
            }
        }
    }

    pub(super) fn reserve(self, bytes: usize) -> Result<SparseCharge<'m>, TreeError> {
        match self {
            Self::Preparation(memory) => Ok(SparseCharge::Preparation(memory.reserve(bytes)?)),
            Self::Query { memory, .. } => Ok(SparseCharge::Query(
                memory
                    .reserve(bytes)
                    .map_err(RuntimeError::Memory)
                    .map_err(TreeError::Runtime)?,
            )),
        }
    }

    pub(super) fn catalog_allocation_error(self, error: CatalogAllocation) -> TreeError {
        match self {
            Self::Preparation(_) => TreeError::Memory,
            Self::Query { .. } => TreeError::Runtime(RuntimeError::Memory(match error {
                CatalogAllocation::Capacity => MemoryError::Limit,
                CatalogAllocation::Allocation => MemoryError::Allocation,
            })),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum CatalogAllocation {
    Capacity,
    Allocation,
}

pub(super) enum SparseCharge<'m> {
    Preparation(StorageReservation<'m>),
    Query(QueryReservation<'m, 'm>),
}

impl SparseCharge<'_> {
    pub(super) fn resize(&mut self, bytes: usize) -> Result<(), TreeError> {
        match self {
            Self::Preparation(charge) => charge.resize(bytes),
            Self::Query(charge) => charge
                .resize(bytes)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime),
        }
    }
}

enum SparseBytes<'m> {
    Preparation(StorageBuffer<'m, u8>),
    Query(QueryArena<'m, 'm, u8>),
}

impl<'m> SparseBytes<'m> {
    fn new(
        owner: SparseOwner<'m>,
        length: usize,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        owner.check(resources)?;
        let mut bytes = match owner {
            SparseOwner::Preparation(memory) => {
                Self::Preparation(StorageBuffer::new(memory, length)?)
            }
            SparseOwner::Query { memory, .. } => Self::Query(
                QueryArena::new(memory, length)
                    .map_err(RuntimeError::Memory)
                    .map_err(TreeError::Runtime)?,
            ),
        };
        for _ in 0..length {
            resources.step(1)?;
            match &mut bytes {
                Self::Preparation(values) => values.push(0)?,
                Self::Query(values) => values
                    .push(0)
                    .map_err(RuntimeError::Memory)
                    .map_err(TreeError::Runtime)?,
            }
        }
        Ok(bytes)
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Preparation(values) => values.as_slice(),
            Self::Query(values) => values.as_slice(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Preparation(values) => values.as_mut_slice(),
            Self::Query(values) => values.as_mut_slice(),
        }
    }
}

pub(super) struct SparseLexical<'m> {
    decoded: DecodedGraphLexical<'m>,
    _encoded: SparseBytes<'m>,
}

impl SparseLexical<'_> {
    pub(super) fn row_count(&self) -> u32 {
        self.decoded.row_count()
    }

    pub(super) fn row_lengths(&self) -> &[u32] {
        self.decoded.row_lengths()
    }

    fn sealed(&self) -> &SealedSegment {
        self.decoded.sealed()
    }
}

pub(super) fn decode_lexical<'m, S: BlockSource>(
    source: &S,
    store: StoreInstanceId,
    generation: crate::property_graph::GraphGeneration,
    lexical: PayloadRef,
    epoch: TokenizerEpoch,
    owner: SparseOwner<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<SparseLexical<'m>, TreeError> {
    let bytes = PayloadSlice::new(source, store, generation, lexical);
    let length = usize::try_from(bytes.len()).map_err(|_| TreeError::Memory)?;
    let mut encoded = SparseBytes::new(owner, length, resources)?;
    if bytes.read_at(0, encoded.as_mut_slice(), resources)? != length {
        return Err(TreeError::Invalid("short sparse lexical region"));
    }
    let decoded = match owner {
        SparseOwner::Preparation(memory) => {
            DecodedGraphLexical::decode_prepare(encoded.as_slice(), epoch, memory, resources)
        }
        SparseOwner::Query { memory, .. } => {
            resources.decode_graph_lexical(encoded.as_slice(), epoch, memory)
        }
    }
    .map_err(lexical_error)?;
    Ok(SparseLexical {
        decoded,
        _encoded: encoded,
    })
}

pub(super) fn decode_lexical_prepare<'m, S: BlockSource>(
    source: &S,
    store: StoreInstanceId,
    generation: crate::property_graph::GraphGeneration,
    lexical: PayloadRef,
    epoch: TokenizerEpoch,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<SparseLexical<'m>, TreeError> {
    decode_lexical(
        source,
        store,
        generation,
        lexical,
        epoch,
        SparseOwner::preparation(memory),
        resources,
    )
}

/// One completely correlated sparse row and its optional original vector.
pub(crate) struct SparseMember<'a, S: BlockSource> {
    pub(crate) node: NodeId,
    pub(crate) revision: u64,
    pub(crate) row: u32,
    pub(crate) analyzed_length: u32,
    pub(crate) vector: Option<StoredVector<'a, S>>,
}

/// Bounded source-directory cursor retaining the exact sparse view and owner.
pub(crate) struct SparseSources<'v, 'a, 'm, 'r, S, C> {
    view: &'v SparseView<'a, 'm, S, C>,
    cursor: Option<DirectoryCursor<'a, 'r, S>>,
    modality: Modality,
    failed: bool,
    _charge: SparseCharge<'m>,
}

/// One immutable source with its checked live mask and guarded lexical owner.
pub(crate) struct SparseSource<'v, 'a, 'm, S, C> {
    view: &'v SparseView<'a, 'm, S, C>,
    descriptor: RootDescriptor,
    reference: crate::property_graph::storage::artifact::PhysicalRef,
    value: SourceValue,
    manifest: SourceManifest,
    mask_generation: crate::property_graph::GraphGeneration,
    lexical: Option<SparseLexical<'m>>,
    _charge: SparseCharge<'m>,
}

/// Checked descriptor pair bound to the same native roots and catalog.
pub(crate) struct SparseView<'a, 'm, S, C> {
    source: &'a S,
    catalog: &'a C,
    document: Option<&'a EmbeddingTower>,
    native: GraphRoots,
    text: Option<RootDescriptor>,
    vector: Option<RootDescriptor>,
    catalog_required: RequiredRef,
    sequence: u64,
    owner: SparseOwner<'m>,
}

fn open_descriptor<S: BlockSource>(
    source: &S,
    required: RequiredRef,
    modality: Modality,
    store: StoreInstanceId,
    catalog: RequiredRef,
    lexical: TokenizerEpoch,
    document: Option<&EmbeddingTower>,
    owner: SparseOwner<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<RootDescriptor, TreeError> {
    let block = source.resolve(required.block, resources)?;
    let identity = block.identity();
    if required.block.kind != BlockKind::CommitParticipant
        || required.object.family != crate::format::FormatFamily::NativeGraphObject.id()
        || required.object.version != 1
        || block.reference() != required.block
        || identity.store != required.object.store
        || identity.artifact != required.object.artifact
        || identity.generation != required.object.generation
        || identity.creation_serial != required.object.serial
        || block.file_length() != required.object.bytes as usize
        || block.file_checksum() != required.object.checksum
    {
        return Err(TreeError::Invalid("sparse required root mismatch"));
    }
    let descriptor = RootDescriptor::decode(block.payload())?;
    if descriptor.modality != modality || descriptor.store != store || descriptor.lexical != lexical
    {
        return Err(TreeError::Invalid("sparse root interpretation mismatch"));
    }
    validate_catalog_interpretation(
        source,
        descriptor.catalog,
        store,
        lexical,
        document,
        owner,
        resources,
    )?;
    let _ = catalog;
    Ok(descriptor)
}

impl<'a, 'm, S: BlockSource, C: RecordCatalog<S>> SparseView<'a, 'm, S, C> {
    #[allow(
        clippy::too_many_arguments,
        reason = "all interpretation owners are checked at admission"
    )]
    pub(crate) fn open(
        source: &'a S,
        roots: SparseRoots,
        native: GraphRoots,
        catalog_required: RequiredRef,
        catalog: &'a C,
        document: Option<&'a EmbeddingTower>,
        lexical: TokenizerEpoch,
        memory: &'m StorageMemory<'m>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        Self::open_in(
            source,
            roots,
            native,
            catalog_required,
            catalog,
            document,
            lexical,
            SparseOwner::preparation(memory),
            None,
            resources,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "all interpretation owners are checked at admission"
    )]
    pub(crate) fn open_query(
        source: &'a S,
        roots: SparseRoots,
        native: GraphRoots,
        catalog_required: RequiredRef,
        catalog: &'a C,
        document: Option<&'a EmbeddingTower>,
        lexical: TokenizerEpoch,
        memory: &'m QueryMemory<'m>,
        lease: &'m NativeReadLease,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        let owner = SparseOwner::query(memory, lease, resources)?;
        Self::open_in(
            source,
            roots,
            native,
            catalog_required,
            catalog,
            document,
            lexical,
            owner,
            Some(lease.bundle().sequence()),
            resources,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one admitted sparse interpretation bundle"
    )]
    fn open_in(
        source: &'a S,
        roots: SparseRoots,
        native: GraphRoots,
        catalog_required: RequiredRef,
        catalog: &'a C,
        document: Option<&'a EmbeddingTower>,
        lexical: TokenizerEpoch,
        owner: SparseOwner<'m>,
        admitted_sequence: Option<u64>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        owner.check(resources)?;
        let initial_empty =
            native.generation().get() == 0 && roots.text.is_none() && roots.vector.is_none();
        if roots.text.is_none() && !initial_empty {
            return Err(TreeError::Invalid("missing sparse text root"));
        }
        let text = roots
            .text
            .map(|required| {
                open_descriptor(
                    source,
                    required,
                    Modality::Text,
                    native.store(),
                    catalog_required,
                    lexical,
                    document,
                    owner,
                    resources,
                )
            })
            .transpose()?;
        let vector = roots
            .vector
            .map(|required| {
                open_descriptor(
                    source,
                    required,
                    Modality::Vector,
                    native.store(),
                    catalog_required,
                    lexical,
                    document,
                    owner,
                    resources,
                )
            })
            .transpose()?;
        let sequence =
            admitted_sequence.unwrap_or_else(|| text.map_or(0, |descriptor| descriptor.sequence));
        if (!initial_empty && document.is_some() != vector.is_some())
            || text.is_some_and(|descriptor| descriptor.generation != native.generation())
            || text.is_some_and(|descriptor| {
                admitted_sequence.is_some_and(|admitted| descriptor.sequence != admitted)
            })
            || vector.is_some_and(|descriptor| descriptor.generation != native.generation())
            || vector.is_some_and(|vector_descriptor| {
                text.is_none_or(|text_descriptor| {
                    vector_descriptor.sequence != text_descriptor.sequence
                        || vector_descriptor.checkpoint != text_descriptor.checkpoint
                        || vector_descriptor.catalog != text_descriptor.catalog
                })
            })
        {
            return Err(TreeError::Invalid(
                "sparse/native generation or document mismatch",
            ));
        }
        Ok(Self {
            source,
            catalog,
            document,
            native,
            text,
            vector,
            catalog_required,
            sequence,
            owner,
        })
    }

    pub(crate) const fn text_count(&self) -> u64 {
        match self.text {
            Some(descriptor) => descriptor.live_rows,
            None => 0,
        }
    }
    pub(crate) const fn vector_count(&self) -> u64 {
        match self.vector {
            Some(descriptor) => descriptor.live_rows,
            None => 0,
        }
    }
    pub(crate) const fn text_length(&self) -> u64 {
        match self.text {
            Some(descriptor) => descriptor.live_length,
            None => 0,
        }
    }
    pub(crate) const fn generation(&self) -> crate::property_graph::GraphGeneration {
        self.native.generation()
    }
    pub(crate) const fn sequence(&self) -> u64 {
        self.sequence
    }
    pub(crate) const fn checkpoint(&self) -> u64 {
        match self.text {
            Some(descriptor) => descriptor.checkpoint,
            None => 0,
        }
    }
    pub(crate) const fn interpretation_catalog(&self) -> RequiredRef {
        match self.text {
            Some(descriptor) => descriptor.catalog,
            None => self.catalog_required,
        }
    }
    pub(super) const fn root_state(&self, modality: Modality) -> Option<SparseRootState> {
        let descriptor = match modality {
            Modality::Text => match self.text {
                Some(descriptor) => descriptor,
                None => return None,
            },
            Modality::Vector => match self.vector {
                Some(descriptor) => descriptor,
                None => return None,
            },
        };
        Some(SparseRootState {
            members: descriptor.members,
            sources: descriptor.sources,
            live_rows: descriptor.live_rows,
            live_length: descriptor.live_length,
            checkpoint: descriptor.checkpoint,
        })
    }

    fn descriptor(&self, modality: Modality) -> Option<RootDescriptor> {
        match modality {
            Modality::Text => self.text,
            Modality::Vector => self.vector,
        }
    }

    fn resolve_member_row(
        &self,
        descriptor: RootDescriptor,
        modality: Modality,
        node: NodeId,
        member: MembershipRow,
        source: crate::property_graph::storage::artifact::PhysicalRef,
        ordinal: u32,
        row: SparseRow,
        lexical: Option<&SparseLexical<'m>>,
        resources: &mut TreeResources<'_>,
    ) -> Result<SparseMember<'a, S>, TreeError> {
        validate_row_correlation(node, member, source, ordinal, row)?;
        let key = node.get().to_le_bytes();
        let node_root = self.native.directory(TreeKind::Nodes)?;
        let native_entry = lookup_entry(self.source, node_root, &key, resources)?
            .ok_or(TreeError::Invalid("sparse native node is absent"))?;
        if PayloadRef::decode(native_entry.value())? != row.record {
            return Err(TreeError::Invalid("sparse/native record mismatch"));
        }
        let record = verify_node_state(
            PayloadSlice::new(
                self.source,
                descriptor.store,
                native_entry.creation_generation(),
                row.record,
            ),
            node,
            self.catalog,
            self.document,
            resources,
        )?;
        let NodeRecordState::Live(record) = record else {
            return Err(TreeError::Invalid("sparse row names a tombstone"));
        };
        if record.revision().get() != row.revision {
            return Err(TreeError::Invalid("sparse/native revision mismatch"));
        }
        let vector = match modality {
            Modality::Text => {
                if record.canonical().stored_text().is_none() || row.analyzed_length == 0 {
                    return Err(TreeError::Invalid("sparse text row payload mismatch"));
                }
                let lexical = lexical.ok_or(TreeError::Invalid("missing sparse lexical owner"))?;
                if lexical.row_count() <= ordinal
                    || lexical.row_lengths().get(ordinal as usize).copied()
                        != Some(row.analyzed_length)
                {
                    return Err(TreeError::Invalid("sparse lexical row mismatch"));
                }
                None
            }
            Modality::Vector => {
                if row.analyzed_length != 0 {
                    return Err(TreeError::Invalid("sparse vector row length"));
                }
                Some(
                    record
                        .canonical()
                        .stored_vector()
                        .ok_or(TreeError::Invalid("sparse vector row payload mismatch"))?,
                )
            }
        };
        Ok(SparseMember {
            node,
            revision: row.revision,
            row: ordinal,
            analyzed_length: row.analyzed_length,
            vector,
        })
    }

    pub(crate) fn sources<'v, 'r>(
        &'v self,
        modality: Modality,
        resources: &mut TreeResources<'r>,
    ) -> Result<SparseSources<'v, 'a, 'm, 'r, S, C>, TreeError> {
        self.owner.check(resources)?;
        let cursor = self
            .descriptor(modality)
            .map(|descriptor| {
                DirectoryCursor::seek(self.source, descriptor.sources, None, resources)
            })
            .transpose()?;
        let cursor_charge = cursor
            .as_ref()
            .map_or(0, |_| std::mem::size_of::<DirectoryCursor<'_, '_, S>>());
        let wrapper_bytes = std::mem::size_of::<SparseSources<'v, 'a, 'm, 'r, S, C>>()
            .saturating_sub(cursor_charge);
        let charge = self.owner.reserve(wrapper_bytes)?;
        Ok(SparseSources {
            view: self,
            cursor,
            modality,
            failed: false,
            _charge: charge,
        })
    }

    pub(crate) fn lookup(
        &self,
        modality: Modality,
        node: NodeId,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<SparseMember<'a, S>>, TreeError> {
        self.owner.check(resources)?;
        let descriptor = match modality {
            Modality::Text => match self.text {
                Some(descriptor) => descriptor,
                None => return Ok(None),
            },
            Modality::Vector => match self.vector {
                Some(descriptor) => descriptor,
                None => return Ok(None),
            },
        };
        let key = node.get().to_le_bytes();
        let Some(entry) = lookup_entry(self.source, descriptor.members, &key, resources)? else {
            return Ok(None);
        };
        if entry.value().len() != MEMBERSHIP_BYTES {
            return Err(TreeError::Invalid("sparse membership width"));
        }
        let member = MembershipRow::decode(entry.value())?;
        let mut source_key = [0_u8; 32];
        artifact::encode_reference(member.source, &mut source_key)?;
        let source_entry = lookup_entry(self.source, descriptor.sources, &source_key, resources)?
            .ok_or(TreeError::Invalid("sparse member source is absent"))?;
        if source_entry.value().len() != SOURCE_VALUE_BYTES {
            return Err(TreeError::Invalid("sparse source value width"));
        }
        let source_value = SourceValue::decode(source_entry.value())?;
        let manifest_block = self.source.resolve(member.source, resources)?;
        if manifest_block.reference() != member.source
            || member.source.kind != BlockKind::CommitParticipant
        {
            return Err(TreeError::Invalid("sparse source reference mismatch"));
        }
        let manifest = SourceManifest::decode(manifest_block.payload())?;
        if manifest.modality != modality
            || manifest_block.identity().generation != manifest.generation
            || manifest.generation > descriptor.generation
            || manifest.sequence > descriptor.sequence
            || member.row >= manifest.rows
            || source_value.live_rows > u64::from(manifest.rows)
        {
            return Err(TreeError::Invalid("sparse source cutoff or row geometry"));
        }
        let mask_byte = u64::from(member.row / 8);
        let mut bit = [0_u8; 1];
        if PayloadSlice::new(
            self.source,
            descriptor.store,
            source_entry.creation_generation(),
            source_value.mask,
        )
        .read_at(mask_byte, &mut bit, resources)?
            != 1
            || bit.first().copied().unwrap_or(0) & (1_u8 << (member.row % 8)) == 0
        {
            return Err(TreeError::Invalid("sparse row is not live"));
        }
        let mut row_bytes = [0_u8; ROW_BYTES];
        let row_table = PayloadSlice::new(
            self.source,
            descriptor.store,
            manifest.generation,
            manifest.row_table,
        );
        if row_table.read_at(
            u64::from(member.row) * ROW_BYTES as u64,
            &mut row_bytes,
            resources,
        )? != ROW_BYTES
        {
            return Err(TreeError::Invalid("short sparse row table"));
        }
        let row = SparseRow::decode(&row_bytes)?;
        let lexical = match modality {
            Modality::Text => {
                let lexical = manifest
                    .lexical
                    .ok_or(TreeError::Invalid("missing sparse lexical region"))?;
                let decoded = decode_lexical(
                    self.source,
                    descriptor.store,
                    manifest.generation,
                    lexical,
                    descriptor.lexical,
                    self.owner,
                    resources,
                )?;
                if decoded.row_count() != manifest.rows {
                    return Err(TreeError::Invalid("sparse lexical row count"));
                }
                Some(decoded)
            }
            Modality::Vector => None,
        };
        Ok(Some(self.resolve_member_row(
            descriptor,
            modality,
            node,
            member,
            member.source,
            member.row,
            row,
            lexical.as_ref(),
            resources,
        )?))
    }

    pub(crate) fn validate_all(
        &self,
        modality: Modality,
        resources: &mut TreeResources<'_>,
    ) -> Result<u64, TreeError> {
        let descriptor = match modality {
            Modality::Text => match self.text {
                Some(descriptor) => descriptor,
                None => return Ok(0),
            },
            Modality::Vector => match self.vector {
                Some(descriptor) => descriptor,
                None => return Ok(0),
            },
        };
        let mut members = DirectoryCursor::seek(self.source, descriptor.members, None, resources)?;
        let mut member_key = [0_u8; 16];
        let mut member_value = [0_u8; MEMBERSHIP_BYTES];
        let mut member_count = 0_u64;
        while let Some((key_len, value_len)) =
            members.next(&mut member_key, &mut member_value, resources)?
        {
            if key_len != member_key.len() || value_len != member_value.len() {
                return Err(TreeError::Invalid("sparse membership entry width"));
            }
            let node = NodeId::new(u128::from_le_bytes(member_key))
                .map_err(|_| TreeError::Invalid("zero sparse member node"))?;
            let observed = self
                .lookup(modality, node, resources)?
                .ok_or(TreeError::Invalid("scanned sparse member disappeared"))?;
            if observed.node != node {
                return Err(TreeError::Invalid("scanned sparse member identity"));
            }
            member_count = member_count.checked_add(1).ok_or(TreeError::Work)?;
        }
        let mut sources = DirectoryCursor::seek(self.source, descriptor.sources, None, resources)?;
        let mut source_key = [0_u8; 32];
        let mut source_value = [0_u8; SOURCE_VALUE_BYTES];
        let mut source_rows = 0_u64;
        let mut source_length = 0_u64;
        while let Some((key_len, value_len)) =
            sources.next(&mut source_key, &mut source_value, resources)?
        {
            if key_len != source_key.len() || value_len != source_value.len() {
                return Err(TreeError::Invalid("sparse source entry width"));
            }
            let reference = artifact::decode_reference(&source_key)?;
            let value = SourceValue::decode(&source_value)?;
            let block = self.source.resolve(reference, resources)?;
            if block.reference() != reference {
                return Err(TreeError::Invalid("sparse scanned source reference"));
            }
            let manifest = SourceManifest::decode(block.payload())?;
            if manifest.modality != modality {
                return Err(TreeError::Invalid("sparse scanned source modality"));
            }
            let mask_len = (manifest.rows as usize).saturating_add(7) / 8;
            if value.mask.len() != mask_len as u64 {
                return Err(TreeError::Invalid("sparse scanned mask length"));
            }
            let mask = PayloadSlice::new(
                self.source,
                descriptor.store,
                descriptor.generation,
                value.mask,
            );
            let mut live = 0_u64;
            let mut length = 0_u64;
            for byte_index in 0..mask_len {
                let mut encoded = [0_u8; 1];
                if mask.read_at(byte_index as u64, &mut encoded, resources)? != 1 {
                    return Err(TreeError::Invalid("short sparse scanned mask"));
                }
                let byte = encoded.first().copied().unwrap_or(0);
                if byte_index + 1 == mask_len && manifest.rows % 8 != 0 {
                    let valid = (1_u16 << (manifest.rows % 8)) as u8 - 1;
                    if byte & !valid != 0 {
                        return Err(TreeError::Invalid("sparse mask tail bits"));
                    }
                }
                for bit_index in 0..8_u32 {
                    let row_index = byte_index as u32 * 8 + bit_index;
                    if row_index >= manifest.rows || byte & (1_u8 << bit_index) == 0 {
                        continue;
                    }
                    let mut row_bytes = [0_u8; ROW_BYTES];
                    let rows = PayloadSlice::new(
                        self.source,
                        descriptor.store,
                        manifest.generation,
                        manifest.row_table,
                    );
                    if rows.read_at(
                        u64::from(row_index) * ROW_BYTES as u64,
                        &mut row_bytes,
                        resources,
                    )? != ROW_BYTES
                    {
                        return Err(TreeError::Invalid("short sparse scanned row"));
                    }
                    let row = SparseRow::decode(&row_bytes)?;
                    let member_entry = lookup_entry(
                        self.source,
                        descriptor.members,
                        &row.node.get().to_le_bytes(),
                        resources,
                    )?
                    .ok_or(TreeError::Invalid("live sparse row lacks membership"))?;
                    let member = MembershipRow::decode(member_entry.value())?;
                    if member.source != reference
                        || member.row != row_index
                        || member.revision != row.revision
                    {
                        return Err(TreeError::Invalid("sparse row/membership bijection"));
                    }
                    live = live.checked_add(1).ok_or(TreeError::Work)?;
                    length = length
                        .checked_add(u64::from(row.analyzed_length))
                        .ok_or(TreeError::Work)?;
                }
            }
            if live != value.live_rows
                || (modality == Modality::Text && length != value.live_length)
                || (modality == Modality::Vector && (length != 0 || value.live_length != 0))
            {
                return Err(TreeError::Invalid("sparse source aggregate mismatch"));
            }
            source_rows = source_rows.checked_add(live).ok_or(TreeError::Work)?;
            source_length = source_length.checked_add(length).ok_or(TreeError::Work)?;
        }
        if member_count != descriptor.live_rows
            || source_rows != descriptor.live_rows
            || source_length != descriptor.live_length
        {
            return Err(TreeError::Invalid("sparse root aggregate mismatch"));
        }
        Ok(member_count)
    }

    pub(crate) fn validate_unchanged_membership(
        &self,
        target: &Self,
        modality: Modality,
        changes: &[super::PreparedMembershipChange],
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let descriptor = match modality {
            Modality::Text => self.text,
            Modality::Vector => self.vector,
        };
        let target_descriptor = match modality {
            Modality::Text => target.text,
            Modality::Vector => target.vector,
        };
        let (Some(descriptor), Some(target_descriptor)) = (descriptor, target_descriptor) else {
            if descriptor.is_some() != target_descriptor.is_some() {
                return Err(TreeError::Invalid("sparse replay modality space changed"));
            }
            return Ok(());
        };
        let mut base = DirectoryCursor::seek(self.source, descriptor.members, None, resources)?;
        let mut active =
            DirectoryCursor::seek(target.source, target_descriptor.members, None, resources)?;
        let mut base_key = [0_u8; 16];
        let mut base_value = [0_u8; MEMBERSHIP_BYTES];
        let mut active_key = [0_u8; 16];
        let mut active_value = [0_u8; MEMBERSHIP_BYTES];
        let mut base_present = base
            .next(&mut base_key, &mut base_value, resources)?
            .is_some();
        let mut active_present = active
            .next(&mut active_key, &mut active_value, resources)?
            .is_some();
        while base_present || active_present {
            let order = match (base_present, active_present) {
                (true, true) => u128::from_le_bytes(base_key).cmp(&u128::from_le_bytes(active_key)),
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => break,
            };
            let node_bytes = if order == std::cmp::Ordering::Greater {
                active_key
            } else {
                base_key
            };
            let node = NodeId::new(u128::from_le_bytes(node_bytes))
                .map_err(|_| TreeError::Invalid("zero sparse replay member"))?;
            let changed = changes.iter().any(|change| change.node == Some(node));
            if !changed && (order != std::cmp::Ordering::Equal || base_value != active_value) {
                return Err(TreeError::Invalid(
                    "sparse replay unexplained membership change",
                ));
            }
            if order != std::cmp::Ordering::Greater {
                base_present = base
                    .next(&mut base_key, &mut base_value, resources)?
                    .is_some();
            }
            if order != std::cmp::Ordering::Less {
                active_present = active
                    .next(&mut active_key, &mut active_value, resources)?
                    .is_some();
            }
        }
        Ok(())
    }
}

impl<'v, 'a, 'm, 'r, S: BlockSource, C: RecordCatalog<S>> SparseSources<'v, 'a, 'm, 'r, S, C> {
    pub(crate) fn next(
        &mut self,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<SparseSource<'v, 'a, 'm, S, C>>, TreeError> {
        if self.failed {
            return Err(TreeError::Invalid("sparse source cursor previously failed"));
        }
        let result = self.next_inner(resources);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn next_inner(
        &mut self,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<SparseSource<'v, 'a, 'm, S, C>>, TreeError> {
        self.view.owner.check(resources)?;
        let Some(descriptor) = self.view.descriptor(self.modality) else {
            return Ok(None);
        };
        let Some(cursor) = &mut self.cursor else {
            return Ok(None);
        };
        let Some(entry) = cursor.next_entry(resources)? else {
            return Ok(None);
        };
        entry.require_root(descriptor.sources)?;
        let Key::Inline(key) = entry.key() else {
            return Err(TreeError::Invalid("sparse source key must be inline"));
        };
        if key.len() != 32 || entry.value().len() != SOURCE_VALUE_BYTES {
            return Err(TreeError::Invalid("sparse source entry width"));
        }
        let reference = artifact::decode_reference(key)?;
        let value = SourceValue::decode(entry.value())?;
        let block = self.view.source.resolve(reference, resources)?;
        if block.reference() != reference || reference.kind != BlockKind::CommitParticipant {
            return Err(TreeError::Invalid("sparse source reference mismatch"));
        }
        let manifest = SourceManifest::decode(block.payload())?;
        let mask_length = (u64::from(manifest.rows) + 7) / 8;
        if manifest.modality != self.modality
            || block.identity().generation != manifest.generation
            || manifest.generation > descriptor.generation
            || manifest.sequence > descriptor.sequence
            || value.live_rows > u64::from(manifest.rows)
            || value.mask.len() != mask_length
        {
            return Err(TreeError::Invalid("sparse source cutoff or row geometry"));
        }
        if manifest.rows % 8 != 0 {
            let mut tail = [0_u8; 1];
            if PayloadSlice::new(
                self.view.source,
                descriptor.store,
                entry.creation_generation(),
                value.mask,
            )
            .read_at(mask_length.saturating_sub(1), &mut tail, resources)?
                != 1
            {
                return Err(TreeError::Invalid("short sparse source mask"));
            }
            let valid = (1_u16 << (manifest.rows % 8)) as u8 - 1;
            if tail.first().copied().unwrap_or(0) & !valid != 0 {
                return Err(TreeError::Invalid("sparse mask tail bits"));
            }
        }
        let lexical = match self.modality {
            Modality::Text => {
                let lexical = manifest
                    .lexical
                    .ok_or(TreeError::Invalid("missing sparse lexical region"))?;
                let decoded = decode_lexical(
                    self.view.source,
                    descriptor.store,
                    manifest.generation,
                    lexical,
                    descriptor.lexical,
                    self.view.owner,
                    resources,
                )?;
                if decoded.row_count() != manifest.rows {
                    return Err(TreeError::Invalid("sparse lexical row count"));
                }
                Some(decoded)
            }
            Modality::Vector => None,
        };
        if manifest.row_table.len() != u64::from(manifest.rows) * ROW_BYTES as u64 {
            return Err(TreeError::Invalid("sparse row table length"));
        }
        let mask = PayloadSlice::new(
            self.view.source,
            descriptor.store,
            entry.creation_generation(),
            value.mask,
        );
        let rows = PayloadSlice::new(
            self.view.source,
            descriptor.store,
            manifest.generation,
            manifest.row_table,
        );
        let mut live_rows = 0_u64;
        let mut live_length = 0_u64;
        for ordinal in 0..manifest.rows {
            let mut bit = [0_u8; 1];
            if mask.read_at(u64::from(ordinal / 8), &mut bit, resources)? != 1 {
                return Err(TreeError::Invalid("short sparse source mask"));
            }
            let mut row_bytes = [0_u8; ROW_BYTES];
            if rows.read_at(
                u64::from(ordinal) * ROW_BYTES as u64,
                &mut row_bytes,
                resources,
            )? != ROW_BYTES
            {
                return Err(TreeError::Invalid("short sparse row table"));
            }
            let row = SparseRow::decode(&row_bytes)?;
            match self.modality {
                Modality::Text => {
                    if lexical
                        .as_ref()
                        .and_then(|lexical| lexical.row_lengths().get(ordinal as usize))
                        .copied()
                        != Some(row.analyzed_length)
                    {
                        return Err(TreeError::Invalid("sparse lexical row mismatch"));
                    }
                }
                Modality::Vector if row.analyzed_length != 0 => {
                    return Err(TreeError::Invalid("sparse vector row length"));
                }
                Modality::Vector => {}
            }
            if bit.first().copied().unwrap_or(0) & (1_u8 << (ordinal % 8)) != 0 {
                live_rows = live_rows.checked_add(1).ok_or(TreeError::Work)?;
                if self.modality == Modality::Text {
                    live_length = live_length
                        .checked_add(u64::from(row.analyzed_length))
                        .ok_or(TreeError::Work)?;
                }
            }
        }
        if live_rows != value.live_rows || live_length != value.live_length {
            return Err(TreeError::Invalid("sparse source aggregate mismatch"));
        }
        let charge = self
            .view
            .owner
            .reserve(std::mem::size_of::<SparseSource<'v, 'a, 'm, S, C>>())?;
        Ok(Some(SparseSource {
            view: self.view,
            descriptor,
            reference,
            value,
            manifest,
            mask_generation: entry.creation_generation(),
            lexical,
            _charge: charge,
        }))
    }
}

impl<'v, 'a, 'm, S: BlockSource, C: RecordCatalog<S>> SparseSource<'v, 'a, 'm, S, C> {
    pub(crate) const fn row_count(&self) -> u32 {
        self.manifest.rows
    }

    fn live_bit(&self, row: u32, resources: &mut TreeResources<'_>) -> Result<bool, TreeError> {
        if row >= self.manifest.rows {
            return Err(TreeError::Invalid("sparse source row out of range"));
        }
        let mut byte = [0_u8; 1];
        if PayloadSlice::new(
            self.view.source,
            self.descriptor.store,
            self.mask_generation,
            self.value.mask,
        )
        .read_at(u64::from(row / 8), &mut byte, resources)?
            != 1
        {
            return Err(TreeError::Invalid("short sparse source mask"));
        }
        Ok(byte.first().copied().unwrap_or(0) & (1_u8 << (row % 8)) != 0)
    }

    pub(crate) fn is_live(
        &self,
        row: u32,
        resources: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        self.view.owner.check(resources)?;
        self.live_bit(row, resources)
    }

    pub(crate) fn resolve_row(
        &self,
        ordinal: u32,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<SparseMember<'a, S>>, TreeError> {
        self.view.owner.check(resources)?;
        let live = self.live_bit(ordinal, resources)?;
        let mut row_bytes = [0_u8; ROW_BYTES];
        if PayloadSlice::new(
            self.view.source,
            self.descriptor.store,
            self.manifest.generation,
            self.manifest.row_table,
        )
        .read_at(
            u64::from(ordinal) * ROW_BYTES as u64,
            &mut row_bytes,
            resources,
        )? != ROW_BYTES
        {
            return Err(TreeError::Invalid("short sparse row table"));
        }
        let row = SparseRow::decode(&row_bytes)?;
        if self.manifest.modality == Modality::Text
            && self
                .lexical
                .as_ref()
                .and_then(|lexical| lexical.row_lengths().get(ordinal as usize))
                .copied()
                != Some(row.analyzed_length)
        {
            return Err(TreeError::Invalid("sparse lexical row mismatch"));
        }
        let member_entry = lookup_entry(
            self.view.source,
            self.descriptor.members,
            &row.node.get().to_le_bytes(),
            resources,
        )?;
        let member = member_entry
            .map(|entry| MembershipRow::decode(entry.value()))
            .transpose()?;
        if !live {
            if member.is_some_and(|member| member.source == self.reference && member.row == ordinal)
            {
                return Err(TreeError::Invalid("dead sparse row retains membership"));
            }
            return Ok(None);
        }
        let member = member.ok_or(TreeError::Invalid("live sparse row lacks membership"))?;
        Ok(Some(self.view.resolve_member_row(
            self.descriptor,
            self.manifest.modality,
            row.node,
            member,
            self.reference,
            ordinal,
            row,
            self.lexical.as_ref(),
            resources,
        )?))
    }

    pub(crate) fn lexical(
        &self,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<&SealedSegment>, TreeError> {
        self.view.owner.check(resources)?;
        Ok(self.lexical.as_ref().map(SparseLexical::sealed))
    }
}
