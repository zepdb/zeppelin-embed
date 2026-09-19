//! Logical comparison framing, independent of persisted families and blob locations.

use std::io::{self, Read, Write};

use crate::epoch::EmbeddingTower;

use super::{
    DomainError, GraphName, GraphVector, MAX_GRAPH_INPUT_BYTES, NodeId, PropertyData, PropertyValue,
};

/// Maximum total scratch supplied to one exact stream comparison.
pub const MAX_CANONICAL_SCRATCH: usize = 64 * 1024;

/// Invalid canonical contents, failed bounded streaming, or cancelled work.
#[derive(Debug)]
pub enum CanonicalError {
    /// A borrowed domain input is invalid.
    Domain(DomainError),
    /// Every repeated property name is invalid, including equal values.
    DuplicateProperty,
    /// The complete framing and payload exceed the graph input limit.
    InputTooLarge,
    /// Comparison needs between two bytes and 64 KiB of caller-owned scratch.
    InvalidScratch,
    /// Work was cancelled before completing an image or comparison.
    Cancelled,
    /// A versioned provenance record is missing or unsupported.
    UnsupportedProvenanceVersion,
    /// A provenance field contradicts its entity kind.
    ProvenanceKindMismatch,
    /// Source or destination I/O failed, including truncated input.
    Io(io::Error),
}

impl std::fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Domain(error) => error.fmt(f),
            Self::DuplicateProperty => f.write_str("duplicate canonical graph property"),
            Self::InputTooLarge => f.write_str("canonical graph contents exceed the byte limit"),
            Self::InvalidScratch => {
                f.write_str("canonical comparison scratch must be 2..=65536 bytes")
            }
            Self::Cancelled => f.write_str("canonical graph work cancelled"),
            Self::UnsupportedProvenanceVersion => {
                f.write_str("unsupported or missing graph operation provenance version")
            }
            Self::ProvenanceKindMismatch => {
                f.write_str("graph operation provenance entity kinds differ")
            }
            Self::Io(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for CanonicalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Domain(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for CanonicalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// One validated named property, borrowing its original scalar bits.
#[derive(Clone, Copy, Debug)]
pub struct GraphProperty<'a> {
    name: GraphName<'a>,
    value: PropertyValue<'a>,
}
impl<'a> GraphProperty<'a> {
    /// Combines validated borrowed inputs; allocates nothing.
    #[must_use]
    pub const fn new(name: GraphName<'a>, value: PropertyValue<'a>) -> Self {
        Self { name, value }
    }
    /// Returns the byte-exact name.
    #[must_use]
    pub const fn name(self) -> GraphName<'a> {
        self.name
    }
    /// Returns the original typed value.
    #[must_use]
    pub const fn value(self) -> PropertyValue<'a> {
        self.value
    }
}

/// Lossless document interpretation and supplied coordinates. Query-tower and
/// alignment changes are deliberately absent, matching `document_epoch`.
#[derive(Clone, Copy, Debug)]
pub struct CanonicalEmbedding<'a> {
    document: &'a EmbeddingTower,
    vector: GraphVector<'a>,
}
impl<'a> CanonicalEmbedding<'a> {
    /// Validates the supplied coordinates against the declared document width.
    /// Catalog admission owns compatibility with the store's selected space.
    pub fn new(
        document: &'a EmbeddingTower,
        coordinates: &'a [f32],
    ) -> Result<Self, CanonicalError> {
        Ok(Self {
            document,
            vector: GraphVector::new(coordinates, document.dims).map_err(CanonicalError::Domain)?,
        })
    }
    /// Returns the complete borrowed document interpretation, never a hash alias.
    #[must_use]
    pub const fn document(self) -> &'a EmbeddingTower {
        self.document
    }
    /// Returns original finite coordinates without normalization or quantization.
    #[must_use]
    pub const fn vector(self) -> GraphVector<'a> {
        self.vector
    }
}

