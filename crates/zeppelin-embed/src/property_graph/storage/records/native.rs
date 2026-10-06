use super::*;
use crate::property_graph::catalog::{LabelId, PropertyKeyId, RelTypeId, Symbol, SymbolKind};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::tree::directory::NativeReadEvent;
use crate::property_graph::{EntityId, ExpectedGraphState, GraphOperation, GraphRevision, RelId};

/// A retained admitted catalog's exact, bijective name-to-symbol lookup. The
/// adapter must use the same store/view as the record source and reject missing
/// names. Implementations must not intern names or invent symbols during reads.
pub trait RecordCatalog<S: BlockSource> {
    /// Resolve one exact validated UTF-8 name in its separate symbol domain.
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, S>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError>;
}

/// Native record identity and fixed topology, correlated with canonical bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordShape {
    /// Node with its full-width identity and complete label count.
    Node {
        /// Full-width store-local node identity.
        id: NodeId,
        /// Complete unique symbolized label count.
        labels: u32,
    },
    /// Directed relationship with one immutable type and endpoints.
    Relationship {
        /// Full-width store-local relationship identity.
        id: RelId,
        /// Directed source identity.
        source: NodeId,
        /// Directed target identity.
        target: NodeId,
        /// Single relationship type in the admitted catalog.
        relationship_type: RelTypeId,
    },
}
impl RecordShape {
    /// Full-width identity preserving the node/relationship domain.
    pub const fn incarnation(self) -> EntityId {
        match self {
            Self::Node { id, .. } => EntityId::Node(id),
            Self::Relationship { id, .. } => EntityId::Relationship(id),
        }
    }
}

/// A completely correlated live native record under its immutable source.
/// This is not store-wide endpoint liveness or GraphReadView admission proof.
pub struct RecordView<'a, S: BlockSource> {
    shape: RecordShape,
    revision: GraphRevision,
    labels: PayloadSlice<'a, S>,
    properties: PayloadSlice<'a, S>,
    canonical_bytes: PayloadSlice<'a, S>,
    canonical_ref: PayloadRef,
    canonical: CanonicalView<'a, S>,
    provenance_ref: PayloadRef,
    provenance: StoredProvenance<'a, S>,
}
impl<'a, S: BlockSource> RecordView<'a, S> {
    /// Correlated native identity and topology.
    pub const fn shape(&self) -> RecordShape {
        self.shape
    }
    /// Full-width record identity, checked against the directory key.
    pub const fn incarnation(&self) -> EntityId {
        self.shape.incarnation()
    }
    /// Correlated installed revision.
    pub const fn revision(&self) -> GraphRevision {
        self.revision
    }
    /// Complete checked canonical contents and optional original payloads.
    pub const fn canonical(&self) -> &CanonicalView<'a, S> {
        &self.canonical
    }
    /// Exact checked canonical bytes, retaining the same immutable source.
    pub const fn canonical_bytes(&self) -> PayloadSlice<'a, S> {
        self.canonical_bytes
    }
    /// Complete checked installing provenance retaining exact key readers.
    pub const fn provenance(&self) -> &StoredProvenance<'a, S> {
        &self.provenance
    }
    /// Physical payload descendants already decoded and verified for this record.
    pub(crate) const fn required_payloads(&self) -> [PayloadRef; 2] {
        [self.canonical_ref, self.provenance_ref]
    }
    /// Read a complete label by index without materializing the label array.
    pub fn label(&self, index: u32, r: &mut TreeResources<'_>) -> Result<LabelId, TreeError> {
        let mut c =
            PayloadCursor::new_with_resources(self.labels.subslice(u64::from(index) * 8, 8)?, r)?;
        LabelId::new(u64::from_le_bytes(c.read_array(r)?))
            .map_err(|_| TreeError::Invalid("zero native label"))
    }
    /// Point lookup returns the exact canonical value encoding, retaining its
    /// original tag and scalar/list bits. An absent property returns None.
    pub fn property(
        &self,
        key: PropertyKeyId,
        r: &mut TreeResources<'_>,
    ) -> Result<Option<PayloadSlice<'a, S>>, TreeError> {
        let Some(row) = find_property(self.properties, key.get(), r)? else {
            return Ok(None);
        };
        let value = self.canonical_bytes.subslice(row.offset, row.length)?;
        r.read_event(NativeReadEvent::PropertyValue(row.length))?;
        Ok(Some(value))
    }
    /// Enumerates one verified native property without reconstructing the
    /// canonical record. The checked index and correlated key/payload extent
    /// come from the record's already validated property table.
    pub fn property_at(
        &self,
        index: u64,
        r: &mut TreeResources<'_>,
    ) -> Result<(PropertyKeyId, PayloadSlice<'a, S>), TreeError> {
        if index >= self.canonical.property_count() {
            return Err(TreeError::Invalid("native property index"));
        }
        let row = property_row(self.properties, index, r)?;
        let key = PropertyKeyId::new(row.key)
            .map_err(|_| TreeError::Invalid("zero native property key"))?;
        let value = self.canonical_bytes.subslice(row.offset, row.length)?;
        r.read_event(NativeReadEvent::PropertyValue(row.length))?;
        Ok((key, value))
    }
}

