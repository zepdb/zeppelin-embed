/// Frozen append-only status code returned by the C ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ZeErrorCode {
    /// Success.
    Ok = 0,
    /// A pointer, size, enum value, or request field was invalid.
    InvalidArgument = 1,
    /// The handle value was never valid.
    InvalidHandle = 2,
    /// The generation-tagged handle is closed or stale.
    Closed = 3,
    /// The store is currently closing.
    Closing = 4,
    /// A prior caught panic poisoned the handle.
    Poisoned = 5,
    /// A Rust panic was caught at the ABI boundary.
    Panic = 6,
    /// Another FFI writer call owns this handle's writer slot.
    Busy = 7,
    /// Another process or handle owns the store's kernel writer lock.
    StoreBusy = 8,
    /// An operating-system I/O operation failed.
    Io = 9,
    /// Persisted data failed validation.
    Corrupt = 10,
    /// The requested mode or operation is unsupported.
    Unsupported = 11,
    /// Cooperative cancellation stopped the operation.
    Cancelled = 12,
    /// A monotonic deadline expired.
    Timeout = 13,
    /// A checked allocation could not be reserved.
    OutOfMemory = 14,
    /// A configured memory, disk, or work budget was exceeded.
    BudgetExceeded = 15,
    /// A mutation batch or active segment was empty.
    EmptyBatch = 16,
    /// A document revision would move backward.
    StaleRevision = 17,
    /// Vector dimensions disagreed.
    DimensionMismatch = 18,
    /// A requested object was not found.
    NotFound = 19,
    /// A synchronization primitive was poisoned or unavailable.
    Synchronization = 20,
    /// The handle's access mode forbids the operation.
    AccessMode = 21,
    /// An invariant failed without a more specific public classification.
    Internal = 22,
    /// The caller's declared embedding epoch differs from the store identity.
    EpochMismatch = 23,
    /// The store requires an embedding epoch declaration, but none was supplied.
    EpochUndeclared = 24,
    /// An embedding epoch was declared for a store that has no stamped identity.
    EpochUnstamped = 25,
}

/// Opaque generation-tagged store handle.
pub type ZeHandle = u64;

/// Opaque generation-tagged cooperative-cancellation handle.
pub type ZeCancelToken = u64;

/// Largest request structure accepted by ABI v1.
pub const ZE_ABI_MAX_STRUCT_SIZE: u32 = 65_536;

/// Largest top-k request accepted by ABI v1.
pub const ZE_MAX_K: usize = 1 << 20;

/// Opens one store directory.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeOpenRequest {
    /// Caller-provided `sizeof(ZeOpenRequest)`.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned UTF-8 path bytes.
    pub path: *const u8,
    /// Number of path bytes.
    pub path_len: usize,
    /// `0` for read-write, `1` for read-only.
    pub access_mode: i32,
    /// `0` for derived, `1` for durable, `2` for attached.
    pub durability_mode: i32,
    /// `0` for none, `1` for ordered, `2` for durable.
    pub commit_tier: i32,
    /// Reader drain grace period in milliseconds.
    pub reader_drain_timeout_ms: u64,
    /// Exact resident-owned byte ceiling.
    pub max_resident_bytes: u64,
    /// Exact temporary byte ceiling.
    pub max_temp_bytes: u64,
}

/// Store lifecycle state report.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeStateReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// `0` open, `1` closing, or `2` closed.
    pub state: i32,
    /// Must be zero.
    pub reserved: u32,
}