enum Shape<'a> {
    Node(&'a [GraphName<'a>]),
    Relationship {
        source: NodeId,
        target: NodeId,
        relationship_type: GraphName<'a>,
    },
}

/// Validated normalized logical contents. Sorting changes only caller-owned
/// descriptors in place; no allocation or payload copy occurs. The immutable
/// borrow then prevents changes while the image is streamed. Caller staging
/// retains and accounts the descriptors and backing values for this lifetime.
pub struct CanonicalContents<'a> {
    shape: Shape<'a>,
    properties: &'a [GraphProperty<'a>],
    text: Option<&'a str>,
    embedding: Option<CanonicalEmbedding<'a>>,
    encoded_len: u64,
}
impl<'a> CanonicalContents<'a> {
    /// Normalizes exact-byte labels and properties, rejecting repeated properties.
    /// Labels deduplicate in the emitted set; no Unicode normalization occurs.
    pub fn node(
        labels: &'a mut [GraphName<'a>],
        properties: &'a mut [GraphProperty<'a>],
        text: Option<&'a str>,
        embedding: Option<CanonicalEmbedding<'a>>,
    ) -> Result<Self, CanonicalError> {
        admit_descriptors(labels, properties)?;
        labels.sort_unstable();
        sort_properties(properties)?;
        Self::finish(Shape::Node(labels), properties, text, embedding)
    }

    /// Preserves directed full-width endpoints and exactly one fixed type.
    /// Relationships have properties; searchable text/vector payloads are node-only.
    pub fn relationship(
        source: NodeId,
        target: NodeId,
        relationship_type: GraphName<'a>,
        properties: &'a mut [GraphProperty<'a>],
    ) -> Result<Self, CanonicalError> {
        admit_descriptors(&[], properties)?;
        sort_properties(properties)?;
        Self::finish(
            Shape::Relationship {
                source,
                target,
                relationship_type,
            },
            properties,
            None,
            None,
        )
    }

    fn finish(
        shape: Shape<'a>,
        properties: &'a [GraphProperty<'a>],
        text: Option<&'a str>,
        embedding: Option<CanonicalEmbedding<'a>>,
    ) -> Result<Self, CanonicalError> {
        let mut image = Self {
            shape,
            properties,
            text,
            embedding,
            encoded_len: 0,
        };
        image.encoded_len = image.write_to(&mut io::sink(), &mut || Ok(()))?.bytes;
        Ok(image)
    }

    /// Complete canonical bytes, including every tag, length and presence bit.
    #[must_use]
    pub const fn encoded_len(&self) -> u64 {
        self.encoded_len
    }

    /// Streams the logical image. This is not a persisted-format family codec.
    /// Polls cancellation between bounded chunks, with no heap scratch. On an
    /// error, the sink may hold a private prefix; it must never be published.
    pub fn write_to(
        &self,
        output: &mut dyn Write,
        checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
    ) -> Result<CanonicalStats, CanonicalError> {
        let mut encoder = Encoder::new(output, checkpoint);
        encoder.emit(b"ZGCI")?;
        encoder.emit(&1_u16.to_le_bytes())?;
        match self.shape {
            Shape::Node(labels) => {
                encoder.byte(1)?;
                let mut previous = None;
                let count = labels
                    .iter()
                    .filter(|name| {
                        let distinct = previous != Some(**name);
                        previous = Some(**name);
                        distinct
                    })
                    .count();
                encoder.count(count)?;
                previous = None;
                for name in labels {
                    if previous != Some(*name) {
                        encoder.blob(name.as_str().as_bytes())?;
                    }
                    previous = Some(*name);
                }
            }
            Shape::Relationship {
                source,
                target,
                relationship_type,
            } => {
                encoder.byte(2)?;
                encoder.emit(&source.get().to_le_bytes())?;
                encoder.emit(&target.get().to_le_bytes())?;
                encoder.blob(relationship_type.as_str().as_bytes())?;
            }
        }
        encoder.count(self.properties.len())?;
        for property in self.properties {
            encoder.blob(property.name.as_str().as_bytes())?;
            encode_value(&mut encoder, property.value.data())?;
        }
        encoder.optional_blob(self.text.map(str::as_bytes))?;
        encoder.byte(u8::from(self.embedding.is_some()))?;
        if let Some(embedding) = self.embedding {
            encode_document(&mut encoder, embedding.document)?;
            encoder.count(embedding.vector.coordinates().len())?;
            for value in embedding.vector.coordinates() {
                encoder.emit(&value.to_bits().to_le_bytes())?;
            }
        }
        Ok(encoder.stats)
    }

    /// Computes a non-authoritative mismatch precheck from the complete image.
    pub fn fingerprint(
        &self,
        checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
    ) -> Result<CanonicalFingerprint, CanonicalError> {
        let mut hash = HashWriter(xxhash_rust::xxh3::Xxh3::new());
        let stats = self.write_to(&mut hash, checkpoint)?;
        Ok(CanonicalFingerprint {
            bytes: stats.bytes,
            hash: hash.0.digest(),
        })
    }
}

