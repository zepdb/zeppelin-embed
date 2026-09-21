//! Native sparse-source vector index ownership and persisted image.

mod prepare;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support;

use super::codec::{decode_required, validate_catalog_interpretation};
use super::view::{SparseBytes, SparseOwner};
use crate::graph::GraphParams;
use crate::graph::block::{
    GraphNodeBlocks, ValidatedGraphNodeBlocks, decode_node_blocks_controlled,
};
use crate::property_graph::storage::artifact::BlockKind;
use crate::property_graph::storage::memory::StorageBuffer;
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::records::{NodeRecordState, RecordCatalog, verify_node_state};
use crate::property_graph::storage::search::codec::{ROW_BYTES, SparseRow};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError, TreeResources};
use crate::property_graph::wal::RequiredRef;
use crate::property_graph::{GraphGeneration, NodeId, StoreInstanceId};

pub(super) use prepare::{NativeVectorRow, prepare_vector_index};

const HEADER_BYTES: usize = 256;
const MAGIC: &[u8; 8] = b"ZGNVIDX1";
pub(super) const NATIVE_BUILD_SEED: u64 = 0x20_00c0_ffee;

enum IndexFloats<'m> {
    Preparation(StorageBuffer<'m, f32>),
    Query(crate::property_graph::query::resources::QueryArena<'m, 'm, f32>),
}

impl<'m> IndexFloats<'m> {
    fn new(owner: SparseOwner<'m>, capacity: usize) -> Result<Self, TreeError> {
        match owner {
            SparseOwner::Preparation(memory) => {
                Ok(Self::Preparation(StorageBuffer::new(memory, capacity)?))
            }
            SparseOwner::Query { memory, .. } => Ok(Self::Query(
                crate::property_graph::query::resources::QueryArena::new(memory, capacity)
                    .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                    .map_err(TreeError::Runtime)?,
            )),
        }
    }

    fn push(&mut self, value: f32) -> Result<(), TreeError> {
        match self {
            Self::Preparation(values) => values.push(value),
            Self::Query(values) => values
                .push(value)
                .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                .map_err(TreeError::Runtime),
        }
    }

    fn as_slice(&self) -> &[f32] {
        match self {
            Self::Preparation(values) => values.as_slice(),
            Self::Query(values) => values.as_slice(),
        }
    }
}

/// Complete checked source-local vector index retained under its sparse owner.
pub(crate) struct NativeVectorIndex<'m> {
    payload: PayloadRef,
    encoded: SparseBytes<'m>,
    rescore: IndexFloats<'m>,
    rows: u32,
    dimensions: u32,
    padded_dimensions: u32,
    profile: u8,
    graph_offset: usize,
    graph_length: usize,
    graph_descriptor: ValidatedGraphNodeBlocks,
    identities_offset: usize,
    codes_offset: usize,
    factors_offset: usize,
    seeds: [u32; 4],
    interpretation_catalog: RequiredRef,
}

