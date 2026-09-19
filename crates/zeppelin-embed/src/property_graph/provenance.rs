//! Lossless installing-operation evidence. Lifecycle classification and durable
//! envelope/checkpoint codecs consume this; neither is implemented here.

use std::io::{self, Read, Write};

use super::canonical::Encoder;
use super::{
    ApplicationKey, CanonicalComparison, CanonicalError, CanonicalFingerprint, CanonicalStats,
    EntityId, EntityKind, GraphGeneration, GraphRevision, compare_canonical_streams,
};

/// The origin of the operation that installed a live revision or deletion fence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphOperation {
    /// A keyed first create expecting absence.
    StructuredCreate,
    /// A complete keyed replacement expecting a current incarnation.
    StructuredPut,
    /// An explicit structured deletion.
    StructuredDelete,
    /// An explicit recreate against a deletion revision.
    StructuredRecreate,
    /// A statement edit; this is not a generic Cypher retry receipt.
    CypherEdit,
}

/// The request's explicit state precondition, not an inferred current state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpectedGraphState {
    /// The key must never have existed.
    Absent,
    /// The observed current entity incarnation.
    Entity(EntityId),
    /// The observed deletion revision preceding an explicit recreation.
    Deletion(GraphRevision),
}

/// Explicit deletion semantics retained in retry evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphDeleteMode {
    /// Refuse a node deletion while incident live relationships remain.
    Restrict,
    /// Delete a node with the accepted detach visibility semantics.
    Detach,
}

/// Every ZE-98 logical provenance field, before version/bound checks. No field
/// has an inferred/default value. The lifecycle classifier owns operation and
/// precondition admissibility; these fields describe its normalized outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationFields<'a> {
    /// Installing request/statement kind.
    pub operation: GraphOperation,
    /// Complete kind-scoped key, absent only for unkeyed entities.
    pub key: Option<ApplicationKey<'a>>,
    /// Explicit request revision.
    pub requested_revision: GraphRevision,
    /// Revision actually installed by the classified operation.
    pub installed_revision: GraphRevision,
    /// Original absence, incarnation or deletion-revision precondition.
    pub expected: ExpectedGraphState,
    /// Full-width affected entity identity and kind.
    pub incarnation: EntityId,
    /// Explicit deletion policy, when applicable.
    pub delete_mode: Option<GraphDeleteMode>,
    /// Original changed generation; physical relocation never changes this field.
    pub original_generation: GraphGeneration,
}

