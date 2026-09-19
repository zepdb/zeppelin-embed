//! Pure logical key decisions. No allocator, durable write or publication lives here.
use super::{
    ApplicationKey, CanonicalError, CanonicalFingerprint, EntityId, EntityKind, EntityShape,
    ExpectedGraphState, GraphDeleteMode, GraphGeneration, GraphOperation, GraphRevision,
    MAX_GRAPH_CHANGES, MAX_GRAPH_INPUT_BYTES, OperationFields, OperationProvenance,
    compare_canonical_streams,
};
use std::io::Read;
mod bounded;

/// Validated lossless image and immutable topology. Storage/staging validates
/// framing, fingerprint and topology and retains the source lease.
pub struct CanonicalRecord<'a> {
    shape: EntityShape<'a>,
    fingerprint: CanonicalFingerprint,
    source: &'a mut dyn Read,
}
impl<'a> CanonicalRecord<'a> {
    /// Borrows an already validated canonical source without copying its bytes.
    #[must_use]
    pub fn from_validated(
        shape: EntityShape<'a>,
        fingerprint: CanonicalFingerprint,
        source: &'a mut dyn Read,
    ) -> Self {
        Self {
            shape,
            fingerprint,
            source,
        }
    }
}

/// Complete admitted live entity evidence.
pub struct CurrentEntity<'a> {
    /// Explicit versioned installing operation.
    pub provenance: OperationProvenance<'a>,
    /// Validated retained canonical contents.
    pub contents: CanonicalRecord<'a>,
}

/// Admitted state of one exact kind-scoped application key.
pub enum KeyState<'a> {
    /// No live record or retained deletion fence has ever used this key.
    NeverUsed,
    /// Current incarnation and its lossless canonical source.
    Live(CurrentEntity<'a>),
    /// Permanent key history, retained independently of swept entity bytes.
    Deleted(OperationProvenance<'a>),
}

/// Full-record structured input; no arbitrary patch or generic retry token.
pub enum KeyRequest<'a> {
    /// First installation, explicitly expecting absence.
    Create {
        /// Explicit positive request revision.
        revision: GraphRevision,
        /// Complete normalized image.
        contents: CanonicalRecord<'a>,
    },
    /// Full replacement of exactly the expected live incarnation.
    Put {
        /// Explicit positive request revision.
        revision: GraphRevision,
        /// Exact observed live incarnation.
        expected: EntityId,
        /// Complete replacement image.
        contents: CanonicalRecord<'a>,
    },
    /// Delete exactly the expected live incarnation with explicit integrity mode.
    Delete {
        /// Explicit positive deletion revision.
        revision: GraphRevision,
        /// Exact observed live incarnation.
        expected: EntityId,
        /// Integrity semantics that staging must enforce before publication.
        mode: GraphDeleteMode,
    },
    /// New incarnation, acknowledging the observed deletion revision.
    Recreate {
        /// Revision strictly newer than the acknowledged deletion.
        revision: GraphRevision,
        /// Exact observed deletion revision, never an inferred absence.
        deleted_revision: GraphRevision,
        /// Complete new incarnation's image.
        contents: CanonicalRecord<'a>,
    },
}

