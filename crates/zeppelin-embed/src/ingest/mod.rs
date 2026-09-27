//! Ingest and mutation coordination.

mod active;
mod atomic_batch;
mod delete_matching;
mod purge;

/// Survivor-rewrite helpers shared with graph-segment consolidation.
///
/// Purge's per-segment gathers are re-exported unchanged so consolidation
/// concatenates their survivor-order output across N inputs without forking
/// the rewrite logic; purge's own behavior stays byte-identical.
/// The pending purge intent decoder, for read-only store verification.
pub(crate) use purge::{PURGE_INTENT_FILE, read_intent};

pub(crate) mod purge_support {
    pub(crate) use super::purge::{
        OwnedFactors, PURGE_INTENT_FILE, append_survivor_columns, clustering_range,
        gather_survivor_codes, gather_survivor_documents, gather_survivor_rescore,
    };
}
mod retention;
#[cfg(any(test, feature = "test-support"))]
mod retention_fault;
mod revise;
mod seal;
pub mod wal_payload;

use std::path::PathBuf;
use std::sync::Arc;

use crate::lifecycle::{PublishedSnapshot, Store, StoreError, StoreState};
use crate::meta::{ColumnId, ColumnInput, ColumnStoreBuilder, ColumnValue, PredicateValue, Schema};
use crate::scan::ScanStats;
use crate::segment::SegmentId;
use crate::wal::{LogSeq, WalWriteError};

pub use delete_matching::{DeleteMatchingError, DeleteMatchingReport};
pub use purge::{PurgeError, PurgeReport, PurgeToken};
pub use retention::{DropPartitionReport, RetentionPolicy, RetentionPolicyError};
#[cfg(any(test, feature = "test-support"))]
pub use retention_fault::{
    IngestRetentionCheckpoint, IngestRetentionFaultController, IngestRetentionFaultEffect,
    IngestRetentionFaultKind, IngestRetentionFaultReceiptV1, IngestRetentionIoKind,
    IngestRetentionOperation, IngestRetentionPurgeCrashCheckpoint, IngestRetentionTestFault,
    PartialBatchAppendVfs, PurgeUnlinkErrorVfs,
};

pub(crate) use active::{ActiveSegment, ActiveState, SealedTombstoneDemand, StoreWal};
pub(crate) use atomic_batch::cut_interrupted_append;

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

/// A precondition on one document's live revision, checked atomically with
/// the batch that carries it.
///
/// Conditions are evaluated by the single writer against the latest committed
/// state (active rows and sealed segments) as it was before the batch. A
/// tombstoned id has no live document, so it matches [`Self::Absent`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExpectedRevision {
    /// A live document exists and its revision is exactly this value.
    Exactly(Revision),
    /// No live document exists: the id was never written or was deleted.
    Absent,
}

/// One owned vector mutation supplied to an ingest batch.
#[derive(Clone, Debug, PartialEq)]
pub struct IngestDocument {
    version: DocumentVersion,
    expected_revision: Option<ExpectedRevision>,
    vector: Vec<f32>,
    timestamp: i64,
    timestamp_present: bool,
    metadata: Vec<u8>,
    metadata_present: bool,
    text: Option<String>,
    columns: Vec<(ColumnId, PredicateValue)>,
    columns_present: bool,
}

impl IngestDocument {
    /// Constructs one document upsert.
    #[must_use]
    pub fn new(version: DocumentVersion, vector: Vec<f32>) -> Self {
        Self {
            version,
            expected_revision: None,
            vector,
            timestamp: 0,
            timestamp_present: false,
            metadata: Vec::new(),
            metadata_present: false,
            text: None,
            columns: Vec::new(),
            columns_present: false,
        }
    }

    /// Assigns the canonical `ts` clustering-key value.
    #[must_use]
    pub const fn with_timestamp(mut self, timestamp: i64) -> Self {
        self.timestamp = timestamp;
        self.timestamp_present = true;
        self
    }

    /// Assigns opaque stored metadata that must remain physically purgeable.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Vec<u8>) -> Self {
        self.metadata = metadata;
        self.metadata_present = true;
        self
    }

    /// Assigns the UTF-8 body indexed by the store's frozen text analyzer.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Assigns typed user-column values keyed by the store schema.
    #[must_use]
    pub fn with_columns(mut self, columns: Vec<(ColumnId, PredicateValue)>) -> Self {
        self.columns = columns;
        self.columns_present = true;
        self
    }

    /// Makes the whole batch conditional on this document's live revision.
    /// The condition is not persisted.
    #[must_use]
    pub const fn with_expected_revision(mut self, expected: ExpectedRevision) -> Self {
        self.expected_revision = Some(expected);
        self
    }

    /// Returns the live-revision precondition, if any.
    #[must_use]
    pub const fn expected_revision(&self) -> Option<ExpectedRevision> {
        self.expected_revision
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

    pub(crate) const fn has_timestamp(&self) -> bool {
        self.timestamp_present
    }

    /// Returns the opaque stored metadata bytes.
    #[must_use]
    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }

    pub(crate) const fn has_metadata(&self) -> bool {
        self.metadata_present
    }

    /// Returns the optional UTF-8 body indexed by lexical search.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Returns the typed schema-column values in caller order.
    #[must_use]
    pub fn columns(&self) -> &[(ColumnId, PredicateValue)] {
        &self.columns
    }

    pub(crate) const fn has_columns(&self) -> bool {
        self.columns_present
    }

    pub(crate) fn column_inputs(&self) -> Vec<ColumnInput<'_>> {
        column_inputs(&self.columns)
    }
}

