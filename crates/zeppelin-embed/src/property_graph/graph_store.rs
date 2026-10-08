//! Graph operations on the unified Store handle.

#![allow(
    clippy::result_large_err,
    reason = "the typed graph cause stays unboxed and allocation-free, as in GraphQueryError"
)]

use crate::epoch::EmbeddingTower;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeGraphError;
use crate::lifecycle::{AccessMode, OpenOptions, QueryControl, Store, StoreErrorKind};
use crate::property_graph::query::completed::{
    GraphQueryCause, GraphQueryError, GraphQueryErrorKind, native_graph_error_kind,
};
use crate::property_graph::query::plan::PlanNodeId;
use crate::property_graph::query::runtime::WorkCounters;
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{
    ItemReceipt, StageError, StructuredOperation, StructuredWrite,
};
use crate::property_graph::{BatchDisposition, EntityId, GraphGeneration, NodeId};
use std::path::{Path, PathBuf};

#[cfg(any(test, feature = "test-seams"))]
pub(crate) mod commit_recovery_test_support;

mod maintenance;
pub use maintenance::{GraphMaintenancePolicy, GraphMaintenanceReport};
mod query;
pub use query::{GraphPlanBacking, GraphQueryPlan};

/// The graph API uses the unified Store handle.
///
/// The removed owner cannot be imported:
/// ```compile_fail
/// use zeppelin_embed::property_graph::GraphStore;
/// ```
/// ```compile_fail
/// use zeppelin_embed::GraphStore;
/// ```
impl Store {
    /// Creates a store with immutable per-type incoming-reference policies.
    /// Rules are persisted in the catalog and need not be supplied on reopen.
    /// Undeclared types retain ordinary DELETE/DETACH DELETE semantics.
    ///
    /// # Errors
    /// Invalid/duplicate declarations or the classified creation failure.
    #[doc(hidden)]
    pub fn create_graph_with_relationship_types(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
        rules: &[crate::property_graph::catalog::RelationshipRule<'_>],
    ) -> Result<Self, GraphStoreError> {
        refuse_unsupported_platform()?;
        let path = path.as_ref();
        refuse_legacy_directory(path)?;
        let rules =
            crate::property_graph::catalog::RelationshipRules::new(rules).map_err(|error| {
                GraphStoreError::graph(NativeGraphError::Stage(StageError::Catalog(error)))
            })?;
        let store = Store::create_native_graph_with_relationship_types(
            path,
            graph_options(options, AccessMode::ReadWrite),
            document,
            rules,
        )?;
        Ok(store)
    }

    /// Creates a new, empty graph store at `path`, which must not exist yet.
    ///
    /// `options` supplies memory limits, the reader drain timeout and the
    /// lexical interpretation. Its access mode is replaced with read-write
    /// and its durability with `Durable`/`Durable`. `document` is the stored
    /// document embedding tower, or `None` for a store without vectors.
    ///
    /// # Errors
    ///
    /// [`GraphStoreErrorKind::LegacyStore`] when `path` holds a legacy store,
    /// and otherwise the classified creation failure.
    #[doc(hidden)]
    pub fn create_graph(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
    ) -> Result<Self, GraphStoreError> {
        refuse_unsupported_platform()?;
        let path = path.as_ref();
        refuse_legacy_directory(path)?;
        let store = Store::create_native_graph(
            path,
            graph_options(options, AccessMode::ReadWrite),
            document,
        )?;
        Ok(store)
    }

    /// Opens an existing graph store for reading and writing. The store's
    /// writer lock is held until [`close`](Self::close) or drop.
    ///
    /// `options` and `document` are interpreted as for
    /// [`create`](Self::create); `document` must match the persisted tower.
    ///
    /// # Errors
    ///
    /// [`GraphStoreErrorKind::LegacyStore`] when `path` holds a legacy store,
    /// [`GraphStoreErrorKind::Busy`] when another writer owns it, and
    /// otherwise the classified open or recovery failure.
    #[doc(hidden)]
    pub fn open_graph(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
    ) -> Result<Self, GraphStoreError> {
        Self::open_graph_as(path.as_ref(), options, document, AccessMode::ReadWrite)
    }

    /// Opens an existing graph store without write authority. It takes a
    /// shared lock, performs no recovery write, and refuses every write with
    /// [`GraphStoreErrorKind::ReadOnly`].
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open).
    #[doc(hidden)]
    pub fn open_graph_read_only(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
    ) -> Result<Self, GraphStoreError> {
        Self::open_graph_as(path.as_ref(), options, document, AccessMode::ReadOnly)
    }