/// Invalid request or inconsistent admitted evidence.
#[derive(Debug)]
pub enum KeyLifecycleError {
    /// Canonical stream validation, cancellation or I/O failed.
    Canonical(CanonicalError),
    /// An identity/image belongs to another key domain.
    KindMismatch,
    /// Retained evidence does not describe this key/current state.
    InvalidState,
    /// No incarnation or deletion fence exists for this operation.
    MissingKey,
    /// Requested revision is older than the current revision.
    Stale {
        /// Current live or deletion-fence revision.
        current: GraphRevision,
    },
    /// Same revision, different operation/precondition/content.
    RevisionConflict,
    /// Create cannot replace an already live key.
    AlreadyExists,
    /// Ordinary create/put cannot cross a deletion fence.
    DeletedKey,
    /// Recreate requires a currently deleted key.
    NotDeleted,
    /// Update/delete names a different current or retired incarnation.
    IncarnationConflict,
    /// Recreate did not name the current deletion revision.
    DeletionRevisionConflict,
    /// Replacement changes fixed relationship endpoints/type.
    RelationshipIdentityChange,
    /// Private allocation changed the required ID or reused a retired ID.
    InvalidInstalledIdentity,
    /// Changed generation is no newer than the prior operation.
    InvalidGeneration,
    /// An actual Cypher change cannot advance beyond the maximum revision.
    RevisionOverflow,
    /// Changed work cannot advance the admitted generation.
    GenerationOverflow,
    /// A structured batch repeats a full key or resolved entity identity.
    DuplicateTarget,
    /// A normalized target has neither a key nor an entity identity.
    MissingTarget,
    /// More structured targets than the shared batch limit.
    TooManyTargets,
    /// Complete target descriptors exceed the bounded input admission.
    InputTooLarge,
}
impl From<CanonicalError> for KeyLifecycleError {
    fn from(error: CanonicalError) -> Self {
        Self::Canonical(error)
    }
}
impl std::fmt::Display for KeyLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canonical(error) => error.fmt(f),
            Self::KindMismatch => f.write_str("graph key and contents kinds differ"),
            Self::InvalidState => f.write_str("inconsistent graph key evidence"),
            Self::MissingKey => f.write_str("graph key has never existed"),
            Self::Stale { .. } => f.write_str("stale graph key revision"),
            Self::RevisionConflict => {
                f.write_str("graph revision has different installing evidence")
            }
            Self::AlreadyExists => f.write_str("graph key already exists"),
            Self::DeletedKey => f.write_str("graph key requires explicit recreation"),
            Self::NotDeleted => f.write_str("graph key is not deleted"),
            Self::IncarnationConflict => f.write_str("graph request names a different incarnation"),
            Self::DeletionRevisionConflict => {
                f.write_str("graph request names a different deletion revision")
            }
            Self::RelationshipIdentityChange => {
                f.write_str("graph relationship endpoints and type are immutable")
            }
            Self::InvalidInstalledIdentity => {
                f.write_str("invalid privately installed graph identity")
            }
            Self::InvalidGeneration => f.write_str("invalid changed graph generation"),
            Self::RevisionOverflow => f.write_str("graph revision overflow"),
            Self::GenerationOverflow => f.write_str("graph generation overflow"),
            Self::DuplicateTarget => f.write_str("duplicate structured graph target"),
            Self::MissingTarget => f.write_str("structured graph target is absent"),
            Self::TooManyTargets => f.write_str("structured graph target limit exceeded"),
            Self::InputTooLarge => f.write_str("graph target descriptors exceed the byte limit"),
        }
    }
}
impl std::error::Error for KeyLifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Canonical(error) => Some(error),
            _ => None,
        }
    }
}

/// Private preparation finalized using coordinator-selected identity/generation.
/// This logical value cannot allocate, publish or mutate a store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingKeyChange<'a> {
    key: Option<ApplicationKey<'a>>,
    kind: EntityKind,
    operation: GraphOperation,
    revision: GraphRevision,
    expected: ExpectedGraphState,
    delete_mode: Option<GraphDeleteMode>,
    existing: Option<EntityId>,
    retired: Option<EntityId>,
    previous_generation: GraphGeneration,
}
impl<'a> PendingKeyChange<'a> {
    /// ID to retain, or None when private allocation is required.
    #[must_use]
    pub const fn existing_incarnation(self) -> Option<EntityId> {
        self.existing
    }
    /// Whether this installs a deletion fence rather than live content.
    #[must_use]
    pub const fn is_deletion(self) -> bool {
        self.delete_mode.is_some()
    }
    /// Finalizes evidence after private allocator/generation selection. This is
    /// not a caller-selected ID feature on a store write API. The real allocator
    /// must additionally prove never-used IDs across the store's entire history.
    pub fn install(
        self,
        incarnation: EntityId,
        generation: GraphGeneration,
        checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
    ) -> Result<OperationProvenance<'a>, KeyLifecycleError> {
        if incarnation.kind() != self.kind {
            return Err(KeyLifecycleError::KindMismatch);
        }
        if self.existing.is_some_and(|id| id != incarnation) || self.retired == Some(incarnation) {
            return Err(KeyLifecycleError::InvalidInstalledIdentity);
        }
        if generation <= self.previous_generation {
            return Err(KeyLifecycleError::InvalidGeneration);
        }
        Ok(OperationProvenance::from_fields_with_control(
            Some(1),
            OperationFields {
                operation: self.operation,
                key: self.key,
                requested_revision: self.revision,
                installed_revision: self.revision,
                expected: self.expected,
                incarnation,
                delete_mode: self.delete_mode,
                original_generation: generation,
            },
            checkpoint,
        )?)
    }
}

