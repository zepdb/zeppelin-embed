use super::view::{CatalogAllocation, SparseOwner};
use crate::format::FormatFamily;
use crate::fts::tokenizer::TokenizerEpoch;
use crate::property_graph::catalog::{CatalogError, CatalogImage, GraphInterpretation};
use crate::property_graph::storage::artifact::{self, ArtifactId, BlockKind, PhysicalRef};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::tree::directory::{BlockSource, TreeResources};
use crate::property_graph::storage::tree::directory::{DirectoryRoot, TreeError};
use crate::property_graph::wal::{ArtifactDescriptor, RequiredRef};
use crate::property_graph::{GraphGeneration, NodeId, StoreInstanceId};

pub(super) const ROOT_BYTES: usize = 256;
pub(super) const SOURCE_V1_BYTES: usize = 144;
pub(super) const SOURCE_V2_BYTES: usize = 200;
pub(super) const ROW_BYTES: usize = 80;
pub(super) const MEMBERSHIP_BYTES: usize = 48;
pub(super) const SOURCE_VALUE_BYTES: usize = 64;
const ROLE: u16 = 6;
const VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Modality {
    Text = 1,
    Vector = 2,
}

impl Modality {
    fn decode(value: u8) -> Result<Self, TreeError> {
        match value {
            1 => Ok(Self::Text),
            2 => Ok(Self::Vector),
            _ => Err(TreeError::Invalid("sparse modality")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SourceFormat {
    V1,
    V2,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SparseRoots {
    pub(crate) text: Option<RequiredRef>,
    pub(crate) vector: Option<RequiredRef>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SparsePhysicalRoots {
    pub(super) text: Option<PhysicalRef>,
    pub(super) vector: Option<PhysicalRef>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SparseRootState {
    pub(super) members: DirectoryRoot,
    pub(super) sources: DirectoryRoot,
    pub(super) live_rows: u64,
    pub(super) live_length: u64,
    pub(super) checkpoint: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MembershipRow {
    pub(super) revision: u64,
    pub(super) source: PhysicalRef,
    pub(super) row: u32,
}

impl MembershipRow {
    pub(super) fn encode(self, output: &mut [u8]) -> Result<(), TreeError> {
        if output.len() != MEMBERSHIP_BYTES || self.revision == 0 {
            return Err(TreeError::Invalid("sparse membership width or revision"));
        }
        output.fill(0);
        put(output, 0, &self.revision.to_le_bytes())?;
        artifact::encode_reference(self.source, range_mut(output, 8, 32)?)?;
        put(output, 40, &self.row.to_le_bytes())
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, TreeError> {
        if bytes.len() != MEMBERSHIP_BYTES || read_u32(bytes, 44)? != 0 {
            return Err(TreeError::Invalid("sparse membership width or reserved"));
        }
        let revision = read_u64(bytes, 0)?;
        let source = artifact::decode_reference(range(bytes, 8, 32)?)?;
        if revision == 0 || source.kind != BlockKind::CommitParticipant || source.version != 1 {
            return Err(TreeError::Invalid("sparse membership revision or source"));
        }
        Ok(Self {
            revision,
            source,
            row: read_u32(bytes, 40)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SourceValue {
    pub(super) mask: PayloadRef,
    pub(super) live_rows: u64,
    pub(super) live_length: u64,
}

impl SourceValue {
    pub(super) fn encode(self, output: &mut [u8]) -> Result<(), TreeError> {
        if output.len() != SOURCE_VALUE_BYTES || self.live_rows == 0 {
            return Err(TreeError::Invalid("sparse source value width or count"));
        }
        self.mask.encode_into(range_mut(output, 0, 48)?)?;
        put(output, 48, &self.live_rows.to_le_bytes())?;
        put(output, 56, &self.live_length.to_le_bytes())
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, TreeError> {
        if bytes.len() != SOURCE_VALUE_BYTES {
            return Err(TreeError::Invalid("sparse source value width"));
        }
        let value = Self {
            mask: PayloadRef::decode(range(bytes, 0, 48)?)?,
            live_rows: read_u64(bytes, 48)?,
            live_length: read_u64(bytes, 56)?,
        };
        if value.mask.role() != BlockKind::RetrievalLiveRows || value.live_rows == 0 {
            return Err(TreeError::Invalid("sparse source live mask or count"));
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SourceManifest {
    pub(super) format: SourceFormat,
    pub(super) modality: Modality,
    pub(super) generation: GraphGeneration,
    pub(super) sequence: u64,
    pub(super) rows: u32,
    pub(super) row_table: PayloadRef,
    pub(super) lexical: Option<PayloadRef>,
    pub(super) vector_index: Option<PayloadRef>,
}

impl SourceManifest {
    pub(super) fn encode(self, output: &mut [u8]) -> Result<(), TreeError> {
        let (version, width) = match self.format {
            SourceFormat::V1 => (1_u16, SOURCE_V1_BYTES),
            SourceFormat::V2 => (2_u16, SOURCE_V2_BYTES),
        };
        if output.len() != width || self.rows == 0 {
            return Err(TreeError::Invalid("sparse source width or rows"));
        }
        if self.row_table.role() != BlockKind::RetrievalRows
            || self.row_table.len() != u64::from(self.rows) * ROW_BYTES as u64
            || (self.modality == Modality::Text) != self.lexical.is_some()
            || self
                .lexical
                .is_some_and(|value| value.role() != BlockKind::RetrievalLexical)
            || (self.format == SourceFormat::V1 && self.vector_index.is_some())
            || (self.format == SourceFormat::V2
                && ((self.modality == Modality::Vector) != self.vector_index.is_some()))
            || self
                .vector_index
                .is_some_and(|value| value.role() != BlockKind::RetrievalVectorIndex)
        {
            return Err(TreeError::Invalid("sparse source geometry or modality"));
        }
        output.fill(0);
        put(output, 0, b"ZGCP")?;
        put(output, 4, &ROLE.to_le_bytes())?;
        put(output, 6, &version.to_le_bytes())?;
        put(output, 8, &[2])?;
        put(output, 9, &[self.modality as u8])?;
        put(output, 16, &self.generation.get().to_le_bytes())?;
        put(output, 24, &self.sequence.to_le_bytes())?;
        put(output, 32, &self.rows.to_le_bytes())?;
        self.row_table.encode_into(range_mut(output, 40, 48)?)?;
        if let Some(lexical) = self.lexical {
            put(output, 88, &[1])?;
            lexical.encode_into(range_mut(output, 96, 48)?)?;
        }
        if let Some(vector_index) = self.vector_index {
            put(output, 144, &[1])?;
            vector_index.encode_into(range_mut(output, 152, 48)?)?;
        }
        Ok(())
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, TreeError> {
        if bytes.get(..4) != Some(b"ZGCP".as_slice())
            || read_u16(bytes, 4)? != ROLE
            || read_u8(bytes, 8)? != 2
        {
            return Err(TreeError::Invalid("sparse source role or subtype"));
        }
        let format = match (read_u16(bytes, 6)?, bytes.len()) {
            (1, SOURCE_V1_BYTES) => SourceFormat::V1,
            (2, SOURCE_V2_BYTES) => SourceFormat::V2,
            _ => return Err(TreeError::Invalid("sparse source version or width")),
        };
        if range(bytes, 10, 6)?.iter().any(|byte| *byte != 0)
            || read_u32(bytes, 36)? != 0
            || range(bytes, 89, 7)?.iter().any(|byte| *byte != 0)
        {
            return Err(TreeError::Invalid("sparse source reserved or width"));
        }
        let modality = Modality::decode(read_u8(bytes, 9)?)?;
        let rows = read_u32(bytes, 32)?;
        let row_table = PayloadRef::decode(range(bytes, 40, 48)?)?;
        let lexical = match read_u8(bytes, 88)? {
            0 if range(bytes, 96, 48)?.iter().all(|byte| *byte == 0) => None,
            1 => Some(PayloadRef::decode(range(bytes, 96, 48)?)?),
            _ => return Err(TreeError::Invalid("sparse lexical presence")),
        };
        let vector_index = match format {
            SourceFormat::V1 => None,
            SourceFormat::V2 => {
                if range(bytes, 145, 7)?.iter().any(|byte| *byte != 0) {
                    return Err(TreeError::Invalid("sparse vector index reserved"));
                }
                match read_u8(bytes, 144)? {
                    0 if range(bytes, 152, 48)?.iter().all(|byte| *byte == 0) => None,
                    1 => Some(PayloadRef::decode(range(bytes, 152, 48)?)?),
                    _ => return Err(TreeError::Invalid("sparse vector index presence")),
                }
            }
        };
        if rows == 0
            || row_table.role() != BlockKind::RetrievalRows
            || row_table.len() != u64::from(rows) * ROW_BYTES as u64
            || (modality == Modality::Text) != lexical.is_some()
            || lexical.is_some_and(|value| value.role() != BlockKind::RetrievalLexical)
            || (format == SourceFormat::V2
                && ((modality == Modality::Vector) != vector_index.is_some()))
            || vector_index.is_some_and(|value| value.role() != BlockKind::RetrievalVectorIndex)
        {
            return Err(TreeError::Invalid("sparse source geometry or modality"));
        }
        Ok(Self {
            format,
            modality,
            generation: GraphGeneration::new(read_u64(bytes, 16)?),
            sequence: read_u64(bytes, 24)?,
            rows,
            row_table,
            lexical,
            vector_index,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SparseRow {
    pub(super) node: NodeId,
    pub(super) revision: u64,
    pub(super) record: PayloadRef,
    pub(super) analyzed_length: u32,
}

impl SparseRow {
    pub(super) fn encode(self, output: &mut [u8]) -> Result<(), TreeError> {
        if output.len() != ROW_BYTES || self.revision == 0 {
            return Err(TreeError::Invalid("sparse row width or revision"));
        }
        output.fill(0);
        put(output, 0, &self.node.get().to_le_bytes())?;
        put(output, 16, &self.revision.to_le_bytes())?;
        self.record.encode_into(range_mut(output, 24, 48)?)?;
        put(output, 72, &self.analyzed_length.to_le_bytes())
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, TreeError> {
        if bytes.len() != ROW_BYTES || read_u32(bytes, 76)? != 0 {
            return Err(TreeError::Invalid("sparse row width or reserved"));
        }
        let node = NodeId::new(read_u128(bytes, 0)?)
            .map_err(|_| TreeError::Invalid("zero sparse node"))?;
        let revision = read_u64(bytes, 16)?;
        let record = PayloadRef::decode(range(bytes, 24, 48)?)?;
        if revision == 0 || record.role() != BlockKind::NodeRecord {
            return Err(TreeError::Invalid("sparse row revision or record"));
        }
        Ok(Self {
            node,
            revision,
            record,
            analyzed_length: read_u32(bytes, 72)?,
        })
    }
}

pub(super) fn validate_row_correlation(
    expected_node: NodeId,
    member: MembershipRow,
    expected_source: PhysicalRef,
    expected_row: u32,
    row: SparseRow,
) -> Result<(), TreeError> {
    if member.source != expected_source
        || member.row != expected_row
        || row.node != expected_node
        || row.revision != member.revision
    {
        return Err(TreeError::Invalid("sparse membership/row correlation"));
    }
    Ok(())
}

pub(super) fn validate_catalog_interpretation<S: BlockSource>(
    source: &S,
    required: RequiredRef,
    store: StoreInstanceId,
    lexical: TokenizerEpoch,
    document: Option<&crate::epoch::EmbeddingTower>,
    owner: SparseOwner<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    source.with_block(required.block, resources, |block, resources| {
        owner.check(resources)?;
        let identity = block.identity();
        if block.reference() != required.block
            || required.object.family != FormatFamily::NativeGraphObject.id()
            || required.object.version != 1
            || identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || block.file_length() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
            || required.block.kind != BlockKind::CommitParticipant
        {
            return Err(TreeError::Invalid("sparse historical catalog descriptor"));
        }
        let payload = block.payload();
        if payload.get(..4) != Some(b"ZGCP".as_slice())
            || read_u16(payload, 4)? != 1
            || read_u16(payload, 6)? != 1
        {
            return Err(TreeError::Invalid(
                "sparse historical catalog role or version",
            ));
        }
        let encoded = range(payload, 8, payload.len().saturating_sub(8))?;
        let count = usize::try_from(read_u64(encoded, 104)?).map_err(|_| TreeError::Memory)?;
        let allowance = count
            .checked_mul(std::mem::size_of::<
                crate::property_graph::catalog::SymbolEntry<'_>,
            >())
            .ok_or(TreeError::Memory)?;
        let mut descriptors = owner.reserve(allowance)?;
        let mut callback_error = None;
        let image = CatalogImage::decode(encoded, allowance, &mut || {
            resources.step(1).map_err(|error| {
                if callback_error.is_none() {
                    callback_error = Some(error);
                }
                CatalogError::Cancelled
            })
        })
        .map_err(|error| match error {
            CatalogError::Cancelled => callback_error
                .take()
                .unwrap_or(TreeError::Invalid("historical catalog cancelled")),
            CatalogError::Capacity => owner.catalog_allocation_error(CatalogAllocation::Capacity),
            CatalogError::Allocation => {
                owner.catalog_allocation_error(CatalogAllocation::Allocation)
            }
            _ => TreeError::Invalid("invalid historical sparse catalog"),
        })?;
        descriptors.resize(image.symbols.allocated_bytes())?;
        let interpretation = GraphInterpretation::new(lexical, document)
            .map_err(|_| TreeError::Invalid("invalid sparse interpretation"))?;
        let mut callback_error = None;
        image
            .declaration
            .validate_for(store, interpretation, &mut || {
                resources.step(1).map_err(|error| {
                    if callback_error.is_none() {
                        callback_error = Some(error);
                    }
                    CatalogError::Cancelled
                })
            })
            .map_err(|error| match error {
                CatalogError::Cancelled => callback_error
                    .take()
                    .unwrap_or(TreeError::Invalid("historical catalog cancelled")),
                CatalogError::Capacity => {
                    owner.catalog_allocation_error(CatalogAllocation::Capacity)
                }
                CatalogError::Allocation => {
                    owner.catalog_allocation_error(CatalogAllocation::Allocation)
                }
                _ => TreeError::Invalid("historical sparse catalog interpretation"),
            })
    })
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RootDescriptor {
    pub(super) modality: Modality,
    pub(super) store: StoreInstanceId,
    pub(super) generation: GraphGeneration,
    pub(super) sequence: u64,
    pub(super) checkpoint: u64,
    pub(super) catalog: RequiredRef,
    pub(super) lexical: TokenizerEpoch,
    pub(super) members: DirectoryRoot,
    pub(super) sources: DirectoryRoot,
    pub(super) live_rows: u64,
    pub(super) live_length: u64,
}

impl RootDescriptor {
    pub(super) fn encode(self, output: &mut [u8]) -> Result<(), TreeError> {
        if output.len() != ROOT_BYTES
            || self.members.kind()
                != crate::property_graph::storage::tree::TreeKind::SparseMembership
            || self.sources.kind() != crate::property_graph::storage::tree::TreeKind::SparseSources
        {
            return Err(TreeError::Invalid("sparse root width or tree kind"));
        }
        output.fill(0);
        put(output, 0, b"ZGCP")?;
        put(output, 4, &ROLE.to_le_bytes())?;
        put(output, 6, &VERSION.to_le_bytes())?;
        put(output, 8, &[1])?;
        put(output, 9, &[self.modality as u8])?;
        put(output, 16, &self.store.get().to_le_bytes())?;
        put(output, 32, &self.generation.get().to_le_bytes())?;
        put(output, 40, &self.sequence.to_le_bytes())?;
        put(output, 48, &self.checkpoint.to_le_bytes())?;
        put(output, 56, &self.lexical.value().to_le_bytes())?;
        encode_required(self.catalog, range_mut(output, 64, 96)?)?;
        encode_optional(self.members.reference(), range_mut(output, 160, 40)?)?;
        encode_optional(self.sources.reference(), range_mut(output, 200, 40)?)?;
        put(output, 240, &self.live_rows.to_le_bytes())?;
        put(output, 248, &self.live_length.to_le_bytes())
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, TreeError> {
        participant_header(bytes, 1)?;
        if bytes.len() != ROOT_BYTES || range(bytes, 10, 6)?.iter().any(|byte| *byte != 0) {
            return Err(TreeError::Invalid("sparse root reserved or width"));
        }
        let modality = Modality::decode(read_u8(bytes, 9)?)?;
        let store = StoreInstanceId::new(read_u128(bytes, 16)?)
            .map_err(|_| TreeError::Invalid("zero sparse store"))?;
        let generation = GraphGeneration::new(read_u64(bytes, 32)?);
        let members = DirectoryRoot::from_reference(
            store,
            crate::property_graph::storage::tree::TreeKind::SparseMembership,
            generation,
            decode_optional(range(bytes, 160, 40)?)?,
        )?;
        let sources = DirectoryRoot::from_reference(
            store,
            crate::property_graph::storage::tree::TreeKind::SparseSources,
            generation,
            decode_optional(range(bytes, 200, 40)?)?,
        )?;
        let descriptor = Self {
            modality,
            store,
            generation,
            sequence: read_u64(bytes, 40)?,
            checkpoint: read_u64(bytes, 48)?,
            catalog: decode_required(range(bytes, 64, 96)?)?,
            lexical: TokenizerEpoch::from_value(read_u64(bytes, 56)?),
            members,
            sources,
            live_rows: read_u64(bytes, 240)?,
            live_length: read_u64(bytes, 248)?,
        };
        if descriptor.modality == Modality::Vector && descriptor.live_length != 0 {
            return Err(TreeError::Invalid("vector sparse length must be zero"));
        }
        Ok(descriptor)
    }
}

fn participant_header(bytes: &[u8], subtype: u8) -> Result<(), TreeError> {
    if bytes.get(..4) != Some(b"ZGCP".as_slice())
        || read_u16(bytes, 4)? != ROLE
        || read_u16(bytes, 6)? != VERSION
        || bytes.get(8).copied() != Some(subtype)
    {
        return Err(TreeError::Invalid(
            "sparse participant role version or subtype",
        ));
    }
    Ok(())
}

fn encode_optional(value: Option<PhysicalRef>, output: &mut [u8]) -> Result<(), TreeError> {
    if output.len() != 40 {
        return Err(TreeError::Invalid("sparse optional reference width"));
    }
    output.fill(0);
    if let Some(value) = value {
        put(output, 0, &[1])?;
        artifact::encode_reference(value, range_mut(output, 8, 32)?)?;
    }
    Ok(())
}

fn decode_optional(bytes: &[u8]) -> Result<Option<PhysicalRef>, TreeError> {
    if bytes.len() != 40 || range(bytes, 1, 7)?.iter().any(|byte| *byte != 0) {
        return Err(TreeError::Invalid("sparse optional reference reserved"));
    }
    match read_u8(bytes, 0)? {
        0 if range(bytes, 8, 32)?.iter().all(|byte| *byte == 0) => Ok(None),
        1 => Ok(Some(artifact::decode_reference(range(bytes, 8, 32)?)?)),
        _ => Err(TreeError::Invalid("sparse optional reference presence")),
    }
}

pub(super) fn encode_required(value: RequiredRef, output: &mut [u8]) -> Result<(), TreeError> {
    if output.len() != 96 {
        return Err(TreeError::Invalid("required reference width"));
    }
    put(output, 0, &value.object.store.get().to_le_bytes())?;
    put(output, 16, &value.object.artifact.get().to_le_bytes())?;
    put(output, 32, &value.object.generation.get().to_le_bytes())?;
    put(output, 40, &value.object.serial.to_le_bytes())?;
    put(output, 48, &value.object.bytes.to_le_bytes())?;
    put(output, 52, &value.object.family.to_le_bytes())?;
    put(output, 54, &value.object.version.to_le_bytes())?;
    put(output, 56, &value.object.checksum.to_le_bytes())?;
    artifact::encode_reference(value.block, range_mut(output, 64, 32)?)?;
    Ok(())
}

pub(super) fn decode_required(bytes: &[u8]) -> Result<RequiredRef, TreeError> {
    if bytes.len() != 96 {
        return Err(TreeError::Invalid("required reference width"));
    }
    Ok(RequiredRef {
        object: ArtifactDescriptor {
            store: StoreInstanceId::new(read_u128(bytes, 0)?)
                .map_err(|_| TreeError::Invalid("zero required store"))?,
            artifact: ArtifactId::new(read_u128(bytes, 16)?)?,
            generation: GraphGeneration::new(read_u64(bytes, 32)?),
            serial: read_u64(bytes, 40)?,
            bytes: read_u32(bytes, 48)?,
            family: read_u16(bytes, 52)?,
            version: read_u16(bytes, 54)?,
            checksum: read_u64(bytes, 56)?,
        },
        block: artifact::decode_reference(range(bytes, 64, 32)?)?,
    })
}

fn range(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8], TreeError> {
    bytes
        .get(offset..offset + length)
        .ok_or(TreeError::Invalid("sparse field extent"))
}
fn range_mut(bytes: &mut [u8], offset: usize, length: usize) -> Result<&mut [u8], TreeError> {
    bytes
        .get_mut(offset..offset + length)
        .ok_or(TreeError::Invalid("sparse field extent"))
}
fn put(bytes: &mut [u8], offset: usize, value: &[u8]) -> Result<(), TreeError> {
    range_mut(bytes, offset, value.len())?.copy_from_slice(value);
    Ok(())
}
fn read_u8(bytes: &[u8], offset: usize) -> Result<u8, TreeError> {
    range(bytes, offset, 1)?
        .first()
        .copied()
        .ok_or(TreeError::Invalid("u8"))
}
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, TreeError> {
    Ok(u16::from_le_bytes(
        range(bytes, offset, 2)?
            .try_into()
            .map_err(|_| TreeError::Invalid("u16"))?,
    ))
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, TreeError> {
    Ok(u32::from_le_bytes(
        range(bytes, offset, 4)?
            .try_into()
            .map_err(|_| TreeError::Invalid("u32"))?,
    ))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, TreeError> {
    Ok(u64::from_le_bytes(
        range(bytes, offset, 8)?
            .try_into()
            .map_err(|_| TreeError::Invalid("u64"))?,
    ))
}
fn read_u128(bytes: &[u8], offset: usize) -> Result<u128, TreeError> {
    Ok(u128::from_le_bytes(
        range(bytes, offset, 16)?
            .try_into()
            .map_err(|_| TreeError::Invalid("u128"))?,
    ))
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

    #[test]
    fn sparse_row_codec_preserves_full_identity_and_rejects_mismatch() {
        let low = NodeId::new(7).unwrap();
        let high = NodeId::new((1_u128 << 64) + 7).unwrap();
        assert_eq!(low.get() as u64, high.get() as u64);
        assert_ne!(low.get().to_le_bytes(), high.get().to_le_bytes());
        let source = PhysicalRef {
            artifact: ArtifactId::new(61).unwrap(),
            offset: 96,
            length: 168,
            kind: BlockKind::CommitParticipant,
            version: 1,
        };
        let record_reference = PhysicalRef {
            artifact: ArtifactId::new(62).unwrap(),
            offset: 96,
            length: 25,
            kind: BlockKind::NodeRecord,
            version: 1,
        };
        let record = PayloadRef::new(BlockKind::NodeRecord, 1, record_reference).unwrap();
        let member = MembershipRow {
            revision: 9,
            source,
            row: 3,
        };
        let row = SparseRow {
            node: high,
            revision: 9,
            record,
            analyzed_length: 2,
        };
        let mut member_bytes = [0_u8; MEMBERSHIP_BYTES];
        member.encode(&mut member_bytes).unwrap();
        let decoded_member = MembershipRow::decode(&member_bytes).unwrap();
        let mut row_bytes = [0_u8; ROW_BYTES];
        row.encode(&mut row_bytes).unwrap();
        let decoded_row = SparseRow::decode(&row_bytes).unwrap();
        validate_row_correlation(high, decoded_member, source, 3, decoded_row).unwrap();

        assert!(matches!(
            validate_row_correlation(low, decoded_member, source, 3, decoded_row),
            Err(TreeError::Invalid("sparse membership/row correlation"))
        ));
        let stale = MembershipRow {
            revision: 8,
            ..decoded_member
        };
        assert!(validate_row_correlation(high, stale, source, 3, decoded_row).is_err());
        assert!(validate_row_correlation(high, decoded_member, source, 2, decoded_row).is_err());

        let mut semantically_corrupt = row_bytes;
        semantically_corrupt
            .get_mut(0..16)
            .unwrap()
            .copy_from_slice(&low.get().to_le_bytes());
        let corrupt_row = SparseRow::decode(&semantically_corrupt).unwrap();
        assert!(validate_row_correlation(high, decoded_member, source, 3, corrupt_row).is_err());
    }
}