    fn open_graph_as(
        path: &Path,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
        access: AccessMode,
    ) -> Result<Self, GraphStoreError> {
        refuse_unsupported_platform()?;
        refuse_legacy_directory(path)?;
        let store = Store::open_native_graph(path, graph_options(options, access), document)?;
        Ok(store)
    }

    /// Closes the store: admissions stop, acknowledged state is checkpointed,
    /// admitted reads drain, and the writer lock is released.
    /// Values already returned, including every
    /// [`GraphWriteResult`], are owned copies and stay valid. Closing again
    /// succeeds and has no further effect.
    ///
    /// # Errors
    ///
    /// The classified teardown failure.
    #[doc(hidden)]
    pub fn close_graph(&self) -> Result<(), GraphStoreError> {
        let mut checkpoint = Ok(());
        let closed = self.close_with_final_writer(|| {
            checkpoint = self.checkpoint_native_graph_for_close();
        });
        checkpoint.map_err(GraphStoreError::graph)?;
        closed.map_err(|error| GraphStoreError::graph(NativeGraphError::Store(error)))
    }

    /// Applies one atomic structured batch. Every item is classified against
    /// the same admitted generation; either the whole batch commits durably
    /// or nothing does.
    ///
    /// The result holds one receipt per request, in request order, so a
    /// batch-local reference resolves to the identity at its request index.
    /// Nothing is retried: on any error the caller decides whether to
    /// resubmit, and an exact keyed resubmission replays.
    ///
    /// Identities keep their kind. A relationship endpoint is always a node,
    /// so a relationship identity cannot name one:
    ///
    /// ```compile_fail
    /// use zeppelin_embed::property_graph::{GraphName, NodeRef, RelId};
    /// use zeppelin_embed::property_graph::staging::WriteImage;
    /// let relationship = RelId::new(1).unwrap();
    /// let _image: WriteImage<'_, '_> = WriteImage::Relationship {
    ///     source: NodeRef::Existing(relationship),
    ///     target: NodeRef::Existing(relationship),
    ///     relationship_type: GraphName::new("LINKS").unwrap(),
    ///     properties: &[],
    /// };
    /// ```
    ///
    /// A node identity cannot name a relationship to put or delete:
    ///
    /// ```compile_fail
    /// use zeppelin_embed::property_graph::{EntityId, NodeId};
    /// use zeppelin_embed::property_graph::staging::StructuredOperation;
    /// let node = NodeId::new(1).unwrap();
    /// let _put = StructuredOperation::Put(EntityId::Relationship(node));
    /// ```
    ///
    /// # Errors
    ///
    /// A classified rejection. Unless
    /// [`nothing_committed`](GraphStoreError::nothing_committed) is false,
    /// the requested batch did not commit. Automatic maintenance may have
    /// completed before the batch was admitted.
    pub fn graph_apply<'a, 'b: 'a, 'c: 'a>(
        &self,
        batch: impl Into<GraphBatch<'a, 'b, 'c>>,
        control: &QueryControl,
    ) -> Result<GraphWriteResult, GraphStoreError> {
        let batch = batch.into();
        let BoundGraphBatch { writes, documents } = batch.bind(self)?;
        let prepared = match documents.as_deref() {
            Some(documents) => self.apply_native_mixed(documents, &writes, control)?,
            None => self.apply_native_graph(&writes, control)?,
        };
        // A published generation is the commit tail's own proof, so it alone
        // decides `Committed`. Without one, a `Changed` batch never reached
        // durability, which breaks the writer's contract: nothing committed.
        let outcome = match (prepared.changed_generation(), prepared.disposition()) {
            (Some(generation), _) => GraphWriteOutcome::Committed { generation },
            (None, BatchDisposition::Replayed) => GraphWriteOutcome::Replayed,
            (None, BatchDisposition::NoOp) => GraphWriteOutcome::NoOp,
            (None, BatchDisposition::Changed) => {
                return Err(GraphStoreError::contract(
                    "changed batch returned without a published generation",
                ));
            }
        };
        let admitted_generation = prepared.admitted_generation();
        let ack = crate::ingest::IngestAck::mixed(
            prepared.seq,
            prepared
                .changed_generation()
                .unwrap_or(admitted_generation)
                .get(),
        );
        let receipts = prepared.into_registration().into_receipts();
        Ok(GraphWriteResult {
            ack,
            receipts,
            outcome,
            admitted_generation,
        })
    }

    /// Applies the same atomic batch with binding materialization before commit.
    /// The returned registration is already owned; publication performs no copy.
    #[doc(hidden)]
    pub fn graph_apply_with_materializer<
        'a,
        'b: 'a,
        'c: 'a,
        M: crate::property_graph::staging::ResultMaterializer,
    >(
        &self,
        batch: impl Into<GraphBatch<'a, 'b, 'c>>,
        control: &QueryControl,
        materializer: &mut M,
    ) -> Result<(GraphWriteOutcome, GraphGeneration, M::Registration), GraphStoreError> {
        let BoundGraphBatch { writes, documents } = batch.into().bind(self)?;
        let prepared = self.apply_native_mixed_with_materializer(
            documents.as_deref(),
            &writes,
            control,
            materializer,
        )?;
        let outcome = match (prepared.changed_generation(), prepared.disposition()) {
            (Some(generation), _) => GraphWriteOutcome::Committed { generation },
            (None, BatchDisposition::Replayed) => GraphWriteOutcome::Replayed,
            (None, BatchDisposition::NoOp) => GraphWriteOutcome::NoOp,
            (None, BatchDisposition::Changed) => {
                return Err(GraphStoreError::contract(
                    "changed batch returned without a published generation",
                ));
            }
        };
        let admitted = prepared.admitted_generation();
        Ok((outcome, admitted, prepared.into_registration()))
    }

    /// Exposes this store's shared, read-only resource-accounting handle.
    ///
    /// A caller can use it to build its own `RuntimeContext` for work that
    /// happens after this call has already returned an owned result (for
    /// example, converting a [`GraphWriteResult`] or a [`query`](Self::graph_query)
    /// result into another representation): [`GraphResources`] admits no
    /// view and grants no additional store capability beyond accounting, so
    /// this leaks no mutation or admission authority.
    ///
    /// # Errors
    ///
    /// The classified accounting-configuration failure.
    pub fn graph_resources(&self) -> Result<GraphResources, GraphStoreError> {
        GraphResources::from_store(self)
            .map_err(|error| GraphStoreError::graph(NativeGraphError::Store(error)))
    }

    #[cfg(any(test, feature = "test-seams"))]
    #[doc(hidden)]
    pub fn create_graph_with_allocator_seed_for_test(
        path: impl AsRef<Path>,
        options: OpenOptions,
        first_node: crate::property_graph::NodeId,
        first_relationship: crate::property_graph::RelId,
    ) -> Result<Self, GraphStoreError> {
        let path = path.as_ref();
        refuse_legacy_directory(path)?;
        let store = Store::create_native_graph_with_allocator_seed_for_test(
            path,
            graph_options(options, AccessMode::ReadWrite),
            None,
            first_node,
            first_relationship,
        )?;
        Ok(store)
    }

    /// Nonshipping allocator setup for full-width binding fixtures. Subsequent
    /// mutations and reads use the ordinary public lifecycle.
    #[cfg(feature = "test-seams")]
    #[doc(hidden)]
    pub fn jump_allocators_for_test(
        &self,
        next_node: crate::property_graph::NodeId,
        next_relationship: crate::property_graph::RelId,
        control: &QueryControl,
    ) -> Result<crate::property_graph::GraphGeneration, GraphStoreError> {
        Ok(self.jump_native_graph_allocators_for_test(next_node, next_relationship, control)?)
    }

    #[cfg(test)]
    pub(crate) const fn store_for_test(&self) -> &Store {
        self
    }
}

