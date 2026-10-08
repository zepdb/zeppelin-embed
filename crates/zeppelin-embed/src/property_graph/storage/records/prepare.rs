//! Charged symbol-index construction and bounded native record emission.
use super::*;
use crate::property_graph::catalog::{Symbol, SymbolKind};
use crate::property_graph::storage::artifact::put;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::payload::{PayloadRef, prepare_stream};
use crate::property_graph::storage::tree::directory::BlockSink;
use crate::property_graph::{EntityId, GraphGeneration, GraphRevision, StoreInstanceId};

/// Already normalized logical record input. Referenced bytes and all duplicated
/// metadata are still fully checked before a prepared record is returned.
#[derive(Clone, Copy, Debug)]
pub struct RecordInput {
    /// Same immutable base/preparation store as the sink.
    pub store: StoreInstanceId,
    /// Preparation's target generation.
    pub generation: GraphGeneration,
    /// Full identity supplied by the logical allocator/classifier.
    pub entity: EntityId,
    /// Required exact canonical image.
    pub canonical: PayloadRef,
    /// Required complete installing operation evidence.
    pub provenance: PayloadRef,
}

/// Derive sorted checked symbols while preserving the complete canonical stream.
/// Scratch, descriptor arrays and the sink must share this preparation owner.
/// Failure may retain private sink objects for its explicit abort inventory;
/// no root publication or cleanup authority is created here.
pub fn prepare_record<S: BlockSink>(
    sink: &mut S,
    input: RecordInput,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    memory: &StorageMemory<'_>,
    r: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    prepare_record_bound(sink, input, catalog, document, None, memory, r)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_record_bound<S: BlockSink>(
    sink: &mut S,
    input: RecordInput,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    version: Option<crate::ingest::DocumentVersion>,
    memory: &StorageMemory<'_>,
    r: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    r.require_preparation(memory)?;
    if let Some(version) = version
        && input.entity != EntityId::Node(crate::property_graph::NodeId::from(version.doc_id()))
    {
        return Err(TreeError::Invalid("document node version"));
    }
    let (shape, revision, properties) = {
        let source = PayloadSlice::new(&*sink, input.store, input.generation, input.canonical);
        let canonical = verify_canonical(source, document, &mut CountOnly, r)?;
        let shape = match (input.entity, canonical.shape()) {
            (EntityId::Node(id), CanonicalShape::Node { labels }) => RecordShape::Node {
                id,
                labels: u32::try_from(*labels).map_err(|_| TreeError::Memory)?,
            },
            (
                EntityId::Relationship(id),
                CanonicalShape::Relationship {
                    source,
                    target,
                    relationship_type,
                },
            ) => {
                let Symbol::RelationshipType(relationship_type) =
                    catalog.resolve(SymbolKind::RelationshipType, *relationship_type, r)?
                else {
                    return Err(TreeError::Invalid("catalog relationship type"));
                };
                RecordShape::Relationship {
                    id,
                    source: *source,
                    target: *target,
                    relationship_type,
                }
            }
            _ => return Err(TreeError::Invalid("prepared record entity kind")),
        };
        let provenance = verify_provenance(
            PayloadSlice::new(&*sink, input.store, input.generation, input.provenance),
            r,
        )?;
        (
            shape,
            provenance.installed_revision(),
            usize::try_from(canonical.property_count()).map_err(|_| TreeError::Memory)?,
        )
    };
    let label_count = match shape {
        RecordShape::Node { labels, .. } => labels as usize,
        _ => 0,
    };
    let mut labels = StorageBuffer::<u64>::new(memory, label_count)?;
    let mut properties = StorageBuffer::<IndexRow>::new(memory, properties)?;
    {
        let mut collect = CollectIndex {
            catalog,
            labels: &mut labels,
            properties: &mut properties,
        };
        verify_canonical(
            PayloadSlice::new(&*sink, input.store, input.generation, input.canonical),
            document,
            &mut collect,
            r,
        )?;
    }
    sort_by_symbol(labels.as_mut_slice(), |id| *id, r)?;
    sort_by_symbol(properties.as_mut_slice(), |row| row.key, r)?;
    unique(labels.as_slice(), |id| *id, r)?;
    unique(properties.as_slice(), |row| row.key, r)?;
    let encoding = RecordEncoding::new(
        shape,
        revision,
        labels.as_slice(),
        properties.as_slice(),
        input.canonical,
        input.provenance,
        version,
    )?;
    let result = prepare_stream(
        sink,
        input.store,
        input.generation,
        encoding.role(),
        encoding.length()?,
        &mut |offset, output, r| encoding.read_at(offset, output, r),
        r,
    )?;
    verify_record(
        PayloadSlice::new(&*sink, input.store, input.generation, result),
        input.entity,
        catalog,
        document,
        r,
    )?;
    r.step(0)?;
    Ok(result)
}
struct CountOnly;
impl<S: BlockSource> CanonicalVisitor<S> for CountOnly {
    fn label(
        &mut self,
        _: PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        r.step(1)
    }
    fn property(
        &mut self,
        _: PayloadSlice<'_, S>,
        _: StoredProperty<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        r.step(1)
    }
}
#[derive(Clone, Copy)]
struct IndexRow {
    key: u64,
    offset: u64,
    length: u64,
}
struct CollectIndex<'a, 'm, C> {
    catalog: &'a C,
    labels: &'a mut StorageBuffer<'m, u64>,
    properties: &'a mut StorageBuffer<'m, IndexRow>,
}
impl<S: BlockSource, C: RecordCatalog<S>> CanonicalVisitor<S> for CollectIndex<'_, '_, C> {
    fn label(
        &mut self,
        name: PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let Symbol::Label(id) = self.catalog.resolve(SymbolKind::Label, name, r)? else {
            return Err(TreeError::Invalid("catalog label kind"));
        };
        self.labels.push(id.get())
    }
    fn property(
        &mut self,
        name: PayloadSlice<'_, S>,
        value: StoredProperty<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let Symbol::Property(id) = self.catalog.resolve(SymbolKind::Property, name, r)? else {
            return Err(TreeError::Invalid("catalog property kind"));
        };
        self.properties.push(IndexRow {
            key: id.get(),
            offset: value.offset(),
            length: value.encoded().len(),
        })
    }
}
fn unique<T>(
    items: &[T],
    key: impl Fn(&T) -> u64,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut previous = 0;
    for item in items {
        r.step(1)?;
        let current = key(item);
        if current <= previous {
            return Err(TreeError::Invalid("catalog symbols are not bijective"));
        }
        previous = current;
    }
    Ok(())
}
pub(crate) fn sort_by_symbol<T, K: Ord>(
    items: &mut [T],
    key: impl Fn(&T) -> K,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for start in (0..items.len() / 2).rev() {
        sift(items, start, items.len(), &key, r)?;
    }
    for end in (1..items.len()).rev() {
        swap(items, 0, end, r)?;
        sift(items, 0, end, &key, r)?;
    }
    r.step(0)
}
fn sift<T, K: Ord>(
    items: &mut [T],
    mut root: usize,
    end: usize,
    key: &impl Fn(&T) -> K,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    loop {
        r.step(1)?;
        let Some(mut child) = root
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .filter(|n| *n < end)
        else {
            return Ok(());
        };
        if child + 1 < end
            && key(items.get(child).ok_or(TreeError::Memory)?)
                < key(items.get(child + 1).ok_or(TreeError::Memory)?)
        {
            child += 1;
        }
        if key(items.get(root).ok_or(TreeError::Memory)?)
            >= key(items.get(child).ok_or(TreeError::Memory)?)
        {
            return Ok(());
        }
        swap(items, root, child, r)?;
        root = child;
    }
}
fn swap<T>(
    items: &mut [T],
    left: usize,
    right: usize,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    r.step(1)?;
    let [left, right] = items
        .get_disjoint_mut([left, right])
        .map_err(|_| TreeError::Invalid("index sort bounds"))?;
    std::mem::swap(left, right);
    Ok(())
}
struct RecordEncoding<'a> {
    header: [u8; 80],
    header_len: usize,
    labels: &'a [u64],
    properties: &'a [IndexRow],
    index_header: [u8; 8],
    references: [u8; 120],
    references_len: usize,
}
impl<'a> RecordEncoding<'a> {
    fn new(
        shape: RecordShape,
        revision: GraphRevision,
        labels: &'a [u64],
        properties: &'a [IndexRow],
        canonical: PayloadRef,
        provenance: PayloadRef,
        version: Option<crate::ingest::DocumentVersion>,
    ) -> Result<Self, TreeError> {
        let mut result = Self {
            header: [0; 80],
            header_len: 0,
            labels,
            properties,
            index_header: [0; 8],
            references: [0; 120],
            references_len: if version.is_some() { 120 } else { 96 },
        };
        let property_bytes = 8usize
            .checked_add(properties.len().checked_mul(24).ok_or(TreeError::Memory)?)
            .ok_or(TreeError::Memory)?;
        match shape {
            RecordShape::Node { id, labels } => {
                result.header_len = 40;
                put(&mut result.header, 0, &id.get().to_le_bytes())?;
                put(&mut result.header, 16, &revision.get().to_le_bytes())?;
                put(&mut result.header, 28, &labels.to_le_bytes())?;
                put(
                    &mut result.header,
                    32,
                    &(property_bytes as u64).to_le_bytes(),
                )?;
            }
            RecordShape::Relationship {
                id,
                source,
                target,
                relationship_type,
            } => {
                result.header_len = 80;
                put(&mut result.header, 0, &id.get().to_le_bytes())?;
                put(&mut result.header, 16, &source.get().to_le_bytes())?;
                put(&mut result.header, 32, &target.get().to_le_bytes())?;
                put(
                    &mut result.header,
                    48,
                    &relationship_type.get().to_le_bytes(),
                )?;
                put(&mut result.header, 56, &revision.get().to_le_bytes())?;
                put(
                    &mut result.header,
                    72,
                    &(property_bytes as u64).to_le_bytes(),
                )?;
            }
        }
        put(
            &mut result.index_header,
            0,
            &u32::try_from(properties.len())
                .map_err(|_| TreeError::Memory)?
                .to_le_bytes(),
        )?;
        canonical.encode_into(result.references.get_mut(..48).ok_or(TreeError::Memory)?)?;
        provenance.encode_into(result.references.get_mut(48..96).ok_or(TreeError::Memory)?)?;
        if let Some(version) = version {
            put(&mut result.header, 24, &2_u32.to_le_bytes())?;
            put(
                &mut result.references,
                96,
                &version.doc_id().get().to_le_bytes(),
            )?;
            put(
                &mut result.references,
                112,
                &version.revision().get().to_le_bytes(),
            )?;
        }
        Ok(result)
    }
    fn role(&self) -> BlockKind {
        if self.header_len == 40 {
            BlockKind::NodeRecord
        } else {
            BlockKind::RelRecord
        }
    }
    fn length(&self) -> Result<usize, TreeError> {
        self.header_len
            .checked_add(self.labels.len().checked_mul(8).ok_or(TreeError::Memory)?)
            .and_then(|n| n.checked_add(8))
            .and_then(|n| n.checked_add(self.properties.len().checked_mul(24)?))
            .and_then(|n| n.checked_add(self.references_len))
            .ok_or(TreeError::Memory)
    }
    fn read_at(
        &self,
        offset: u64,
        output: &mut [u8],
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let mut position = usize::try_from(offset).map_err(|_| TreeError::Memory)?;
        let mut copied = 0;
        let label_end = self.header_len + self.labels.len() * 8;
        let index_start = label_end + 8;
        let references_start = index_start + self.properties.len() * 24;
        while copied < output.len() {
            let mut temporary = [0; 24];
            let part = if position < self.header_len {
                self.header
                    .get(position..self.header_len)
                    .ok_or(TreeError::Memory)?
            } else if position < label_end {
                let offset = position - self.header_len;
                put(
                    &mut temporary,
                    0,
                    &self
                        .labels
                        .get(offset / 8)
                        .ok_or(TreeError::Memory)?
                        .to_le_bytes(),
                )?;
                temporary.get(offset % 8..8).ok_or(TreeError::Memory)?
            } else if position < index_start {
                self.index_header
                    .get(position - label_end..)
                    .ok_or(TreeError::Memory)?
            } else if position < references_start {
                let offset = position - index_start;
                let row = self.properties.get(offset / 24).ok_or(TreeError::Memory)?;
                put(&mut temporary, 0, &row.key.to_le_bytes())?;
                put(&mut temporary, 8, &row.offset.to_le_bytes())?;
                put(&mut temporary, 16, &row.length.to_le_bytes())?;
                temporary.get(offset % 24..).ok_or(TreeError::Memory)?
            } else {
                self.references
                    .get(position - references_start..self.references_len)
                    .filter(|bytes| !bytes.is_empty())
                    .ok_or(TreeError::Memory)?
            };
            let count = part.len().min(output.len() - copied);
            r.step(count as u64)?;
            output
                .get_mut(copied..copied + count)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(part.get(..count).ok_or(TreeError::Memory)?);
            copied += count;
            position += count;
        }
        Ok(())
    }
}