impl NativeVectorIndex<'_> {
    pub(crate) const fn row_count(&self) -> u32 {
        self.rows
    }

    pub(crate) const fn dimensions(&self) -> u32 {
        self.dimensions
    }

    pub(crate) const fn profile_tag(&self) -> u8 {
        self.profile
    }

    pub(crate) const fn seed_row_ids(&self) -> [u32; 4] {
        self.seeds
    }

    pub(crate) const fn interpretation_catalog(&self) -> RequiredRef {
        self.interpretation_catalog
    }

    pub(crate) fn graph(&self) -> Result<GraphNodeBlocks<'_>, TreeError> {
        let graph = section(
            self.encoded.as_slice(),
            self.graph_offset,
            self.graph_length,
        )?;
        self.graph_descriptor
            .bind(graph)
            .map_err(|_| TreeError::Invalid("native vector graph"))
    }

    pub(crate) fn rescore(&self) -> &[f32] {
        self.rescore.as_slice()
    }

    #[cfg(any(test, all(feature = "graph-cypher", feature = "test-support")))]
    pub(crate) fn encoded_bytes(&self) -> &[u8] {
        self.encoded.as_slice()
    }

    #[cfg(any(test, all(feature = "graph-cypher", feature = "test-support")))]
    pub(crate) const fn payload_reference(&self) -> PayloadRef {
        self.payload
    }

    pub(crate) fn identity(&self, row: u32) -> Result<(NodeId, u64), TreeError> {
        if row >= self.rows {
            return Err(TreeError::Invalid("native vector identity row"));
        }
        let start = self
            .identities_offset
            .checked_add(row as usize * 24)
            .ok_or(TreeError::Memory)?;
        let bytes = section(self.encoded.as_slice(), start, 24)?;
        let node = NodeId::new(u128::from_le_bytes(array(bytes, 0)?))
            .map_err(|_| TreeError::Invalid("native vector zero identity"))?;
        let revision = u64::from_le_bytes(array(bytes, 16)?);
        if revision == 0 {
            return Err(TreeError::Invalid("native vector zero revision"));
        }
        Ok((node, revision))
    }

    pub(crate) fn code(&self, row: u32) -> Result<&[u8], TreeError> {
        if row >= self.rows {
            return Err(TreeError::Invalid("native vector code row"));
        }
        let stride = (self.dimensions as usize).div_ceil(2);
        let start = self
            .codes_offset
            .checked_add(row as usize * stride)
            .ok_or(TreeError::Memory)?;
        section(self.encoded.as_slice(), start, stride)
    }

    pub(crate) fn factors(&self, row: u32) -> Result<crate::quant::Bit4Factors, TreeError> {
        if row >= self.rows {
            return Err(TreeError::Invalid("native vector factor row"));
        }
        let start = self
            .factors_offset
            .checked_add(row as usize * 12)
            .ok_or(TreeError::Memory)?;
        let bytes = section(self.encoded.as_slice(), start, 12)?;
        Ok(crate::quant::Bit4Factors::from_persisted(
            f32::from_le_bytes(array(bytes, 0)?),
            f32::from_le_bytes(array(bytes, 4)?),
            f32::from_le_bytes(array(bytes, 8)?),
        ))
    }

    pub(crate) fn coordinate(&self, row: u32, dimension: u32) -> Result<f32, TreeError> {
        if row >= self.rows || dimension >= self.dimensions {
            return Err(TreeError::Invalid("native vector coordinate"));
        }
        let index = row as usize * self.dimensions as usize + dimension as usize;
        self.rescore
            .as_slice()
            .get(index)
            .copied()
            .ok_or(TreeError::Invalid("native vector coordinate extent"))
    }
}

