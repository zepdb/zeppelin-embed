//! Ingest and mutation coordination.

mod active;
mod revise;
mod seal;
pub mod wal_payload;

use std::sync::Arc;

use crate::lifecycle::{Store, StoreError, StoreState};
use crate::scan::ScanStats;
use crate::segment::SegmentId;
use crate::wal::{LogSeq, WalWriteError};

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
}

impl IngestDocument {
    /// Constructs one document upsert.
    #[must_use]
    pub fn new(version: DocumentVersion, vector: Vec<f32>) -> Self {
        Self { version, vector }
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
}

/// One atomic caller batch of document upserts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IngestBatch {
    documents: Vec<IngestDocument>,
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
        Self { documents }
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
    /// The attempted revision precedes the active document revision.
    StaleRevision {
        /// Document whose history would have moved backward.
        doc_id: DocId,
        /// Revision currently visible in the active segment.
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
            Self::Vector(error) => Some(error),
            Self::Payload(error) => Some(error),
            Self::EmptyBatch | Self::StaleRevision { .. } => None,
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
}

impl SearchCandidate {
    pub(crate) const fn new(
        row_id: GlobalRowId,
        document: Option<DocumentVersion>,
        score: f32,
    ) -> Self {
        Self {
            row_id,
            document,
            score,
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
    /// Retained graph candidates read from full-precision storage.
    pub candidates_rescored: usize,
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
        let mut working = None;
        let mut records = Vec::new();
        let mut replay_seq = None;
        for document in &batch.documents {
            let segment = working.as_ref().unwrap_or(current.segment.as_ref());
            let action = revise::classify(
                segment.existing(document.version().doc_id()),
                document.version(),
                segment.row_count(),
            );
            match action {
                revise::RevisionAction::Replace { row } => {
                    let next = segment.replace(row, document, &self.accounting)?;
                    let payload =
                        wal_payload::encode_upsert(document).map_err(IngestError::Payload)?;
                    working = Some(next);
                    records.push((row, payload));
                }
                revise::RevisionAction::Insert { row } => {
                    let next = segment.insert(document, &self.accounting)?;
                    let payload =
                        wal_payload::encode_upsert(document).map_err(IngestError::Payload)?;
                    working = Some(next);
                    records.push((row, payload));
                }
                revise::RevisionAction::Replay { seq } => {
                    replay_seq = Some(replay_seq.map_or(seq, |current: LogSeq| current.max(seq)));
                }
                revise::RevisionAction::Reject { current } => {
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
        let record_refs = records
            .iter()
            .map(|(_, payload)| (wal_payload::UPSERT_V1, payload.as_slice()))
            .collect::<Vec<_>>();
        let sequences = writer.commit_many(&record_refs)?;
        for ((row, _), sequence) in records
            .iter()
            .zip(sequences.start.get()..sequences.end.get())
        {
            next.set_sequence(*row, LogSeq::new(sequence))?;
        }
        let seq = LogSeq::new(sequences.end.get().saturating_sub(1));
        *active = Some(ActiveState {
            generation,
            segment: Arc::new(next),
        });
        drop(active);
        drop(wal);
        drop(state);
        Ok(IngestAck { seq, generation })
    }

    /// Durably tombstones documents in the active segment before acknowledging.
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
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let (mut next, rows) = current
            .segment
            .tombstone(&batch.doc_ids, &self.accounting)?;
        let payload = wal_payload::encode_delete(&batch.doc_ids).map_err(IngestError::Payload)?;
        let seq = writer.commit(wal_payload::DELETE_V1, &payload)?;
        for row in rows {
            next.set_sequence(row, seq)?;
        }
        *active = Some(ActiveState {
            generation,
            segment: Arc::new(next),
        });
        Ok(IngestAck { seq, generation })
    }
}

impl From<WalWriteError> for IngestError {
    fn from(error: WalWriteError) -> Self {
        Self::Store(StoreError::WalWrite(error))
    }
}