pub(crate) fn column_inputs(columns: &[(ColumnId, PredicateValue)]) -> Vec<ColumnInput<'_>> {
    columns
        .iter()
        .map(|(column, value)| ColumnInput {
            column: *column,
            value: match value {
                PredicateValue::U64(value) => ColumnValue::U64(*value),
                PredicateValue::I64(value) => ColumnValue::I64(*value),
                PredicateValue::F64(value) => ColumnValue::F64(*value),
                PredicateValue::Bool(value) => ColumnValue::Bool(*value),
                PredicateValue::String(value) => ColumnValue::String(value),
            },
        })
        .collect()
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
    expected: Vec<Option<ExpectedRevision>>,
}

impl DeleteBatch {
    /// Takes ownership of document ids in caller order.
    #[must_use]
    pub const fn new(doc_ids: Vec<DocId>) -> Self {
        Self {
            doc_ids,
            expected: Vec::new(),
        }
    }

    /// Takes ids in caller order, each with an optional live-revision
    /// precondition; one failed condition aborts the whole batch.
    #[must_use]
    pub fn conditional(targets: Vec<(DocId, Option<ExpectedRevision>)>) -> Self {
        let (doc_ids, expected) = targets.into_iter().unzip();
        Self { doc_ids, expected }
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
    /// A document's expected-revision condition did not hold; nothing was
    /// written.
    RevisionConflict {
        /// Position of the failed condition in the caller's batch.
        index: usize,
        /// Document whose condition failed.
        doc_id: DocId,
        /// The caller's condition.
        expected: ExpectedRevision,
        /// Live revision at evaluation, or `None` when no live document exists.
        current: Option<Revision>,
    },
    /// A vector could not be quantized under the frozen Bit4 contract.
    Vector(crate::quant::QuantError),
    /// The frozen analyzer or active lexical index rejected the text row.
    Lexical(crate::fts::index::IndexError),
    /// The frozen text analyzer configuration failed to compile.
    Tokenizer(crate::fts::tokenizer::TokenizerError),
    /// Typed values did not satisfy the store's declared schema.
    Columns(crate::meta::BuildError),
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
            Self::RevisionConflict {
                index,
                doc_id,
                expected,
                current,
            } => {
                write!(
                    formatter,
                    "revision condition failed for document {} at batch index {index}: expected ",
                    doc_id.get()
                )?;
                match expected {
                    ExpectedRevision::Exactly(revision) => {
                        write!(formatter, "revision {}", revision.get())?;
                    }
                    ExpectedRevision::Absent => formatter.write_str("no live document")?,
                }
                match current {
                    Some(revision) => write!(formatter, ", current revision is {}", revision.get()),
                    None => formatter.write_str(", no live document exists"),
                }
            }
            Self::Vector(error) => write!(formatter, "ingest vector: {error}"),
            Self::Lexical(error) => write!(formatter, "ingest text: {error}"),
            Self::Tokenizer(error) => write!(formatter, "ingest tokenizer: {error}"),
            Self::Columns(error) => write!(formatter, "ingest columns: {error}"),
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
            Self::Lexical(error) => Some(error),
            Self::Tokenizer(error) => Some(error),
            Self::Columns(error) => Some(error),
            Self::Payload(error) => Some(error),
            Self::EmptyBatch
            | Self::EpochUndeclared
            | Self::EpochUnstamped
            | Self::StaleRevision { .. }
            | Self::RevisionConflict { .. } => None,
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
    pub(crate) filter: Option<&'a crate::lifecycle::QueryFilter>,
}

impl<'a> SearchRequest<'a> {
    /// Constructs a full-precision vector request.
    #[must_use]
    pub const fn new(vector: &'a [f32]) -> Self {
        Self {
            vector,
            filter: None,
        }
    }