fn admit_descriptors(
    labels: &[GraphName<'_>],
    properties: &[GraphProperty<'_>],
) -> Result<(), CanonicalError> {
    // Bound normalization work before sorting. Each supplied name needs a length
    // frame, even when repeated labels later collapse to one canonical member.
    let mut bytes = 0_usize;
    for name in labels
        .iter()
        .copied()
        .chain(properties.iter().map(|p| p.name))
    {
        bytes = bytes
            .checked_add(8)
            .and_then(|n| n.checked_add(name.as_str().len()))
            .ok_or(CanonicalError::InputTooLarge)?;
        if bytes > MAX_GRAPH_INPUT_BYTES {
            return Err(CanonicalError::InputTooLarge);
        }
    }
    Ok(())
}
fn sort_properties(properties: &mut [GraphProperty<'_>]) -> Result<(), CanonicalError> {
    properties.sort_unstable_by_key(|property| property.name);
    let mut previous = None;
    for property in properties {
        if previous == Some(property.name) {
            return Err(CanonicalError::DuplicateProperty);
        }
        previous = Some(property.name);
    }
    Ok(())
}

pub(super) fn encode_document(
    e: &mut Encoder<'_>,
    tower: &EmbeddingTower,
) -> Result<(), CanonicalError> {
    e.blob(tower.model_id.as_bytes())?;
    e.blob(tower.model_version.as_bytes())?;
    e.blob(&tower.weights_digest)?;
    e.emit(&tower.dims.to_le_bytes())?;
    e.emit(&(tower.normalization as u16).to_le_bytes())?;
    e.blob(tower.prompt_prefix.as_bytes())?;
    e.emit(&tower.max_tokens.to_le_bytes())?;
    e.emit(&(tower.runtime as u16).to_le_bytes())?;
    e.emit(&(tower.compute_units as u16).to_le_bytes())?;
    e.optional_blob(tower.os_build.as_deref().map(str::as_bytes))
}
fn encode_value(e: &mut Encoder<'_>, value: PropertyData<'_>) -> Result<(), CanonicalError> {
    match value {
        PropertyData::String(value) => {
            e.byte(1)?;
            e.blob(value.as_bytes())
        }
        PropertyData::Bool(value) => {
            e.byte(2)?;
            e.byte(u8::from(value))
        }
        PropertyData::I64(value) => {
            e.byte(3)?;
            e.emit(&value.to_le_bytes())
        }
        PropertyData::F64(value) => {
            e.byte(4)?;
            e.emit(&value.to_bits().to_le_bytes())
        }
        PropertyData::EmptyList { count } => {
            e.byte(5)?;
            e.count(count)
        }
        PropertyData::Strings(values) => {
            e.byte(6)?;
            e.count(values.len())?;
            for value in values {
                e.blob(value.as_bytes())?;
            }
            Ok(())
        }
        PropertyData::Bools(values) => {
            e.byte(7)?;
            e.count(values.len())?;
            for value in values {
                e.byte(u8::from(*value))?;
            }
            Ok(())
        }
        PropertyData::Integers(values) => {
            e.byte(8)?;
            e.count(values.len())?;
            for value in values {
                e.emit(&value.to_le_bytes())?;
            }
            Ok(())
        }
        PropertyData::Floats(values) => {
            e.byte(9)?;
            e.count(values.len())?;
            for value in values {
                e.emit(&value.to_bits().to_le_bytes())?;
            }
            Ok(())
        }
    }
}

/// Exact successful encoding work; borrowed payload bytes are not heap usage.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CanonicalStats {
    /// Canonical bytes emitted, including framing.
    pub bytes: u64,
    /// Cancellation checkpoints actually called.
    pub checkpoints: u64,
}

pub(super) struct Encoder<'a> {
    output: &'a mut dyn Write,
    checkpoint: &'a mut dyn FnMut() -> Result<(), CanonicalError>,
    stats: CanonicalStats,
}
impl<'a> Encoder<'a> {
    pub(super) fn new(
        output: &'a mut dyn Write,
        checkpoint: &'a mut dyn FnMut() -> Result<(), CanonicalError>,
    ) -> Self {
        Self {
            output,
            checkpoint,
            stats: CanonicalStats::default(),
        }
    }
    pub(super) fn emit(&mut self, bytes: &[u8]) -> Result<(), CanonicalError> {
        let length = u64::try_from(bytes.len()).map_err(|_| CanonicalError::InputTooLarge)?;
        let total = self
            .stats
            .bytes
            .checked_add(length)
            .ok_or(CanonicalError::InputTooLarge)?;
        if total > MAX_GRAPH_INPUT_BYTES as u64 {
            return Err(CanonicalError::InputTooLarge);
        }
        for chunk in bytes.chunks(MAX_CANONICAL_SCRATCH) {
            (self.checkpoint)()?;
            self.stats.checkpoints += 1;
            self.output.write_all(chunk)?;
            self.stats.bytes += chunk.len() as u64;
        }
        Ok(())
    }
    pub(super) fn byte(&mut self, value: u8) -> Result<(), CanonicalError> {
        self.emit(&[value])
    }
    pub(super) fn count(&mut self, value: usize) -> Result<(), CanonicalError> {
        self.emit(
            &u64::try_from(value)
                .map_err(|_| CanonicalError::InputTooLarge)?
                .to_le_bytes(),
        )
    }
    pub(super) fn blob(&mut self, value: &[u8]) -> Result<(), CanonicalError> {
        self.count(value.len())?;
        self.emit(value)
    }
    pub(super) fn optional_blob(&mut self, value: Option<&[u8]>) -> Result<(), CanonicalError> {
        self.byte(u8::from(value.is_some()))?;
        if let Some(value) = value {
            self.blob(value)?;
        }
        Ok(())
    }
    pub(super) const fn stats(&self) -> CanonicalStats {
        self.stats
    }
}
struct HashWriter(xxhash_rust::xxh3::Xxh3);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stored checksum/length metadata is a mismatch precheck, never equality proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalFingerprint {
    bytes: u64,
    hash: u64,
}
impl CanonicalFingerprint {
    /// Accepts metadata from an already validated canonical-image descriptor.
    /// This validates bounds only; storage owns source framing/checksum validation.
    pub fn new(bytes: u64, hash: u64) -> Result<Self, CanonicalError> {
        if bytes > MAX_GRAPH_INPUT_BYTES as u64 {
            return Err(CanonicalError::InputTooLarge);
        }
        Ok(Self { bytes, hash })
    }
    /// Complete encoded byte count.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.bytes
    }
    /// Non-authoritative xxh3-64 mismatch precheck.
    #[must_use]
    pub const fn hash(self) -> u64 {
        self.hash
    }
}

