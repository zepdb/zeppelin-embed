//! Permanent kind-scoped key history; physical relocation never rewrites provenance.
use super::*;
use crate::property_graph::catalog::{NamespaceId, Symbol, SymbolKind};
use crate::property_graph::storage::artifact::put;
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::tree::directory::{DirectoryEntry, DirectoryRoot, FenceKey};
use crate::property_graph::{
    EntityId, EntityKind, GraphGeneration, GraphRevision, RelId, StoreInstanceId,
};

/// Preparation inputs; installing revision/incarnation are derived from provenance.
#[derive(Clone, Copy)]
pub struct FenceInput<'a> {
    /// Exact store owner of all referenced streams.
    pub store: StoreInstanceId,
    /// Creation generation of the replacement leaf.
    pub generation: GraphGeneration,
    /// Exact logical key under the retained catalog.
    pub key: FenceKey<'a>,
    /// Complete installing operation, including key and original changed generation.
    pub provenance: PayloadRef,
    /// Required for live entries, forbidden for deleted entries.
    pub canonical: Option<PayloadRef>,
}
/// Checked permanent key ledger state. This is independent of entity liveness
/// and cannot authorize endpoint visibility or recycling a deleted incarnation.
pub struct FenceView<'a, S: BlockSource> {
    incarnation: EntityId,
    revision: GraphRevision,
    provenance: StoredProvenance<'a, S>,
    provenance_ref: PayloadRef,
    canonical: Option<CanonicalView<'a, S>>,
    canonical_bytes: Option<PayloadSlice<'a, S>>,
    canonical_ref: Option<PayloadRef>,
}
impl<'a, S: BlockSource> FenceView<'a, S> {
    /// Full original/current incarnation, even when deleted.
    pub const fn incarnation(&self) -> EntityId {
        self.incarnation
    }
    /// Explicit installed live/deletion revision.
    pub const fn revision(&self) -> GraphRevision {
        self.revision
    }
    /// Whether only durable deletion evidence remains.
    pub const fn is_deleted(&self) -> bool {
        self.canonical.is_none()
    }
    /// Complete original operation, including precondition and delete mode.
    pub const fn provenance(&self) -> &StoredProvenance<'a, S> {
        &self.provenance
    }
    /// Fully verified live contents; deleted entries carry no canonical image.
    pub const fn canonical(&self) -> Option<&CanonicalView<'a, S>> {
        self.canonical.as_ref()
    }
    /// Exact live canonical bytes for replay comparison, without a whole-image copy.
    pub const fn canonical_bytes(&self) -> Option<PayloadSlice<'a, S>> {
        self.canonical_bytes
    }
    /// Authentic topology of a physically swept, still nondeleted fence.
    /// Both endpoints are checked through the same immutable roots.
    pub(crate) fn hidden_relationship(
        &self,
        source: &S,
        roots: super::super::tree::directory::GraphRoots,
        catalog: &impl RecordCatalog<S>,
        document: Option<&EmbeddingTower>,
        r: &mut TreeResources<'_>,
    ) -> Result<super::super::adjacency::RelationshipRow, TreeError> {
        let EntityId::Relationship(rel) = self.incarnation else {
            return Err(TreeError::Invalid("swept fence is not a relationship"));
        };
        let Some(canonical) = self.canonical() else {
            return Err(TreeError::Invalid("swept fence is deleted"));
        };
        let CanonicalShape::Relationship {
            source: from,
            target,
            relationship_type,
        } = canonical.shape()
        else {
            return Err(TreeError::Invalid("swept fence canonical role"));
        };
        let from_live = endpoint_present(source, roots, *from, catalog, document, r)?;
        let target_live = endpoint_present(source, roots, *target, catalog, document, r)?;
        if from_live && target_live {
            return Err(TreeError::Invalid(
                "missing relationship has two live endpoints",
            ));
        }
        let Symbol::RelationshipType(relationship_type) =
            catalog.resolve(SymbolKind::RelationshipType, *relationship_type, r)?
        else {
            return Err(TreeError::Invalid("swept fence relationship type"));
        };
        Ok(super::super::adjacency::RelationshipRow {
            rel,
            source: *from,
            target: *target,
            relationship_type,
        })
    }

    pub(crate) const fn required_payloads(&self) -> (PayloadRef, Option<PayloadRef>) {
        (self.provenance_ref, self.canonical_ref)
    }
}

