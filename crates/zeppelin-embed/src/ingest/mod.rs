//! Ingest and mutation coordination.

mod active;
mod purge;
mod retention;
mod revise;
mod seal;
pub mod wal_payload;

use std::sync::Arc;

use crate::lifecycle::{Store, StoreError, StoreState};
use crate::scan::ScanStats;
use crate::segment::SegmentId;
use crate::vfs::StdVfs;
use crate::wal::{LogSeq, WalWriteError};

pub use purge::{PurgeError, PurgeReport, PurgeToken};
pub use retention::{DropPartitionReport, RetentionPolicy, RetentionPolicyError};

pub(crate) use active::{ActiveSegment, ActiveState, StoreWal};

/// Stable application document identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct DocId(u128);

impl DocId {
    /// Constructs an identifier from its permanent integer value.
    #[must_use]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    /// Returns the permanent integer value.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }
}

/// Monotonic application revision of one document.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct Revision(u64);

impl Revision {
    /// Constructs a revision from its permanent integer value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the permanent integer value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One idempotency key identifying a particular document revision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentVersion {
    doc_id: DocId,
    revision: Revision,
}

impl DocumentVersion {
    /// Constructs a document/revision pair.
    #[must_use]
    pub const fn new(doc_id: DocId, revision: Revision) -> Self {
        Self { doc_id, revision }
    }

    /// Returns the stable application document identifier.
    #[must_use]
    pub const fn doc_id(self) -> DocId {
        self.doc_id
    }

    /// Returns the monotonic application revision.
    #[must_use]
    pub const fn revision(self) -> Revision {
        self.revision
    }
}

/// One owned vector mutation supplied to an ingest batch.
#[derive(Clone, Debug, PartialEq)]
pub struct IngestDocument {
    version: DocumentVersion,
    vector: Vec<f32>,
    timestamp: i64,
    metadata: Vec<u8>,
}

impl IngestDocument {
    /// Constructs one document upsert.
    #[must_use]
    pub fn new(version: DocumentVersion, vector: Vec<f32>) -> Self {
        Self {
            version,
            vector,
            timestamp: 0,
            metadata: Vec::new(),
        }
    }

    /// Assigns the canonical `ts` clustering-key value.
    #[must_use]
    pub const fn with_timestamp(mut self, timestamp: i64) -> Self {
        self.timestamp = timestamp;
        self
    }

    /// Assigns opaque stored metadata that must remain physically purgeable.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Vec<u8>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Returns the document/revision idempotency key.
    #[must_use]
    pub const fn version(&self) -> DocumentVersion {
        self.version
    }

    /// Returns the full-precision vector retained for future sealing.
    #[must_use]
    pub fn vector(&self) -> &[f32] {
        &self.vector
    }

    /// Returns the canonical `ts` clustering-key value.
    #[must_use]
    pub const fn timestamp(&self) -> i64 {
        self.timestamp
    }

    /// Returns the opaque stored metadata bytes.
    #[must_use]
    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }
}

/// One atomic caller batch of document upserts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IngestBatch {
    documents: Vec<IngestDocument>,
    epoch: Option<crate::epoch::EpochIdentity>,
}

/// One atomic caller batch of stable document ids to tombstone.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeleteBatch {
    doc_ids: Vec<DocId>,
}

impl DeleteBatch {
    /// Takes ownership of document ids in caller order.
    #[must_use]
    pub const fn new(doc_ids: Vec<DocId>) -> Self {
        Self { doc_ids }
    }

    /// Returns the requested stable document ids.
    #[must_use]
    pub fn doc_ids(&self) -> &[DocId] {
        &self.doc_ids
    }
}

impl IngestBatch {
    /// Takes ownership of the upserts in caller order.
    #[must_use]
    pub const fn new(documents: Vec<IngestDocument>) -> Self {
        Self {
            documents,
            epoch: None,
        }
    }

    /// Declares the interpretation identity that produced every vector.
    #[must_use]
    pub const fn with_epoch(mut self, epoch: crate::epoch::EpochIdentity) -> Self {
        self.epoch = Some(epoch);
        self
    }