/// Versioned exact operation evidence retained even for tombstones. This is a
/// logical record, not a caller-selected ID API or a durable frame decoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationProvenance<'a> {
    fields: OperationFields<'a>,
    encoded_len: u64,
}
impl<'a> OperationProvenance<'a> {
    /// Admits an explicitly supplied version and complete fields. A decoder
    /// must pass None for an absent version; there is no legacy inference.
    pub fn from_fields(
        version: Option<u16>,
        fields: OperationFields<'a>,
    ) -> Result<Self, CanonicalError> {
        if version != Some(1) {
            return Err(CanonicalError::UnsupportedProvenanceVersion);
        }
        let kind = fields.incarnation.kind();
        if fields.key.is_some_and(|key| key.kind() != kind)
            || matches!(fields.expected, ExpectedGraphState::Entity(id) if id.kind() != kind)
        {
            return Err(CanonicalError::ProvenanceKindMismatch);
        }
        let mut value = Self {
            fields,
            encoded_len: 0,
        };
        value.encoded_len = value.write_to(&mut io::sink(), &mut || Ok(()))?.bytes;
        Ok(value)
    }
    /// Supported logical field interpretation.
    #[must_use]
    pub const fn version(self) -> u16 {
        1
    }
    /// All exact fields, including original generation and request preconditions.
    #[must_use]
    pub const fn fields(self) -> OperationFields<'a> {
        self.fields
    }
    /// Complete logical framing and payload length.
    #[must_use]
    pub const fn encoded_len(self) -> u64 {
        self.encoded_len
    }
    /// Streams every field with fixed type/presence tags and little-endian widths.
    /// Writes owns its future enclosing persisted frame family and checksum.
    pub fn write_to(
        &self,
        output: &mut dyn Write,
        checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
    ) -> Result<CanonicalStats, CanonicalError> {
        let mut e = Encoder::new(output, checkpoint);
        e.emit(b"ZGOP")?;
        e.emit(&self.version().to_le_bytes())?;
        e.byte(match self.fields.operation {
            GraphOperation::StructuredCreate => 1,
            GraphOperation::StructuredPut => 2,
            GraphOperation::StructuredDelete => 3,
            GraphOperation::StructuredRecreate => 4,
            GraphOperation::CypherEdit => 5,
        })?;
        e.byte(u8::from(self.fields.key.is_some()))?;
        if let Some(key) = self.fields.key {
            e.byte(kind_tag(key.kind()))?;
            e.blob(key.namespace().as_str().as_bytes())?;
            e.blob(key.key().as_str().as_bytes())?;
        }
        e.emit(&self.fields.requested_revision.get().to_le_bytes())?;
        e.emit(&self.fields.installed_revision.get().to_le_bytes())?;
        match self.fields.expected {
            ExpectedGraphState::Absent => e.byte(1)?,
            ExpectedGraphState::Entity(id) => {
                e.byte(2)?;
                encode_id(&mut e, id)?;
            }
            ExpectedGraphState::Deletion(revision) => {
                e.byte(3)?;
                e.emit(&revision.get().to_le_bytes())?;
            }
        }
        encode_id(&mut e, self.fields.incarnation)?;
        e.byte(match self.fields.delete_mode {
            None => 0,
            Some(GraphDeleteMode::Restrict) => 1,
            Some(GraphDeleteMode::Detach) => 2,
        })?;
        e.emit(&self.fields.original_generation.get().to_le_bytes())?;
        Ok(e.stats())
    }
}
fn kind_tag(kind: EntityKind) -> u8 {
    match kind {
        EntityKind::Node => 1,
        EntityKind::Relationship => 2,
    }
}
fn encode_id(e: &mut Encoder<'_>, id: EntityId) -> Result<(), CanonicalError> {
    e.byte(kind_tag(id.kind()))?;
    let value = match id {
        EntityId::Node(id) => id.get(),
        EntityId::Relationship(id) => id.get(),
    };
    e.emit(&value.to_le_bytes())
}

/// Complete normalized installing evidence and its validated lossless content
/// source. Storage retains the reader/lease; physical references do not enter
/// comparison. Request classification must supply the candidate installing
/// fields, not invent unknown IDs/generations from an unclassified create.
pub struct ReplayEvidence<'a> {
    /// Explicit complete versioned operation record.
    pub provenance: OperationProvenance<'a>,
    /// Validated descriptor's non-authoritative checksum and image length.
    pub fingerprint: CanonicalFingerprint,
    /// Bounded canonical image source, retained independently of its location.
    pub contents: &'a mut dyn Read,
}

/// Exact normalized replay-evidence equality. Same revision alone is never
/// sufficient. Lifecycle classification of stale/incarnation conflicts is later;
/// this compares both full installing provenance and complete live contents.
pub fn compare_replay_evidence(
    left: ReplayEvidence<'_>,
    right: ReplayEvidence<'_>,
    scratch: &mut [u8],
    checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
) -> Result<CanonicalComparison, CanonicalError> {
    checkpoint()?;
    if left.provenance != right.provenance {
        return Ok(CanonicalComparison {
            equal: false,
            bytes_compared: 0,
        });
    }
    compare_canonical_streams(
        left.contents,
        left.fingerprint,
        right.contents,
        right.fingerprint,
        scratch,
        checkpoint,
    )
}

impl<'a> OperationProvenance<'a> {
    /// Admits the same explicit fields as `from_fields`, polling the caller
    /// throughout complete provenance framing before logical finalization.
    pub fn from_fields_with_control(
        version: Option<u16>,
        fields: OperationFields<'a>,
        checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
    ) -> Result<Self, CanonicalError> {
        checkpoint()?;
        if version != Some(1) {
            return Err(CanonicalError::UnsupportedProvenanceVersion);
        }
        let kind = fields.incarnation.kind();
        if fields.key.is_some_and(|key| key.kind() != kind)
            || matches!(fields.expected, ExpectedGraphState::Entity(id) if id.kind() != kind)
        {
            return Err(CanonicalError::ProvenanceKindMismatch);
        }
        let mut value = Self {
            fields,
            encoded_len: 0,
        };
        value.encoded_len = value.write_to(&mut io::sink(), checkpoint)?.bytes;
        Ok(value)
    }
}