/// Exact store resource counters.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeStatsReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Engine-owned anonymous bytes.
    pub resident_owned_bytes: u64,
    /// Live read-only mapping bytes.
    pub mapped_bytes: u64,
    /// Resident mapped-page bytes.
    pub mapped_resident_bytes: u64,
    /// Immutable segment mapping bytes.
    pub segment_bytes: u64,
    /// Active-segment allocation capacity.
    pub active_segment_bytes: u64,
    /// Active row count.
    pub active_row_count: u64,
    /// Active tombstone count.
    pub tombstone_count: u64,
    /// Active tombstone bytes.
    pub tombstone_bytes: u64,
    /// In-memory WAL bytes.
    pub wal_bytes: u64,
    /// Reusable graph-cache bytes.
    pub cache_bytes: u64,
    /// Live temporary bytes.
    pub temporary_bytes: u64,
    /// Persistent query-pool registry bytes.
    pub query_pool_bytes: u64,
    /// Retained file descriptor count.
    pub open_files: u64,
    /// Admitted active-query count.
    pub active_queries: u64,
    /// Externally held snapshot lease count.
    pub active_snapshot_leases: u64,
    /// Darwin physical footprint, or zero when unavailable.
    pub phys_footprint: u64,
    /// One when `phys_footprint` is available.
    pub has_phys_footprint: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Stable 128-bit application document identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct ZeDocId {
    /// Most-significant 64 bits.
    pub high: u64,
    /// Least-significant 64 bits.
    pub low: u64,
}

/// One document supplied to an ingest request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeIngestDocument {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Stable document identifier.
    pub doc_id: ZeDocId,
    /// Monotonic document revision.
    pub revision: u64,
    /// Canonical clustering timestamp.
    pub timestamp: i64,
    /// Caller-owned aligned f32 vector.
    pub vector: *const f32,
    /// Scalar count in `vector`.
    pub vector_len: usize,
    /// Caller-owned opaque metadata bytes.
    pub metadata: *const u8,
    /// Number of metadata bytes.
    pub metadata_len: usize,
}

/// Atomic document-ingest request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeIngestRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned `ZeIngestDocument` array.
    pub documents: *const ZeIngestDocument,
    /// Number of document records.
    pub document_count: usize,
    /// Required vector dimension for every record.
    pub dimension: usize,
}

/// Atomic document-delete request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeDeleteRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned document-id array.
    pub doc_ids: *const ZeDocId,
    /// Number of identifiers.
    pub doc_id_count: usize,
}

/// WAL and generation coordinates returned by ingest and delete.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeMutationReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Last committed WAL sequence.
    pub sequence: u64,
    /// Store generation changed by the mutation.
    pub generation: u64,
}

/// Vector search request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeSearchRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1, except for the feature-gated panic probe.
    pub abi_reserved: u32,
    /// Caller-owned aligned f32 query vector.
    pub vector: *const f32,
    /// Scalar count in `vector`.
    pub vector_len: usize,
    /// Declared vector dimension; must equal `vector_len`.
    pub dimension: usize,
    /// Number of requested candidates.
    pub k: usize,
    /// Zero selects all detected physical performance cores.
    pub thread_budget: usize,
    /// `0` automatic, `1` exact scan, `2` explicit graph.
    pub search_tier: i32,
    /// `0` SIFT-class or `1` angular graph defaults.
    pub graph_profile: i32,
    /// Explicit graph width, or zero for adaptive width.
    pub graph_ef: usize,
    /// Deterministic graph query-preparation seed.
    pub graph_seed: u64,
    /// Optional generation-tagged cancellation token; zero means absent.
    pub cancel_token: ZeCancelToken,
    /// Relative monotonic deadline in nanoseconds; zero means absent.
    pub deadline_ns: u64,
}

/// One callee-owned search hit.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeSearchHit {
    /// `0` for active state or `1` for an immutable segment.
    pub source_kind: u32,
    /// Must be zero.
    pub reserved: u32,
    /// Immutable segment id; all zero for active state.
    pub segment_id: [u8; 16],
    /// Dense source-local row id.
    pub local_row: u32,
    /// One when document identity is present.
    pub has_document: u32,
    /// Document id when `has_document` is one.
    pub doc_id: ZeDocId,
    /// Document revision when `has_document` is one.
    pub revision: u64,
    /// Larger-is-better exact score.
    pub score: f32,
    /// Must be zero.
    pub reserved_tail: u32,
}

