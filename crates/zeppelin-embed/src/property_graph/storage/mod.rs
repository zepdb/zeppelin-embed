//! Checked immutable graph containers. These codecs do not admit a GraphStore,
//! publish roots, replay a WAL, or authorize cleanup.

pub mod allocation;
pub mod artifact;
pub mod tree;

/// Bounded immutable adjacency codecs and range merging.
pub mod adjacency;

/// Bounded, role-checked logical streams over immutable physical chunks.
pub mod payload;

/// Retained logical windows and bounded field decoding over payload extents.
pub mod stream;

/// Lossless logical records and their required semantic validation hooks.
pub mod records;

mod view;
#[cfg(feature = "graph-cypher")]
#[allow(
    unused_imports,
    reason = "crate-private ZE-45 interface is consumed by ZE-50 after this dependency lands"
)]
pub(crate) use view::{
    CursorState, DirectionSelection, ExpandCursor, GraphReadView, LabelSelection, NativeCatalog,
    NativeQuerySource, NativeReadCapability, NodeCursor, NodeView, PreparedGraphArtifacts,
    PreparedGraphFailure, RelationshipTypeSelection, TextPayloadReader,
};

/// Combined private storage capacity inside the authentic writer/store owner.
pub mod memory;

/// Packed private artifacts and explicit owned abort inventories.
pub mod prepared;

/// Persisted immutable allocation descriptors and reclamation state.
pub mod inventory;

/// Native directory changes from one authentic normalized writer batch.
pub mod participant;
