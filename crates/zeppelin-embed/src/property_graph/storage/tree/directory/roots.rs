//! Fixed physical root positions, never a read-view admission capability.
use super::*;

/// The eight native directories at one proposed/admitted generation. Constructing
/// this metadata does not validate artifacts or atomically acquire a store lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphRoots {
    store: StoreInstanceId,
    generation: GraphGeneration,
    references: [Option<PhysicalRef>; 8],
}
const KINDS: [TreeKind; 8] = [
    TreeKind::Nodes,
    TreeKind::Relationships,
    TreeKind::KeyFences,
    TreeKind::Labels,
    TreeKind::RelationshipTypes,
    TreeKind::OutRanges,
    TreeKind::InRanges,
    TreeKind::ObjectInventory,
];
impl GraphRoots {
    /// Decode the established Node/Rel/Fence/Label/Type/OUT/IN/Inventory order.
    /// Every present reference must name a supported TreePage. Actual comparator
    /// role and required descendants are checked by tree/record verification.
    pub fn from_references(
        store: StoreInstanceId,
        generation: GraphGeneration,
        references: [Option<PhysicalRef>; 8],
    ) -> Result<Self, TreeError> {
        for (kind, reference) in KINDS.into_iter().zip(references) {
            DirectoryRoot::from_reference(store, kind, generation, reference)?;
        }
        Ok(Self {
            store,
            generation,
            references,
        })
    }
    /// Exact immutable store incarnation; this is not an allocation namespace.
    pub const fn store(self) -> StoreInstanceId {
        self.store
    }
    /// The common upper generation of this physical bundle.
    pub const fn generation(self) -> GraphGeneration {
        self.generation
    }
    /// Fixed ordered physical descriptors for the sole publication owner.
    pub const fn references(self) -> [Option<PhysicalRef>; 8] {
        self.references
    }
    /// One directory with its comparator role bound by its root position.
    pub fn directory(self, kind: TreeKind) -> Result<DirectoryRoot, TreeError> {
        let reference = self
            .references
            .get(kind as usize - 1)
            .copied()
            .ok_or(TreeError::Invalid("graph root position"))?;
        DirectoryRoot::from_reference(self.store, kind, self.generation, reference)
    }
    /// Start a private candidate sharing every old immutable physical root.
    pub fn for_generation(self, generation: GraphGeneration) -> Result<Self, TreeError> {
        if generation < self.generation {
            return Err(TreeError::Invalid("root bundle generation regressed"));
        }
        Ok(Self { generation, ..self })
    }
    /// Replace one private root only within this exact store and generation.
    /// Publishing graph/search roots atomically remains the coordinator's task.
    pub fn replace(&mut self, root: DirectoryRoot) -> Result<(), TreeError> {
        if root.store != self.store || root.generation != self.generation {
            return Err(TreeError::Invalid(
                "mixed store or generation in graph roots",
            ));
        }
        *self
            .references
            .get_mut(root.kind as usize - 1)
            .ok_or(TreeError::Invalid("root position"))? = root.reference;
        Ok(())
    }
}
