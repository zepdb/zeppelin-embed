//! Logical record decoding over retained immutable streams. Canonical validation
//! is distinct from a native record's catalog/index/provenance correlation.

use super::artifact::BlockKind;
use super::stream::{PayloadCursor, PayloadSlice};
use super::tree::directory::{BlockSource, TreeError, TreeResources};
use crate::epoch::EmbeddingTower;
use crate::property_graph::{MAX_GRAPH_INPUT_BYTES, MAX_PROPERTY_LIST_ELEMENTS, NodeId};
use std::cmp::Ordering;

pub use super::payload::writer::prepare_provenance;
mod directory;
pub use directory::NativeDirectoryValues;
mod provenance;
pub use provenance::{StoredKey, StoredProvenance, verify_provenance};
mod native;
pub use native::{RecordCatalog, RecordShape, RecordView, verify_record};
mod prepare;
pub(crate) use prepare::sort_by_symbol;
pub use prepare::{RecordInput, prepare_record};
mod fence;
pub(crate) use fence::fence_window_reference;
pub use fence::{FenceInput, FenceView, prepare_fence, verify_fence_entry};
mod tombstone;
pub use tombstone::{
    NodeRecordState, NodeTombstone, prepare_node_tombstone, verify_node_state,
    verify_node_tombstone,
};

/// The exact canonical value encoding and its verified whole-field boundary.
/// Consumers decode into their own charged representation; no backing is copied.
pub struct StoredProperty<'a, S: BlockSource> {
    encoded: PayloadSlice<'a, S>,
    offset: u64,
    tag: u8,
    count: Option<u64>,
}
impl<S: BlockSource> Copy for StoredProperty<'_, S> {}
impl<S: BlockSource> Clone for StoredProperty<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'a, S: BlockSource> StoredProperty<'a, S> {
    /// Full encoding, beginning at its type tag; scalar bits are unchanged.
    pub const fn encoded(self) -> PayloadSlice<'a, S> {
        self.encoded
    }
    /// Canonical offset at the type tag, used by the native property index.
    pub const fn offset(self) -> u64 {
        self.offset
    }
    /// ZGCIv1 tag: string1, bool2, i643, f644, empty5, typed lists6..9.
    pub const fn tag(self) -> u8 {
        self.tag
    }
    /// Exact list element count, or None for a scalar.
    pub const fn count(self) -> Option<u64> {
        self.count
    }
}
/// Mandatory semantic observations. Native record validation uses these to prove
/// the complete catalog/index bijection; there is no permissive default hook.
pub trait CanonicalVisitor<S: BlockSource> {
    /// One exact UTF-8 label, in strictly increasing canonical byte order.
    fn label(
        &mut self,
        name: PayloadSlice<'_, S>,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError>;
    /// One complete value with its exact name and canonical boundaries.
    fn property(
        &mut self,
        name: PayloadSlice<'_, S>,
        value: StoredProperty<'_, S>,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError>;
}
/// Immutable topology decoded from the exact canonical payload.
pub enum CanonicalShape<'a, S: BlockSource> {
    /// Complete deduplicated label count; names are delivered to the visitor.
    Node {
        /// Complete count of unique canonical label names.
        labels: u64,
    },
    /// Full-width directed endpoint identities and byte-exact single type name.
    Relationship {
        /// Directed full-width source node identity.
        source: NodeId,
        /// Directed full-width target node identity.
        target: NodeId,
        /// Single exact type name, retained under the source lease.
        relationship_type: PayloadSlice<'a, S>,
    },
}
/// Original finite f32 bytes with the complete validated document declaration.
pub struct StoredVector<'a, S: BlockSource> {
    coordinates: PayloadSlice<'a, S>,
    document: PayloadSlice<'a, S>,
    dimensions: u32,
}
impl<S: BlockSource> Copy for StoredVector<'_, S> {}
impl<S: BlockSource> Clone for StoredVector<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'a, S: BlockSource> StoredVector<'a, S> {
    /// Original dimension count, validated against the admitted document tower.
    pub const fn dimensions(self) -> u32 {
        self.dimensions
    }
    /// Complete declaration bytes, retaining fields rather than a hash identity.
    pub const fn document(self) -> PayloadSlice<'a, S> {
        self.document
    }
    /// Lossless little-endian coordinates for bounded scoring passes.
    pub const fn coordinates(self) -> PayloadSlice<'a, S> {
        self.coordinates
    }
    /// Read one original coordinate; no normalization or quantization is applied.
    pub fn coordinate(
        self,
        index: u32,
        resources: &mut TreeResources<'_>,
    ) -> Result<f32, TreeError> {
        if index >= self.dimensions {
            return Err(TreeError::Invalid("vector coordinate index"));
        }
        let mut c = PayloadCursor::new_with_resources(
            self.coordinates.subslice(u64::from(index) * 4, 4)?,
            resources,
        )?;
        Ok(f32::from_bits(u32::from_le_bytes(c.read_array(resources)?)))
    }
}
/// Whole canonical framing and field validity under the retained source. It is
/// not a record/catalog/index validity proof or a GraphReadView admission.
pub struct CanonicalView<'a, S: BlockSource> {
    shape: CanonicalShape<'a, S>,
    properties: u64,
    text: Option<PayloadSlice<'a, S>>,
    vector: Option<StoredVector<'a, S>>,
}
impl<'a, S: BlockSource> CanonicalView<'a, S> {
    /// Complete checked logical topology.
    pub const fn shape(&self) -> &CanonicalShape<'a, S> {
        &self.shape
    }
    /// Count of complete named properties, independent of record index rows.
    pub const fn property_count(&self) -> u64 {
        self.properties
    }
    /// Original text preserving absent versus present empty.
    pub const fn stored_text(&self) -> Option<PayloadSlice<'a, S>> {
        self.text
    }
    /// Original finite coordinates with exact document declaration.
    pub const fn stored_vector(&self) -> Option<StoredVector<'a, S>> {
        self.vector
    }
}