/// Outcome from exact comparison, including observed comparison work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalComparison {
    /// True only when every byte and the declared stream boundary agree.
    pub equal: bool,
    /// Byte pairs compared; a hash/length mismatch compares zero bytes.
    pub bytes_compared: u64,
}

/// Compares two bounded lossless logical-image streams, ignoring physical origins.
/// Hash equality always falls through to byte comparison. Each source must end
/// exactly at its descriptor's boundary (use a bounded reader for a packed blob).
/// Scratch is caller-owned and split between the streams; this allocates nothing.
pub fn compare_canonical_streams(
    left: &mut dyn Read,
    left_fingerprint: CanonicalFingerprint,
    right: &mut dyn Read,
    right_fingerprint: CanonicalFingerprint,
    scratch: &mut [u8],
    checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
) -> Result<CanonicalComparison, CanonicalError> {
    if !(2..=MAX_CANONICAL_SCRATCH).contains(&scratch.len()) {
        return Err(CanonicalError::InvalidScratch);
    }
    checkpoint()?;
    if left_fingerprint != right_fingerprint {
        return Ok(CanonicalComparison {
            equal: false,
            bytes_compared: 0,
        });
    }
    let width = scratch.len() / 2;
    let (left_buffer, right_buffer) = scratch.split_at_mut(width);
    let mut remaining = left_fingerprint.bytes;
    let mut compared = 0_u64;
    while remaining != 0 {
        checkpoint()?;
        let count = usize::try_from(remaining.min(width as u64))
            .map_err(|_| CanonicalError::InputTooLarge)?;
        let l = left_buffer
            .get_mut(..count)
            .ok_or(CanonicalError::InvalidScratch)?;
        let r = right_buffer
            .get_mut(..count)
            .ok_or(CanonicalError::InvalidScratch)?;
        left.read_exact(l)?;
        right.read_exact(r)?;
        compared += count as u64;
        if l != r {
            return Ok(CanonicalComparison {
                equal: false,
                bytes_compared: compared,
            });
        }
        remaining -= count as u64;
    }
    checkpoint()?;
    let mut trailing = [0_u8; 1];
    if left.read(&mut trailing)? != 0 || right.read(&mut trailing)? != 0 {
        return Err(CanonicalError::Io(io::ErrorKind::InvalidData.into()));
    }
    Ok(CanonicalComparison {
        equal: true,
        bytes_compared: compared,
    })
}

