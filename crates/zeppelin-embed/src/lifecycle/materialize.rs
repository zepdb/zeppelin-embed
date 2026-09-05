//! Scoped ownership of text returned from a ranked store query.

use super::{PublishedSnapshot, QueryCancellation, QueryError, SnapshotLease, Store, StoreError};
use crate::ingest::{
    ActiveSegment, DocId, DocumentVersion, GlobalRowId, RowSource, SearchCandidate,
};
use std::cell::Cell;
use std::sync::Arc;

/// Preserves a deferred vector producer's typed error separately from search.
#[derive(Debug)]
pub enum HybridPreparationError<E> {
    /// The caller could not produce a validated query vector.
    Preparation(E),
    /// Admission, retrieval, fusion, cancellation or materialization failed.
    Search(crate::fusion::FusionError),
}

impl<E> From<crate::fusion::FusionError> for HybridPreparationError<E> {
    fn from(error: crate::fusion::FusionError) -> Self {
        Self::Search(error)
    }
}

impl<E> From<QueryError> for HybridPreparationError<E> {
    fn from(error: QueryError) -> Self {
        Self::Search(error.into())
    }
}

impl<E: std::fmt::Display> std::fmt::Display for HybridPreparationError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preparation(error) => error.fmt(formatter),
            Self::Search(error) => error.fmt(formatter),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for HybridPreparationError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Preparation(error) => Some(error),
            Self::Search(error) => Some(error),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub(super) struct TestMaterializationWork {
    pub(super) admissions: std::sync::atomic::AtomicU64,
    pub(super) reverse_version_lookups: std::sync::atomic::AtomicU64,
    pub(super) direct_row_lookups: std::sync::atomic::AtomicU64,
    #[cfg(feature = "query-timing")]
    pub(super) lexical_admission_locks: std::sync::atomic::AtomicU64,
    #[cfg(feature = "query-timing")]
    pub(super) lexical_admission_lock_nanos: std::sync::atomic::AtomicU64,
}

#[cfg(all(feature = "query-timing", any(test, feature = "test-support")))]
impl TestMaterializationWork {
    pub(super) fn observe_lexical_lock(&self, started: std::time::Instant) {
        use std::sync::atomic::Ordering::Relaxed;
        self.lexical_admission_locks.fetch_add(1, Relaxed);
        self.lexical_admission_lock_nanos.fetch_add(
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            Relaxed,
        );
    }
}

/// Literal lifecycle and source-lookup observations for directed query tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializationTestCounters {
    /// Invocations of vector or lexical admission, including standalone stored_text.
    pub admissions: u64,
    /// Invocations of active or sealed reverse document-version lookup.
    pub reverse_version_lookups: u64,
    /// Ranked physical rows requested through the scoped materializer.
    pub direct_row_lookups: u64,
    /// Measured state, active, and snapshot lock acquisitions in lexical admission.
    #[cfg(feature = "query-timing")]
    pub lexical_admission_locks: u64,
    /// Wall nanoseconds in those acquisitions, including uncontended call overhead.
    #[cfg(feature = "query-timing")]
    pub lexical_admission_lock_nanos: u64,
}

/// One owned source row corresponding to a ranked query hit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedRow {
    /// The exact document version whose row was scored.
    pub document: DocumentVersion,
    /// Original physical row coordinates within the retrieval snapshot.
    pub row_id: GlobalRowId,
    /// One owned copy of the stored UTF-8 body.
    pub text: String,
}