/// Validate the complete native record, index/canonical bijection, topology,
/// provenance and linked stream availability. A repaired checksum never excuses
/// omitted index rows, stale duplicated fields or a mismatched directory key.
pub fn verify_record<'a, S: BlockSource>(
    source: PayloadSlice<'a, S>,
    expected: EntityId,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<RecordView<'a, S>, TreeError> {
    r.step(1)?;
    if !source.is_whole() {
        return Err(TreeError::Invalid("partial native record"));
    }
    let created = source.creation_generation(r)?;
    let mut c = PayloadCursor::new_with_resources(source, r)?;
    let id = u128::from_le_bytes(c.read_array(r)?);
    let (shape, revision, property_bytes) = match source.role() {
        BlockKind::NodeRecord => {
            let id = NodeId::new(id).map_err(|_| TreeError::Invalid("zero native node"))?;
            let revision = revision(&mut c, r)?;
            zero_u32(&mut c, r)?;
            let labels = u32::from_le_bytes(c.read_array(r)?);
            (
                RecordShape::Node { id, labels },
                revision,
                u64::from_le_bytes(c.read_array(r)?),
            )
        }
        BlockKind::RelRecord => {
            let id = RelId::new(id).map_err(|_| TreeError::Invalid("zero native relationship"))?;
            let source = super::node(&mut c, r)?;
            let target = super::node(&mut c, r)?;
            let relationship_type = RelTypeId::new(u64::from_le_bytes(c.read_array(r)?))
                .map_err(|_| TreeError::Invalid("zero native relationship type"))?;
            let revision = revision(&mut c, r)?;
            zero_u32(&mut c, r)?;
            zero_u32(&mut c, r)?;
            (
                RecordShape::Relationship {
                    id,
                    source,
                    target,
                    relationship_type,
                },
                revision,
                u64::from_le_bytes(c.read_array(r)?),
            )
        }
        _ => return Err(TreeError::Invalid("native record role")),
    };
    if shape.incarnation() != expected {
        return Err(TreeError::Invalid("native directory identity mismatch"));
    }
    let labels = c.take(
        match shape {
            RecordShape::Node { labels, .. } => u64::from(labels) * 8,
            _ => 0,
        },
        r,
    )?;
    let property_count = u32::from_le_bytes(c.read_array(r)?);
    zero_u32(&mut c, r)?;
    if property_bytes != 8 + u64::from(property_count) * 24 {
        return Err(TreeError::Invalid("native property index length"));
    }
    let properties = c.take(u64::from(property_count) * 24, r)?;
    let canonical_ref = PayloadRef::decode(&c.read_array::<48>(r)?)?;
    let provenance_ref = PayloadRef::decode(&c.read_array::<48>(r)?)?;
    c.finish(r)?;
    let canonical_bytes = source.linked(canonical_ref, r)?;
    let provenance_bytes = source.linked(provenance_ref, r)?;
    check_labels(labels, r)?;
    let mut previous = 0;
    for index in 0..u64::from(property_count) {
        let row = property_row(properties, index, r)?;
        if row.key <= previous
            || row.length == 0
            || row
                .offset
                .checked_add(row.length)
                .is_none_or(|end| end > canonical_bytes.len())
        {
            return Err(TreeError::Invalid("native property order or extent"));
        }
        previous = row.key;
    }
    let mut visitor = VerifyIndex {
        labels,
        properties,
        catalog,
    };
    let canonical = verify_canonical(canonical_bytes, document, &mut visitor, r)?;
    if canonical.property_count() != u64::from(property_count) {
        return Err(TreeError::Invalid(
            "native property index omits canonical fields",
        ));
    }
    match (shape, canonical.shape()) {
        (RecordShape::Node { labels, .. }, CanonicalShape::Node { labels: count })
            if u64::from(labels) == *count => {}
        (
            RecordShape::Relationship {
                source,
                target,
                relationship_type,
                ..
            },
            CanonicalShape::Relationship {
                source: cs,
                target: ct,
                relationship_type: name,
            },
        ) if source == *cs
            && target == *ct
            && catalog.resolve(SymbolKind::RelationshipType, *name, r)?
                == Symbol::RelationshipType(relationship_type) => {}
        _ => return Err(TreeError::Invalid("native canonical topology mismatch")),
    }
    let provenance = verify_provenance(provenance_bytes, r)?;
    validate_installing_provenance(&provenance, expected, revision, created, false)?;
    r.step(0)?;
    Ok(RecordView {
        shape,
        revision,
        labels,
        properties,
        canonical_bytes,
        canonical_ref,
        canonical,
        provenance_ref,
        provenance,
    })
}

