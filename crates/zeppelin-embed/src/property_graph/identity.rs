use super::DomainError;

/// Independent application-key and identity domains.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EntityKind {
    /// A graph node.
    Node,
    /// A directed graph relationship.
    Relationship,
}

/// A logical identity retaining its entity kind.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EntityId {
    /// A node identity.
    Node(NodeId),
    /// A relationship identity.
    Relationship(RelId),
}

impl EntityId {
    /// Returns the identity domain, without consulting storage.
    #[must_use]
    pub const fn kind(self) -> EntityKind {
        match self {
            Self::Node(_) => EntityKind::Node,
            Self::Relationship(_) => EntityKind::Relationship,
        }
    }
}

/// The generation of a coherent graph/search view; zero is the initial root.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct GraphGeneration(u64);

impl GraphGeneration {
    /// Decodes a full-width generation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the full generation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

macro_rules! identity {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u128);

        impl $name {
            /// Validates all 128 bits of a store-local identity; zero is invalid.
            /// This decodes an identity, it does not allocate or prove existence.
            pub const fn new(value: u128) -> Result<Self, DomainError> {
                if value == 0 {
                    Err(DomainError::ZeroIdentity)
                } else {
                    Ok(Self(value))
                }
            }

            /// Returns the complete identity without narrowing.
            #[must_use]
            pub const fn get(self) -> u128 {
                self.0
            }
        }
    };
}

identity!(
    NodeId,
    "Stable store-local node identity, unrelated to a physical row.\n\n```compile_fail\nuse zeppelin_embed::property_graph::{NodeId, RelId};\nlet node: NodeId = RelId::new(1).unwrap();\n```\n\n```\nuse zeppelin_embed::{ingest::DocId, property_graph::NodeId};\nlet node: NodeId = DocId::new(0).into();\nassert_eq!(node.get(), 0);\n```"
);
identity!(
    RelId,
    "Stable store-local relationship identity, distinct from node identity."
);
identity!(
    StoreInstanceId,
    "Persisted store identity, distinct from either kind of entity identity."
);

/// Positive revision of a graph entity or application key, including deletion fences.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct GraphRevision(u64);

impl GraphRevision {
    /// Validates a revision independently of legacy document revisions.
    pub const fn new(value: u64) -> Result<Self, DomainError> {
        if value == 0 {
            Err(DomainError::ZeroRevision)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the full revision.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances once or rejects; never wraps or saturates.
    pub const fn checked_next(self) -> Result<Self, DomainError> {
        match self.0.checked_add(1) {
            Some(value) => Ok(Self(value)),
            None => Err(DomainError::RevisionOverflow),
        }
    }
}

impl From<crate::ingest::DocId> for NodeId {
    fn from(document: crate::ingest::DocId) -> Self {
        Self(document.get())
    }
}
impl From<NodeId> for crate::ingest::DocId {
    fn from(node: NodeId) -> Self {
        Self::new(node.get())
    }
}