#[cfg(all(test, feature = "allocation-audit"))]
#[allow(clippy::expect_used)]
mod allocation_tests {
    use super::*;

    #[test]
    fn malformed_stream_rejection_allocates_zero_bytes() {
        let fp = CanonicalFingerprint::new(1, 0).expect("metadata");
        let (result, audit) = crate::allocation_audit::audit_engine_path(|| {
            compare_canonical_streams(
                &mut &[1, 2][..],
                fp,
                &mut &[1][..],
                fp,
                &mut [0; 2],
                &mut || Ok(()),
            )
        });
        assert!(
            matches!(result, Err(CanonicalError::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
        );
        assert_eq!(
            audit.allocations, 0,
            "malformed input must not allocate an uncharged error"
        );
        assert_eq!(audit.unattributed_bytes, 0);
    }

    #[test]
    fn eight_mib_canonical_work_allocates_zero_bytes() {
        let text = "x".repeat(MAX_GRAPH_INPUT_BYTES - 33);
        let mut scratch = [0; MAX_CANONICAL_SCRATCH];
        let mut labels = [];
        let mut properties = [];
        let ((encoded_len, compared), audit) = crate::allocation_audit::audit_engine_path(|| {
            let image = CanonicalContents::node(&mut labels, &mut properties, Some(&text), None)
                .expect("exact maximum image");
            let fingerprint = image.fingerprint(&mut || Ok(())).expect("stream hash");
            image
                .write_to(&mut io::sink(), &mut || Ok(()))
                .expect("stream image");
            // The source is supplied by storage; a repeated byte stream exercises
            // the full admitted length without allocating a retained image here.
            let mut left = io::repeat(b'x').take(fingerprint.bytes());
            let mut right = io::repeat(b'x').take(fingerprint.bytes());
            let compared = compare_canonical_streams(
                &mut left,
                fingerprint,
                &mut right,
                fingerprint,
                &mut scratch,
                &mut || Ok(()),
            )
            .expect("bounded exact comparison");
            (image.encoded_len(), compared)
        });
        assert_eq!(encoded_len, MAX_GRAPH_INPUT_BYTES as u64);
        assert!(compared.equal);
        assert_eq!(compared.bytes_compared, MAX_GRAPH_INPUT_BYTES as u64);
        assert_eq!(audit.allocations, 0);
        assert_eq!(audit.attributed_bytes, 0);
        assert_eq!(audit.unattributed_bytes, 0);
        assert_eq!(audit.full_segment_clones, 0);
        eprintln!(
            "canonical_bytes={encoded_len} scratch_bytes={} allocator_calls={} allocated_bytes=0",
            scratch.len(),
            audit.allocations
        );
        // This feature's allocator is a real counter, not a permanently-zero
        // observation: a private deliberate allocation must be visible.
        let (control, positive) =
            crate::allocation_audit::audit_engine_path(|| std::hint::black_box(vec![0_u8; 17]));
        assert_eq!(control.len(), 17);
        assert_eq!(positive.allocations, 1);
        assert_eq!(positive.unattributed_bytes, 17);
    }
}

/// Immutable logical topology, independent of properties and physical placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityShape<'a> {
    /// Node labels and contents may change while its identity is retained.
    Node,
    /// A relationship keeps both directed endpoints and its exact type forever.
    Relationship {
        /// Full-width source node identity.
        source: NodeId,
        /// Full-width target node identity.
        target: NodeId,
        /// Exact relationship type.
        relationship_type: GraphName<'a>,
    },
}

impl EntityShape<'_> {
    /// Identity domain implied by the logical topology.
    #[must_use]
    pub const fn kind(self) -> super::EntityKind {
        match self {
            Self::Node => super::EntityKind::Node,
            Self::Relationship { .. } => super::EntityKind::Relationship,
        }
    }
}

