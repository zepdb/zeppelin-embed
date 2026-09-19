use super::{
    DomainError, EntityId, EntityKind, GraphGeneration, GraphRevision, MAX_GRAPH_INPUT_BYTES,
};

/// Borrowed, byte-exact UTF-8 graph name. No normalization or path restrictions.
/// Empty names and embedded NUL are data, not C string terminators.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphName<'a>(&'a str);

impl<'a> GraphName<'a> {
    /// Validates a name without allocating or changing its bytes.
    pub const fn new(value: &'a str) -> Result<Self, DomainError> {
        if value.len() > MAX_GRAPH_INPUT_BYTES {
            Err(DomainError::InputTooLarge)
        } else {
            Ok(Self(value))
        }
    }

    /// Validates an external byte span before treating it as a name.
    pub fn from_utf8(value: &'a [u8]) -> Result<Self, DomainError> {
        if value.len() > MAX_GRAPH_INPUT_BYTES {
            return Err(DomainError::InputTooLarge);
        }
        Self::new(std::str::from_utf8(value).map_err(|_| DomainError::InvalidUtf8)?)
    }

    /// Returns the original bytes as UTF-8.
    #[must_use]
    pub const fn as_str(self) -> &'a str {
        self.0
    }
}

/// An application identity, independent of labels and display-name properties.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ApplicationKey<'a> {
    kind: EntityKind,
    namespace: GraphName<'a>,
    key: GraphName<'a>,
}

impl<'a> ApplicationKey<'a> {
    /// Validates a kind-scoped namespace/key pair without copying it.
    pub fn new(kind: EntityKind, namespace: &'a str, key: &'a str) -> Result<Self, DomainError> {
        if namespace
            .len()
            .checked_add(key.len())
            .is_none_or(|bytes| bytes > MAX_GRAPH_INPUT_BYTES)
        {
            return Err(DomainError::InputTooLarge);
        }
        Ok(Self {
            kind,
            namespace: GraphName::new(namespace)?,
            key: GraphName::new(key)?,
        })
    }

    /// Returns the key domain.
    #[must_use]
    pub const fn kind(self) -> EntityKind {
        self.kind
    }
    /// Returns the exact application namespace.
    #[must_use]
    pub const fn namespace(self) -> GraphName<'a> {
        self.namespace
    }
    /// Returns the exact key within the namespace.
    #[must_use]
    pub const fn key(self) -> GraphName<'a> {
        self.key
    }
}

/// Engine identity/retry metadata, kept outside the user property map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntityMetadata<'a> {
    id: EntityId,
    key: Option<ApplicationKey<'a>>,
    revision: GraphRevision,
    last_change_generation: GraphGeneration,
}

impl<'a> EntityMetadata<'a> {
    /// Validates that the optional key belongs to the entity's domain.
    pub fn new(
        id: EntityId,
        key: Option<ApplicationKey<'a>>,
        revision: GraphRevision,
        last_change_generation: GraphGeneration,
    ) -> Result<Self, DomainError> {
        if key.is_some_and(|key| key.kind() != id.kind()) {
            return Err(DomainError::EntityKindMismatch);
        }
        Ok(Self {
            id,
            key,
            revision,
            last_change_generation,
        })
    }

    /// Returns the complete identity and kind.
    #[must_use]
    pub const fn id(self) -> EntityId {
        self.id
    }
    /// Returns the optional application identity.
    #[must_use]
    pub const fn key(self) -> Option<ApplicationKey<'a>> {
        self.key
    }
    /// Returns the current revision, including on unkeyed entities.
    #[must_use]
    pub const fn revision(self) -> GraphRevision {
        self.revision
    }
    /// Returns the generation that last changed the entity.
    #[must_use]
    pub const fn last_change_generation(self) -> GraphGeneration {
        self.last_change_generation
    }
}