    /// Returns the ordered document upserts.
    #[must_use]
    pub fn documents(&self) -> &[IngestDocument] {
        &self.documents
    }
}

/// A committed mutation's WAL and point-in-time generation coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IngestAck {
    seq: LogSeq,
    generation: u64,
}

impl IngestAck {
    /// Returns the last WAL sequence covered by this acknowledgement.
    #[must_use]
    pub const fn seq(self) -> LogSeq {
        self.seq
    }

    /// Returns the store generation on which this mutation acted.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Typed ingest rejection that leaves active state unchanged.
#[derive(Debug)]
pub enum IngestError {
    /// Lifecycle, access-mode, budget, or synchronization failure.
    Store(StoreError),
    /// The caller supplied no document mutation.
    EmptyBatch,
    /// The batch's declared interpretation differs from the store identity.
    EpochMismatch(crate::epoch::EpochMismatch),
    /// The store is stamped but the batch did not declare its identity.
    EpochUndeclared,
    /// The batch declared an identity for an unstamped store.
    EpochUnstamped,
    /// The attempted revision precedes the highest active or sealed revision.
    StaleRevision {
        /// Document whose history would have moved backward.
        doc_id: DocId,
        /// Highest revision currently known across active and sealed state.
        current: Revision,
        /// Rejected older revision.
        attempted: Revision,
    },
    /// A vector could not be quantized under the frozen Bit4 contract.
    Vector(crate::quant::QuantError),
    /// A WAL operation payload could not be encoded.
    Payload(wal_payload::PayloadError),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::EmptyBatch => formatter.write_str("ingest batch is empty"),
            Self::EpochMismatch(error) => error.fmt(formatter),
            Self::EpochUndeclared => formatter.write_str("ingest epoch is required by this store"),
            Self::EpochUnstamped => {
                formatter.write_str("an unstamped store cannot accept an epoch declaration")
            }
            Self::StaleRevision {
                doc_id,
                current,
                attempted,
            } => write!(
                formatter,
                "document {} revision {} is stale; current revision is {}",
                doc_id.get(),
                attempted.get(),
                current.get()
            ),
            Self::Vector(error) => write!(formatter, "ingest vector: {error}"),
            Self::Payload(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for IngestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::EpochMismatch(error) => Some(error),
            Self::Vector(error) => Some(error),
            Self::Payload(error) => Some(error),
            Self::EmptyBatch
            | Self::EpochUndeclared
            | Self::EpochUnstamped
            | Self::StaleRevision { .. } => None,
        }
    }
}

impl From<StoreError> for IngestError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Borrowed structured vector request for store-owned data.
#[derive(Clone, Copy, Debug)]
pub struct SearchRequest<'a> {
    vector: &'a [f32],
}

impl<'a> SearchRequest<'a> {
    /// Constructs a full-precision vector request.
    #[must_use]
    pub const fn new(vector: &'a [f32]) -> Self {
        Self { vector }
    }

    /// Returns the full-precision query coordinates.
    #[must_use]
    pub const fn vector(self) -> &'a [f32] {
        self.vector
    }
}

/// The immutable object namespace containing a segment-local row.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RowSource {
    /// The in-RAM active segment pinned for this query.
    Active,
    /// One immutable segment identified independently of manifest order.
    Sealed(SegmentId),
}

/// Collision-free store row address `(source, local_row)`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GlobalRowId {
    source: RowSource,
    local_row: u32,
}

impl GlobalRowId {
    pub(crate) const fn new(source: RowSource, local_row: u32) -> Self {
        Self { source, local_row }
    }

    /// Returns the active or immutable object namespace.
    #[must_use]
    pub const fn source(self) -> RowSource {
        self.source
    }

    /// Returns the dense row number within `source`.
    #[must_use]
    pub const fn local_row(self) -> u32 {
        self.local_row
    }
}

/// One globally addressed store-search candidate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchCandidate {
    row_id: GlobalRowId,
    document: Option<DocumentVersion>,
    score: f32,
    exact_score: bool,
}

