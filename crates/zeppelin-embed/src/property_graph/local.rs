use std::marker::PhantomData;

use super::{DomainError, MAX_GRAPH_CHANGES, NodeId, RelId};

// Invariance prevents references from different callback scopes being unified.
type Scope<'batch> = PhantomData<fn(&'batch ()) -> &'batch ()>;

/// A constructor capability for local slots in exactly one batch scope.
/// Slot existence/kind are additionally checked against the submitted batch.
#[derive(Clone, Copy, Debug)]
pub struct LocalRefs<'batch>(Scope<'batch>);

/// Runs a batch builder in a fresh scope. Local references cannot escape it.
/// No graph mutations occur here; the callback submits through the write API.
///
/// ```compile_fail
/// use zeppelin_embed::property_graph::with_local_refs;
/// let escaped = with_local_refs(|scope| scope.node(0).unwrap());
/// ```
///
/// ```compile_fail
/// use zeppelin_embed::property_graph::{with_local_refs, NodeRef};
/// with_local_refs(|outer| with_local_refs(|inner| {
///     let mut endpoint = NodeRef::Local(outer.node(0).unwrap());
///     endpoint = NodeRef::Local(inner.node(0).unwrap());
/// }));
/// ```
pub fn with_local_refs<R>(build: impl for<'batch> FnOnce(LocalRefs<'batch>) -> R) -> R {
    build(LocalRefs(PhantomData))
}

macro_rules! local_ref {
    ($name:ident, $method:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name<'batch> {
            index: u32,
            scope: Scope<'batch>,
        }

        impl $name<'_> {
            /// Returns the batch-local slot, never a durable entity identity.
            #[must_use]
            pub const fn index(self) -> u32 {
                self.index
            }
        }

        impl<'batch> LocalRefs<'batch> {
            #[doc = $doc]
            pub fn $method(self, index: usize) -> Result<$name<'batch>, DomainError> {
                if index >= MAX_GRAPH_CHANGES {
                    return Err(DomainError::LocalReferenceOutOfRange);
                }
                let index =
                    u32::try_from(index).map_err(|_| DomainError::LocalReferenceOutOfRange)?;
                Ok($name {
                    index,
                    scope: self.0,
                })
            }
        }
    };
}

local_ref!(
    LocalNodeRef,
    node,
    "A node creation slot, confined to its batch scope."
);
local_ref!(
    LocalRelRef,
    relationship,
    "A relationship creation slot, confined to its batch scope."
);

/// A node endpoint distinguishes existing identity from a private creation slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeRef<'batch> {
    /// A store-local identity whose liveness is checked during staging.
    Existing(NodeId),
    /// A node created in this same batch.
    Local(LocalNodeRef<'batch>),
}

/// A relationship target distinguishes durable identity from a creation slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelRef<'batch> {
    /// A store-local relationship identity.
    Existing(RelId),
    /// A relationship created in this same batch.
    Local(LocalRelRef<'batch>),
}