    /// Restricts candidates before ranking, including both hybrid legs.
    #[must_use]
    pub const fn with_filter(mut self, filter: Option<&'a crate::lifecycle::QueryFilter>) -> Self {
        self.filter = filter;
        self
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

/// Permanent public ordering for globally merged vector candidates.
///
/// Score is descending; documented rows then use document ID ascending and
/// revision descending before the physical row identity. Legacy rows without
/// a document-version region fall back directly to physical identity.
pub(crate) fn compare_search_candidates(
    left: &SearchCandidate,
    right: &SearchCandidate,
) -> std::cmp::Ordering {
    right.score().total_cmp(&left.score()).then_with(|| {
        match (left.document(), right.document()) {
            (Some(left_document), Some(right_document)) => left_document
                .doc_id()
                .cmp(&right_document.doc_id())
                .then_with(|| right_document.revision().cmp(&left_document.revision())),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.row_id().cmp(&right.row_id()))
    })
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
    /// Candidates merged across the active segment and the published epoch's sealed segments.
    pub candidates: Vec<SearchCandidate>,
    /// Producer-supplied squared-L2 ceiling used to normalize a bounded hybrid
    /// vector leg. It is absent when the selected producer cannot yet provide
    /// a corpus-wide bound.
    pub vector_ceiling: Option<f64>,
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

/// One store-backed lexical hit joined to its stable document identity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LexicalCandidate {
    /// Stable application document version.
    pub document: DocumentVersion,
    /// Larger-is-better BM25 score.
    pub score: f64,
}

/// Store-backed lexical hits and truthful diagnostics from one pinned generation.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreLexicalSearchOutcome {
    /// Globally ranked lexical hits.
    pub candidates: Vec<LexicalCandidate>,
    /// Active generation pinned with the immutable snapshot.
    pub generation: u64,
    /// Unconditional report of the lexical work actually performed.
    pub diagnostics: crate::diag::QueryDiagnostics,
}

/// One explained structured lexical hit with owned persisted-source text.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainedLexicalCandidate {
    /// Stable application document version.
    pub document: DocumentVersion,
    /// Larger-is-better boosted BM25 score.
    pub score: f64,
    /// Expansions that contributed to this row's score.
    pub provenance: Vec<crate::fts::query::LexicalExpansion>,
    /// UTF-8-valid window copied from the exact persisted source text.
    pub snippet: crate::fts::query::OwnedLexicalSnippet,
}

/// Explained structured lexical hits from one pinned generation.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreStructuredLexicalSearchOutcome {
    /// Globally ranked explained hits.
    pub candidates: Vec<ExplainedLexicalCandidate>,
    /// Every vocabulary expansion and its exact boost, in deterministic order.
    pub expansions: Vec<crate::fts::query::LexicalExpansion>,
    /// Active generation pinned with the immutable snapshot.
    pub generation: u64,
    /// Unconditional report of the lexical work actually performed.
    pub diagnostics: crate::diag::QueryDiagnostics,
}

/// Store-level exact vector/lexical fusion over stable DocIds.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreHybridSearchOutcome {
    /// Fused hits keyed by stable store document id.
    pub hits: Vec<crate::fusion::FusedHit<DocId>>,
    /// Structured lexical expansions used by the lexical leg.
    pub lexical_expansions: Vec<crate::fts::query::LexicalExpansion>,
    /// Active generation pinned with the immutable snapshot.
    pub generation: u64,
    /// Unconditional report including the existing fusion report type.
    pub diagnostics: crate::diag::QueryDiagnostics,
}

/// Typed store lexical query failure.
#[derive(Debug)]
pub enum StoreLexicalError {
    /// Store admission, cancellation, or lifecycle failed.
    Query(crate::lifecycle::QueryError),
    /// Lexical planning or scoring failed.
    Lexical(crate::planner::LexicalFilterError),
    /// A postings-bearing immutable segment has no document-identity region.
    MissingDocumentIdentity {
        /// Immutable segment that cannot join local rows to store DocIds.
        segment_id: SegmentId,
    },
    /// A matching row predates the append-only stored-text region.
    MissingStoredText {
        /// Immutable segment containing the matching row.
        segment_id: SegmentId,
        /// Dense row whose source text was requested.
        row: u32,
    },
    /// Stored text existed but none of the reported terms mapped to a source span.
    MissingSnippetMatch {
        /// Dense lexical source ordinal.
        segment: u32,
        /// Dense row within that source.
        row: u32,
    },
    /// Structured query shape was invalid.
    Structured(crate::fts::query::LexicalQueryError),
    /// Snippet options were invalid.
    Snippet(crate::fts::snippet::SnippetError),
    /// The pinned tokenizer configuration could not be constructed.
    Tokenizer(crate::fts::tokenizer::TokenizerError),
}

impl std::fmt::Display for StoreLexicalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(error) => error.fmt(formatter),
            Self::Lexical(error) => error.fmt(formatter),
            Self::MissingDocumentIdentity { segment_id } => write!(
                formatter,
                "sealed lexical segment {segment_id} has no document identity"
            ),
            Self::MissingStoredText { segment_id, row } => write!(
                formatter,
                "sealed lexical segment {segment_id} row {row} has no stored source text"
            ),
            Self::MissingSnippetMatch { segment, row } => write!(
                formatter,
                "lexical source {segment} row {row} has no snippet match span"
            ),
            Self::Structured(error) => error.fmt(formatter),
            Self::Snippet(error) => error.fmt(formatter),
            Self::Tokenizer(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for StoreLexicalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Query(error) => Some(error),
            Self::Lexical(error) => Some(error),
            Self::Structured(error) => Some(error),
            Self::Snippet(error) => Some(error),
            Self::Tokenizer(error) => Some(error),
            Self::MissingDocumentIdentity { .. }
            | Self::MissingStoredText { .. }
            | Self::MissingSnippetMatch { .. } => None,
        }
    }
}

impl From<crate::lifecycle::QueryError> for StoreLexicalError {
    fn from(error: crate::lifecycle::QueryError) -> Self {
        Self::Query(error)
    }
}

impl From<StoreError> for StoreLexicalError {
    fn from(error: StoreError) -> Self {
        Self::Query(crate::lifecycle::QueryError::Store(error))
    }
}