impl SearchCandidate {
    pub(crate) const fn new(
        row_id: GlobalRowId,
        document: Option<DocumentVersion>,
        score: f32,
        exact_score: bool,
    ) -> Self {
        Self {
            row_id,
            document,
            score,
            exact_score,
        }
    }

    /// Returns the globally collision-free row address.
    #[must_use]
    pub const fn row_id(self) -> GlobalRowId {
        self.row_id
    }

    /// Returns the document identity, or `None` for task-07 segments whose
    /// frozen format predates task-10's document-version region.
    #[must_use]
    pub const fn document(self) -> Option<DocumentVersion> {
        self.document
    }

    /// Returns the larger-is-better scan score.
    #[must_use]
    pub const fn score(self) -> f32 {
        self.score
    }

    pub(crate) const fn exact_score(self) -> bool {
        self.exact_score
    }
}

/// Global top-k results and aggregate deterministic scan counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GraphSearchStats {
    /// Sealed segments that completed graph traversal.
    pub segments_traversed: usize,
    /// Complete immutable graph validations performed before descriptor reuse.
    pub graph_validations: usize,
    /// Full segment scans performed to discover persisted entry seeds.
    pub entry_seed_discoveries: usize,
    /// Visited arrays cleared after their epoch byte wrapped.
    pub visited_epoch_clears: usize,
    /// Distinct graph candidates scored by the Bit4 traversal estimator.
    pub candidates_scored: usize,
    /// Retained graph candidates read from full-precision storage.
    pub candidates_rescored: usize,
    /// Sealed graph segments rejected by the query-local competitive bound.
    pub segments_pruned_by_bound: usize,
}

/// Global top-k results and aggregate deterministic query counters.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchOutcome {
    /// Candidates merged across the active and every sealed segment.
    pub candidates: Vec<SearchCandidate>,
    /// Aggregate work from all per-segment query-pool executions.
    pub stats: ScanStats,
    /// Graph-only deterministic work; fields are zero when no segment used a graph.
    pub graph_stats: GraphSearchStats,
    /// Pinned active-state generation searched by this request.
    pub generation: u64,
    /// Embedding and tokenizer identity that interpreted this query.
    pub epoch: Option<crate::epoch::EpochIdentity>,
    /// Unconditional report of the executed query path.
    pub diagnostics: crate::diag::QueryDiagnostics,
}

#[derive(Clone, Copy)]
struct ResolvedRevision {
    version: DocumentVersion,
    seq: LogSeq,
    active_row: Option<usize>,
}

fn resolve_revision(
    active: &ActiveSegment,
    sealed: &[purge::SealedDocumentMatch],
    doc_id: DocId,
    sealed_seq: LogSeq,
) -> Option<ResolvedRevision> {
    let active_existing = active.existing(doc_id);
    let active_row = active_existing.map(|(row, _, _)| row);
    let mut resolved = active_existing.map(|(_, version, seq)| ResolvedRevision {
        version,
        seq,
        active_row,
    });
    for matched in sealed
        .iter()
        .filter(|matched| matched.version.doc_id() == doc_id)
    {
        if resolved.is_none_or(|current| matched.version.revision() > current.version.revision()) {
            resolved = Some(ResolvedRevision {
                version: matched.version,
                seq: sealed_seq,
                active_row,
            });
        }
    }
    resolved
}