/// The only options a graph store accepts: the requested access, Durable
/// data and Durable directory synchronization.
const fn graph_options(options: OpenOptions, access: AccessMode) -> OpenOptions {
    options
        .with_access_mode(access)
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) const GRAPH_MIN_MACOS: (u32, u32) = (14, 0);

#[cfg(any(target_os = "macos", test))]
pub(crate) const fn macos_admits_graph(observed: (u32, u32)) -> bool {
    observed.0 > GRAPH_MIN_MACOS.0
        || (observed.0 == GRAPH_MIN_MACOS.0 && observed.1 >= GRAPH_MIN_MACOS.1)
}

fn refuse_unsupported_platform() -> Result<(), GraphStoreError> {
    #[cfg(target_os = "macos")]
    {
        match crate::sys::darwin::os_product_version() {
            Ok(observed) if macos_admits_graph(observed) => Ok(()),
            Ok(observed) => Err(GraphStoreError {
                cause: Cause::UnsupportedPlatform {
                    required: GRAPH_MIN_MACOS,
                    observed: Some(observed),
                    probe_error: String::new(),
                },
            }),
            Err(error) => Err(GraphStoreError {
                cause: Cause::UnsupportedPlatform {
                    required: GRAPH_MIN_MACOS,
                    observed: None,
                    probe_error: error.to_string(),
                },
            }),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(())
    }
}

fn refuse_legacy_directory(path: &Path) -> Result<(), GraphStoreError> {
    let legacy = crate::lifecycle::is_legacy_store_directory(&crate::vfs::StdVfs, path)
        .map_err(|error| GraphStoreError::graph(NativeGraphError::Store(error)))?;
    if legacy {
        let manifest_path = path.join(crate::manifest::io::MANIFEST_FILE);
        if let Ok(bytes) = crate::vfs::Vfs::read(&crate::vfs::StdVfs, &manifest_path) {
            let manifest =
                crate::manifest::decode_manifest(&manifest_path.to_string_lossy(), &bytes)
                    .map_err(|error| {
                        GraphStoreError::graph(NativeGraphError::Store(
                            crate::lifecycle::StoreError::Manifest(error),
                        ))
                    })?;
            if manifest.graph.is_some() {
                return Ok(());
            }
        }
        return Err(GraphStoreError {
            cause: Cause::LegacyStore {
                path: path.to_path_buf(),
            },
        });
    }
    Ok(())
}

/// How one batch affected the store.
///
/// A batch that mixes new work with exact replays is `Committed`; each
/// receipt's `replayed` flag is the per-item record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphWriteOutcome {
    /// The batch changed durable state and is published at `generation`.
    Committed {
        /// The new generation this batch published.
        generation: GraphGeneration,
    },
    /// Every item is an exact replay; each receipt keeps its original
    /// generation and nothing new was written.
    Replayed,
    /// Nothing changed and nothing was written.
    NoOp,
}