/// Verify ZGCIv1 without constructing a whole record or property-value collection.
/// Optional vector content must match every field of the admitted document tower.
pub fn verify_canonical<'a, S: BlockSource>(
    source: PayloadSlice<'a, S>,
    document: Option<&EmbeddingTower>,
    visitor: &mut impl CanonicalVisitor<S>,
    resources: &mut TreeResources<'_>,
) -> Result<CanonicalView<'a, S>, TreeError> {
    resources.step(1)?;
    if !source.is_whole()
        || source.role() != BlockKind::CanonicalImage
        || source.len() > MAX_GRAPH_INPUT_BYTES as u64
    {
        return Err(TreeError::Invalid("canonical role or bound"));
    }
    let mut cursor = PayloadCursor::new_with_resources(source, resources)?;
    if cursor.read_array::<4>(resources)? != *b"ZGCI"
        || u16::from_le_bytes(cursor.read_array(resources)?) != 1
    {
        return Err(TreeError::Invalid("canonical magic/version"));
    }
    let shape = match byte(&mut cursor, resources)? {
        1 => {
            let labels = count(&mut cursor, resources)?;
            let mut previous: Option<PayloadSlice<'_, S>> = None;
            for _ in 0..labels {
                let name = text(&mut cursor, resources)?;
                increasing(previous, name, resources)?;
                visitor.label(name, resources)?;
                resources.step(0)?;
                previous = Some(name);
            }
            CanonicalShape::Node { labels }
        }
        2 => CanonicalShape::Relationship {
            source: node(&mut cursor, resources)?,
            target: node(&mut cursor, resources)?,
            relationship_type: text(&mut cursor, resources)?,
        },
        _ => return Err(TreeError::Invalid("canonical shape tag")),
    };
    let properties = count(&mut cursor, resources)?;
    let mut previous: Option<PayloadSlice<'_, S>> = None;
    for _ in 0..properties {
        let name = text(&mut cursor, resources)?;
        increasing(previous, name, resources)?;
        let offset = cursor.position();
        let (tag, count) = property(&mut cursor, resources)?;
        let encoded = source.subslice(offset, cursor.position() - offset)?;
        visitor.property(
            name,
            StoredProperty {
                encoded,
                offset,
                tag,
                count,
            },
            resources,
        )?;
        resources.step(0)?;
        previous = Some(name);
    }
    let text = optional_text(&mut cursor, resources)?;
    let vector = match byte(&mut cursor, resources)? {
        0 => None,
        1 => Some(vector(
            source,
            &mut cursor,
            document.ok_or(TreeError::Invalid("vector without document declaration"))?,
            resources,
        )?),
        _ => return Err(TreeError::Invalid("canonical vector presence")),
    };
    if matches!(shape, CanonicalShape::Relationship { .. }) && (text.is_some() || vector.is_some())
    {
        return Err(TreeError::Invalid("relationship search payload"));
    }
    cursor.finish(resources)?;
    Ok(CanonicalView {
        shape,
        properties,
        text,
        vector,
    })
}
fn byte<S: BlockSource>(
    c: &mut PayloadCursor<'_, '_, S>,
    r: &mut TreeResources<'_>,
) -> Result<u8, TreeError> {
    Ok(u8::from_le_bytes(c.read_array(r)?))
}
fn count<S: BlockSource>(
    c: &mut PayloadCursor<'_, '_, S>,
    r: &mut TreeResources<'_>,
) -> Result<u64, TreeError> {
    let count = u64::from_le_bytes(c.read_array(r)?);
    if count > MAX_GRAPH_INPUT_BYTES as u64 / 8 {
        return Err(TreeError::Invalid("canonical count bound"));
    }
    Ok(count)
}
fn node<S: BlockSource>(
    c: &mut PayloadCursor<'_, '_, S>,
    r: &mut TreeResources<'_>,
) -> Result<NodeId, TreeError> {
    NodeId::new(u128::from_le_bytes(c.read_array(r)?))
        .map_err(|_| TreeError::Invalid("zero canonical endpoint"))
}
fn text<'a, S: BlockSource>(
    c: &mut PayloadCursor<'a, '_, S>,
    r: &mut TreeResources<'_>,
) -> Result<PayloadSlice<'a, S>, TreeError> {
    let value = c.blob(r)?;
    value.validate_utf8(r)?;
    Ok(value)
}
fn optional_text<'a, S: BlockSource>(
    c: &mut PayloadCursor<'a, '_, S>,
    r: &mut TreeResources<'_>,
) -> Result<Option<PayloadSlice<'a, S>>, TreeError> {
    match byte(c, r)? {
        0 => Ok(None),
        1 => Ok(Some(text(c, r)?)),
        _ => Err(TreeError::Invalid("canonical text presence")),
    }
}
fn increasing<S: BlockSource>(
    previous: Option<PayloadSlice<'_, S>>,
    next: PayloadSlice<'_, S>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if let Some(previous) = previous
        && previous.compare(next, r)? != Ordering::Less
    {
        return Err(TreeError::Invalid("canonical name order or duplicate"));
    }
    Ok(())
}
fn property<S: BlockSource>(
    c: &mut PayloadCursor<'_, '_, S>,
    r: &mut TreeResources<'_>,
) -> Result<(u8, Option<u64>), TreeError> {
    let tag = byte(c, r)?;
    let count = match tag {
        1 => {
            text(c, r)?;
            None
        }
        2 => {
            if byte(c, r)? > 1 {
                return Err(TreeError::Invalid("canonical boolean"));
            }
            None
        }
        3 | 4 => {
            c.read_array::<8>(r)?;
            None
        }
        5..=9 => {
            let count = u64::from_le_bytes(c.read_array(r)?);
            if count > MAX_PROPERTY_LIST_ELEMENTS as u64 || (tag == 5 && count != 0) {
                return Err(TreeError::Invalid("canonical list count"));
            }
            for _ in 0..count {
                match tag {
                    6 => {
                        text(c, r)?;
                    }
                    7 => {
                        if byte(c, r)? > 1 {
                            return Err(TreeError::Invalid("canonical list boolean"));
                        }
                    }
                    8 | 9 => {
                        c.read_array::<8>(r)?;
                    }
                    _ => return Err(TreeError::Invalid("nonempty untyped list")),
                }
            }
            Some(count)
        }
        _ => return Err(TreeError::Invalid("canonical property tag")),
    };
    Ok((tag, count))
}
fn match_blob<S: BlockSource>(
    c: &mut PayloadCursor<'_, '_, S>,
    expected: &[u8],
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if c.blob(r)?.compare_bytes(expected, r)? != Ordering::Equal {
        return Err(TreeError::Invalid("canonical document mismatch"));
    }
    Ok(())
}
fn vector<'a, S: BlockSource>(
    source: PayloadSlice<'a, S>,
    c: &mut PayloadCursor<'a, '_, S>,
    tower: &EmbeddingTower,
    r: &mut TreeResources<'_>,
) -> Result<StoredVector<'a, S>, TreeError> {
    // Exact equality to typed UTF-8 tower fields also validates their encoding.
    let start = c.position();
    match_blob(c, tower.model_id.as_bytes(), r)?;
    match_blob(c, tower.model_version.as_bytes(), r)?;
    match_blob(c, &tower.weights_digest, r)?;
    let dimensions = u32::from_le_bytes(c.read_array(r)?);
    let normalization = u16::from_le_bytes(c.read_array(r)?);
    if dimensions == 0
        || dimensions != tower.dims
        || u64::from(dimensions) * 4 > MAX_GRAPH_INPUT_BYTES as u64
        || normalization != tower.normalization as u16
    {
        return Err(TreeError::Invalid(
            "canonical document dimensions/normalization",
        ));
    }
    match_blob(c, tower.prompt_prefix.as_bytes(), r)?;
    let max_tokens = u32::from_le_bytes(c.read_array(r)?);
    let runtime = u16::from_le_bytes(c.read_array(r)?);
    let compute = u16::from_le_bytes(c.read_array(r)?);
    if max_tokens != tower.max_tokens
        || runtime != tower.runtime as u16
        || compute != tower.compute_units as u16
    {
        return Err(TreeError::Invalid("canonical document execution fields"));
    }
    match (byte(c, r)?, &tower.os_build) {
        (0, None) => {}
        (1, Some(os)) => match_blob(c, os.as_bytes(), r)?,
        _ => return Err(TreeError::Invalid("canonical document OS build")),
    }
    let document = source.subslice(start, c.position() - start)?;
    if u64::from_le_bytes(c.read_array(r)?) != u64::from(dimensions) {
        return Err(TreeError::Invalid("canonical vector length"));
    }
    let start = c.position();
    for _ in 0..dimensions {
        if !f32::from_bits(u32::from_le_bytes(c.read_array(r)?)).is_finite() {
            return Err(TreeError::Invalid("nonfinite canonical vector"));
        }
    }
    let coordinates = source.subslice(start, c.position() - start)?;
    Ok(StoredVector {
        coordinates,
        document,
        dimensions,
    })
}
