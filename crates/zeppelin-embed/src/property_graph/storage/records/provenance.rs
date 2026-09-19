//! Structural ZGOPv1 decoding. Lifecycle admissibility and correlation with the
//! native entity/fence remain separate mandatory validation steps.

use super::{BlockKind, BlockSource, PayloadCursor, PayloadSlice, TreeError, TreeResources};
use crate::property_graph::{
    ApplicationKey, EntityId, EntityKind, ExpectedGraphState, GraphDeleteMode, GraphGeneration,
    GraphOperation, GraphRevision, MAX_GRAPH_INPUT_BYTES, NodeId, OperationFields, RelId,
};
use std::cmp::Ordering;

/// Exact kind-scoped key bytes, retaining their immutable source rather than
/// allocating a whole namespace/key or pretending discontiguous text is `str`.
pub struct StoredKey<'a, S: BlockSource> {
    kind: EntityKind,
    namespace: PayloadSlice<'a, S>,
    key: PayloadSlice<'a, S>,
}
impl<S: BlockSource> Copy for StoredKey<'_, S> {}
impl<S: BlockSource> Clone for StoredKey<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'a, S: BlockSource> StoredKey<'a, S> {
    /// Node and relationship key domains remain distinct.
    pub const fn kind(self) -> EntityKind {
        self.kind
    }
    /// Complete checked UTF-8 namespace, including empty or embedded NUL.
    pub const fn namespace(self) -> PayloadSlice<'a, S> {
        self.namespace
    }
    /// Complete checked UTF-8 key, including empty or embedded NUL.
    pub const fn key(self) -> PayloadSlice<'a, S> {
        self.key
    }
}

/// Complete structurally checked provenance. This proves framing and domain
/// validity, not that an operation was admissible against a prior graph state.
pub struct StoredProvenance<'a, S: BlockSource> {
    operation: GraphOperation,
    key: Option<StoredKey<'a, S>>,
    requested_revision: GraphRevision,
    installed_revision: GraphRevision,
    expected: ExpectedGraphState,
    incarnation: EntityId,
    delete_mode: Option<GraphDeleteMode>,
    original_generation: GraphGeneration,
}
impl<'a, S: BlockSource> StoredProvenance<'a, S> {
    /// Original operation, without inference from liveness or current revision.
    pub const fn operation(&self) -> GraphOperation {
        self.operation
    }
    /// Exact optional original key.
    pub const fn key(&self) -> Option<StoredKey<'a, S>> {
        self.key
    }
    /// Explicit original requested revision.
    pub const fn requested_revision(&self) -> GraphRevision {
        self.requested_revision
    }
    /// Original installed revision.
    pub const fn installed_revision(&self) -> GraphRevision {
        self.installed_revision
    }
    /// Explicit prior-state precondition.
    pub const fn expected(&self) -> ExpectedGraphState {
        self.expected
    }
    /// Full-width affected incarnation and kind.
    pub const fn incarnation(&self) -> EntityId {
        self.incarnation
    }
    /// Explicit original deletion policy.
    pub const fn delete_mode(&self) -> Option<GraphDeleteMode> {
        self.delete_mode
    }
    /// Original changed generation, unchanged by physical relocation.
    pub const fn original_generation(&self) -> GraphGeneration {
        self.original_generation
    }
    /// Reconstruct exact identity-owned fields from caller-retained contiguous
    /// key text only after bounded byte-for-byte comparison. The caller owns and
    /// charges that text; no scratch copy, normalization or default is supplied.
    pub fn fields_with_key<'k>(
        &self,
        key: Option<ApplicationKey<'k>>,
        resources: &mut TreeResources<'_>,
    ) -> Result<OperationFields<'k>, TreeError> {
        resources.step(1)?;
        match (self.key, key) {
            (None, None) => {}
            (Some(stored), Some(key))
                if stored.kind == key.kind()
                    && stored
                        .namespace
                        .compare_bytes(key.namespace().as_str().as_bytes(), resources)?
                        == Ordering::Equal
                    && stored
                        .key
                        .compare_bytes(key.key().as_str().as_bytes(), resources)?
                        == Ordering::Equal => {}
            _ => {
                return Err(TreeError::Invalid(
                    "provenance key differs from retained fields",
                ));
            }
        }
        resources.step(0)?;
        Ok(OperationFields {
            operation: self.operation,
            key,
            requested_revision: self.requested_revision,
            installed_revision: self.installed_revision,
            expected: self.expected,
            incarnation: self.incarnation,
            delete_mode: self.delete_mode,
            original_generation: self.original_generation,
        })
    }
}