/// A ranked row could not be materialized faithfully.
#[derive(Debug)]
pub enum MaterializationError {
    /// The caller requested a rank outside the returned list.
    RankOutOfRange {
        /// Zero-based rank requested by the caller.
        rank: usize,
        /// Number of ranked hits available.
        returned: usize,
    },
    /// The ranked row has no application identity.
    MissingIdentity {
        /// Physical row lacking an application identity.
        row_id: GlobalRowId,
    },
    /// The ranked document has no stored text.
    MissingText {
        /// Physical row lacking stored text.
        row_id: GlobalRowId,
        /// Ranked application identity.
        document: DocumentVersion,
    },
    /// The ranked physical source is absent from the admitted snapshot.
    MissingSource {
        /// Physical row whose source is absent.
        row_id: GlobalRowId,
    },
    /// A lexical source ordinal has no physical source in the admitted assembly.
    InvalidLexicalSource {
        /// Source ordinal from the ranked lexical hit.
        source: u32,
    },
    /// A fused application key has no retained source identity.
    MissingFusedIdentity {
        /// Fused application key lacking source coordinates.
        document: DocId,
    },
    /// The physical row does not contain the version that was ranked.
    IdentityMismatch {
        /// Physical row that failed identity validation.
        row_id: GlobalRowId,
        /// Application identity retained during ranking.
        expected: DocumentVersion,
        /// Application identity found at the physical row.
        actual: Option<DocumentVersion>,
    },
    /// The result could not reserve its owned text buffer.
    AllocationFailed {
        /// UTF-8 text bytes requested from the allocator.
        bytes: usize,
    },
    /// A materialization work count overflowed.
    ArithmeticOverflow,
    /// Cancellation or deadline prevented complete materialization.
    Query(QueryError),
    /// Storage validation or lifecycle failure.
    Storage(StoreError),
}

impl std::fmt::Display for MaterializationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RankOutOfRange { rank, returned } => {
                write!(f, "result rank {rank} is outside {returned} returned rows")
            }
            Self::MissingIdentity { row_id } => {
                write!(f, "result row {row_id:?} has no document identity")
            }
            Self::MissingText { row_id, document } => write!(
                f,
                "result row {row_id:?} for {document:?} has no stored text"
            ),
            Self::MissingSource { row_id } => {
                write!(f, "result row {row_id:?} is outside the admitted snapshot")
            }
            Self::InvalidLexicalSource { source } => write!(
                f,
                "lexical result source {source} is outside the admitted assembly"
            ),
            Self::MissingFusedIdentity { document } => write!(
                f,
                "fused document {document:?} has no ranked source identity"
            ),
            Self::IdentityMismatch {
                row_id,
                expected,
                actual,
            } => write!(
                f,
                "result row {row_id:?} expected {expected:?}, found {actual:?}"
            ),
            Self::AllocationFailed { bytes } => {
                write!(f, "result text allocation of {bytes} bytes failed")
            }
            Self::ArithmeticOverflow => f.write_str("result materialization counter overflow"),
            Self::Query(error) => error.fmt(f),
            Self::Storage(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for MaterializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Query(error) => Some(error),
            _ => None,
        }
    }
}

/// Rank-addressed text access available only inside a Store query callback.
pub struct QueryMaterializer<'a> {
    #[cfg(any(test, feature = "test-support"))]
    pub(super) store: &'a Store,
    pub(super) snapshot: &'a PublishedSnapshot,
    pub(super) active: &'a ActiveSegment,
    pub(super) cancellation: &'a QueryCancellation<'a>,
    pub(super) address: &'a dyn Fn(usize) -> Result<ResultAddress, MaterializationError>,
    pub(super) counts: Cell<crate::diag::MaterializationCounters>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct ResultAddress {
    pub(super) row_id: GlobalRowId,
    pub(super) document: Option<DocumentVersion>,
}

pub(super) type HybridAddresses = super::stats::Accounted<Vec<(DocId, ResultAddress)>>;

impl From<&SearchCandidate> for ResultAddress {
    fn from(candidate: &SearchCandidate) -> Self {
        Self {
            row_id: candidate.row_id(),
            document: candidate.document(),
        }
    }
}