impl Store {
    /// Commits one active-segment upsert and returns its WAL/generation coordinates.
    pub fn ingest(&self, batch: IngestBatch) -> Result<IngestAck, IngestError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing.into()),
            StoreState::Closed => return Err(StoreError::Closed.into()),
        }
        match (self.epoch_identity(), batch.epoch) {
            (Some(expected), Some(declared)) if expected != declared => {
                return Err(IngestError::EpochMismatch(crate::epoch::EpochMismatch {
                    expected,
                    declared,
                }));
            }
            (Some(_), None) => return Err(IngestError::EpochUndeclared),
            (None, Some(_)) => return Err(IngestError::EpochUnstamped),
            (Some(_), Some(_)) | (None, None) => {}
        }
        if batch.documents.is_empty() {
            return Err(IngestError::EmptyBatch);
        }
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_mut().ok_or(StoreError::ReadOnly)?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let current = active.as_ref().ok_or(StoreError::Closed)?;
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        let requested_ids = batch
            .documents
            .iter()
            .map(|document| document.version().doc_id())
            .collect::<Vec<_>>();
        let sealed = purge::sealed_document_matches(&snapshot, &requested_ids)?;
        let sealed_seq = LogSeq::new(snapshot.absorbed_through());
        let mut working = None;
        let mut records = Vec::new();
        let mut replay_seq = None;
        let mut sealed_tombstones = Vec::new();
        for document in &batch.documents {
            let segment = working.as_ref().unwrap_or(current.segment.as_ref());
            let existing =
                resolve_revision(segment, &sealed, document.version().doc_id(), sealed_seq);
            let action = revise::classify_revision(
                existing.map(|resolved| (resolved.version, resolved.seq)),
                document.version(),
            );
            match action {
                revise::RevisionDecision::Replace => {
                    let row = existing.and_then(|resolved| resolved.active_row);
                    let (next, row) = if let Some(row) = row {
                        (segment.replace(row, document, &self.accounting)?, row)
                    } else {
                        let row = segment.row_count();
                        (segment.insert(document, &self.accounting)?, row)
                    };
                    let (op, payload) = encode_persisted_upsert(document)?;
                    working = Some(next);
                    records.push((row, op, payload));
                    if sealed
                        .iter()
                        .any(|matched| matched.version.doc_id() == document.version().doc_id())
                        && !sealed_tombstones.contains(&document.version().doc_id())
                    {
                        sealed_tombstones.push(document.version().doc_id());
                    }
                }
                revise::RevisionDecision::Insert => {
                    let row = segment.row_count();
                    let next = segment.insert(document, &self.accounting)?;
                    let (op, payload) = encode_persisted_upsert(document)?;
                    working = Some(next);
                    records.push((row, op, payload));
                }
                revise::RevisionDecision::Replay { seq } => {
                    replay_seq = Some(replay_seq.map_or(seq, |current: LogSeq| current.max(seq)));
                }
                revise::RevisionDecision::Reject { current } => {
                    return Err(IngestError::StaleRevision {
                        doc_id: document.version().doc_id(),
                        current,
                        attempted: document.version().revision(),
                    });
                }
            }
        }
        let Some(mut next) = working else {
            let seq = replay_seq.ok_or(StoreError::Synchronization {
                component: "nonempty ingest replay sequence",
            })?;
            return Ok(IngestAck {
                seq,
                generation: current.generation,
            });
        };
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let prepared = purge::prepare_sealed_tombstones(
            &StdVfs,
            &self.directory,
            &snapshot,
            &sealed,
            &sealed_tombstones,
            writer.durable_end(),
            generation,
            writer.durable_end().saturating_add(1),
            self.durability_policy,
            &self.accounting,
        )?;
        let record_refs = records
            .iter()
            .map(|(_, op, payload)| (*op, payload.as_slice()))
            .collect::<Vec<_>>();
        let sequences = match writer.commit_many(&record_refs) {
            Ok(sequences) => sequences,
            Err(error) => {
                if let Some(prepared) = prepared {
                    prepared.abort(&StdVfs, &self.directory, self.durability_policy)?;
                }
                return Err(error.into());
            }
        };
        for ((row, _, _), sequence) in records
            .iter()
            .zip(sequences.start.get()..sequences.end.get())
        {
            next.set_sequence(*row, LogSeq::new(sequence))?;
        }
        let seq = LogSeq::new(sequences.end.get().saturating_sub(1));
        let committed = prepared
            .map(|prepared| prepared.commit(self, &StdVfs, &self.directory, self.durability_policy))
            .transpose()?;
        let replaced_paths = if let Some((remapped, replaced_paths)) = committed {
            let mut published = self
                .snapshot
                .write()
                .map_err(|_| StoreError::Synchronization {
                    component: "published snapshot",
                })?;
            let previous = published.replace(Arc::new(remapped));
            *active = Some(ActiveState {
                generation,
                segment: Arc::new(next),
            });
            drop(published);
            drop(previous);
            replaced_paths
        } else {
            *active = Some(ActiveState {
                generation,
                segment: Arc::new(next),
            });
            Vec::new()
        };
        purge::unlink_replaced_segments(
            &StdVfs,
            &self.directory,
            &replaced_paths,
            self.durability_policy,
        )?;
        drop(active);
        drop(wal);
        drop(state);
        Ok(IngestAck { seq, generation })
    }

    /// Durably tombstones documents across active and sealed state before acknowledging.
    pub fn delete(&self, batch: DeleteBatch) -> Result<IngestAck, IngestError> {
        if batch.doc_ids.is_empty() {
            return Err(IngestError::EmptyBatch);
        }
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing.into()),
            StoreState::Closed => return Err(StoreError::Closed.into()),
        }
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_mut().ok_or(StoreError::ReadOnly)?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let current = active.as_ref().ok_or(StoreError::Closed)?;
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        let sealed = purge::sealed_document_matches(&snapshot, &batch.doc_ids)?;
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let (mut next, rows) = current
            .segment
            .tombstone(&batch.doc_ids, &self.accounting)?;
        let prepared = purge::prepare_sealed_tombstones(
            &StdVfs,
            &self.directory,
            &snapshot,
            &sealed,
            &batch.doc_ids,
            writer.durable_end(),
            generation,
            writer.durable_end().saturating_add(1),
            self.durability_policy,
            &self.accounting,
        )?;
        let payload = wal_payload::encode_delete(&batch.doc_ids).map_err(IngestError::Payload)?;
        let seq = match writer.commit(wal_payload::DELETE_V1, &payload) {
            Ok(seq) => seq,
            Err(error) => {
                if let Some(prepared) = prepared {
                    prepared.abort(&StdVfs, &self.directory, self.durability_policy)?;
                }
                return Err(error.into());
            }
        };
        for row in rows {
            next.set_sequence(row, seq)?;
        }
        let committed = prepared
            .map(|prepared| prepared.commit(self, &StdVfs, &self.directory, self.durability_policy))
            .transpose()?;
        let replaced_paths = if let Some((remapped, replaced_paths)) = committed {
            let mut published = self
                .snapshot
                .write()
                .map_err(|_| StoreError::Synchronization {
                    component: "published snapshot",
                })?;
            let previous = published.replace(Arc::new(remapped));
            *active = Some(ActiveState {
                generation,
                segment: Arc::new(next),
            });
            drop(published);
            drop(previous);
            replaced_paths
        } else {
            *active = Some(ActiveState {
                generation,
                segment: Arc::new(next),
            });
            Vec::new()
        };
        purge::unlink_replaced_segments(
            &StdVfs,
            &self.directory,
            &replaced_paths,
            self.durability_policy,
        )?;
        Ok(IngestAck { seq, generation })
    }
}

fn encode_persisted_upsert(document: &IngestDocument) -> Result<(u16, Vec<u8>), IngestError> {
    if document.timestamp() == 0 && document.metadata().is_empty() {
        wal_payload::encode_upsert(document)
            .map(|payload| (wal_payload::UPSERT_V1, payload))
            .map_err(IngestError::Payload)
    } else if document.metadata().is_empty() {
        wal_payload::encode_upsert_with_timestamp(document)
            .map(|payload| (wal_payload::UPSERT_WITH_TIMESTAMP_V1, payload))
            .map_err(IngestError::Payload)
    } else if document.timestamp() == 0 {
        wal_payload::encode_upsert_with_metadata(document)
            .map(|payload| (wal_payload::UPSERT_WITH_METADATA_V1, payload))
            .map_err(IngestError::Payload)
    } else {
        wal_payload::encode_upsert_with_timestamp_and_metadata(document)
            .map(|payload| (wal_payload::UPSERT_WITH_TIMESTAMP_AND_METADATA_V1, payload))
            .map_err(IngestError::Payload)
    }
}

impl From<WalWriteError> for IngestError {
    fn from(error: WalWriteError) -> Self {
        Self::Store(StoreError::WalWrite(error))
    }
}