/// Logical result consumed before artifacts, WAL or target generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyDecision<'a> {
    /// No final entity change. Other durable participants must still be checked.
    NoOp,
    /// Changed item needs private allocation/preparation and eventual commit.
    Change(PendingKeyChange<'a>),
    /// Exact repeat: preserve identity/revision/original generation unchanged.
    Replay(OperationProvenance<'a>),
}

/// Final normalized effect of one statement on one already-bound entity.
/// Evaluate every clause/expression first; pass only the final image here once.
pub enum CypherEdit<'a> {
    /// Final complete image after all ordered assignments/removals.
    Put(CanonicalRecord<'a>),
    /// One deletion, regardless of how many incident edges a node has.
    Delete(GraphDeleteMode),
}

/// Advances a genuinely changed entity exactly once, including unkeyed entities.
/// Equal final contents and missing/null targets are NoOp, never Replay. This
/// does not make arbitrary Cypher safe to retry: re-evaluation may change the
/// next final image again. Allocation/fence/catalog changes are separate inputs
/// to batch disposition, so a net-empty create/delete is not discarded here.
pub fn classify_cypher<'a>(
    current: Option<CurrentEntity<'a>>,
    edit: CypherEdit<'a>,
    scratch: &mut [u8],
    checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
) -> Result<KeyDecision<'a>, KeyLifecycleError> {
    checkpoint()?;
    let Some(current) = current else {
        return Ok(KeyDecision::NoOp);
    };
    let fields = current.provenance.fields();
    validate_provenance(fields.key, current.provenance, false, checkpoint)?;
    if current.contents.shape.kind() != fields.incarnation.kind() {
        return Err(KeyLifecycleError::InvalidState);
    }
    let delete_mode = match edit {
        CypherEdit::Put(final_contents) => {
            if final_contents.shape.kind() != fields.incarnation.kind() {
                return Err(KeyLifecycleError::KindMismatch);
            }
            if !bounded::shapes(final_contents.shape, current.contents.shape, checkpoint)? {
                return Err(KeyLifecycleError::RelationshipIdentityChange);
            }
            if contents_equal(current.contents, final_contents, scratch, checkpoint)? {
                return Ok(KeyDecision::NoOp);
            }
            None
        }
        CypherEdit::Delete(mode) => Some(mode),
    };
    let revision = fields
        .installed_revision
        .checked_next()
        .map_err(|_| KeyLifecycleError::RevisionOverflow)?;
    Ok(KeyDecision::Change(PendingKeyChange {
        key: fields.key,
        kind: fields.incarnation.kind(),
        operation: GraphOperation::CypherEdit,
        revision,
        expected: ExpectedGraphState::Entity(fields.incarnation),
        delete_mode,
        existing: Some(fields.incarnation),
        retired: None,
        previous_generation: fields.original_generation,
    }))
}

struct Request<'a> {
    operation: GraphOperation,
    revision: GraphRevision,
    expected: ExpectedGraphState,
    delete_mode: Option<GraphDeleteMode>,
    contents: Option<CanonicalRecord<'a>>,
}
impl<'a> KeyRequest<'a> {
    fn parts(self) -> Request<'a> {
        match self {
            Self::Create { revision, contents } => Request {
                operation: GraphOperation::StructuredCreate,
                revision,
                expected: ExpectedGraphState::Absent,
                delete_mode: None,
                contents: Some(contents),
            },
            Self::Put {
                revision,
                expected,
                contents,
            } => Request {
                operation: GraphOperation::StructuredPut,
                revision,
                expected: ExpectedGraphState::Entity(expected),
                delete_mode: None,
                contents: Some(contents),
            },
            Self::Delete {
                revision,
                expected,
                mode,
            } => Request {
                operation: GraphOperation::StructuredDelete,
                revision,
                expected: ExpectedGraphState::Entity(expected),
                delete_mode: Some(mode),
                contents: None,
            },
            Self::Recreate {
                revision,
                deleted_revision,
                contents,
            } => Request {
                operation: GraphOperation::StructuredRecreate,
                revision,
                expected: ExpectedGraphState::Deletion(deleted_revision),
                delete_mode: None,
                contents: Some(contents),
            },
        }
    }
}