impl From<crate::planner::LexicalFilterError> for StoreLexicalError {
    fn from(error: crate::planner::LexicalFilterError) -> Self {
        Self::Lexical(error)
    }
}

impl From<crate::fts::query::LexicalQueryError> for StoreLexicalError {
    fn from(error: crate::fts::query::LexicalQueryError) -> Self {
        Self::Structured(error)
    }
}

impl From<crate::fts::snippet::SnippetError> for StoreLexicalError {
    fn from(error: crate::fts::snippet::SnippetError) -> Self {
        Self::Snippet(error)
    }
}

impl From<crate::fts::tokenizer::TokenizerError> for StoreLexicalError {
    fn from(error: crate::fts::tokenizer::TokenizerError) -> Self {
        Self::Tokenizer(error)
    }
}

#[derive(Clone, Copy)]
struct ResolvedRevision {
    version: DocumentVersion,
    seq: LogSeq,
    active_row: Option<usize>,
}

struct RevisionProgress {
    working: Option<ActiveSegment>,
    records: Vec<(usize, u16, Vec<u8>)>,
    replay_seq: Option<LogSeq>,
    #[cfg(any(test, feature = "test-support"))]
    replay_count: usize,
    sealed_tombstones: Vec<DocId>,
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

/// Returns the live revision of `doc_id` in the given committed state, or
/// `None` when no live row exists. Tombstoned rows are not live.
fn live_revision(
    active: &ActiveSegment,
    snapshot: &PublishedSnapshot,
    doc_id: DocId,
) -> Result<Option<Revision>, StoreError> {
    if let Some((row, version, _)) = active.existing(doc_id)
        && !active.is_tombstoned(row)
    {
        return Ok(Some(version.revision()));
    }
    for segment in snapshot.segments() {
        for row in segment.query_rows_for_doc_id(doc_id)? {
            if let Some(version) = segment.document_version(row).map_err(StoreError::Segment)? {
                return Ok(Some(version.revision()));
            }
        }
    }
    Ok(None)
}

/// Evaluates every `(index, id, condition)` against one committed state and
/// fails on the first that does not hold. Runs under the WAL writer lock
/// before any mutation work, so a failure writes nothing.
fn check_revision_conditions(
    active: &ActiveSegment,
    snapshot: &PublishedSnapshot,
    conditions: impl Iterator<Item = (usize, DocId, Option<ExpectedRevision>)>,
) -> Result<(), IngestError> {
    for (index, doc_id, expected) in conditions {
        let Some(expected) = expected else {
            continue;
        };
        let current = live_revision(active, snapshot, doc_id)?;
        let holds = match expected {
            ExpectedRevision::Exactly(revision) => current == Some(revision),
            ExpectedRevision::Absent => current.is_none(),
        };
        if !holds {
            return Err(IngestError::RevisionConflict {
                index,
                doc_id,
                expected,
                current,
            });
        }
    }
    Ok(())
}

fn validate_republish_generation(
    expected_generation: u64,
    actual_generation: u64,
) -> Result<(), StoreError> {
    if actual_generation != expected_generation {
        return Err(StoreError::ConcurrentActiveMutation {
            expected_generation,
            actual_generation,
        });
    }
    Ok(())
}

impl Store {
    /// Returns exact live corpus statistics for store-backed lexical scoring.
    ///
    /// # Errors
    ///
    /// Returns a store admission or segment error when the current lexical
    /// corpus cannot be read, or an index error when the corpus is empty.
    pub fn lexical_corpus_stats(&self) -> Result<crate::fts::bm25::CorpusStats, StoreLexicalError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing.into()),
            StoreState::Closed => return Err(StoreError::Closed.into()),
        }
        let active_guard = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let active = active_guard.as_ref().ok_or(StoreError::Closed)?;
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        let mut index = crate::fts::index::LexicalIndex::new();
        for segment in snapshot.segments() {
            if let Some(postings) = segment.query_postings()? {
                let alive = segment.query_alive()?;
                index
                    .push_shared_with_live_rows(postings, alive.alive_bitmap())
                    .map_err(crate::planner::LexicalFilterError::from)?;
            }
        }
        if active.segment.has_text() {
            let sealed = active.segment.sealed_lexical(&self.accounting)?;
            let alive = active.segment.alive()?;
            index
                .push_shared_with_live_rows(sealed, alive.alive_bitmap())
                .map_err(crate::planner::LexicalFilterError::from)?;
        }
        index
            .corpus_stats()
            .map_err(crate::planner::LexicalFilterError::from)
            .map_err(StoreLexicalError::from)
    }

    #[inline(always)]
    fn apply_revision_decision(
        &self,
        current: &ActiveSegment,
        sealed: &[purge::SealedDocumentMatch],
        sealed_seq: LogSeq,
        document: &IngestDocument,
        remaining: &[IngestDocument],
        progress: &mut RevisionProgress,
    ) -> Result<(), IngestError> {
        let segment = progress.working.as_ref().unwrap_or(current);
        let existing = resolve_revision(segment, sealed, document.version().doc_id(), sealed_seq);
        let action = revise::classify_revision(
            existing.map(|resolved| (resolved.version, resolved.seq)),
            document.version(),
        );
        match action {
            revise::RevisionDecision::Replace => {
                let row = existing.and_then(|resolved| resolved.active_row);
                if progress.working.is_none() {
                    progress.working =
                        Some(current.copy_for_batch(document, remaining, &self.accounting)?);
                }
                let working = progress
                    .working
                    .as_mut()
                    .ok_or(StoreError::Synchronization {
                        component: "batch working segment",
                    })?;
                let row = if let Some(row) = row {
                    working.replace_in_place(row, document, &self.accounting, &self.tokenizer)?;
                    row
                } else {
                    working.insert_in_place(document, &self.accounting, &self.tokenizer)?
                };
                let (op, payload) = encode_persisted_upsert(document)?;
                progress.records.push((row, op, payload));
                if sealed
                    .iter()
                    .any(|matched| matched.version.doc_id() == document.version().doc_id())
                    && !progress
                        .sealed_tombstones
                        .contains(&document.version().doc_id())
                {
                    progress.sealed_tombstones.push(document.version().doc_id());
                }
            }
            revise::RevisionDecision::Insert => {
                if progress.working.is_none() {
                    progress.working =
                        Some(current.copy_for_batch(document, remaining, &self.accounting)?);
                }
                let working = progress
                    .working
                    .as_mut()
                    .ok_or(StoreError::Synchronization {
                        component: "batch working segment",
                    })?;
                let row = working.insert_in_place(document, &self.accounting, &self.tokenizer)?;
                let (op, payload) = encode_persisted_upsert(document)?;
                progress.records.push((row, op, payload));
            }
            revise::RevisionDecision::Replay { seq } => {
                progress.replay_seq = Some(
                    progress
                        .replay_seq
                        .map_or(seq, |current: LogSeq| current.max(seq)),
                );
                #[cfg(any(test, feature = "test-support"))]
                {
                    progress.replay_count = progress.replay_count.saturating_add(1);
                }
            }
            revise::RevisionDecision::Reject { current } => {
                return Err(IngestError::StaleRevision {
                    doc_id: document.version().doc_id(),
                    current,
                    attempted: document.version().revision(),
                });
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    fn record_post_ack_retry_receipt(
        &self,
        batch: &IngestBatch,
        replay_count: usize,
        seq: LogSeq,
        generation: u64,
    ) -> Result<(), IngestError> {
        if let Some(controller) = self.ingest_retention_fault_controller.as_ref() {
            let plan =
                controller
                    .post_ack_retry_plan()
                    .map_err(|_| StoreError::Synchronization {
                        component: "ingest-retention controller",
                    })?;
            if let Some((invocation_id, first_ack_seq, first_ack_generation)) = plan
                && first_ack_seq == seq.get()
                && first_ack_generation == generation
            {
                let batch_count = u64::try_from(batch.documents.len())
                    .map_err(|_| StoreError::ActiveRowOverflow)?;
                let replay_count =
                    u64::try_from(replay_count).map_err(|_| StoreError::ActiveRowOverflow)?;
                controller
                    .push_receipt(IngestRetentionFaultReceiptV1::post_ack_retry(
                        invocation_id,
                        batch_count,
                        replay_count,
                        seq.get(),
                        generation,
                    ))
                    .map_err(|_| StoreError::Synchronization {
                        component: "ingest-retention controller",
                    })?;
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    fn recover_partial_batch_append(
        &self,
        writer: &mut StoreWal,
        error: &StoreError,
        partial_append_clean_wal: &Option<Vec<u8>>,
        batch: &IngestBatch,
        records: &[(usize, u16, Vec<u8>)],
    ) -> Result<(), IngestError> {
        if let Some(controller) = self.ingest_retention_fault_controller.as_ref()
            && let Some(observation) =
                controller
                    .take_partial_batch_append_observation()
                    .map_err(|_| StoreError::Synchronization {
                        component: "ingest-retention controller",
                    })?
        {
            let expected_detail = observation.detail.as_str();
            match error {
                StoreError::WalWrite(WalWriteError::Failed { kind, detail })
                    if *kind == std::io::ErrorKind::Other && detail.as_ref() == expected_detail => {
                }
                _ => {
                    return Err(StoreError::Synchronization {
                        component: "partial-batch-append typed WAL error",
                    }
                    .into());
                }
            }
            let clean_wal =
                partial_append_clean_wal
                    .as_deref()
                    .ok_or(StoreError::Synchronization {
                        component: "partial-batch-append clean WAL image",
                    })?;
            let wal_path = self.directory.join("wal.ze");
            writer.restore_after_failed_append(
                self.vfs.as_ref(),
                &wal_path,
                self.durability_policy,
                clean_wal,
            )?;
            let submitted_count =
                u64::try_from(batch.documents.len()).map_err(|_| StoreError::ActiveRowOverflow)?;
            let changed_records =
                u64::try_from(records.len()).map_err(|_| StoreError::ActiveRowOverflow)?;
            let encoded_bytes = u64::try_from(observation.encoded_bytes)
                .map_err(|_| StoreError::ActiveRowOverflow)?;
            let prefix_bytes = u64::try_from(observation.prefix_bytes)
                .map_err(|_| StoreError::ActiveRowOverflow)?;
            controller
                .push_receipt(IngestRetentionFaultReceiptV1::partial_batch_append(
                    observation.invocation_id,
                    submitted_count,
                    changed_records,
                    encoded_bytes,
                    prefix_bytes,
                    observation.detail,
                ))
                .map_err(|_| StoreError::Synchronization {
                    component: "ingest-retention controller",
                })?;
        }
        Ok(())
    }

    #[inline(always)]
    fn publish_committed_active(
        &self,
        active: &mut Option<ActiveState>,
        committed: Option<(PublishedSnapshot, Vec<PathBuf>)>,
        generation: u64,
        next: ActiveSegment,
    ) -> Result<Vec<PathBuf>, StoreError> {
        if let Some((remapped, replaced_paths)) = committed {
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
            Ok(replaced_paths)
        } else {
            *active = Some(ActiveState {
                generation,
                segment: Arc::new(next),
            });
            Ok(Vec::new())
        }
    }

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
        drop(state);
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
        if self.epoch.as_ref().is_some_and(|epoch| {
            epoch.embedding.document.normalization == crate::epoch::Normalization::L2
        }) {
            for document in &batch.documents {
                if let Some(squared_norm) =
                    crate::graph::search::non_unit_squared_norm(document.vector())
                {
                    return Err(IngestError::Vector(crate::quant::QuantError::NonUnitNorm {
                        squared_norm_bits: squared_norm.to_bits(),
                        tolerance_bits: crate::graph::search::UNIT_NORM_SQUARED_TOLERANCE.to_bits(),
                    }));
                }
            }
        }
        for document in &batch.documents {
            validate_document_columns(&self.schema, document)?;
        }
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_mut().ok_or(StoreError::ReadOnly)?;
        let (current_generation, current_segment) = {
            let active = self
                .active
                .lock()
                .map_err(|_| StoreError::Synchronization {
                    component: "active segment",
                })?;
            let current = active.as_ref().ok_or(StoreError::Closed)?;
            (current.generation, Arc::clone(&current.segment))
        };
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        check_revision_conditions(
            current_segment.as_ref(),
            &snapshot,
            batch.documents.iter().enumerate().map(|(index, document)| {
                (
                    index,
                    document.version().doc_id(),
                    document.expected_revision(),
                )
            }),
        )?;
        let requested_ids = batch
            .documents
            .iter()
            .map(|document| document.version().doc_id())
            .collect::<Vec<_>>();
        let sealed = purge::sealed_document_matches(&snapshot, &requested_ids)?;
        let sealed_seq = LogSeq::new(snapshot.absorbed_through());
        let mut progress = RevisionProgress {
            working: None,
            records: Vec::new(),
            replay_seq: None,
            #[cfg(any(test, feature = "test-support"))]
            replay_count: 0,
            sealed_tombstones: Vec::new(),
        };
        for (index, document) in batch.documents.iter().enumerate() {
            let remaining = batch
                .documents
                .get(index..)
                .ok_or(StoreError::ActiveRowOverflow)?;
            self.apply_revision_decision(
                current_segment.as_ref(),
                &sealed,
                sealed_seq,
                document,
                remaining,
                &mut progress,
            )?;
        }
        let RevisionProgress {
            working,
            records,
            replay_seq,
            #[cfg(any(test, feature = "test-support"))]
            replay_count,
            sealed_tombstones,
        } = progress;
        let Some(mut next) = working else {
            let seq = replay_seq.ok_or(StoreError::Synchronization {
                component: "nonempty ingest replay sequence",
            })?;
            #[cfg(any(test, feature = "test-support"))]
            self.record_post_ack_retry_receipt(&batch, replay_count, seq, current_generation)?;
            return Ok(IngestAck {
                seq,
                generation: current_generation,
            });
        };
        let records = atomic_batch::frame_batch(records)?;
        let generation = current_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let prepared = purge::prepare_sealed_tombstones(
            self.vfs.as_ref(),
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
        #[cfg(any(test, feature = "test-support"))]
        let partial_append_clean_wal = match self.ingest_retention_fault_controller.as_ref() {
            Some(controller)
                if controller
                    .partial_batch_append_plan()
                    .map_err(|_| StoreError::Synchronization {
                        component: "ingest-retention controller",
                    })?
                    .is_some() =>
            {
                let path = self.directory.join("wal.ze");
                Some(
                    self.vfs
                        .read(&path)
                        .map_err(|source| StoreError::Io { path, source })?,
                )
            }
            Some(_) | None => None,
        };
        let sequences = match writer.commit_many(&record_refs) {
            Ok(sequences) => sequences,
            Err(error) => {
                if let Some(prepared) = prepared {
                    prepared.abort(self.vfs.as_ref(), &self.directory, self.durability_policy)?;
                }
                #[cfg(any(test, feature = "test-support"))]
                self.recover_partial_batch_append(
                    writer,
                    &error,
                    &partial_append_clean_wal,
                    &batch,
                    &records,
                )?;
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
            .map(|prepared| {
                prepared.commit(
                    self,
                    self.vfs.as_ref(),
                    &self.directory,
                    self.durability_policy,
                )
            })
            .transpose()?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let actual_generation = active.as_ref().ok_or(StoreError::Closed)?.generation;
        validate_republish_generation(current_generation, actual_generation)?;
        let replaced_paths =
            self.publish_committed_active(&mut active, committed, generation, next)?;
        drop(active);
        purge::unlink_replaced_segments(
            self.vfs.as_ref(),
            &self.directory,
            &replaced_paths,
            self.durability_policy,
        );
        drop(wal);
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
        drop(state);
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_mut().ok_or(StoreError::ReadOnly)?;
        self.delete_with_writer(writer, &batch.doc_ids, &batch.expected)
    }

    /// The body of [`Self::delete`] for a caller that already holds the WAL
    /// writer, so one mutation can resolve ids and tombstone them without
    /// another write landing in between. `doc_ids` must be non-empty.
    /// `expected` holds the revision condition of each id by position; a
    /// missing entry is unconditional.
    pub(crate) fn delete_with_writer(
        &self,
        writer: &mut StoreWal,
        doc_ids: &[DocId],
        expected: &[Option<ExpectedRevision>],
    ) -> Result<IngestAck, IngestError> {
        let (current_generation, current_segment) = {
            let active = self
                .active
                .lock()
                .map_err(|_| StoreError::Synchronization {
                    component: "active segment",
                })?;
            let current = active.as_ref().ok_or(StoreError::Closed)?;
            (current.generation, Arc::clone(&current.segment))
        };
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        check_revision_conditions(
            current_segment.as_ref(),
            &snapshot,
            doc_ids
                .iter()
                .enumerate()
                .map(|(index, doc_id)| (index, *doc_id, expected.get(index).copied().flatten())),
        )?;
        let sealed = purge::sealed_document_matches(&snapshot, doc_ids)?;
        let generation = current_generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let (mut next, rows) = current_segment.tombstone(doc_ids, &self.accounting)?;
        let prepared = purge::prepare_sealed_tombstones(
            self.vfs.as_ref(),
            &self.directory,
            &snapshot,
            &sealed,
            doc_ids,
            writer.durable_end(),
            generation,
            writer.durable_end().saturating_add(1),
            self.durability_policy,
            &self.accounting,
        )?;
        let payload = wal_payload::encode_delete(doc_ids).map_err(IngestError::Payload)?;
        let seq = match writer.commit(wal_payload::DELETE_V1, &payload) {
            Ok(seq) => seq,
            Err(error) => {
                if let Some(prepared) = prepared {
                    prepared.abort(self.vfs.as_ref(), &self.directory, self.durability_policy)?;
                }
                return Err(error.into());
            }
        };
        for row in rows {
            next.set_sequence(row, seq)?;
        }
        let committed = prepared
            .map(|prepared| {
                prepared.commit(
                    self,
                    self.vfs.as_ref(),
                    &self.directory,
                    self.durability_policy,
                )
            })
            .transpose()?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let actual_generation = active.as_ref().ok_or(StoreError::Closed)?.generation;
        validate_republish_generation(current_generation, actual_generation)?;
        let replaced_paths =
            self.publish_committed_active(&mut active, committed, generation, next)?;
        drop(active);
        purge::unlink_replaced_segments(
            self.vfs.as_ref(),
            &self.directory,
            &replaced_paths,
            self.durability_policy,
        );
        Ok(IngestAck { seq, generation })
    }
}

fn encode_persisted_upsert(document: &IngestDocument) -> Result<(u16, Vec<u8>), IngestError> {
    wal_payload::encode_upsert_v2(document)
        .map(|payload| (wal_payload::UPSERT_V2, payload))
        .map_err(IngestError::Payload)
}

pub(crate) fn validate_document_columns(
    schema: &Schema,
    document: &IngestDocument,
) -> Result<(), IngestError> {
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    builder
        .push_row(document.timestamp(), &document.column_inputs())
        .map(|_| ())
        .map_err(IngestError::Columns)
}

impl From<WalWriteError> for IngestError {
    fn from(error: WalWriteError) -> Self {
        Self::Store(StoreError::WalWrite(error))
    }
}

#[cfg(test)]
mod vector_order_tests {
    use super::*;

    #[test]
    fn documented_candidate_precedes_legacy_candidate_at_an_equal_score() {
        let segment = SegmentId::from_bytes([0x44; 16]);
        let legacy = SearchCandidate::new(
            GlobalRowId::new(RowSource::Sealed(segment), 0),
            None,
            1.0,
            true,
        );
        let documented = SearchCandidate::new(
            GlobalRowId::new(RowSource::Sealed(segment), 9),
            Some(DocumentVersion::new(DocId::new(9), Revision::new(1))),
            1.0,
            true,
        );

        assert_eq!(
            compare_search_candidates(&documented, &legacy),
            std::cmp::Ordering::Less,
            "documented-vs-legacy order fell back to physical row identity"
        );
    }
}

#[cfg(test)]
mod active_publish_tests {
    use super::*;

    #[test]
    fn mismatched_active_generation_on_republish_is_a_typed_error() {
        assert!(matches!(
            validate_republish_generation(4, 5),
            Err(StoreError::ConcurrentActiveMutation {
                expected_generation: 4,
                actual_generation: 5,
            })
        ));
    }
}

#[cfg(all(test, feature = "allocation-audit"))]
#[allow(clippy::expect_used)]
mod batch_working_copy_tests {
    use tempfile::tempdir;

    use crate::lifecycle::OpenOptions;

    use super::*;

    #[test]
    fn batch_ingest_clones_the_active_segment_at_most_once() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )]))
            .expect("seed active segment");
        let documents = (2_u128..=25)
            .map(|doc_id| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(doc_id), Revision::new(1)),
                    vec![1.0, 0.0],
                )
            })
            .collect::<Vec<_>>();

        let (result, report) = crate::allocation_audit::audit_engine_path(|| {
            store.ingest(IngestBatch::new(documents))
        });

        result.expect("ingest audited batch");
        assert_eq!(
            report.full_segment_clones, 1,
            "one ingest batch made more than one full active-segment clone"
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod wal_replay_tests {
    use tempfile::tempdir;

    use crate::lifecycle::{DocumentFields, OpenOptions, StoredDocument};

    use super::*;

    fn document(doc_id: u128, revision: u64, text: Option<&str>) -> IngestDocument {
        let document = IngestDocument::new(
            DocumentVersion::new(DocId::new(doc_id), Revision::new(revision)),
            vec![1.0, doc_id as f32],
        );
        match text {
            Some(text) => document.with_text(text),
            None => document,
        }
    }

    /// Writes one WAL record per mutation: text-less rows before the first
    /// texted row, replacements, deletes, and a deleted id written again.
    pub(super) fn write_mixed_wal(store: &Store) {
        for doc_id in 1_u128..=40 {
            let text = format!("alpha beta note {doc_id}");
            let text = (doc_id > 3 && doc_id % 7 != 0).then_some(text.as_str());
            store
                .ingest(IngestBatch::new(vec![document(doc_id, 1, text)]))
                .expect("ingest one row");
        }
        for doc_id in [3_u128, 10, 21] {
            store
                .ingest(IngestBatch::new(vec![document(
                    doc_id,
                    2,
                    Some("gamma replaced text"),
                )]))
                .expect("replace one row");
        }
        store
            .delete(DeleteBatch::new(vec![
                DocId::new(5),
                DocId::new(6),
                DocId::new(10),
            ]))
            .expect("delete rows");
        store
            .ingest(IngestBatch::new(vec![document(
                5,
                2,
                Some("delta revived"),
            )]))
            .expect("write a deleted id again");
    }

    fn observed(store: &Store) -> (Vec<Option<StoredDocument>>, crate::fts::bm25::CorpusStats) {
        let ids = (1_u128..=41).map(DocId::new).collect::<Vec<_>>();
        (
            store
                .get_documents(&ids, DocumentFields::TEXT)
                .expect("read documents"),
            store.lexical_corpus_stats().expect("corpus stats"),
        )
    }

    #[test]
    fn sealed_revision_lookup_finds_every_sealed_copy_of_an_id() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
        for (doc_id, revision) in [(1_u128, 1_u64), (2, 1), (3, 1)] {
            store
                .ingest(IngestBatch::new(vec![document(
                    doc_id,
                    revision,
                    Some("first"),
                )]))
                .expect("ingest first segment");
        }
        store.seal().expect("seal first segment");
        store
            .ingest(IngestBatch::new(vec![document(2, 2, Some("second"))]))
            .expect("replace a sealed row");
        store.seal().expect("seal second segment");

        let stale = store.ingest(IngestBatch::new(vec![document(2, 1, Some("stale"))]));
        assert!(
            matches!(stale, Err(IngestError::StaleRevision { .. })),
            "a revision older than a sealed one was accepted: {stale:?}"
        );
        store
            .ingest(IngestBatch::new(vec![document(2, 3, Some("third"))]))
            .expect("write a newer revision");
        store
            .delete(DeleteBatch::new(vec![DocId::new(3)]))
            .expect("delete a sealed row");
        let documents = store
            .get_documents(
                &[DocId::new(1), DocId::new(2), DocId::new(3)],
                DocumentFields::TEXT,
            )
            .expect("read documents");
        let texts = documents
            .iter()
            .map(|document| document.as_ref().and_then(|document| document.text.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![Some("first".to_owned()), Some("third".to_owned()), None]
        );
        store.close().expect("close store");
    }

    #[test]
    fn wal_replay_reopens_the_exact_live_state() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
        write_mixed_wal(&store);
        let before = observed(&store);
        store.close().expect("close store");

        let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen");
        assert_eq!(observed(&reopened), before);
        reopened.close().expect("close reopened store");
    }
}

#[cfg(all(test, feature = "allocation-audit"))]
#[allow(clippy::expect_used)]
mod wal_replay_copy_tests {
    use tempfile::tempdir;

    use crate::lifecycle::OpenOptions;

    use super::*;

    #[test]
    fn wal_replay_clones_the_active_segment_at_most_once() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
        super::wal_replay_tests::write_mixed_wal(&store);
        store.close().expect("close store");

        let (reopened, report) = crate::allocation_audit::audit_engine_path(|| {
            Store::open(directory.path(), OpenOptions::default())
        });

        reopened
            .expect("reopen")
            .close()
            .expect("close reopened store");
        assert!(
            report.full_segment_clones <= 1,
            "WAL replay cloned the active segment {} times",
            report.full_segment_clones
        );
    }
}
