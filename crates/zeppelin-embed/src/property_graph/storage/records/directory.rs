//! Mandatory interpretation of native participant leaf values before COW.
use super::*;
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::tree::directory::{
    DirectoryEntry, DirectoryRoot, LeafValidator,
};
use crate::property_graph::storage::tree::{Key, TreeKind};
use crate::property_graph::{EntityId, RelId};

/// Owning-role validation for nodes, relationships, fences and empty memberships.
/// This retains borrowed catalog context and allocates no per-entry backing.
pub struct NativeDirectoryValues<'a, C> {
    catalog: &'a C,
    document: Option<&'a EmbeddingTower>,
}
impl<'a, C> NativeDirectoryValues<'a, C> {
    /// Borrow the same-view catalog and document-vector configuration.
    pub const fn new(catalog: &'a C, document: Option<&'a EmbeddingTower>) -> Self {
        Self { catalog, document }
    }
}
impl<S: BlockSource, C: RecordCatalog<S>> LeafValidator<S> for NativeDirectoryValues<'_, C> {
    fn verify(
        &mut self,
        source: &S,
        root: DirectoryRoot,
        entry: DirectoryEntry<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        entry.require_root(root)?;
        match root.kind() {
            TreeKind::Nodes | TreeKind::Relationships => {
                let Key::Inline(key) = entry.key() else {
                    return Err(TreeError::Invalid("overflow native identity"));
                };
                let id = u128::from_le_bytes(
                    key.try_into()
                        .map_err(|_| TreeError::Invalid("native directory key width"))?,
                );
                let reference = PayloadRef::decode(entry.value())?;
                let bytes =
                    PayloadSlice::new(source, root.store(), entry.creation_generation(), reference);
                if root.kind() == TreeKind::Nodes {
                    verify_node_state(
                        bytes,
                        NodeId::new(id).map_err(|_| TreeError::Invalid("zero node identity"))?,
                        self.catalog,
                        self.document,
                        r,
                    )?;
                } else {
                    verify_record(
                        bytes,
                        EntityId::Relationship(
                            RelId::new(id)
                                .map_err(|_| TreeError::Invalid("zero relationship identity"))?,
                        ),
                        self.catalog,
                        self.document,
                        r,
                    )?;
                }
            }
            TreeKind::KeyFences => {
                verify_fence_entry(source, root, entry, self.catalog, self.document, r)?;
            }
            TreeKind::Labels | TreeKind::RelationshipTypes => {
                if !entry.value().is_empty() {
                    return Err(TreeError::Invalid("nonempty membership value"));
                }
            }
            _ => return Err(TreeError::Invalid("foreign native directory role")),
        }
        r.step(0)
    }
}