/// Classifies against an unchanged admitted key state. Wrong IDs reject before
/// revision ordering; a larger revision never crosses incarnation. Restrict
/// adjacency probing and DETACH endpoint-liveness publication belong to staging/
/// storage: deletion here is one logical fence, never an incident-edge list.
pub fn classify_key<'a>(
    key: ApplicationKey<'a>,
    state: KeyState<'a>,
    request: KeyRequest<'a>,
    scratch: &mut [u8],
    checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
) -> Result<KeyDecision<'a>, KeyLifecycleError> {
    checkpoint()?;
    let request = request.parts();
    if request
        .contents
        .as_ref()
        .is_some_and(|c| c.shape.kind() != key.kind())
        || matches!(request.expected, ExpectedGraphState::Entity(id) if id.kind() != key.kind())
    {
        return Err(KeyLifecycleError::KindMismatch);
    }
    let (provenance, current_contents, deleted) = match state {
        KeyState::NeverUsed => {
            return if request.operation == GraphOperation::StructuredCreate {
                Ok(KeyDecision::Change(pending(key, &request, None, false)))
            } else {
                Err(KeyLifecycleError::MissingKey)
            };
        }
        KeyState::Live(current) => (current.provenance, Some(current.contents), false),
        KeyState::Deleted(provenance) => (provenance, None, true),
    };
    validate_provenance(Some(key), provenance, deleted, checkpoint)?;
    let fields = provenance.fields();
    if current_contents
        .as_ref()
        .is_some_and(|c| c.shape.kind() != key.kind())
    {
        return Err(KeyLifecycleError::InvalidState);
    }
    if matches!(request.expected, ExpectedGraphState::Entity(id) if id != fields.incarnation) {
        return Err(KeyLifecycleError::IncarnationConflict);
    }
    match request.revision.cmp(&fields.installed_revision) {
        std::cmp::Ordering::Less => {
            return Err(KeyLifecycleError::Stale {
                current: fields.installed_revision,
            });
        }
        std::cmp::Ordering::Equal => {
            if request.operation != fields.operation
                || request.revision != fields.requested_revision
                || request.expected != fields.expected
                || request.delete_mode != fields.delete_mode
            {
                return Err(KeyLifecycleError::RevisionConflict);
            }
            let equal = match (current_contents, request.contents) {
                (Some(current), Some(attempted)) => {
                    contents_equal(current, attempted, scratch, checkpoint)?
                }
                (None, None) => true,
                _ => false,
            };
            return if equal {
                Ok(KeyDecision::Replay(provenance))
            } else {
                Err(KeyLifecycleError::RevisionConflict)
            };
        }
        std::cmp::Ordering::Greater => {}
    }
    match (deleted, request.operation) {
        (false, GraphOperation::StructuredPut) => {
            let current = current_contents
                .as_ref()
                .ok_or(KeyLifecycleError::InvalidState)?;
            let requested = request
                .contents
                .as_ref()
                .ok_or(KeyLifecycleError::InvalidState)?;
            if !bounded::shapes(current.shape, requested.shape, checkpoint)? {
                return Err(KeyLifecycleError::RelationshipIdentityChange);
            }
        }
        (false, GraphOperation::StructuredDelete) => {}
        (false, GraphOperation::StructuredCreate) => return Err(KeyLifecycleError::AlreadyExists),
        (false, _) => return Err(KeyLifecycleError::NotDeleted),
        (true, GraphOperation::StructuredRecreate) => {
            if request.expected != ExpectedGraphState::Deletion(fields.installed_revision) {
                return Err(KeyLifecycleError::DeletionRevisionConflict);
            }
        }
        (true, _) => return Err(KeyLifecycleError::DeletedKey),
    }
    Ok(KeyDecision::Change(pending(
        key,
        &request,
        Some(provenance),
        deleted,
    )))
}