/// Callee-owned search result; release with `ze_search_result_free`.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeSearchResult {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Callee-owned hit array, or null when `hit_count` is zero.
    pub hits: *mut ZeSearchHit,
    /// Number of initialized hits.
    pub hit_count: usize,
    /// Pinned store generation searched.
    pub generation: u64,
    /// Logical row-coordinate multiply-accumulates.
    pub dims_touched: u64,
    /// Row-major payload bytes read.
    pub bytes_read: u64,
    /// Query workers that scored partitions.
    pub threads_used: u64,
    /// Sealed segments traversed through graphs.
    pub graph_segments_traversed: u64,
    /// Complete graph validations.
    pub graph_validations: u64,
    /// Entry-seed full scans.
    pub graph_entry_seed_discoveries: u64,
    /// Visited-array epoch clears.
    pub graph_visited_epoch_clears: u64,
    /// Graph estimator candidates scored.
    pub graph_candidates_scored: u64,
    /// Graph candidates exactly rescored.
    pub graph_candidates_rescored: u64,
    /// Segments rejected by the shared bound.
    pub graph_segments_pruned_by_bound: u64,
}

/// Explicit seal request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeSealRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Optional generation-tagged cancellation token.
    pub cancel_token: ZeCancelToken,
}

/// A generation returned by seal.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeGenerationReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Committed generation.
    pub generation: u64,
}

/// Whole-segment timestamp partition request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeDropPartitionRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Inclusive timestamp lower bound.
    pub start_ts: i64,
    /// Exclusive timestamp upper bound.
    pub end_ts: i64,
}

/// Retention-window request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeRetentionRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Positive retention window in caller timestamp units.
    pub window: i64,
    /// Current timestamp in the same unit.
    pub now_ts: i64,
}

/// Whole-segment partition mutation report.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZePartitionReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Committed or observed generation.
    pub generation: u64,
    /// Number of immutable segments dropped.
    pub segments_dropped: u64,
    /// Exact immutable bytes reclaimed.
    pub bytes_reclaimed: u64,
    /// Number of overlapping segments retained.
    pub straddlers_skipped: u64,
    /// One when no manifest commit or unlink occurred.
    pub is_no_op: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Physical-purge scheduling request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZePurgeRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Caller-owned document-id array.
    pub doc_ids: *const ZeDocId,
    /// Number of identifiers.
    pub doc_id_count: usize,
}

/// Opaque physical-purge token report.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZePurgeTokenReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Store-scoped opaque token id.
    pub token_id: u64,
    /// Generation observed when scheduled.
    pub generation: u64,
    /// Number of requested ids absent from the store.
    pub unknown_id_count: u64,
    /// One when no physical work is required.
    pub is_no_op: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Waits for one previously scheduled physical purge.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeAwaitPurgeRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Store-scoped opaque token id returned by `ze_purge`.
    pub token_id: u64,
}

/// Completed physical-purge report.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZePurgeReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Generation at which the physical guarantee holds.
    pub generation: u64,
    /// Number of sealed segments rewritten.
    pub segments_rewritten: u64,
    /// Number of requested ids absent from the store.
    pub unknown_id_count: u64,
    /// One when the WAL was atomically replaced.
    pub wal_rewritten: u32,
    /// One when no artifact needed mutation.
    pub is_no_op: u32,
}

/// Host-bounded maintenance request.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeMaintainRequest {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Maximum monotonic wall time in nanoseconds.
    pub wall_time_ns: u64,
    /// Maximum graph work bytes.
    pub bytes: u64,
}

/// Host-bounded maintenance report.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ZeMaintainReport {
    /// Caller-provided structure size.
    pub abi_size: u32,
    /// Must be zero in ABI v1.
    pub abi_reserved: u32,
    /// Graph generations published.
    pub graphs_built: u64,
    /// Graph work bytes consumed.
    pub bytes_consumed: u64,
    /// Checkpointed graph builds resumed.
    pub checkpoints_resumed: u64,
    /// `0` complete or `1` budget exhausted.
    pub status: i32,
    /// Must be zero.
    pub reserved: u32,
}