impl QueryMaterializer<'_> {
    /// Copies the exact stored text and version associated with one result rank.
    pub fn text(&self, rank: usize) -> Result<MaterializedRow, MaterializationError> {
        self.check()?;
        let address = (self.address)(rank)?;
        self.materialize_address(address)
    }

    /// Plants a wrong or missing ranked identity at the actual physical-row validation seam.
    #[cfg(any(test, feature = "test-support"))]
    pub fn text_with_test_document(
        &self,
        rank: usize,
        document: Option<DocumentVersion>,
    ) -> Result<MaterializedRow, MaterializationError> {
        self.check()?;
        let mut address = (self.address)(rank)?;
        address.document = document;
        self.materialize_address(address)
    }

    fn materialize_address(
        &self,
        address: ResultAddress,
    ) -> Result<MaterializedRow, MaterializationError> {
        let row_id = address.row_id;
        let document = address
            .document
            .ok_or(MaterializationError::MissingIdentity { row_id })?;
        #[cfg(any(test, feature = "test-support"))]
        self.store
            .text_materialization_work
            .direct_row_lookups
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut counts = self.counts.get();
        counts.row_lookups = counts
            .row_lookups
            .checked_add(1)
            .ok_or(MaterializationError::ArithmeticOverflow)?;
        self.counts.set(counts);
        let row = row_id.local_row() as usize;
        let (actual, source) = match row_id.source() {
            RowSource::Active => (
                self.active.document(row),
                self.active
                    .text(row)
                    .map_err(MaterializationError::Storage)?,
            ),
            RowSource::Sealed(id) => {
                let segment = self
                    .snapshot
                    .segments()
                    .iter()
                    .find(|segment| segment.meta().id == id)
                    .ok_or(MaterializationError::MissingSource { row_id })?;
                let actual = segment
                    .document_version(row)
                    .map_err(StoreError::Segment)
                    .map_err(MaterializationError::Storage)?;
                let source = segment
                    .query_stored_text()
                    .map_err(StoreError::Segment)
                    .map_err(MaterializationError::Storage)?
                    .and_then(|text| text.row(row).flatten());
                (actual, source)
            }
        };
        if actual != Some(document) {
            return Err(MaterializationError::IdentityMismatch {
                row_id,
                expected: document,
                actual,
            });
        }
        let source = source.ok_or(MaterializationError::MissingText { row_id, document })?;
        let mut text = String::new();
        text.try_reserve_exact(source.len()).map_err(|_| {
            MaterializationError::AllocationFailed {
                bytes: source.len(),
            }
        })?;
        text.push_str(source);
        let mut counts = self.counts.get();
        counts.text_copies = counts
            .text_copies
            .checked_add(1)
            .ok_or(MaterializationError::ArithmeticOverflow)?;
        counts.text_bytes = counts
            .text_bytes
            .checked_add(source.len() as u64)
            .ok_or(MaterializationError::ArithmeticOverflow)?;
        self.counts.set(counts);
        self.check()?;
        Ok(MaterializedRow {
            document,
            row_id,
            text,
        })
    }

    fn check(&self) -> Result<(), MaterializationError> {
        self.cancellation
            .check_graph()
            .map_err(super::map_scan_error)
            .map_err(MaterializationError::Query)
    }
}