impl<'a> CanonicalContents<'a> {
    /// Returns immutable topology for same-incarnation replacement validation.
    #[must_use]
    pub const fn shape(&self) -> EntityShape<'a> {
        match self.shape {
            Shape::Node(_) => EntityShape::Node,
            Shape::Relationship {
                source,
                target,
                relationship_type,
            } => EntityShape::Relationship {
                source,
                target,
                relationship_type,
            },
        }
    }
}

// Staging borrows the validated payload while owning its encoded copy. Keeping
// this accessor private avoids a second externally constructible image format.
type StagingNodeParts<'view, 'a> = (
    &'view [GraphName<'a>],
    &'view [GraphProperty<'a>],
    Option<&'a str>,
    Option<CanonicalEmbedding<'a>>,
);
impl<'a> CanonicalContents<'a> {
    pub(super) fn staging_node_parts(&self) -> Option<StagingNodeParts<'_, 'a>> {
        match &self.shape {
            Shape::Node(labels) => Some((labels, self.properties, self.text, self.embedding)),
            Shape::Relationship { .. } => None,
        }
    }
}
pub(super) fn encode_staging_value(
    encoder: &mut Encoder<'_>,
    value: PropertyData<'_>,
) -> Result<(), CanonicalError> {
    encode_value(encoder, value)
}
impl Encoder<'_> {
    pub(super) fn staging_checkpoint(&mut self) -> Result<(), CanonicalError> {
        (self.checkpoint)()?;
        self.stats.checkpoints += 1;
        Ok(())
    }
}