/// Decode every original ZGOPv1 field with bounded controlled stream reads.
/// Unknown/absent versions, tags, zero IDs/revisions, cross-kind fields, malformed
/// UTF-8 and trailing bytes reject. Native record/fence verification must also
/// correlate these fields and validate normalized operation admissibility.
pub fn verify_provenance<'a, S: BlockSource>(
    source: PayloadSlice<'a, S>,
    resources: &mut TreeResources<'_>,
) -> Result<StoredProvenance<'a, S>, TreeError> {
    resources.step(1)?;
    if !source.is_whole()
        || source.role() != BlockKind::OperationProvenance
        || source.len() > MAX_GRAPH_INPUT_BYTES as u64
    {
        return Err(TreeError::Invalid("provenance role or bound"));
    }
    let mut cursor = PayloadCursor::new(source);
    if cursor.read_array::<4>(resources)? != *b"ZGOP"
        || u16::from_le_bytes(cursor.read_array(resources)?) != 1
    {
        return Err(TreeError::Invalid("provenance magic/version"));
    }
    let operation = match super::byte(&mut cursor, resources)? {
        1 => GraphOperation::StructuredCreate,
        2 => GraphOperation::StructuredPut,
        3 => GraphOperation::StructuredDelete,
        4 => GraphOperation::StructuredRecreate,
        5 => GraphOperation::CypherEdit,
        _ => return Err(TreeError::Invalid("provenance operation tag")),
    };
    let key = match super::byte(&mut cursor, resources)? {
        0 => None,
        1 => Some(StoredKey {
            kind: kind(&mut cursor, resources)?,
            namespace: super::text(&mut cursor, resources)?,
            key: super::text(&mut cursor, resources)?,
        }),
        _ => return Err(TreeError::Invalid("provenance key presence")),
    };
    let requested_revision = revision(&mut cursor, resources)?;
    let installed_revision = revision(&mut cursor, resources)?;
    let expected = match super::byte(&mut cursor, resources)? {
        1 => ExpectedGraphState::Absent,
        2 => ExpectedGraphState::Entity(entity(&mut cursor, resources)?),
        3 => ExpectedGraphState::Deletion(revision(&mut cursor, resources)?),
        _ => return Err(TreeError::Invalid("provenance expected state tag")),
    };
    let incarnation = entity(&mut cursor, resources)?;
    let delete_mode = match super::byte(&mut cursor, resources)? {
        0 => None,
        1 => Some(GraphDeleteMode::Restrict),
        2 => Some(GraphDeleteMode::Detach),
        _ => return Err(TreeError::Invalid("provenance delete tag")),
    };
    let original_generation =
        GraphGeneration::new(u64::from_le_bytes(cursor.read_array(resources)?));
    if key.is_some_and(|k| k.kind != incarnation.kind())
        || matches!(expected, ExpectedGraphState::Entity(id) if id.kind() != incarnation.kind())
    {
        return Err(TreeError::Invalid("provenance entity kind mismatch"));
    }
    cursor.finish(resources)?;
    Ok(StoredProvenance {
        operation,
        key,
        requested_revision,
        installed_revision,
        expected,
        incarnation,
        delete_mode,
        original_generation,
    })
}
fn kind<S: BlockSource>(
    cursor: &mut PayloadCursor<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<EntityKind, TreeError> {
    match super::byte(cursor, resources)? {
        1 => Ok(EntityKind::Node),
        2 => Ok(EntityKind::Relationship),
        _ => Err(TreeError::Invalid("provenance entity kind")),
    }
}
fn revision<S: BlockSource>(
    cursor: &mut PayloadCursor<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<GraphRevision, TreeError> {
    GraphRevision::new(u64::from_le_bytes(cursor.read_array(resources)?))
        .map_err(|_| TreeError::Invalid("zero provenance revision"))
}
fn entity<S: BlockSource>(
    cursor: &mut PayloadCursor<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<EntityId, TreeError> {
    let kind = kind(cursor, resources)?;
    let bits = u128::from_le_bytes(cursor.read_array(resources)?);
    match kind {
        EntityKind::Node => NodeId::new(bits).map(EntityId::Node),
        EntityKind::Relationship => RelId::new(bits).map(EntityId::Relationship),
    }
    .map_err(|_| TreeError::Invalid("zero provenance incarnation"))
}