impl Store {
    /// Starts lexical retrieval before obtaining a caller-owned query vector.
    /// Both legs and returned text use one admission pinned before the producer
    /// runs. All submitted lexical work is joined on producer failure or panic.
    ///
    /// The returned vector borrow must remain valid for the complete query.
    /// Producer errors retain their original type; core failures are separate.
    ///
    /// # Errors
    /// Returns the producer's error or a typed admission/search failure.
    pub fn search_hybrid_with_text_deferred<'vector, E, R>(
        &self,
        prepare_vector: impl FnOnce() -> Result<crate::ingest::SearchRequest<'vector>, E>,
        lexical: &crate::fts::search::TermQuery,
        query: &crate::fusion::HybridQuery,
        options: impl Into<super::SearchOptions>,
        control: super::QueryControl,
        finish: impl FnOnce(&crate::ingest::StoreHybridSearchOutcome, &QueryMaterializer<'_>) -> R,
    ) -> Result<(crate::ingest::StoreHybridSearchOutcome, R), HybridPreparationError<E>> {
        self.search_hybrid_prepared_then(
            prepare_vector,
            super::PinnedLexicalQuery::Term(lexical),
            query,
            options.into(),
            control,
            true,
            |mut outcome, snapshot, active, cancellation, addresses| {
                cancellation.check_graph().map_err(super::map_scan_error)?;
                let address = |rank| {
                    let hit =
                        outcome
                            .hits
                            .get(rank)
                            .ok_or(MaterializationError::RankOutOfRange {
                                rank,
                                returned: outcome.hits.len(),
                            })?;
                    let addresses = addresses
                        .ok_or(MaterializationError::MissingFusedIdentity { document: hit.key })?;
                    let index = addresses
                        .binary_search_by_key(&hit.key, |(key, _)| *key)
                        .map_err(|_| MaterializationError::MissingFusedIdentity {
                            document: hit.key,
                        })?;
                    addresses
                        .get(index)
                        .map(|(_, address)| *address)
                        .ok_or(MaterializationError::MissingFusedIdentity { document: hit.key })
                };
                let materializer = QueryMaterializer {
                    #[cfg(any(test, feature = "test-support"))]
                    store: self,
                    snapshot,
                    active,
                    cancellation,
                    address: &address,
                    counts: Cell::new(Default::default()),
                };
                let value = finish(&outcome, &materializer);
                let counts = materializer.counts.get();
                outcome.diagnostics.materialization = Some(counts);
                cancellation.check_graph().map_err(super::map_scan_error)?;
                Ok((outcome, value))
            },
        )
    }

