//! Bounded private write admission with read-only base access and no publication.
use super::catalog::{GraphInterpretation, Symbol, SymbolHighWaters, SymbolKind};
use super::storage::artifact::ArtifactId;
use super::*;
mod bounded;
mod encode;
mod images;
mod integrity;
mod memory;
mod overlay;
mod symbols;
pub use images::{
    NodeImageBudget, NodeImageBuilder, RelationshipImageBudget, RelationshipImageBuilder,
    StatementImages,
};
pub use memory::{WriteAdoptionError, WriteMemory, WriteReservation};
pub use overlay::{BatchEntityRef, GraphBatchReadView, OverlayCounters};
mod result;
pub use result::{
    MaterializedBatch, ResultLayout, ResultMaterializer, ResultRegistration,
    ScopedMaterializedBatch, stage_structured_with_results,
    stage_structured_with_results_at_generation,
};

/// Identity of one retained, coherent graph/search root set.
#[derive(Clone, Copy, Debug)]
pub struct BaseIdentity {
    /// Persisted store incarnation.
    pub store: StoreInstanceId,
    /// Admitted published generation.
    pub generation: GraphGeneration,
    /// Legacy checkpoint artifact used only by the old reclaim/recovery path.
    /// removed by ZE-346 when the graph WAL and root selector are deleted
    pub roots: Option<ArtifactId>,
    /// In-memory position of the last graph fold.
    pub fold: FoldMark,
}
impl PartialEq for BaseIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.store == other.store && self.generation == other.generation && self.fold == other.fold
    }
}
impl Eq for BaseIdentity {}
/// In-memory fold position; never encoded in a persisted format.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FoldMark {
    /// Generation of the manifest that folded the graph state.
    pub manifest_generation: u64,
    /// Log sequence absorbed by that fold.
    pub graph_absorbed_through: u64,
    /// Graph commit-envelope sequence at the fold.
    pub envelope_sequence: u64,
}
/// Inclusive logical allocation fences from the admitted root.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HighWaters {
    /// Largest allocated node identity, including subsequently deleted nodes.
    pub node: u128,
    /// Largest allocated relationship identity.
    pub relationship: u128,
    /// Independently allocated dictionary domains.
    pub symbols: SymbolHighWaters,
}
/// Cooperative cancellation and deterministic private-preparation fault sites.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WritePhase {
    /// Input, reference and admitted-base checks.
    Validate,
    /// All preconditions passed; checked private logical identity assignment.
    Identity,
    /// A fallible charged allocation, before requesting backing memory.
    Allocate,
    /// Exact canonical comparison/encoding work.
    Canonical,
    /// Bounded alive-incident probe for ordinary DELETE.
    Incident,
    /// Core result materialization before any publication.
    CoreResult,
    /// Binding result and registry reservation before any publication.
    AbiResult,
    /// Progressive pending property/text access.
    Overlay,
    /// Final coherent-view recheck.
    Finalize,
}
/// One fallible checkpoint invoked during bounded work, never after publication.
pub type WriteControl<'a> = dyn FnMut(WritePhase) -> Result<(), StageError> + 'a;
/// A definite rejection: no durable mutation or newly allocated ID escapes.
#[derive(Debug)]
pub enum StageError {
    /// Input or normalized output exceeds a declared admission cap.
    Limit,
    /// Requested caps widen the accepted profile.
    InvalidLimits,
    /// Caller cancellation or injected private-preparation failure.
    Cancelled,
    /// Existing endpoint is absent/deleted, or a local slot is invalid.
    Endpoint,
    /// An admitted base record comes from a different root set.
    ViewMismatch,
    /// A logical allocator cannot advance without wrapping.
    IdentityOverflow,
    /// A required existing target is absent.
    MissingEntity,
    /// Ordinary DELETE leaves a live incident relationship.
    IncidentRelationship,
    /// A pending access targets an entity already deleted in this statement.
    DeletedEntity,
    /// Input shape or entity kind is inconsistent.
    InvalidInput,
    /// Exact canonical/lifecycle validation failed.
    Lifecycle(KeyLifecycleError),
    /// Canonical input or streaming failed.
    Canonical(CanonicalError),
    /// Catalog interpretation or symbol validation failed.
    Catalog(catalog::CatalogError),
    /// Shared store memory accounting or backing allocation failed.
    Memory(crate::lifecycle::StoreError),
    /// Exact native storage lookup/stream failure from the admitted base.
    NativeStorage(crate::property_graph::storage::tree::directory::TreeError),
}
impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lifecycle(e) => e.fmt(f),
            Self::Canonical(e) => e.fmt(f),
            Self::Catalog(e) => e.fmt(f),
            Self::Memory(e) => e.fmt(f),
            Self::NativeStorage(e) => e.fmt(f),
            other => write!(f, "graph staging rejected: {other:?}"),
        }
    }
}
impl std::error::Error for StageError {}
impl From<KeyLifecycleError> for StageError {
    fn from(e: KeyLifecycleError) -> Self {
        Self::Lifecycle(e)
    }
}
impl From<CanonicalError> for StageError {
    fn from(e: CanonicalError) -> Self {
        Self::Canonical(e)
    }
}
impl From<catalog::CatalogError> for StageError {
    fn from(e: catalog::CatalogError) -> Self {
        Self::Catalog(e)
    }
}
impl From<crate::lifecycle::StoreError> for StageError {
    fn from(e: crate::lifecycle::StoreError) -> Self {
        Self::Memory(e)
    }
}
impl From<crate::property_graph::storage::tree::directory::TreeError> for StageError {
    fn from(e: crate::property_graph::storage::tree::directory::TreeError) -> Self {
        Self::NativeStorage(e)
    }
}
/// Limits may only tighten the accepted bounded profile.
#[derive(Clone, Copy, Debug)]
pub struct WriteLimits {
    /// Total normalized entity changes.
    pub changes: usize,
    /// Complete canonical payload and provenance framing.
    pub input_bytes: usize,
    /// Complete overlapping writer preparation, including results.
    pub writer_bytes: usize,
    /// Materialized result rows.
    pub result_rows: usize,
    /// Each complete core or ABI arena; registry overhead also charges writer/shared.
    pub result_bytes: usize,
}
impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            changes: MAX_GRAPH_CHANGES,
            input_bytes: MAX_GRAPH_INPUT_BYTES,
            writer_bytes: 64 * 1024 * 1024,
            result_rows: 65_536,
            result_bytes: 4 * 1024 * 1024,
        }
    }
}
impl WriteLimits {
    fn validate(self) -> Result<(), StageError> {
        let max = Self::default();
        if self.changes > max.changes
            || self.input_bytes > max.input_bytes
            || self.writer_bytes > max.writer_bytes
            || self.result_rows > max.result_rows
            || self.result_bytes > max.result_bytes
        {
            return Err(StageError::InvalidLimits);
        }
        Ok(())
    }
}
/// Original membership, independent of whether an optional payload is empty.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Membership {
    /// Node belongs to the lexical participant.
    pub text: bool,
    /// Node belongs to the vector participant.
    pub vector: bool,
}
/// A retained live record; all fields must come from `view`.
#[derive(Clone, Copy)]
pub struct BaseEntity<'a> {
    /// Exact admitted root token.
    pub view: BaseIdentity,
    /// Complete installing operation evidence.
    pub provenance: OperationProvenance<'a>,
    /// Validated immutable topology.
    pub shape: EntityShape<'a>,
    /// Exact length and non-authoritative hash precheck.
    pub fingerprint: CanonicalFingerprint,
    /// Retained, bounded random-read logical image; extents need not be copied.
    pub source: &'a dyn CanonicalSource,
    /// Original node search membership.
    pub membership: Membership,
}
/// Retained canonical byte source. Implementations must honor the requested
/// bounded span and reject truncated/corrupt extents; no implicit full-image copy.
pub trait CanonicalSource {
    /// Reads at most `output.len()` bytes from the logical image.
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize>;
}
/// Borrowed contiguous implementation useful for retained inline images.
pub struct CanonicalSlice<'a>(pub &'a [u8]);
impl CanonicalSource for CanonicalSlice<'_> {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        let offset = usize::try_from(offset).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        let source = self
            .0
            .get(offset..)
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        let count = source.len().min(output.len());
        output
            .get_mut(..count)
            .ok_or(std::io::ErrorKind::InvalidInput)?
            .copy_from_slice(
                source
                    .get(..count)
                    .ok_or(std::io::ErrorKind::InvalidInput)?,
            );
        Ok(count)
    }
}
/// Kind-scoped application-key state at one admitted base.
pub enum BaseKeyState<'a> {
    /// No prior incarnation or fence.
    NeverUsed,
    /// Live canonical entity.
    Live(BaseEntity<'a>),
    /// Retained deletion fence and its coherent-view token.
    Deleted(BaseIdentity, OperationProvenance<'a>),
}
/// Read-only adapter for an already retained graph/search view. Implementations
/// must stream/page their bounded probes and honor the supplied checkpoint;
/// this trait has no mutation, traversal-overlay or publication method.
pub trait AdmittedBase {
    /// Live document identity sharing this node ID, in the same admission.
    fn document_version(
        &self,
        _node: NodeId,
    ) -> Result<Option<crate::ingest::DocumentVersion>, StageError> {
        Ok(None)
    }
    /// A directory record, including a retained node tombstone.
    fn has_node_record(
        &self,
        node: NodeId,
        control: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        Ok(self.entity(EntityId::Node(node), control)?.is_some())
    }

    /// Whether a candidate identity is already occupied in this admitted store.
    fn node_id_reserved(
        &self,
        node: NodeId,
        control: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        Ok(self.entity(EntityId::Node(node), control)?.is_some())
    }

    /// Whether a caller-selected identity existed before this mixed batch.
    fn caller_node_id_reserved(
        &self,
        node: NodeId,
        control: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        self.node_id_reserved(node, control)
    }

    /// Whether this admitted catalog declares any incoming-reference policies.
    fn has_relationship_rules(&self) -> bool {
        false
    }
    /// Visits live incoming edges with declared policies. Empty catalogs never
    /// invoke this operation; an adapter claiming policies must implement it.
    fn visit_incoming_rules(
        &self,
        _node: NodeId,
        _visit: &mut dyn FnMut(RelId, NodeId, catalog::OnDelete) -> Result<(), StageError>,
        _control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        Err(StageError::InvalidInput)
    }

    /// Exact coherent view identity, stable throughout admission.
    fn identity(&self) -> BaseIdentity;
    /// Inclusive persisted allocator fences.
    fn high_waters(&self) -> HighWaters;
    /// Store document and lexical interpretation.
    fn interpretation(&self) -> GraphInterpretation<'_>;
    /// Key lookup against this base, never a progressive overlay.
    fn key(
        &self,
        key: ApplicationKey<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError>;
    /// Live entity lookup against this base.
    fn entity(
        &self,
        id: EntityId,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError>;
    /// Stops on the first live incident edge not explicitly removed. Both
    /// endpoints must be alive in the admitted graph. No incident list is copied.
    fn has_live_incident(
        &self,
        node: NodeId,
        removed: &[RelId],
        control: &mut WriteControl<'_>,
    ) -> Result<bool, StageError>;
    /// Borrowed exact property at the admitted base; absence is distinct from
    /// a present empty value. The source lease owns any returned backing bytes.
    fn property(
        &self,
        entity: EntityId,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'_>>, StageError>;
    /// Original lexical payload, preserving absent versus present-empty text.
    fn stored_text(
        &self,
        node: NodeId,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<&str>, StageError>;
    /// Existing exact dictionary name; None requests a private new assignment.
    fn symbol(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError>;
}
/// Full logical image input; local relationship endpoints resolve within this batch.
#[derive(Clone, Copy)]
pub enum WriteImage<'a, 'batch> {
    /// Already validated node image. A relationship image is rejected here.
    Node(&'a CanonicalContents<'a>),
    /// Relationship properties and fixed directed endpoints.
    Relationship {
        /// Existing or branded batch-local source.
        source: NodeRef<'batch>,
        /// Existing or branded batch-local target.
        target: NodeRef<'batch>,
        /// Exactly one byte-exact relationship type.
        relationship_type: GraphName<'a>,
        /// Borrowed full replacement property map, normalized privately.
        properties: &'a [GraphProperty<'a>],
    },
}
/// Structured key request. Every item is classified against the same base.
#[derive(Clone, Copy)]
pub struct StructuredWrite<'a, 'batch> {
    /// Kind-scoped application identity.
    pub key: ApplicationKey<'a>,
    /// Explicit positive requested revision.
    pub revision: GraphRevision,
    /// Complete operation/precondition.
    pub operation: StructuredOperation,
    /// Required for live writes and absent for deletion.
    pub image: Option<WriteImage<'a, 'batch>>,
}
/// Exact structured lifecycle operation.
#[derive(Clone, Copy, Debug)]
pub enum StructuredOperation {
    /// First creation with explicit absence.
    Create,
    /// First creation of a document node with a caller-selected identity.
    CreateWithId(NodeId),
    /// Full replacement of the expected incarnation.
    Put(EntityId),
    /// Deletion of the expected incarnation and explicit integrity mode.
    Delete(EntityId, GraphDeleteMode),
    /// Explicit acknowledgement of the current deletion fence.
    Recreate(GraphRevision),
}
/// Fully reserved per-item acknowledgement; no ID is returned on rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ItemReceipt {
    /// Installed or replayed incarnation.
    pub entity: EntityId,
    /// Installed or replayed revision.
    pub revision: GraphRevision,
    /// Original generation for replay, new generation for changed work.
    pub generation: GraphGeneration,
    /// Exact replay classification.
    pub replayed: bool,
}
/// One finalized changed entity; exact replays never enter this participant list.
pub struct NormalizedDelta<'a> {
    document: Option<crate::ingest::DocumentVersion>,
    provenance: OperationProvenance<'a>,
    canonical: Option<memory::Arena<'a, u8>>,
    shape: Option<EntityShape<'a>>,
    before: Membership,
    after: Membership,
}
impl<'a> NormalizedDelta<'a> {
    /// Exact backing document version, when this node is document-backed.
    pub const fn document_version(&self) -> Option<crate::ingest::DocumentVersion> {
        self.document
    }
    /// Complete installing operation and key-fence evidence.
    pub const fn provenance(&self) -> OperationProvenance<'a> {
        self.provenance
    }
    /// Owned exact logical image, absent for a tombstone.
    pub fn canonical(&self) -> Option<&[u8]> {
        self.canonical.as_deref()
    }
    /// Fixed live topology; absent for a tombstone.
    pub const fn shape(&self) -> Option<EntityShape<'a>> {
        self.shape
    }
    /// Atomic lexical/vector membership transition alongside the entity change.
    pub const fn membership(&self) -> (Membership, Membership) {
        (self.before, self.after)
    }
}
/// Private normalized batch. Publication is exclusively the coordinator's job.
pub struct StagedBatch<'a> {
    base: BaseIdentity,
    target_generation: GraphGeneration,
    high_waters: HighWaters,
    receipts: memory::Arena<'a, ItemReceipt>,
    deltas: memory::Arena<'a, NormalizedDelta<'a>>,
    disposition: BatchDisposition,
    symbols: memory::Arena<'a, catalog::SymbolEntry<'a>>,
}
impl StagedBatch<'_> {
    pub(crate) fn include_document_change(&mut self, generation: GraphGeneration) {
        self.disposition = BatchDisposition::Changed;
        self.target_generation = generation;
    }

    /// Advances inclusive logical fences on an otherwise empty test batch.
    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn jump_allocators_for_test(
        &mut self,
        next_node: crate::property_graph::NodeId,
        next_relationship: crate::property_graph::RelId,
    ) -> Result<(), StageError> {
        let node = next_node
            .get()
            .checked_sub(1)
            .ok_or(StageError::InvalidInput)?;
        let relationship = next_relationship
            .get()
            .checked_sub(1)
            .ok_or(StageError::InvalidInput)?;
        if !self.deltas.is_empty()
            || !self.receipts.is_empty()
            || !self.symbols.is_empty()
            || node < self.high_waters.node
            || relationship < self.high_waters.relationship
            || (node == self.high_waters.node && relationship == self.high_waters.relationship)
        {
            return Err(StageError::InvalidInput);
        }
        self.high_waters.node = node;
        self.high_waters.relationship = relationship;
        self.disposition = BatchDisposition::Changed;
        Ok(())
    }

    pub(crate) fn uses_memory(&self, memory: &WriteMemory<'_>) -> bool {
        self.deltas.uses_memory(memory)
            && self.receipts.uses_memory(memory)
            && self.symbols.uses_memory(memory)
    }
    /// Retained base token for prepare/recheck.
    pub const fn base(&self) -> BaseIdentity {
        self.base
    }
    /// Proposed commit generation; unchanged batches do not publish it.
    pub const fn target_generation(&self) -> GraphGeneration {
        self.target_generation
    }
    /// Private inclusive allocation fences; not installed by staging.
    pub const fn high_waters(&self) -> HighWaters {
        self.high_waters
    }
    /// Changed graph/search/key-fence participants; never contains replay rows.
    pub fn deltas(&self) -> &[NormalizedDelta<'_>] {
        &self.deltas
    }
    /// Newly assigned names only; the admitted catalog is never copied.
    pub fn symbols(&self) -> &[catalog::SymbolEntry<'_>] {
        &self.symbols
    }
    /// Successful logical disposition before any publication.
    pub const fn disposition(&self) -> BatchDisposition {
        self.disposition
    }
    /// Fully materialized item acknowledgements.
    pub fn receipts(&self) -> &[ItemReceipt] {
        &self.receipts
    }
}
mod documents;
mod structured;
pub use structured::{stage_structured, stage_structured_at_generation};

#[cfg(all(test, feature = "allocation-audit"))]
mod tests;