fn pending<'a>(
    key: ApplicationKey<'a>,
    request: &Request<'a>,
    previous: Option<OperationProvenance<'a>>,
    deleted: bool,
) -> PendingKeyChange<'a> {
    let fields = previous.map(OperationProvenance::fields);
    PendingKeyChange {
        key: Some(key),
        kind: key.kind(),
        operation: request.operation,
        revision: request.revision,
        expected: request.expected,
        delete_mode: request.delete_mode,
        existing: fields.filter(|_| !deleted).map(|f| f.incarnation),
        retired: fields.filter(|_| deleted).map(|f| f.incarnation),
        previous_generation: fields.map_or(GraphGeneration::new(0), |f| f.original_generation),
    }
}

fn validate_provenance(
    key: Option<ApplicationKey<'_>>,
    provenance: OperationProvenance<'_>,
    deleted: bool,
    checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
) -> Result<(), KeyLifecycleError> {
    let f = provenance.fields();
    let valid_origin = match f.operation {
        GraphOperation::StructuredCreate => {
            !deleted && f.key.is_some() && f.expected == ExpectedGraphState::Absent
        }
        GraphOperation::StructuredPut => {
            !deleted && f.key.is_some() && f.expected == ExpectedGraphState::Entity(f.incarnation)
        }
        GraphOperation::StructuredDelete => {
            deleted && f.key.is_some() && f.expected == ExpectedGraphState::Entity(f.incarnation)
        }
        GraphOperation::StructuredRecreate => {
            !deleted
                && f.key.is_some()
                && matches!(f.expected, ExpectedGraphState::Deletion(revision) if revision < f.installed_revision)
        }
        GraphOperation::CypherEdit => {
            f.expected == ExpectedGraphState::Entity(f.incarnation)
                || (!deleted && f.expected == ExpectedGraphState::Absent)
        }
    };
    if !valid_origin
        || bounded::keys(f.key, key, checkpoint)? != std::cmp::Ordering::Equal
        || f.requested_revision != f.installed_revision
        || f.original_generation.get() == 0
        || f.delete_mode.is_some() != deleted
    {
        return Err(KeyLifecycleError::InvalidState);
    }
    Ok(())
}

fn contents_equal(
    left: CanonicalRecord<'_>,
    right: CanonicalRecord<'_>,
    scratch: &mut [u8],
    checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
) -> Result<bool, KeyLifecycleError> {
    if !bounded::shapes(left.shape, right.shape, checkpoint)? {
        return Ok(false);
    }
    Ok(compare_canonical_streams(
        left.source,
        left.fingerprint,
        right.source,
        right.fingerprint,
        scratch,
        checkpoint,
    )?
    .equal)
}

/// Resolved structured target identities. Staging supplies both the complete key
/// and the resolved entity when present, so aliases cannot evade this gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchTarget<'a> {
    key: Option<ApplicationKey<'a>>,
    entity: Option<EntityId>,
}
impl<'a> BatchTarget<'a> {
    /// Validates the logical target without allocating or proving existence.
    pub fn new(
        key: Option<ApplicationKey<'a>>,
        entity: Option<EntityId>,
    ) -> Result<Self, KeyLifecycleError> {
        if key.is_none() && entity.is_none() {
            return Err(KeyLifecycleError::MissingTarget);
        }
        if key
            .zip(entity)
            .is_some_and(|(key, entity)| key.kind() != entity.kind())
        {
            return Err(KeyLifecycleError::KindMismatch);
        }
        Ok(Self { key, entity })
    }
}