/// Select the authenticated provenance artifact used to bound one fence
/// semantic read window. Full fence validation still owns every field check.
pub(crate) fn fence_window_reference(entry: DirectoryEntry<'_>) -> Result<PayloadRef, TreeError> {
    if entry.value().len() != 144 {
        return Err(TreeError::Invalid("fence window value extent"));
    }
    PayloadRef::decode(
        entry
            .value()
            .get(40..88)
            .ok_or(TreeError::Invalid("fence window provenance extent"))?,
    )
}
/// Validate exact144-byte layout, its descendants, and the actual leaf key.
/// A later root generation never relaxes this leaf's descendant generation bound.
pub fn verify_fence_entry<'a, S: BlockSource>(
    source: &'a S,
    root: DirectoryRoot,
    entry: DirectoryEntry<'a>,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<FenceView<'a, S>, TreeError> {
    entry.require_root(root)?;
    if root.kind() != super::super::tree::TreeKind::KeyFences
        || entry.creation_generation() > root.generation()
    {
        return Err(TreeError::Invalid("fence tree context"));
    }
    let fence = verify_value(
        source,
        root.store(),
        entry.creation_generation(),
        entry.value(),
        catalog,
        document,
        r,
    )?;
    let key = fence
        .provenance
        .key()
        .ok_or(TreeError::Invalid("unkeyed fence"))?;
    let namespace = namespace(catalog, key, r)?;
    if !entry.matches_fence(source, root, key.kind(), namespace, key.key(), r)? {
        return Err(TreeError::Invalid("fence key/provenance mismatch"));
    }
    r.step(0)?;
    Ok(fence)
}
/// Produce one compact private value only after whole provenance/canonical checks.
/// The directory participant retains this value through deletion of all live rows.
pub fn prepare_fence<S: BlockSource>(
    source: &S,
    input: FenceInput<'_>,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<[u8; 144], TreeError> {
    let provenance = verify_provenance(
        PayloadSlice::new(source, input.store, input.generation, input.provenance),
        r,
    )?;
    let key = provenance
        .key()
        .ok_or(TreeError::Invalid("unkeyed fence"))?;
    if key.kind() != input.key.kind()
        || namespace(catalog, key, r)? != input.key.namespace()
        || !key
            .key()
            .compare_bytes(input.key.text().as_bytes(), r)?
            .is_eq()
    {
        return Err(TreeError::Invalid("prepared fence key mismatch"));
    }
    let mut bytes = [0; 144];
    r.step(bytes.len() as u64)?;
    put(&mut bytes, 0, &1u16.to_le_bytes())?;
    let (kind, id) = match provenance.incarnation() {
        EntityId::Node(id) => (1, id.get()),
        EntityId::Relationship(id) => (2, id.get()),
    };
    put(
        &mut bytes,
        2,
        &[kind, if input.canonical.is_some() { 1 } else { 2 }],
    )?;
    put(&mut bytes, 8, &id.to_le_bytes())?;
    put(
        &mut bytes,
        24,
        &provenance.installed_revision().get().to_le_bytes(),
    )?;
    put(
        &mut bytes,
        32,
        &provenance.original_generation().get().to_le_bytes(),
    )?;
    input
        .provenance
        .encode_into(bytes.get_mut(40..88).ok_or(TreeError::Memory)?)?;
    if let Some(canonical) = input.canonical {
        put(&mut bytes, 88, &[1])?;
        canonical.encode_into(bytes.get_mut(96..144).ok_or(TreeError::Memory)?)?;
    }
    verify_value(
        source,
        input.store,
        input.generation,
        &bytes,
        catalog,
        document,
        r,
    )?;
    r.step(0)?;
    Ok(bytes)
}
fn namespace<S: BlockSource>(
    catalog: &impl RecordCatalog<S>,
    key: StoredKey<'_, S>,
    r: &mut TreeResources<'_>,
) -> Result<NamespaceId, TreeError> {
    match catalog.resolve(SymbolKind::Namespace, key.namespace(), r)? {
        Symbol::Namespace(id) => Ok(id),
        _ => Err(TreeError::Invalid("namespace catalog domain")),
    }
}
fn verify_value<'a, S: BlockSource>(
    source: &'a S,
    store: StoreInstanceId,
    generation: GraphGeneration,
    bytes: &[u8],
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<FenceView<'a, S>, TreeError> {
    r.step(144)?;
    if bytes.len() != 144
        || u16::from_le_bytes(read(bytes, 0)?) != 1
        || read::<4>(bytes, 4)? != [0; 4]
        || read::<7>(bytes, 89)? != [0; 7]
    {
        return Err(TreeError::Invalid(
            "fence version, reserved bytes or length",
        ));
    }
    let kind = match read::<1>(bytes, 2)? {
        [1] => EntityKind::Node,
        [2] => EntityKind::Relationship,
        _ => return Err(TreeError::Invalid("fence kind")),
    };
    let deleted = match read::<1>(bytes, 3)? {
        [1] => false,
        [2] => true,
        _ => return Err(TreeError::Invalid("fence state")),
    };
    let raw = u128::from_le_bytes(read(bytes, 8)?);
    let incarnation = match kind {
        EntityKind::Node => EntityId::Node(NodeId::from(crate::ingest::DocId::new(raw))),
        EntityKind::Relationship => EntityId::Relationship(
            RelId::new(raw).map_err(|_| TreeError::Invalid("zero fence relationship"))?,
        ),
    };
    let revision = GraphRevision::new(u64::from_le_bytes(read(bytes, 24)?))
        .map_err(|_| TreeError::Invalid("zero fence revision"))?;
    let original_generation = GraphGeneration::new(u64::from_le_bytes(read(bytes, 32)?));
    let provenance_ref = PayloadRef::decode(
        bytes
            .get(40..88)
            .ok_or(TreeError::Invalid("fence provenance extent"))?,
    )?;
    let provenance = verify_provenance(
        PayloadSlice::new(source, store, generation, provenance_ref),
        r,
    )?;
    native::validate_installing_provenance(
        &provenance,
        incarnation,
        revision,
        generation,
        deleted,
    )?;
    if provenance.original_generation() != original_generation
        || provenance.key().is_none_or(|key| key.kind() != kind)
    {
        return Err(TreeError::Invalid("fence provenance identity"));
    }
    let canonical_ref = match (read::<1>(bytes, 88)?, deleted) {
        ([0], true) if read::<48>(bytes, 96)? == [0; 48] => None,
        ([1], false) => Some(PayloadRef::decode(
            bytes
                .get(96..144)
                .ok_or(TreeError::Invalid("fence canonical extent"))?,
        )?),
        _ => return Err(TreeError::Invalid("fence canonical presence")),
    };
    let canonical_bytes =
        canonical_ref.map(|reference| PayloadSlice::new(source, store, generation, reference));
    let canonical = match canonical_bytes {
        Some(bytes) => {
            let image = verify_canonical(bytes, document, &mut CatalogVisitor(catalog), r)?;
            match (image.shape(), kind) {
                (CanonicalShape::Node { .. }, EntityKind::Node) => {}
                (
                    CanonicalShape::Relationship {
                        relationship_type, ..
                    },
                    EntityKind::Relationship,
                ) => {
                    if catalog
                        .resolve(SymbolKind::RelationshipType, *relationship_type, r)?
                        .kind()
                        != SymbolKind::RelationshipType
                    {
                        return Err(TreeError::Invalid("fence relationship type domain"));
                    }
                }
                _ => return Err(TreeError::Invalid("fence canonical entity kind")),
            }
            Some(image)
        }
        None => None,
    };
    r.step(0)?;
    Ok(FenceView {
        incarnation,
        revision,
        provenance,
        provenance_ref,
        canonical,
        canonical_bytes,
        canonical_ref,
    })
}
struct CatalogVisitor<'a, C>(&'a C);
impl<S: BlockSource, C: RecordCatalog<S>> CanonicalVisitor<S> for CatalogVisitor<'_, C> {
    fn label(
        &mut self,
        name: PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if self.0.resolve(SymbolKind::Label, name, r)?.kind() != SymbolKind::Label {
            return Err(TreeError::Invalid("fence label domain"));
        }
        Ok(())
    }
    fn property(
        &mut self,
        name: PayloadSlice<'_, S>,
        _value: StoredProperty<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if self.0.resolve(SymbolKind::Property, name, r)?.kind() != SymbolKind::Property {
            return Err(TreeError::Invalid("fence property domain"));
        }
        Ok(())
    }
}