/// Documents and structured graph writes committed atomically through wal.ze.
/// Receipts correspond to `writes` in request order; document ids are supplied
/// by their ingest documents.
pub struct GraphBatch<'a, 'b, 'c> {
    /// Optional document participant of the unified batch.
    pub documents: Option<&'a crate::ingest::IngestBatch>,
    /// Existing keyed graph operations and batch-local references.
    pub writes: &'a [StructuredWrite<'b, 'c>],
    /// Document replacements bound to individual node items.
    pub node_documents: &'a [GraphNodeDocument],
}

/// A document whose identity is the identity of one graph node item.
#[derive(Clone, Debug)]
pub struct GraphNodeDocument {
    /// Index in the graph batch's writes, including relationship items.
    pub item_index: usize,
    /// Full replacement document. Create requires a nonzero caller ID;
    /// Put requires the expected node ID and the same revision as the item.
    pub document: crate::ingest::IngestDocument,
}

struct BoundGraphBatch<'a, 'b, 'c> {
    writes: std::borrow::Cow<'a, [StructuredWrite<'b, 'c>]>,
    documents: Option<std::borrow::Cow<'a, crate::ingest::IngestBatch>>,
}

impl<'a, 'b, 'c> GraphBatch<'a, 'b, 'c> {
    /// Associates full document replacements with node items in one mixed commit.
    pub const fn with_node_documents(
        writes: &'a [StructuredWrite<'b, 'c>],
        node_documents: &'a [GraphNodeDocument],
    ) -> Self {
        Self {
            documents: None,
            writes,
            node_documents,
        }
    }