/// Rejects identical as well as conflicting duplicate targets before per-item
/// staging. Sorts only caller-owned descriptors in place; request order remains
/// in the separate staging input. Uses bounded work and no heap allocation.
pub fn validate_distinct_targets(
    targets: &mut [BatchTarget<'_>],
    checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
) -> Result<(), KeyLifecycleError> {
    checkpoint()?;
    if targets.len() > MAX_GRAPH_CHANGES {
        return Err(KeyLifecycleError::TooManyTargets);
    }
    let mut bytes = 0_usize;
    for target in targets.iter() {
        checkpoint()?;
        let names = target.key.map_or(0, |key| {
            key.namespace().as_str().len() + key.key().as_str().len()
        });
        bytes = bytes
            .checked_add(32)
            .and_then(|bytes| bytes.checked_add(names))
            .ok_or(KeyLifecycleError::InputTooLarge)?;
        if bytes > MAX_GRAPH_INPUT_BYTES {
            return Err(KeyLifecycleError::InputTooLarge);
        }
    }
    bounded::sort(targets, checkpoint, |left, right, checkpoint| {
        bounded::keys(left.key, right.key, checkpoint)
    })?;
    let mut previous = None;
    for target in targets.iter() {
        checkpoint()?;
        if let Some(key) = target.key {
            if previous.is_some()
                && bounded::keys(previous, Some(key), checkpoint)? == std::cmp::Ordering::Equal
            {
                return Err(KeyLifecycleError::DuplicateTarget);
            }
            previous = Some(key);
        }
    }
    bounded::sort(targets, checkpoint, |left, right, checkpoint| {
        checkpoint()?;
        Ok(left.entity.cmp(&right.entity))
    })?;
    let mut previous = None;
    for target in targets.iter() {
        checkpoint()?;
        if let Some(entity) = target.entity {
            if previous == Some(entity) {
                return Err(KeyLifecycleError::DuplicateTarget);
            }
            previous = Some(entity);
        }
    }
    Ok(())
}

/// Logical batch disposition; actual commit/publication is a later stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchDisposition {
    /// No entity, allocator, fence or other durable participant changes.
    NoOp,
    /// Every effective item replays, possibly at different original generations.
    Replayed,
    /// Some required durable state changes; one coordinated commit is needed.
    Changed,
}

/// Bounded logical summary preserving the supplied per-item decisions intact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchClassification {
    /// Effective outcome after all items and durable participants are validated.
    pub disposition: BatchDisposition,
    /// Coherent generation admitted before this request.
    pub admitted_generation: GraphGeneration,
    /// Checked target generation, present only when something must commit.
    pub changed_generation: Option<GraphGeneration>,
    /// Entity changes, excluding replayed and unchanged items.
    pub changed_items: usize,
    /// Items retaining their own original generation in the supplied decisions.
    pub replayed_items: usize,
}

/// Summarizes already validated distinct-target decisions. Call only after the
/// duplicate gate and all per-item validation have succeeded. The coordinator
/// supplies whether allocators/fences/catalog or another durable participant
/// changed; create-then-delete must set that fact despite a net-empty image.
/// This function performs no writes and does not claim public no-WAL proof.
pub fn summarize_key_batch(
    admitted_generation: GraphGeneration,
    decisions: &[KeyDecision<'_>],
    other_durable_changes: bool,
    checkpoint: &mut dyn FnMut() -> Result<(), CanonicalError>,
) -> Result<BatchClassification, KeyLifecycleError> {
    if decisions.len() > MAX_GRAPH_CHANGES {
        return Err(KeyLifecycleError::TooManyTargets);
    }
    let mut changed_items = 0;
    let mut replayed_items = 0;
    checkpoint()?;
    for decision in decisions {
        checkpoint()?;
        match decision {
            KeyDecision::Change(_) => changed_items += 1,
            KeyDecision::Replay(_) => replayed_items += 1,
            KeyDecision::NoOp => {}
        }
    }
    let disposition = if changed_items != 0 || other_durable_changes {
        BatchDisposition::Changed
    } else if replayed_items != 0 {
        BatchDisposition::Replayed
    } else {
        BatchDisposition::NoOp
    };
    let changed_generation = if disposition == BatchDisposition::Changed {
        Some(GraphGeneration::new(
            admitted_generation
                .get()
                .checked_add(1)
                .ok_or(KeyLifecycleError::GenerationOverflow)?,
        ))
    } else {
        None
    };
    Ok(BatchClassification {
        disposition,
        admitted_generation,
        changed_generation,
        changed_items,
        replayed_items,
    })
}

#[cfg(all(test, feature = "allocation-audit"))]
mod allocation_tests;