fn read<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], TreeError> {
    let end = offset
        .checked_add(N)
        .ok_or(TreeError::Invalid("fence offset overflow"))?;
    bytes
        .get(offset..end)
        .and_then(|part| part.try_into().ok())
        .ok_or(TreeError::Invalid("truncated fence field"))
}

/// Physical endpoint liveness for replay evidence only; absent is nonlive.
pub(crate) fn endpoint_present<S: BlockSource>(
    source: &S,
    roots: super::super::tree::directory::GraphRoots,
    node: NodeId,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(matches!(
        super::super::view::lookup_node_state(source, roots, node, catalog, document, r)?,
        Some(NodeRecordState::Live(_))
    ))
}

/// Verify the only missing ExpectedEntity transition: a structured keyed Delete
/// of an independently authenticated swept relationship.
#[allow(clippy::too_many_arguments)]
pub(crate) fn swept_delete<S: BlockSource>(
    source: &S,
    roots: super::super::tree::directory::GraphRoots,
    fields: crate::property_graph::OperationFields<'_>,
    namespace: NamespaceId,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<super::super::adjacency::RelationshipRow, TreeError> {
    use super::super::tree::TreeKind;
    use crate::property_graph::{ExpectedGraphState, GraphOperation};
    if fields.operation != GraphOperation::StructuredDelete
        || fields.expected != ExpectedGraphState::Entity(fields.incarnation)
        || fields.delete_mode.is_none()
    {
        return Err(TreeError::Invalid(
            "missing entity is not a swept structured Delete",
        ));
    }
    let key = fields
        .key
        .ok_or(TreeError::Invalid("swept Delete lacks key"))?;
    let root = roots.directory(TreeKind::KeyFences)?;
    let probe = FenceKey::new(key.kind(), namespace, key.key().as_str())?;
    let entry = super::super::tree::directory::lookup_fence_entry(source, root, probe, r)?
        .ok_or(TreeError::Invalid("swept Delete prior fence absent"))?;
    let fence = verify_fence_entry(source, root, entry, catalog, document, r)?;
    let prior = fence.provenance().fields_with_key(Some(key), r)?;
    if prior.incarnation != fields.incarnation
        || prior.installed_revision >= fields.installed_revision
        || fields.requested_revision != fields.installed_revision
    {
        return Err(TreeError::Invalid(
            "swept Delete prior incarnation/revision",
        ));
    }
    fence.hidden_relationship(source, roots, catalog, document, r)
}