    fn bind(&self, store: &Store) -> Result<BoundGraphBatch<'a, 'b, 'c>, GraphStoreError> {
        let mut writes = std::borrow::Cow::Borrowed(self.writes);
        if self.node_documents.is_empty() {
            return Ok(BoundGraphBatch {
                writes,
                documents: self.documents.map(std::borrow::Cow::Borrowed),
            });
        }
        if self.documents.is_some() {
            return Err(GraphStoreError::graph(NativeGraphError::Stage(
                StageError::InvalidInput,
            )));
        }
        let mut documents = Vec::new();
        for (position, linked) in self.node_documents.iter().enumerate() {
            let version = linked.document.version();
            let id = NodeId::new(version.doc_id().get()).map_err(|_| {
                GraphStoreError::graph(NativeGraphError::Stage(StageError::InvalidInput))
            })?;
            if self.node_documents.iter().take(position).any(|prior| {
                prior.item_index == linked.item_index
                    || prior.document.version().doc_id() == version.doc_id()
            }) {
                return Err(GraphStoreError::graph(NativeGraphError::Stage(
                    StageError::InvalidInput,
                )));
            }
            let write = writes.to_mut().get_mut(linked.item_index).ok_or_else(|| {
                GraphStoreError::graph(NativeGraphError::Stage(StageError::InvalidInput))
            })?;
            if !matches!(
                write.image,
                Some(crate::property_graph::staging::WriteImage::Node(_))
            ) || write.revision.get() != version.revision().get()
            {
                return Err(GraphStoreError::graph(NativeGraphError::Stage(
                    StageError::InvalidInput,
                )));
            }
            write.operation = match write.operation {
                StructuredOperation::Create => StructuredOperation::CreateWithId(id),
                StructuredOperation::Put(EntityId::Node(expected)) if expected == id => {
                    write.operation
                }
                _ => {
                    return Err(GraphStoreError::graph(NativeGraphError::Stage(
                        StageError::InvalidInput,
                    )));
                }
            };
            documents.push(linked.document.clone());
        }
        let mut input_bytes = 0_u64;
        for document in &documents {
            let payload =
                crate::ingest::wal_payload::encode_upsert_v2(document).map_err(|error| {
                    GraphStoreError::graph(NativeGraphError::Ingest(
                        crate::ingest::IngestError::Payload(error),
                    ))
                })?;
            input_bytes = input_bytes
                .checked_add(payload.len() as u64)
                .ok_or_else(|| GraphStoreError::contract("mixed graph input length overflow"))?;
        }
        for write in writes.iter() {
            use crate::property_graph::staging::WriteImage;
            use crate::property_graph::{CanonicalContents, ExpectedGraphState};
            let expected = match write.operation {
                StructuredOperation::Create | StructuredOperation::CreateWithId(_) => {
                    ExpectedGraphState::Absent
                }
                StructuredOperation::Put(id) | StructuredOperation::Delete(id, _) => {
                    ExpectedGraphState::Entity(id)
                }
                StructuredOperation::Recreate(revision) => ExpectedGraphState::Deletion(revision),
            };
            let framing = crate::property_graph::provenance::measure_operation_framing(
                Some(write.key),
                expected,
                &mut || Ok(()),
            )
            .map_err(|error| GraphStoreError::graph(NativeGraphError::Stage(error.into())))?;
            let image_bytes = match write.image {
                None => 0,
                Some(WriteImage::Node(image)) => image.encoded_len(),
                Some(WriteImage::Relationship {
                    relationship_type,
                    properties,
                    ..
                }) => {
                    let mut properties = properties.to_vec();
                    let placeholder = NodeId::new(1)
                        .map_err(|_| GraphStoreError::contract("nonzero sizing identity"))?;
                    CanonicalContents::relationship(
                        placeholder,
                        placeholder,
                        relationship_type,
                        &mut properties,
                    )
                    .map_err(|error| GraphStoreError::graph(NativeGraphError::Stage(error.into())))?
                    .encoded_len()
                }
            };
            input_bytes = input_bytes
                .checked_add(framing)
                .and_then(|bytes| bytes.checked_add(image_bytes))
                .ok_or_else(|| GraphStoreError::contract("mixed graph input length overflow"))?;
        }
        if input_bytes > crate::property_graph::MAX_GRAPH_INPUT_BYTES as u64 {
            return Err(GraphStoreError::graph(NativeGraphError::Stage(
                StageError::InvalidInput,
            )));
        }
        let mut batch = crate::ingest::IngestBatch::new(documents);
        if let Some(epoch) = store.epoch_identity() {
            batch = batch.with_epoch(epoch);
        }
        Ok(BoundGraphBatch {
            writes,
            documents: Some(std::borrow::Cow::Owned(batch)),
        })
    }
}

impl<'a, 'b, 'c> From<&'a [StructuredWrite<'b, 'c>]> for GraphBatch<'a, 'b, 'c> {
    fn from(writes: &'a [StructuredWrite<'b, 'c>]) -> Self {
        Self {
            documents: None,
            writes,
            node_documents: &[],
        }
    }
}

impl<'a, 'b, 'c, const N: usize> From<&'a [StructuredWrite<'b, 'c>; N]> for GraphBatch<'a, 'b, 'c> {
    fn from(writes: &'a [StructuredWrite<'b, 'c>; N]) -> Self {
        Self::from(writes.as_slice())
    }
}