pub(super) fn validate_installing_provenance<S: BlockSource>(
    provenance: &StoredProvenance<'_, S>,
    expected: EntityId,
    revision: GraphRevision,
    created: crate::property_graph::GraphGeneration,
    deleted: bool,
) -> Result<(), TreeError> {
    if provenance.incarnation() != expected
        || provenance.installed_revision() != revision
        || provenance.requested_revision() != revision
        || provenance.original_generation().get() == 0
        || provenance.original_generation() > created
        || provenance.delete_mode().is_some() != deleted
        || matches!(provenance.expected(), ExpectedGraphState::Entity(id) if id != expected)
    {
        return Err(TreeError::Invalid("native installing provenance mismatch"));
    }
    let valid = match provenance.operation() {
        GraphOperation::StructuredCreate => {
            !deleted
                && provenance.key().is_some()
                && provenance.expected() == ExpectedGraphState::Absent
        }
        GraphOperation::StructuredPut => {
            !deleted
                && provenance.key().is_some()
                && provenance.expected() == ExpectedGraphState::Entity(expected)
        }
        GraphOperation::StructuredDelete => {
            deleted
                && provenance.key().is_some()
                && provenance.expected() == ExpectedGraphState::Entity(expected)
        }
        GraphOperation::StructuredRecreate => {
            !deleted
                && provenance.key().is_some()
                && matches!(provenance.expected(), ExpectedGraphState::Deletion(d) if d < revision)
        }
        GraphOperation::CypherEdit => {
            provenance.expected() == ExpectedGraphState::Entity(expected)
                || (!deleted && provenance.expected() == ExpectedGraphState::Absent)
        }
    };
    if !valid {
        return Err(TreeError::Invalid("native installing operation mismatch"));
    }
    Ok(())
}

struct PropertyRow {
    key: u64,
    offset: u64,
    length: u64,
}
fn property_row<S: BlockSource>(
    rows: PayloadSlice<'_, S>,
    index: u64,
    r: &mut TreeResources<'_>,
) -> Result<PropertyRow, TreeError> {
    let mut c = PayloadCursor::new_with_resources(
        rows.subslice(index.checked_mul(24).ok_or(TreeError::Work)?, 24)?,
        r,
    )?;
    Ok(PropertyRow {
        key: u64::from_le_bytes(c.read_array(r)?),
        offset: u64::from_le_bytes(c.read_array(r)?),
        length: u64::from_le_bytes(c.read_array(r)?),
    })
}
fn find_property<S: BlockSource>(
    rows: PayloadSlice<'_, S>,
    key: u64,
    r: &mut TreeResources<'_>,
) -> Result<Option<PropertyRow>, TreeError> {
    let (mut low, mut high) = (0, rows.len() / 24);
    while low < high {
        let mid = low + (high - low) / 2;
        let row = property_row(rows, mid, r)?;
        match row.key.cmp(&key) {
            Ordering::Equal => return Ok(Some(row)),
            Ordering::Less => low = mid + 1,
            Ordering::Greater => high = mid,
        }
    }
    r.step(0)?;
    Ok(None)
}
fn check_labels<S: BlockSource>(
    labels: PayloadSlice<'_, S>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut c = PayloadCursor::new_with_resources(labels, r)?;
    let mut previous = 0;
    for _ in 0..labels.len() / 8 {
        let id = u64::from_le_bytes(c.read_array(r)?);
        if id <= previous {
            return Err(TreeError::Invalid("native label order"));
        }
        previous = id;
    }
    c.finish(r)
}
struct VerifyIndex<'a, 'c, S: BlockSource, C> {
    labels: PayloadSlice<'a, S>,
    properties: PayloadSlice<'a, S>,
    catalog: &'c C,
}
impl<S: BlockSource, C: RecordCatalog<S>> CanonicalVisitor<S> for VerifyIndex<'_, '_, S, C> {
    fn label(
        &mut self,
        name: PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let Symbol::Label(id) = self.catalog.resolve(SymbolKind::Label, name, r)? else {
            return Err(TreeError::Invalid("catalog label kind"));
        };
        let (mut low, mut high) = (0, self.labels.len() / 8);
        while low < high {
            let mid = low + (high - low) / 2;
            let mut c = PayloadCursor::new_with_resources(self.labels.subslice(mid * 8, 8)?, r)?;
            let found = u64::from_le_bytes(c.read_array(r)?);
            match found.cmp(&id.get()) {
                Ordering::Equal => return Ok(()),
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
            }
        }
        Err(TreeError::Invalid(
            "canonical label absent from native record",
        ))
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
        let row = find_property(self.properties, id.get(), r)?.ok_or(TreeError::Invalid(
            "canonical property absent from native index",
        ))?;
        if row.offset != value.offset() || row.length != value.encoded().len() {
            return Err(TreeError::Invalid(
                "native property is not exact canonical field",
            ));
        }
        r.step(0)
    }
}
fn zero_u32<S: BlockSource>(
    c: &mut PayloadCursor<'_, '_, S>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if u32::from_le_bytes(c.read_array(r)?) != 0 {
        return Err(TreeError::Invalid("native record reserved field"));
    }
    Ok(())
}
fn revision<S: BlockSource>(
    c: &mut PayloadCursor<'_, '_, S>,
    r: &mut TreeResources<'_>,
) -> Result<GraphRevision, TreeError> {
    GraphRevision::new(u64::from_le_bytes(c.read_array(r)?))
        .map_err(|_| TreeError::Invalid("zero native revision"))
}