pub(super) fn open_vector_index<'m, S: BlockSource>(
    source: &S,
    store: StoreInstanceId,
    generation: GraphGeneration,
    payload: PayloadRef,
    lexical: crate::fts::tokenizer::TokenizerEpoch,
    document: Option<&crate::epoch::EmbeddingTower>,
    owner: SparseOwner<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<NativeVectorIndex<'m>, TreeError> {
    owner.check(resources)?;
    if payload.role() != BlockKind::RetrievalVectorIndex {
        return Err(TreeError::Invalid("native vector index role"));
    }
    let length = usize::try_from(payload.len()).map_err(|_| TreeError::Memory)?;
    let mut encoded = SparseBytes::new(owner, length, resources)?;
    if PayloadSlice::new(source, store, generation, payload).read_at(
        0,
        encoded.as_mut_slice(),
        resources,
    )? != length
    {
        return Err(TreeError::Invalid("short native vector index"));
    }
    let bytes = encoded.as_slice();
    if bytes.len() < HEADER_BYTES
        || bytes.get(..8) != Some(MAGIC.as_slice())
        || read_u16(bytes, 8)? != 1
        || read_u16(bytes, 10)? != 4
        || read_u16(bytes, 12)? != 1
    {
        return Err(TreeError::Invalid("native vector index header"));
    }
    let profile = *bytes
        .get(14)
        .ok_or(TreeError::Invalid("native vector profile"))?;
    let distinct_seeds = *bytes
        .get(15)
        .ok_or(TreeError::Invalid("native vector seeds"))?;
    if !matches!(profile, 1 | 2) {
        return Err(TreeError::Invalid("native vector profile"));
    }
    let params = if profile == 1 {
        GraphParams::sift_1m()
    } else {
        GraphParams::angular()
    };
    if bytes.get(44).copied() != Some(params.r_target())
        || bytes.get(45).copied() != Some(params.r_max())
        || read_u16(bytes, 46)? != params.l_build()
        || read_u32(bytes, 48)? != params.alpha_build().to_bits()
        || read_u32(bytes, 52)? != params.alpha_refine().to_bits()
    {
        return Err(TreeError::Invalid("native vector build profile"));
    }
    let encoded_store = StoreInstanceId::new(u128::from_le_bytes(array(bytes, 16)?))
        .map_err(|_| TreeError::Invalid("native vector store"))?;
    let rows = read_u32(bytes, 32)?;
    let dimensions = read_u32(bytes, 36)?;
    let padded_dimensions = read_u32(bytes, 40)?;
    if encoded_store != store || rows == 0 || dimensions == 0 || padded_dimensions < dimensions {
        return Err(TreeError::Invalid("native vector geometry"));
    }
    if read_u64(bytes, 56)? != NATIVE_BUILD_SEED {
        return Err(TreeError::Invalid("native vector interpretation"));
    }
    let interpretation_catalog = decode_required(section(bytes, 64, 96)?)?;
    validate_catalog_interpretation(
        source,
        interpretation_catalog,
        store,
        lexical,
        document,
        owner,
        resources,
    )?;
    let document = document.ok_or(TreeError::Invalid("native vector document interpretation"))?;
    if document.dims != dimensions {
        return Err(TreeError::Invalid("native vector document dimensions"));
    }
    let expected_profile = match document.normalization {
        crate::epoch::Normalization::None => 1,
        crate::epoch::Normalization::L2 => 2,
    };
    if profile != expected_profile {
        return Err(TreeError::Invalid("native vector profile interpretation"));
    }
    let mut offsets = [0_usize; 5];
    let mut lengths = [0_usize; 5];
    let mut expected_offset = HEADER_BYTES;
    for (index, (offset, length)) in offsets.iter_mut().zip(lengths.iter_mut()).enumerate() {
        let base = 160 + index * 16;
        *offset = usize::try_from(read_u64(bytes, base)?).map_err(|_| TreeError::Memory)?;
        *length = usize::try_from(read_u64(bytes, base + 8)?).map_err(|_| TreeError::Memory)?;
        if *offset != expected_offset {
            return Err(TreeError::Invalid("native vector section order"));
        }
        expected_offset = expected_offset
            .checked_add(*length)
            .ok_or(TreeError::Memory)?;
    }
    let [
        identities_offset,
        codes_offset,
        factors_offset,
        rescore_offset,
        graph_offset,
    ] = offsets;
    let [
        identities_length,
        codes_length,
        factors_length,
        rescore_length,
        graph_length,
    ] = lengths;
    if expected_offset != bytes.len()
        || identities_length != rows as usize * 24
        || codes_length != rows as usize * (dimensions as usize).div_ceil(2)
        || factors_length != rows as usize * 12
        || rescore_length != rows as usize * dimensions as usize * 4
    {
        return Err(TreeError::Invalid("native vector section geometry"));
    }
    let seeds = [
        read_u32(bytes, 240)?,
        read_u32(bytes, 244)?,
        read_u32(bytes, 248)?,
        read_u32(bytes, 252)?,
    ];
    if distinct_seeds != rows.min(4) as u8 || seeds.iter().any(|seed| *seed >= rows) {
        return Err(TreeError::Invalid("native vector seed geometry"));
    }
    let active_seed_count = usize::from(distinct_seeds);
    for index in 0..active_seed_count {
        let seed = seeds
            .get(index)
            .ok_or(TreeError::Invalid("native vector seed extent"))?;
        if seeds
            .get(..index)
            .ok_or(TreeError::Invalid("native vector seed prefix"))?
            .contains(seed)
        {
            return Err(TreeError::Invalid("native vector duplicate seed"));
        }
    }
    let first_seed = seeds
        .first()
        .ok_or(TreeError::Invalid("native vector first seed"))?;
    if seeds
        .get(active_seed_count..)
        .ok_or(TreeError::Invalid("native vector unused seed extent"))?
        .iter()
        .any(|seed| seed != first_seed)
    {
        return Err(TreeError::Invalid("native vector unused seed adapter"));
    }
    let mut graph_control_error = None;
    let graph_result =
        decode_node_blocks_controlled(section(bytes, graph_offset, graph_length)?, &mut |units| {
            match resources.step(units) {
                Ok(()) => true,
                Err(error) => {
                    graph_control_error = Some(error);
                    false
                }
            }
        });
    if let Some(error) = graph_control_error {
        return Err(error);
    }
    let graph = graph_result.map_err(|_| TreeError::Invalid("native vector graph"))?;
    let graph_descriptor = graph.validated_descriptor();
    if graph.node_count() != rows
        || graph.layout().dims() != dimensions
        || graph.layout().padded_dims() != padded_dimensions
        || graph.layout().max_degree() != params.r_max()
    {
        return Err(TreeError::Invalid("native vector graph geometry"));
    }
    for row in 0..rows {
        resources.step(1)?;
        let block = graph
            .block(row)
            .map_err(|_| TreeError::Invalid("native vector graph row"))?;
        let code_start = codes_offset + row as usize * (dimensions as usize).div_ceil(2);
        let codes = section(bytes, code_start, (dimensions as usize).div_ceil(2))?;
        let graph_codes = block
            .codes()
            .get(..codes.len())
            .ok_or(TreeError::Invalid("native vector graph code extent"))?;
        for (graph_chunk, code_chunk) in graph_codes.chunks(256).zip(codes.chunks(256)) {
            resources.step(code_chunk.len() as u64)?;
            if graph_chunk != code_chunk {
                return Err(TreeError::Invalid("native vector code/graph disagreement"));
            }
        }
        let padding = block
            .codes()
            .get(codes.len()..)
            .ok_or(TreeError::Invalid("native vector graph padding extent"))?;
        for chunk in padding.chunks(256) {
            resources.step(chunk.len() as u64)?;
            if chunk.iter().any(|byte| *byte != 0) {
                return Err(TreeError::Invalid("native vector code/graph disagreement"));
            }
        }
        let factor_start = factors_offset + row as usize * 12;
        let factor_bytes = section(bytes, factor_start, 12)?;
        let stored = block.factors().persisted_fields();
        for (index, value) in stored.iter().enumerate() {
            if value.to_bits().to_le_bytes() != array::<4>(factor_bytes, index * 4)? {
                return Err(TreeError::Invalid(
                    "native vector factor/graph disagreement",
                ));
            }
        }
        let flagged = block.flags() & 1 != 0;
        if flagged
            != seeds
                .get(..usize::from(distinct_seeds))
                .ok_or(TreeError::Invalid("native vector active seed extent"))?
                .contains(&row)
        {
            return Err(TreeError::Invalid("native vector seed/graph disagreement"));
        }
    }
    let mut rescore = IndexFloats::new(owner, rows as usize * dimensions as usize)?;
    for coordinate_chunk in section(bytes, rescore_offset, rescore_length)?.chunks(256 * 4) {
        resources.step((coordinate_chunk.len() / 4) as u64)?;
        for chunk in coordinate_chunk.chunks_exact(4) {
            let value = f32::from_le_bytes(
                chunk
                    .try_into()
                    .map_err(|_| TreeError::Invalid("native vector f32"))?,
            );
            if !value.is_finite() {
                return Err(TreeError::Invalid("native vector nonfinite f32"));
            }
            rescore.push(value)?;
        }
    }
    if profile == 2 {
        for row in rescore.as_slice().chunks_exact(dimensions as usize) {
            if crate::graph::search::non_unit_squared_norm_controlled(row, &mut |units| {
                resources.step(units)
            })?
            .is_some()
            {
                return Err(TreeError::Invalid("native angular vector norm"));
            }
        }
    }
    Ok(NativeVectorIndex {
        payload,
        encoded,
        rescore,
        rows,
        dimensions,
        padded_dimensions,
        profile,
        graph_offset,
        graph_length,
        graph_descriptor,
        identities_offset,
        codes_offset,
        factors_offset,
        seeds,
        interpretation_catalog,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_vector_index_rows<S: BlockSource, C: RecordCatalog<S>>(
    source: &S,
    store: StoreInstanceId,
    generation: GraphGeneration,
    row_table: PayloadRef,
    rows: u32,
    catalog: &C,
    document: Option<&crate::epoch::EmbeddingTower>,
    index: &NativeVectorIndex<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if index.row_count() != rows {
        return Err(TreeError::Invalid("native vector/source row count"));
    }
    let table = PayloadSlice::new(source, store, generation, row_table);
    for ordinal in 0..rows {
        let mut row_bytes = [0_u8; ROW_BYTES];
        if table.read_at(
            u64::from(ordinal) * ROW_BYTES as u64,
            &mut row_bytes,
            resources,
        )? != ROW_BYTES
        {
            return Err(TreeError::Invalid("short native vector row table"));
        }
        let row = SparseRow::decode(&row_bytes)?;
        if index.identity(ordinal)? != (row.node, row.revision) {
            return Err(TreeError::Invalid("native vector/source identity"));
        }
        let state = verify_node_state(
            PayloadSlice::new(source, store, generation, row.record),
            row.node,
            catalog,
            document,
            resources,
        )?;
        let NodeRecordState::Live(record) = state else {
            return Err(TreeError::Invalid("native vector source record state"));
        };
        if record.revision().get() != row.revision {
            return Err(TreeError::Invalid("native vector/source revision"));
        }
        let vector = record
            .canonical()
            .stored_vector()
            .ok_or(TreeError::Invalid("native vector source record"))?;
        if vector.dimensions() != index.dimensions() {
            return Err(TreeError::Invalid("native vector/source dimensions"));
        }
        for dimension in 0..vector.dimensions() {
            if vector.coordinate(dimension, resources)?.to_bits()
                != index.coordinate(ordinal, dimension)?.to_bits()
            {
                return Err(TreeError::Invalid("native vector/source coordinate"));
            }
        }
    }
    Ok(())
}

fn section(bytes: &[u8], start: usize, length: usize) -> Result<&[u8], TreeError> {
    let end = start.checked_add(length).ok_or(TreeError::Memory)?;
    bytes
        .get(start..end)
        .ok_or(TreeError::Invalid("native vector index extent"))
}

fn array<const N: usize>(bytes: &[u8], start: usize) -> Result<[u8; N], TreeError> {
    section(bytes, start, N)?
        .try_into()
        .map_err(|_| TreeError::Invalid("native vector scalar extent"))
}

fn read_u16(bytes: &[u8], start: usize) -> Result<u16, TreeError> {
    Ok(u16::from_le_bytes(array(bytes, start)?))
}

fn read_u32(bytes: &[u8], start: usize) -> Result<u32, TreeError> {
    Ok(u32::from_le_bytes(array(bytes, start)?))
}

fn read_u64(bytes: &[u8], start: usize) -> Result<u64, TreeError> {
    Ok(u64::from_le_bytes(array(bytes, start)?))
}

pub(super) fn put(output: &mut [u8], start: usize, bytes: &[u8]) -> Result<(), TreeError> {
    let end = start.checked_add(bytes.len()).ok_or(TreeError::Memory)?;
    output
        .get_mut(start..end)
        .ok_or(TreeError::Invalid("native vector encode extent"))?
        .copy_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn open_vector_index_image_for_test<'m, S: BlockSource>(
    source: &S,
    store: StoreInstanceId,
    generation: GraphGeneration,
    image: &[u8],
    lexical: crate::fts::tokenizer::TokenizerEpoch,
    document: Option<&crate::epoch::EmbeddingTower>,
    owner: SparseOwner<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<NativeVectorIndex<'m>, TreeError> {
    use crate::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, Block, ContainerKind, FramedBlock,
    };

    struct Overlay<'a, S> {
        base: &'a S,
        bytes: Vec<u8>,
        reference: crate::property_graph::storage::artifact::PhysicalRef,
    }
    impl<S: BlockSource> BlockSource for Overlay<'_, S> {
        fn resolve<'a>(
            &'a self,
            reference: crate::property_graph::storage::artifact::PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            if reference != self.reference {
                return self.base.resolve(reference, resources);
            }
            resources.step(1)?;
            let frame = artifact::decode(
                ContainerKind::Object,
                Some((store_from_bytes(&self.bytes)?, reference.artifact)),
                &self.bytes,
            )?;
            Ok(frame.framed_block(reference)?)
        }
    }
    fn store_from_bytes(bytes: &[u8]) -> Result<StoreInstanceId, TreeError> {
        let raw = bytes
            .get(32..48)
            .ok_or(TreeError::Invalid("test overlay store extent"))?
            .try_into()
            .map_err(|_| TreeError::Invalid("test overlay store width"))?;
        StoreInstanceId::new(u128::from_le_bytes(raw))
            .map_err(|_| TreeError::Invalid("test overlay store identity"))
    }

    let artifact_id = ArtifactId::new(u128::MAX - 158)?;
    let identity = ArtifactIdentity {
        store,
        artifact: artifact_id,
        generation,
        creation_serial: u64::MAX - 158,
    };
    let blocks = [Block {
        kind: BlockKind::RetrievalVectorIndex,
        payload: image,
    }];
    let length = artifact::encoded_len(ContainerKind::Object, &blocks)?;
    let mut bytes = vec![0_u8; length];
    artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut bytes)?;
    let frame = artifact::decode(ContainerKind::Object, Some((store, artifact_id)), &bytes)?;
    let reference = frame.reference(0)?;
    let payload = PayloadRef::new(
        BlockKind::RetrievalVectorIndex,
        u64::try_from(image.len()).map_err(|_| TreeError::Memory)?,
        reference,
    )?;
    let overlay = Overlay {
        base: source,
        bytes,
        reference,
    };
    open_vector_index(
        &overlay, store, generation, payload, lexical, document, owner, resources,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn validate_physical_row_rewrite_for_test<'source, S, C>(
    source: &'source S,
    store: StoreInstanceId,
    generation: GraphGeneration,
    row_table: PayloadRef,
    rows: u32,
    catalog: &C,
    document: Option<&crate::epoch::EmbeddingTower>,
    index: &NativeVectorIndex<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError>
where
    S: BlockSource,
    C: RecordCatalog<S> + RecordCatalog<PhysicalRewriteOverlay<'source, S>>,
{
    use crate::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, Block, ContainerKind,
    };
    fn copied_object(
        store: StoreInstanceId,
        generation: GraphGeneration,
        artifact_value: u128,
        serial: u64,
        kind: BlockKind,
        payload: &[u8],
    ) -> Result<CopiedObject, TreeError> {
        let artifact_id = ArtifactId::new(artifact_value)?;
        let identity = ArtifactIdentity {
            store,
            artifact: artifact_id,
            generation,
            creation_serial: serial,
        };
        let blocks = [Block { kind, payload }];
        let mut bytes = vec![0_u8; artifact::encoded_len(ContainerKind::Object, &blocks)?];
        artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut bytes)?;
        let frame = artifact::decode(ContainerKind::Object, Some((store, artifact_id)), &bytes)?;
        let reference = frame.reference(0)?;
        Ok(CopiedObject { bytes, reference })
    }

    let table_length = usize::try_from(row_table.len()).map_err(|_| TreeError::Memory)?;
    let mut table = vec![0_u8; table_length];
    if PayloadSlice::new(source, store, generation, row_table).read_at(0, &mut table, resources)?
        != table_length
    {
        return Err(TreeError::Invalid("short physical rewrite row table"));
    }
    let first_bytes = table
        .get(..ROW_BYTES)
        .ok_or(TreeError::Invalid("physical rewrite first row"))?;
    let mut first = SparseRow::decode(first_bytes)?;
    let original_record = first.record.reference();
    let record_length = usize::try_from(first.record.len()).map_err(|_| TreeError::Memory)?;
    let mut record = vec![0_u8; record_length];
    if PayloadSlice::new(source, store, generation, first.record).read_at(
        0,
        &mut record,
        resources,
    )? != record_length
    {
        return Err(TreeError::Invalid("short physical rewrite record"));
    }
    let copied_record = copied_object(
        store,
        generation,
        u128::MAX - 159,
        u64::MAX - 159,
        BlockKind::NodeRecord,
        &record,
    )?;
    first.record = PayloadRef::new(
        BlockKind::NodeRecord,
        u64::try_from(record.len()).map_err(|_| TreeError::Memory)?,
        copied_record.reference,
    )?;
    first.encode(
        table
            .get_mut(..ROW_BYTES)
            .ok_or(TreeError::Invalid("physical rewrite row output"))?,
    )?;
    let copied_table = copied_object(
        store,
        generation,
        u128::MAX - 160,
        u64::MAX - 160,
        BlockKind::RetrievalRows,
        &table,
    )?;
    let copied_table_payload = PayloadRef::new(
        BlockKind::RetrievalRows,
        u64::try_from(table.len()).map_err(|_| TreeError::Memory)?,
        copied_table.reference,
    )?;
    if copied_table_payload.reference() == row_table.reference()
        || first.record.reference() == original_record
        || copied_table_payload.reference().version != 1
        || first.record.reference().version != 1
    {
        return Err(TreeError::Invalid("physical rewrite identity"));
    }
    let overlay = PhysicalRewriteOverlay {
        base: source,
        store,
        copied: [copied_record, copied_table],
    };
    validate_vector_index_rows(
        &overlay,
        store,
        generation,
        copied_table_payload,
        rows,
        catalog,
        document,
        index,
        resources,
    )
}

#[cfg(test)]
struct CopiedObject {
    bytes: Vec<u8>,
    reference: crate::property_graph::storage::artifact::PhysicalRef,
}

#[cfg(test)]
pub(super) struct PhysicalRewriteOverlay<'source, S> {
    base: &'source S,
    store: StoreInstanceId,
    copied: [CopiedObject; 2],
}

#[cfg(test)]
impl<S: BlockSource> BlockSource for PhysicalRewriteOverlay<'_, S> {
    fn resolve<'a>(
        &'a self,
        reference: crate::property_graph::storage::artifact::PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<crate::property_graph::storage::artifact::FramedBlock<'a>, TreeError> {
        use crate::property_graph::storage::artifact::{self, ContainerKind};

        let Some(object) = self
            .copied
            .iter()
            .find(|object| object.reference == reference)
        else {
            return self.base.resolve(reference, resources);
        };
        resources.step(1)?;
        let frame = artifact::decode(
            ContainerKind::Object,
            Some((self.store, reference.artifact)),
            &object.bytes,
        )?;
        Ok(frame.framed_block(reference)?)
    }
}

#[cfg(test)]
mod tests;