impl<'a, 'b, 'c> From<&'a Vec<StructuredWrite<'b, 'c>>> for GraphBatch<'a, 'b, 'c> {
    fn from(writes: &'a Vec<StructuredWrite<'b, 'c>>) -> Self {
        Self::from(writes.as_slice())
    }
}

/// The owned result of one [`Store::graph_apply`]. It holds no lease or
/// reservation, so it stays valid after the store closes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphWriteResult {
    ack: crate::ingest::IngestAck,
    receipts: Box<[ItemReceipt]>,
    outcome: GraphWriteOutcome,
    admitted_generation: GraphGeneration,
}

impl GraphWriteResult {
    /// WAL sequence and visible generation returned by the unified writer.
    #[must_use]
    pub const fn ack(&self) -> crate::ingest::IngestAck {
        self.ack
    }

    /// One receipt per request, in request order.
    #[must_use]
    pub fn receipts(&self) -> &[ItemReceipt] {
        &self.receipts
    }

    /// How the batch affected the store.
    #[must_use]
    pub const fn outcome(&self) -> GraphWriteOutcome {
        self.outcome
    }

    /// The published generation the batch was classified against.
    #[must_use]
    pub const fn admitted_generation(&self) -> GraphGeneration {
        self.admitted_generation
    }

    /// Takes the receipts, in request order.
    #[must_use]
    pub fn into_receipts(self) -> Box<[ItemReceipt]> {
        self.receipts
    }
}

