//! `GraphStore`: the mode-safe public owner of one native graph store.
//!
//! A `GraphStore` owns exactly one native graph directory. It cannot open a
//! legacy vector/lexical store, and the legacy [`Store`] cannot open a graph
//! directory: each direction is refused with its own typed error before any
//! file is changed. Every graph write is Durable: the facade replaces the
//! caller's durability mode and commit tier with `Durable`/`Durable`.
//!
//! [`GraphStore::apply_batch`] runs one atomic structured batch and returns
//! owned receipts, the batch outcome and the generation the batch was
//! classified against. It never retries. A caller who wants a retry resubmits
//! the same keyed batch; an exact retry replays with its original generation.

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
use crate::property_graph::staging::{ItemReceipt, StageError, StructuredWrite};
use crate::property_graph::{BatchDisposition, GraphGeneration};
use std::path::{Path, PathBuf};

mod query;
pub use query::{GraphPlanBacking, GraphQueryPlan};

/// One open native graph store. Every write it admits is Durable.
///
/// A `GraphStore` is never a legacy [`Store`]; the two open disjoint
/// directory kinds and refuse each other's directories with typed errors.
/// A legacy store cannot stand in for a graph store:
///
/// ```compile_fail
/// use zeppelin_embed::lifecycle::{OpenOptions, Store};
/// use zeppelin_embed::property_graph::GraphStore;
/// fn needs_graph(_: &GraphStore) {}
/// let legacy = Store::open("legacy", OpenOptions::new()).unwrap();
/// needs_graph(&legacy);
/// ```
pub struct GraphStore {
    store: Store,
}

impl std::fmt::Debug for GraphStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GraphStore")
            .field("directory", &self.store.directory)
            .finish_non_exhaustive()
    }
}

impl GraphStore {
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
    pub fn create(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
    ) -> Result<Self, GraphStoreError> {
        let path = path.as_ref();
        refuse_legacy_directory(path)?;
        let store = Store::create_native_graph(
            path,
            graph_options(options, AccessMode::ReadWrite),
            document,
        )?;
        Ok(Self { store })
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
    pub fn open(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
    ) -> Result<Self, GraphStoreError> {
        Self::open_as(path.as_ref(), options, document, AccessMode::ReadWrite)
    }

    /// Opens an existing graph store without write authority. It takes a
    /// shared lock, performs no recovery write, and refuses every write with
    /// [`GraphStoreErrorKind::ReadOnly`].
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open).
    pub fn open_read_only(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
    ) -> Result<Self, GraphStoreError> {
        Self::open_as(path.as_ref(), options, document, AccessMode::ReadOnly)
    }

    fn open_as(
        path: &Path,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
        access: AccessMode,
    ) -> Result<Self, GraphStoreError> {
        refuse_legacy_directory(path)?;
        let store = Store::open_native_graph(path, graph_options(options, access), document)?;
        Ok(Self { store })
    }