    /// Reads literal counters without resetting concurrent observations.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn query_materialization_test_counters(&self) -> MaterializationTestCounters {
        use std::sync::atomic::Ordering::Relaxed;
        MaterializationTestCounters {
            admissions: self.text_materialization_work.admissions.load(Relaxed),
            reverse_version_lookups: self
                .text_materialization_work
                .reverse_version_lookups
                .load(Relaxed),
            direct_row_lookups: self
                .text_materialization_work
                .direct_row_lookups
                .load(Relaxed),
            #[cfg(feature = "query-timing")]
            lexical_admission_locks: self
                .text_materialization_work
                .lexical_admission_locks
                .load(Relaxed),
            #[cfg(feature = "query-timing")]
            lexical_admission_lock_nanos: self
                .text_materialization_work
                .lexical_admission_lock_nanos
                .load(Relaxed),
        }
    }
    /// Runs hybrid fusion and constructs caller-owned results before releasing its snapshot.
    pub fn search_hybrid_with_text<R>(
        &self,
        vector: crate::ingest::SearchRequest<'_>,
        lexical: &crate::fts::search::TermQuery,
        query: &crate::fusion::HybridQuery,
        options: impl Into<super::SearchOptions>,
        control: super::QueryControl,
        finish: impl FnOnce(&crate::ingest::StoreHybridSearchOutcome, &QueryMaterializer<'_>) -> R,
    ) -> Result<(crate::ingest::StoreHybridSearchOutcome, R), crate::fusion::FusionError> {
        self.search_hybrid_with_text_deferred(
            || Ok::<_, std::convert::Infallible>(vector),
            lexical,
            query,
            options,
            control,
            finish,
        )
        .map_err(|error| match error {
            HybridPreparationError::Preparation(never) => match never {},
            HybridPreparationError::Search(error) => error,
        })
    }

    /// Runs a lexical query and constructs caller-owned results before releasing its snapshot.
    pub fn search_lexical_with_text<R>(
        &self,
        query: &crate::fts::search::TermQuery,
        k: usize,
        control: super::QueryControl,
        finish: impl FnOnce(&crate::ingest::StoreLexicalSearchOutcome, &QueryMaterializer<'_>) -> R,
    ) -> Result<(crate::ingest::StoreLexicalSearchOutcome, R), crate::ingest::StoreLexicalError>
    {
        self.search_lexical_then(
            query,
            k,
            control,
            |mut outcome, snapshot, active, assembly, hits, cancellation| {
                cancellation.check_graph().map_err(super::map_scan_error)?;
                let address = |rank| {
                    let candidate = outcome.candidates.get(rank).ok_or(
                        MaterializationError::RankOutOfRange {
                            rank,
                            returned: outcome.candidates.len(),
                        },
                    )?;
                    let hit = hits.get(rank).ok_or(MaterializationError::RankOutOfRange {
                        rank,
                        returned: hits.len(),
                    })?;
                    Ok(ResultAddress {
                        row_id: lexical_row_id(snapshot, &assembly.sources, hit.doc)?,
                        document: Some(candidate.document),
                    })
                };
                let materializer = QueryMaterializer {
                    #[cfg(any(test, feature = "test-support"))]
                    store: self,
                    snapshot,
                    active,
                    cancellation,
                    address: &address,
                    counts: Cell::new(Default::default()),
                };
                let value = finish(&outcome, &materializer);
                let counts = materializer.counts.get();
                outcome.diagnostics.materialization = Some(counts);
                cancellation.check_graph().map_err(super::map_scan_error)?;
                Ok((outcome, value))
            },
        )
    }

    /// Runs a dense query and constructs owned caller results through rank-addressed text access.
    pub fn search_with_text<R>(
        &self,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: impl Into<super::SearchOptions>,
        control: super::QueryControl,
        finish: impl FnOnce(&crate::ingest::SearchOutcome, &QueryMaterializer<'_>) -> R,
    ) -> Result<(crate::ingest::SearchOutcome, R), super::QueryError> {
        self.search_with_graph_bound_mode_then(
            request,
            k,
            options.into(),
            control,
            super::GraphBoundMode::Shared,
            |mut outcome, admitted, control| {
                let lease =
                    SnapshotLease::new_at(Arc::clone(&admitted.snapshot), admitted.generation);
                let cancellation = QueryCancellation::new(control, &lease);
                cancellation.check_graph().map_err(super::map_scan_error)?;
                let address = |rank| {
                    outcome.candidates.get(rank).map(ResultAddress::from).ok_or(
                        MaterializationError::RankOutOfRange {
                            rank,
                            returned: outcome.candidates.len(),
                        },
                    )
                };
                let materializer = QueryMaterializer {
                    #[cfg(any(test, feature = "test-support"))]
                    store: self,
                    counts: Cell::new(Default::default()),
                    snapshot: &admitted.snapshot,
                    active: &admitted.active_segment,
                    cancellation: &cancellation,
                    address: &address,
                };
                let value = finish(&outcome, &materializer);
                let counts = materializer.counts.get();
                outcome.diagnostics.materialization = Some(counts);
                cancellation.check_graph().map_err(super::map_scan_error)?;
                Ok((outcome, value))
            },
        )
    }
}

fn lexical_row_id(
    snapshot: &PublishedSnapshot,
    sources: &[super::StructuredLexicalSource],
    doc: crate::fts::search::GlobalDocId,
) -> Result<GlobalRowId, MaterializationError> {
    let source =
        sources
            .get(doc.segment as usize)
            .ok_or(MaterializationError::InvalidLexicalSource {
                source: doc.segment,
            })?;
    let source = match source {
        super::StructuredLexicalSource::Active => RowSource::Active,
        super::StructuredLexicalSource::Sealed(ordinal) => RowSource::Sealed(
            snapshot
                .segments()
                .get(*ordinal)
                .ok_or(MaterializationError::InvalidLexicalSource {
                    source: doc.segment,
                })?
                .meta()
                .id,
        ),
    };
    Ok(GlobalRowId::new(source, doc.row))
}