/// The error groups a graph store caller acts on.
///
/// Downstream callers must allow future error groups.
///
/// ```compile_fail
/// use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind;
///
/// fn ze301_query_kind_requires_wildcard(kind: GraphQueryErrorKind) {
///     match kind {
///         GraphQueryErrorKind::InvalidPlan => {},
///         GraphQueryErrorKind::Parameter => {},
///         GraphQueryErrorKind::Expression => {},
///         GraphQueryErrorKind::Constraint => {},
///         GraphQueryErrorKind::Limit => {},
///         GraphQueryErrorKind::Cancelled => {},
///         GraphQueryErrorKind::Timeout => {},
///         GraphQueryErrorKind::Closed => {},
///         GraphQueryErrorKind::Corruption => {},
///         GraphQueryErrorKind::Storage => {},
///         GraphQueryErrorKind::Unavailable => {},
///         GraphQueryErrorKind::WriteIndeterminate => {},
///         GraphQueryErrorKind::Internal => {},
///     }
/// }
/// ```
///
/// ```compile_fail
/// use zeppelin_embed::property_graph::GraphStoreErrorKind;
///
/// fn ze301_store_kind_requires_wildcard(kind: GraphStoreErrorKind) {
///     match kind {
///         GraphStoreErrorKind::LegacyStore => {},
///         GraphStoreErrorKind::Busy => {},
///         GraphStoreErrorKind::ReadOnly => {},
///         GraphStoreErrorKind::InvalidRequest => {},
///         GraphStoreErrorKind::Constraint => {},
///         GraphStoreErrorKind::Limit => {},
///         GraphStoreErrorKind::Cancelled => {},
///         GraphStoreErrorKind::Timeout => {},
///         GraphStoreErrorKind::Closed => {},
///         GraphStoreErrorKind::Corruption => {},
///         GraphStoreErrorKind::Storage => {},
///         GraphStoreErrorKind::Unavailable => {},
///         GraphStoreErrorKind::WriteIndeterminate => {},
///         GraphStoreErrorKind::Unsupported => {},
///         GraphStoreErrorKind::Internal => {},
///     }
/// }
/// ```
///
/// ```
/// use zeppelin_embed::property_graph::GraphStoreErrorKind;
/// use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind;
///
/// fn ze301_kinds_accept_wildcards(query: GraphQueryErrorKind, store: GraphStoreErrorKind) {
///     match query {
///         GraphQueryErrorKind::Internal => {},
///         _ => {},
///     }
///     match store {
///         GraphStoreErrorKind::Internal => {},
///         _ => {},
///     }
/// }
/// ze301_kinds_accept_wildcards(GraphQueryErrorKind::Internal, GraphStoreErrorKind::Internal);
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GraphStoreErrorKind {
    /// The directory holds a legacy vector/lexical store, not a graph store.
    LegacyStore,
    /// Another writer owns the store.
    Busy,
    /// The store was opened read-only and cannot accept a write.
    ReadOnly,
    /// The request is malformed or unsupported.
    InvalidRequest,
    /// The request violates a graph rule: a stale or conflicting key
    /// revision or incarnation, a missing endpoint, or a restricted delete.
    Constraint,
    /// A work, byte or memory limit was reached.
    Limit,
    /// The caller cancelled the operation.
    Cancelled,
    /// The caller's deadline expired.
    Timeout,
    /// The store closed, or began closing, under the operation.
    Closed,
    /// Stored bytes failed validation.
    Corruption,
    /// A filesystem operation failed before anything could commit.
    Storage,
    /// The store cannot admit this operation now: writes stopped after an
    /// earlier indeterminate commit, a checkpoint must run, or an identity
    /// space is exhausted.
    Unavailable,
    /// The commit was attempted and its outcome is unknown. The write may be
    /// durable; the store stops admitting writes until it is reopened.
    WriteIndeterminate,
    /// The host platform or OS version cannot run a graph store.
    Unsupported,
    /// An engine invariant was violated.
    Internal,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "the typed graph and query causes stay unboxed and allocation-free, as in GraphQueryError"
)]
enum Cause {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    UnsupportedPlatform {
        required: (u32, u32),
        observed: Option<(u32, u32)>,
        probe_error: String,
    },
    LegacyStore {
        path: PathBuf,
    },
    Graph(NativeGraphError),
    /// A rejected [`Store::graph_query`] statement (ZE-66 S2). The typed cause
    /// is ZE-53's already-reviewed `GraphQueryError`, kept unchanged per the
    /// owner's decision; only its error group is folded into
    /// [`GraphStoreErrorKind`], through the same table [`Cause::Graph`] uses.
    Query(GraphQueryError),
    Contract(&'static str),
    Limit(&'static str),
}

/// Folds one of ZE-53's plan error groups into the coarser groups a
/// Store graph caller acts on. [`Cause::Graph`] and [`Cause::Query`] both
/// route through this one table so the two paths never drift apart.
///
/// This loses one distinction `Store::graph_apply` can make directly
/// from `NativeGraphError`: a write statement against a read-only store
/// folds here to `Unavailable`, not `ReadOnly`, because `GraphQueryError`
/// itself already groups `StoreErrorKind::ReadOnly` under its own
/// `Unavailable` (`GraphQueryErrorKind` has no separate `ReadOnly`/`Busy`
/// group to preserve). `nothing_committed()` still reports `true`.
const fn from_graph_query_kind(kind: GraphQueryErrorKind) -> GraphStoreErrorKind {
    match kind {
        GraphQueryErrorKind::InvalidPlan
        | GraphQueryErrorKind::Parameter
        | GraphQueryErrorKind::Expression => GraphStoreErrorKind::InvalidRequest,
        GraphQueryErrorKind::Constraint => GraphStoreErrorKind::Constraint,
        GraphQueryErrorKind::Limit => GraphStoreErrorKind::Limit,
        GraphQueryErrorKind::Cancelled => GraphStoreErrorKind::Cancelled,
        GraphQueryErrorKind::Timeout => GraphStoreErrorKind::Timeout,
        GraphQueryErrorKind::Closed => GraphStoreErrorKind::Closed,
        GraphQueryErrorKind::Corruption => GraphStoreErrorKind::Corruption,
        GraphQueryErrorKind::Internal => GraphStoreErrorKind::Internal,
        GraphQueryErrorKind::Storage => GraphStoreErrorKind::Storage,
        GraphQueryErrorKind::Unavailable => GraphStoreErrorKind::Unavailable,
        GraphQueryErrorKind::WriteIndeterminate => GraphStoreErrorKind::WriteIndeterminate,
    }
}

/// A rejected graph store lifecycle or write operation.
#[derive(Debug)]
pub struct GraphStoreError {
    cause: Cause,
}

impl GraphStoreError {
    const fn graph(error: NativeGraphError) -> Self {
        Self {
            cause: Cause::Graph(error),
        }
    }

    const fn contract(reason: &'static str) -> Self {
        Self {
            cause: Cause::Contract(reason),
        }
    }

    const fn limit(reason: &'static str) -> Self {
        Self {
            cause: Cause::Limit(reason),
        }
    }

    /// The error group a caller acts on.
    #[must_use]
    pub fn kind(&self) -> GraphStoreErrorKind {
        match &self.cause {
            Cause::UnsupportedPlatform { .. } => GraphStoreErrorKind::Unsupported,
            Cause::LegacyStore { .. } => GraphStoreErrorKind::LegacyStore,
            Cause::Contract(_) => GraphStoreErrorKind::Corruption,
            Cause::Limit(_) => GraphStoreErrorKind::Limit,
            Cause::Graph(NativeGraphError::Store(error))
                if error.kind() == StoreErrorKind::StoreBusy =>
            {
                GraphStoreErrorKind::Busy
            }
            Cause::Graph(NativeGraphError::Store(error))
                if error.kind() == StoreErrorKind::ReadOnly =>
            {
                GraphStoreErrorKind::ReadOnly
            }
            Cause::Graph(error) => from_graph_query_kind(native_graph_error_kind(error)),
            Cause::Query(error)
                if matches!(
                    error.cause(),
                    GraphQueryCause::Graph(NativeGraphError::Store(inner))
                        if inner.kind() == StoreErrorKind::ReadOnly
                ) =>
            {
                GraphStoreErrorKind::ReadOnly
            }
            Cause::Query(error) => from_graph_query_kind(error.kind()),
        }
    }

    /// The operator [`Store::graph_query`] was running when the statement
    /// failed, when the failure came from inside the driver. `None` for
    /// every other operation and refusal.
    #[must_use]
    pub const fn operator(&self) -> Option<PlanNodeId> {
        match &self.cause {
            Cause::Query(error) => error.operator(),
            Cause::UnsupportedPlatform { .. }
            | Cause::LegacyStore { .. }
            | Cause::Graph(_)
            | Cause::Contract(_)
            | Cause::Limit(_) => None,
        }
    }

    /// The work [`Store::graph_query`]'s driver had done when the statement
    /// failed, when the failure came from inside the driver.
    #[must_use]
    pub const fn counters(&self) -> Option<WorkCounters> {
        match &self.cause {
            Cause::Query(error) => error.counters(),
            Cause::UnsupportedPlatform { .. }
            | Cause::LegacyStore { .. }
            | Cause::Graph(_)
            | Cause::Contract(_)
            | Cause::Limit(_) => None,
        }
    }

    /// True unless a commit was attempted with an unknown outcome.
    #[must_use]
    pub fn nothing_committed(&self) -> bool {
        self.kind() != GraphStoreErrorKind::WriteIndeterminate
    }

    /// The staging refusal, when staging rejected the batch. Key lifecycle
    /// refusals such as a stale incarnation are
    /// [`StageError::Lifecycle`] values.
    #[must_use]
    pub const fn stage_error(&self) -> Option<&StageError> {
        match &self.cause {
            Cause::Graph(NativeGraphError::Stage(error)) => Some(error),
            _ => None,
        }
    }

    /// The legacy store directory, when the refusal is
    /// [`GraphStoreErrorKind::LegacyStore`].
    #[must_use]
    pub fn legacy_store_path(&self) -> Option<&Path> {
        match &self.cause {
            Cause::LegacyStore { path } => Some(path),
            _ => None,
        }
    }
}

impl From<NativeGraphError> for GraphStoreError {
    fn from(error: NativeGraphError) -> Self {
        Self::graph(error)
    }
}

impl From<GraphQueryError> for GraphStoreError {
    fn from(error: GraphQueryError) -> Self {
        Self {
            cause: Cause::Query(error),
        }
    }
}

impl std::fmt::Display for GraphStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "graph store rejected ({:?}): ", self.kind())?;
        match &self.cause {
            Cause::UnsupportedPlatform {
                required,
                observed,
                probe_error,
            } => {
                write!(
                    formatter,
                    "graph store requires macOS {}.{} or newer; ",
                    required.0, required.1
                )?;
                if let Some(observed) = observed {
                    write!(formatter, "this host reports {}.{}", observed.0, observed.1)
                } else {
                    write!(
                        formatter,
                        "could not determine the macOS version: {}",
                        probe_error
                    )
                }
            }
            Cause::LegacyStore { path } => write!(
                formatter,
                "legacy store directory cannot be opened as a graph store: {}",
                path.display()
            ),
            Cause::Graph(error) => error.fmt(formatter),
            Cause::Query(error) => error.fmt(formatter),
            Cause::Contract(reason) | Cause::Limit(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for GraphStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            Cause::Graph(error) => Some(error),
            Cause::Query(error) => Some(error),
            Cause::UnsupportedPlatform { .. }
            | Cause::LegacyStore { .. }
            | Cause::Contract(_)
            | Cause::Limit(_) => None,
        }
    }
}

mod get;
pub use get::{GraphGetOptions, GraphNodesResult, GraphRelationshipsResult};

#[cfg(test)]
mod tests;