    /// Closes the store: admissions stop, admitted reads drain, and the
    /// writer lock is released. Values already returned, including every
    /// [`GraphWriteResult`], are owned copies and stay valid. Closing again
    /// succeeds and has no further effect.
    ///
    /// # Errors
    ///
    /// The classified teardown failure.
    pub fn close(&self) -> Result<(), GraphStoreError> {
        self.store
            .close()
            .map_err(|error| GraphStoreError::graph(NativeGraphError::Store(error)))
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
    /// the store did not change.
    pub fn apply_batch(
        &self,
        requests: &[StructuredWrite<'_, '_>],
        control: &QueryControl,
    ) -> Result<GraphWriteResult, GraphStoreError> {
        let prepared = self.store.apply_native_graph(requests, control)?;
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
        let receipts = prepared.into_registration().into_receipts();
        Ok(GraphWriteResult {
            receipts,
            outcome,
            admitted_generation,
        })
    }

    /// Exposes this store's shared, read-only resource-accounting handle.
    ///
    /// A caller can use it to build its own `RuntimeContext` for work that
    /// happens after this call has already returned an owned result (for
    /// example, converting a [`GraphWriteResult`] or a [`query`](Self::query)
    /// result into another representation): [`GraphResources`] admits no
    /// view and grants no additional store capability beyond accounting, so
    /// this leaks no mutation or admission authority.
    ///
    /// # Errors
    ///
    /// The classified accounting-configuration failure.
    pub fn resources(&self) -> Result<GraphResources, GraphStoreError> {
        GraphResources::from_store(&self.store)
            .map_err(|error| GraphStoreError::graph(NativeGraphError::Store(error)))
    }

    /// The native statement seam this store owns, for the outer Cypher
    /// compiler (`zeppelin_embed_cypher::execute`) only. It grants no
    /// capability the seam does not already check: every statement is
    /// admitted, classified and written exactly as through [`query`](Self::query).
    #[doc(hidden)]
    #[must_use]
    pub const fn statement_store(&self) -> &Store {
        &self.store
    }

    #[cfg(test)]
    pub(crate) fn create_with_allocator_seed_for_test(
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
        Ok(Self { store })
    }

    #[cfg(test)]
    pub(crate) const fn store_for_test(&self) -> &Store {
        &self.store
    }
}

/// The only options a graph store accepts: the requested access, Durable
/// data and Durable directory synchronization.
const fn graph_options(options: OpenOptions, access: AccessMode) -> OpenOptions {
    options
        .with_access_mode(access)
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
}

fn refuse_legacy_directory(path: &Path) -> Result<(), GraphStoreError> {
    let legacy = crate::lifecycle::is_legacy_store_directory(&crate::vfs::StdVfs, path)
        .map_err(|error| GraphStoreError::graph(NativeGraphError::Store(error)))?;
    if legacy {
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

/// The owned result of one [`GraphStore::apply_batch`]. It holds no lease or
/// reservation, so it stays valid after the store closes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphWriteResult {
    receipts: Box<[ItemReceipt]>,
    outcome: GraphWriteOutcome,
    admitted_generation: GraphGeneration,
}

impl GraphWriteResult {
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    /// Stored bytes or an internal invariant failed validation.
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
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "the typed graph and query causes stay unboxed and allocation-free, as in GraphQueryError"
)]
enum Cause {
    LegacyStore {
        path: PathBuf,
    },
    Graph(NativeGraphError),
    /// A rejected [`GraphStore::query`] statement (ZE-66 S2). The typed cause
    /// is ZE-53's already-reviewed `GraphQueryError`, kept unchanged per the
    /// owner's decision; only its error group is folded into
    /// [`GraphStoreErrorKind`], through the same table [`Cause::Graph`] uses.
    Query(GraphQueryError),
    Contract(&'static str),
    Limit(&'static str),
}

/// Folds one of ZE-53's plan error groups into the coarser groups a
/// `GraphStore` caller acts on. [`Cause::Graph`] and [`Cause::Query`] both
/// route through this one table so the two paths never drift apart.
///
/// This loses one distinction `GraphStore::apply_batch` can make directly
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

    /// The operator [`GraphStore::query`] was running when the statement
    /// failed, when the failure came from inside the driver. `None` for
    /// every other operation and refusal.
    #[must_use]
    pub const fn operator(&self) -> Option<PlanNodeId> {
        match &self.cause {
            Cause::Query(error) => error.operator(),
            Cause::LegacyStore { .. } | Cause::Graph(_) | Cause::Contract(_) | Cause::Limit(_) => {
                None
            }
        }
    }

    /// The work [`GraphStore::query`]'s driver had done when the statement
    /// failed, when the failure came from inside the driver.
    #[must_use]
    pub const fn counters(&self) -> Option<WorkCounters> {
        match &self.cause {
            Cause::Query(error) => error.counters(),
            Cause::LegacyStore { .. } | Cause::Graph(_) | Cause::Contract(_) | Cause::Limit(_) => {
                None
            }
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
            Cause::LegacyStore { .. } | Cause::Contract(_) | Cause::Limit(_) => None,
        }
    }
}

mod get;
pub use get::{GraphGetOptions, GraphNodesResult, GraphRelationshipsResult};

#[cfg(test)]
mod tests;
